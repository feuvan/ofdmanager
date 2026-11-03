#!/usr/bin/env bash
#
# Build the native macOS Quick Look extensions for OFD files.
#
# Produces two sandboxed `.appex` bundles (data-based preview + thumbnail) that
# Tauri embeds into `OFD Manager.app/Contents/PlugIns/`:
#
#   $OUT_DIR/OFDQuickLookPreview.appex/Contents/{Info.plist,MacOS/OFDQuickLookPreview}
#   $OUT_DIR/OFDQuickLookThumbnail.appex/Contents/{Info.plist,MacOS/OFDQuickLookThumbnail}
#
# Each appex is a thin Swift executable linked against the Rust staticlib
# `libofd_quicklook.a` (crates/ofd-quicklook) with entry point `_NSExtensionMain`,
# then ad-hoc (or identity-) signed with the Quick Look sandbox entitlements.
#
# Usage:  scripts/build-quicklook.sh [--check]
#
#   --check   after building, validate the bundles: plutil -lint, codesign
#             --verify --strict, sandbox entitlements, NSExtension keys,
#             exec fatality; print PASS lines and exit non-zero on problems.
#
# Environment inputs (all optional):
#   TARGET_TRIPLE   Rust target triple. Default: $TAURI_ENV_TARGET_TRIPLE when set,
#                   else derived from $TAURI_ENV_ARCH (aarch64/x86_64), else the
#                   rustc host triple.
#   PROFILE         cargo profile (default: release)
#   SIGN_IDENTITY   codesign identity (default: $APPLE_SIGNING_IDENTITY, else "-"
#                   ad-hoc, which is what local/CI builds use)
#   OUT_DIR         output dir (default: apps/desktop/src-tauri/gen/quicklook)
#   VERSION         bundle version (default: [workspace.package] version of Cargo.toml)
#
# Local/staging escape hatches used by scripts/test-quicklook.sh and for testing
# the Swift side before the Rust crate lands (never needed by Tauri/CI):
#   OFD_QUICKLOOK_INCLUDE_DIR  dir containing ofd_quicklook.h
#   OFD_QUICKLOOK_LIB_PATH     explicit path to libofd_quicklook.a
#   OFD_QUICKLOOK_SKIP_CARGO   "1" to skip `cargo build` (library already built)
#
# Non-macOS hosts print a skip message and exit 0 so Windows/Linux Tauri builds
# never fail.
#
set -euo pipefail

usage() {
    sed -n '2,45p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

CHECK=0
while (($#)); do
    case "$1" in
        --check) CHECK=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) echo "build-quicklook: unknown argument: $1" >&2; exit 2 ;;
    esac
done

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

log()  { printf '== %s\n' "$*"; }
pass() { printf 'PASS %s\n' "$*"; }
die()  { printf 'FAIL %s\n' "$*" >&2; exit 1; }

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "build-quicklook: host is $(uname -s), not macOS; skipping OFD Quick Look extension build."
    exit 0
fi

[[ -f "$REPO_ROOT/Cargo.toml" ]] \
    || die "cannot locate repository root from script location ($SCRIPT_DIR)"

# ---------------------------------------------------------------------------
# Inputs
# ---------------------------------------------------------------------------
resolve_target_triple() {
    if [[ -n "${TARGET_TRIPLE:-}" ]]; then
        return
    fi
    if [[ -n "${TAURI_ENV_TARGET_TRIPLE:-}" ]]; then
        TARGET_TRIPLE="$TAURI_ENV_TARGET_TRIPLE"
        return
    fi
    if [[ -n "${TAURI_ENV_ARCH:-}" ]]; then
        case "$TAURI_ENV_ARCH" in
            aarch64|arm64) TARGET_TRIPLE="aarch64-apple-darwin" ;;
            x86_64|x64)    TARGET_TRIPLE="x86_64-apple-darwin" ;;
            *) die "unsupported TAURI_ENV_ARCH '$TAURI_ENV_ARCH' (want aarch64 or x86_64)" ;;
        esac
        return
    fi
    command -v rustc >/dev/null 2>&1 || die "rustc not found on PATH"
    TARGET_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
    [[ -n "$TARGET_TRIPLE" ]] || die "could not determine rustc host triple"
}
resolve_target_triple

case "$TARGET_TRIPLE" in
    aarch64-apple-darwin) SWIFT_ARCH="arm64" ;;
    x86_64-apple-darwin)  SWIFT_ARCH="x86_64" ;;
    *) die "unsupported TARGET_TRIPLE '$TARGET_TRIPLE' (want aarch64-apple-darwin or x86_64-apple-darwin)" ;;
esac

PROFILE="${PROFILE:-release}"
if [[ "$PROFILE" == "debug" ]]; then
    CARGO_PROFILE="dev"
    CARGO_PROFILE_DIR="debug"
else
    CARGO_PROFILE="$PROFILE"
    CARGO_PROFILE_DIR="$PROFILE"
fi

SIGN_IDENTITY="${SIGN_IDENTITY:-${APPLE_SIGNING_IDENTITY:--}}"

if [[ -z "${OUT_DIR:-}" ]]; then
    OUT_DIR="$REPO_ROOT/apps/desktop/src-tauri/gen/quicklook"
fi
case "$OUT_DIR" in
    /*) ;;
    *) OUT_DIR="$(pwd)/$OUT_DIR" ;;
esac

if [[ -z "${VERSION:-}" ]]; then
    VERSION="$(awk '
        /^\[workspace\.package\]/ { in_section = 1; next }
        /^\[/                    { in_section = 0 }
        in_section && /^version[[:space:]]*=/ {
            sub(/^[^"]*"/, ""); sub(/".*$/, ""); print; exit
        }' "$REPO_ROOT/Cargo.toml")"
fi
[[ -n "$VERSION" ]] || die "could not determine workspace VERSION from $REPO_ROOT/Cargo.toml"

QUICKLOOK_DIR="$REPO_ROOT/apps/desktop/macos/quicklook"
SOURCES_DIR="$QUICKLOOK_DIR/Sources"
SUPPORT_DIR="$QUICKLOOK_DIR/Support"
INCLUDE_DIR="${OFD_QUICKLOOK_INCLUDE_DIR:-$REPO_ROOT/crates/ofd-quicklook/include}"
HEADER="$INCLUDE_DIR/ofd_quicklook.h"

# Cargo's target root (honours CARGO_TARGET_DIR; relative values resolve against
# the repo root because that is the cwd of our cargo invocation).
CARGO_TARGET_ROOT="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
case "$CARGO_TARGET_ROOT" in
    /*) ;;
    *) CARGO_TARGET_ROOT="$REPO_ROOT/$CARGO_TARGET_ROOT" ;;
esac
LIB_PATH="${OFD_QUICKLOOK_LIB_PATH:-$CARGO_TARGET_ROOT/$TARGET_TRIPLE/$CARGO_PROFILE_DIR/libofd_quicklook.a}"
BUILD_ROOT="$CARGO_TARGET_ROOT/quicklook-build/$TARGET_TRIPLE-$CARGO_PROFILE_DIR"

log "repo:           $REPO_ROOT"
log "target triple:  $TARGET_TRIPLE (swift arch $SWIFT_ARCH, macos12.0)"
log "cargo profile:  $CARGO_PROFILE"
log "sign identity:  $SIGN_IDENTITY"
log "version:        $VERSION"
log "output dir:     $OUT_DIR"

for tool in cargo swiftc codesign plutil; do
    command -v "$tool" >/dev/null 2>&1 || die "$tool not found on PATH"
done
[[ -d "$SOURCES_DIR" ]] || die "missing Swift sources: $SOURCES_DIR"
[[ -f "$SUPPORT_DIR/Preview-Info.plist" ]] || die "missing plist template: $SUPPORT_DIR/Preview-Info.plist"
[[ -f "$SUPPORT_DIR/Thumbnail-Info.plist" ]] || die "missing plist template: $SUPPORT_DIR/Thumbnail-Info.plist"
[[ -f "$SUPPORT_DIR/quicklook.entitlements" ]] || die "missing entitlements: $SUPPORT_DIR/quicklook.entitlements"

# Fail early (with an actionable message) when the Rust side has not landed yet.
[[ -f "$HEADER" ]] \
    || die "C ABI header not found: $HEADER (the Rust crate crates/ofd-quicklook must be built first)"

# ---------------------------------------------------------------------------
# 0. Serialize concurrent invocations
#
# This script is called both by Tauri (beforeBundleCommand) and by developers/CI,
# and it rewrites the same OUT_DIR appex bundles. Two instances racing on one
# output directory can delete a bundle while the other is codesigning it, so take
# a simple mkdir-based lock (portable, no flock on stock macOS).
# ---------------------------------------------------------------------------
BUILD_LOCK="$BUILD_ROOT/.build.lock"
LOCK_HELD=0
release_lock() {
    if [[ "$LOCK_HELD" == "1" ]]; then
        rm -rf "$BUILD_LOCK"
        LOCK_HELD=0
    fi
}
acquire_build_lock() {
    mkdir -p "$BUILD_ROOT"
    local waited=0 owner=""
    while ! mkdir "$BUILD_LOCK" 2>/dev/null; do
        owner="$(cat "$BUILD_LOCK/pid" 2>/dev/null || true)"
        if [[ -n "$owner" ]] && ! kill -0 "$owner" 2>/dev/null; then
            rm -rf "$BUILD_LOCK" # stale lock left by a dead process
            continue
        fi
        waited=$((waited + 1))
        ((waited <= 300)) || die "timed out waiting for build lock: $BUILD_LOCK"
        sleep 1
    done
    printf '%s\n' "$$" > "$BUILD_LOCK/pid"
    LOCK_HELD=1
}
trap release_lock EXIT INT TERM
acquire_build_lock

# ---------------------------------------------------------------------------
# 1. Rust staticlib
# ---------------------------------------------------------------------------
if [[ "${OFD_QUICKLOOK_SKIP_CARGO:-0}" == "1" ]]; then
    log "skipping cargo build (OFD_QUICKLOOK_SKIP_CARGO=1); using $LIB_PATH"
else
    log "cargo build -p ofd-quicklook --profile $CARGO_PROFILE --target $TARGET_TRIPLE"
    ( cd "$REPO_ROOT" && cargo build -p ofd-quicklook --profile "$CARGO_PROFILE" --target "$TARGET_TRIPLE" )
fi
[[ -f "$LIB_PATH" ]] || die "Rust staticlib not found: $LIB_PATH"

LIB_DIR="$(dirname -- "$LIB_PATH")"
LIB_NAME="$(basename -- "$LIB_PATH")"
LIB_NAME="${LIB_NAME#lib}"
LIB_NAME="${LIB_NAME%.a}"
[[ -n "$LIB_NAME" ]] || die "cannot derive -l name from $LIB_PATH"

mkdir -p "$BUILD_ROOT/modulecache" "$OUT_DIR"

# ---------------------------------------------------------------------------
# 2. Assemble + compile + sign each appex
# ---------------------------------------------------------------------------
SWIFT_FLAGS=(
    -parse-as-library
    -O
    -swift-version 5
    -target "$SWIFT_ARCH-apple-macos12.0"
    -module-cache-path "$BUILD_ROOT/modulecache"
    -import-objc-header "$SUPPORT_DIR/OFDQuickLookBridge.h"
    -I "$INCLUDE_DIR"
)
# Frameworks: QuickLookUI (QLPreviewProvider/QLPreviewReply/QLPreviewingController),
# QuickLookThumbnailing (QLThumbnailProvider/QLThumbnailReply), UniformTypeIdentifiers
# (UTType.pdf), AppKit + CoreText keep the Quick Look host protocols / text APIs happy.
LINK_FLAGS=(
    -L "$LIB_DIR" "-l$LIB_NAME"
    -framework AppKit
    -framework QuickLookUI
    -framework QuickLookThumbnailing
    -framework UniformTypeIdentifiers
    -framework CoreText
    -framework Foundation
    -Xlinker -e -Xlinker _NSExtensionMain
)

substitute_plist() {
    local template="$1" destination="$2" executable="$3" bundle_id="$4" principal="$5"
    sed \
        -e "s|@EXECUTABLE@|$executable|g" \
        -e "s|@BUNDLE_IDENTIFIER@|$bundle_id|g" \
        -e "s|@PRINCIPAL_CLASS@|$principal|g" \
        -e "s|@VERSION@|$VERSION|g" \
        "$template" > "$destination"
}

codesign_appex() {
    local appex="$1"
    local sign_args=(--force --sign "$SIGN_IDENTITY" --entitlements "$SUPPORT_DIR/quicklook.entitlements")
    # Ad-hoc signature cannot carry a secure timestamp; skip the network round-trip.
    if [[ "$SIGN_IDENTITY" == "-" ]]; then
        sign_args+=(--timestamp=none)
    fi
    codesign "${sign_args[@]}" "$appex"
}

build_appex() {
    local appex_name="$1"       # OFDQuickLookPreview.appex
    local executable="$2"       # OFDQuickLookPreview
    local bundle_id="$3"        # com.ofdmanager.desktop.quicklook.preview
    local principal="$4"        # PreviewProvider
    local plist_template="$5"   # Support/Preview-Info.plist
    local swift_source="$6"     # OFDQuickLookPreview.swift

    local appex="$OUT_DIR/$appex_name.appex"
    local contents="$appex/Contents"
    local exe="$contents/MacOS/$executable"

    log "compiling $executable -> $appex"
    rm -rf "$appex"
    mkdir -p "$contents/MacOS"

    # Info.plist must exist before signing: codesign seals it and takes the bundle
    # identifier from it.
    substitute_plist "$plist_template" "$contents/Info.plist" "$executable" "$bundle_id" "$principal"

    swiftc "${SWIFT_FLAGS[@]}" \
        "$SOURCES_DIR/OFDQuickLook.swift" \
        "$SOURCES_DIR/$swift_source" \
        -o "$exe" \
        "${LINK_FLAGS[@]}"

    chmod +x "$exe"
    codesign_appex "$appex"
    pass "built + signed $appex_name.appex"
}

build_appex "OFDQuickLookPreview" "OFDQuickLookPreview" \
    "com.ofdmanager.desktop.quicklook.preview" "PreviewProvider" \
    "$SUPPORT_DIR/Preview-Info.plist" "OFDQuickLookPreview.swift"
build_appex "OFDQuickLookThumbnail" "OFDQuickLookThumbnail" \
    "com.ofdmanager.desktop.quicklook.thumbnail" "ThumbnailProvider" \
    "$SUPPORT_DIR/Thumbnail-Info.plist" "OFDQuickLookThumbnail.swift"

# ---------------------------------------------------------------------------
# 3. --check validation
# ---------------------------------------------------------------------------
plist_raw() {
    # $1 = plist path, $2 = key path (dots separate dictionary keys)
    plutil -extract "$2" raw -o - "$1" 2>/dev/null || true
}

check_plist_raw() {
    local plist="$1" keypath="$2" expected="$3" label="$4"
    local actual
    actual="$(plist_raw "$plist" "$keypath")"
    [[ "$actual" == "$expected" ]] \
        || die "$label: expected '$expected', got '${actual:-<missing>}' ($plist)"
    pass "$label = $expected"
}

check_entitlement_true() {
    local appex="$1" entitlement="$2"
    local entitlements
    entitlements="$(codesign -d --entitlements :- "$appex" 2>/dev/null || true)"
    [[ -n "$entitlements" ]] || die "no entitlements embedded in $appex"
    # Entitlement keys contain dots, so compare the exact key/value XML rather than
    # using a plutil key path (plutil would treat the dots as nesting).
    if printf '%s' "$entitlements" | tr -d '[:space:]' | grep -q "<key>$entitlement</key><true/>"; then
        pass "entitlement $entitlement = true ($(basename "$appex"))"
    else
        die "entitlement '$entitlement' is missing or not true in $appex"
    fi
}

check_appex() {
    local appex_name="$1"        # OFDQuickLookPreview.appex
    local bundle_id="$2"
    local extension_point="$3"   # com.apple.quicklook.preview
    local principal="$4"         # PreviewProvider
    local executable="$5"        # OFDQuickLookPreview

    local appex="$OUT_DIR/$appex_name.appex"
    local plist="$appex/Contents/Info.plist"
    local exe="$appex/Contents/MacOS/$executable"

    log "checking $appex_name.appex"
    [[ -d "$appex" ]] || die "missing appex bundle: $appex"
    pass "appex exists: $appex_name.appex"

    [[ -f "$plist" ]] || die "missing Info.plist: $plist"
    plutil -lint "$plist" >/dev/null || die "plutil -lint failed: $plist"
    pass "plutil -lint $appex_name.appex/Contents/Info.plist"

    [[ -x "$exe" ]] || die "missing/not executable: $exe"
    file "$exe" | grep -q 'Mach-O' || die "not a Mach-O executable: $exe"
    pass "executable exists (Mach-O): $appex_name.appex/Contents/MacOS/$executable"

    check_plist_raw "$plist" "CFBundlePackageType" "XPC!" "$appex_name CFBundlePackageType"
    check_plist_raw "$plist" "CFBundleIdentifier" "$bundle_id" "$appex_name CFBundleIdentifier"
    check_plist_raw "$plist" "CFBundleExecutable" "$executable" "$appex_name CFBundleExecutable"
    check_plist_raw "$plist" "LSMinimumSystemVersion" "12.0" "$appex_name LSMinimumSystemVersion"
    check_plist_raw "$plist" "NSExtension.NSExtensionPointIdentifier" "$extension_point" \
        "$appex_name NSExtensionPointIdentifier"
    check_plist_raw "$plist" "NSExtension.NSExtensionPrincipalClass" "$principal" \
        "$appex_name NSExtensionPrincipalClass"

    local content_types
    content_types="$(plutil -extract NSExtension.NSExtensionAttributes.QLSupportedContentTypes json -o - "$plist" 2>/dev/null || true)"
    [[ "$content_types" == *"public.ofd"* && "$content_types" == *"com.ofdmanager.ofd"* ]] \
        || die "$appex_name QLSupportedContentTypes must contain public.ofd and com.ofdmanager.ofd (got: ${content_types:-<missing>})"
    pass "$appex_name QLSupportedContentTypes = public.ofd, com.ofdmanager.ofd"

    codesign --verify --strict "$appex" 2>/dev/null \
        || die "codesign --verify --strict failed: $appex"
    pass "codesign --verify --strict $appex_name.appex"

    local identifier
    identifier="$(codesign -dv --verbose=4 "$appex" 2>&1 >/dev/null | sed -n 's/^Identifier=//p')"
    [[ "$identifier" == "$bundle_id" ]] \
        || die "$appex_name signed identifier mismatch: expected '$bundle_id', got '${identifier:-<unknown>}'"
    pass "$appex_name signed identifier = $bundle_id"

    check_entitlement_true "$appex" "com.apple.security.app-sandbox"
    check_entitlement_true "$appex" "com.apple.security.files.user-selected.read-only"
}

if ((CHECK)); then
    check_appex "OFDQuickLookPreview" \
        "com.ofdmanager.desktop.quicklook.preview" "com.apple.quicklook.preview" \
        "PreviewProvider" "OFDQuickLookPreview"
    check_appex "OFDQuickLookThumbnail" \
        "com.ofdmanager.desktop.quicklook.thumbnail" "com.apple.quicklook.thumbnail" \
        "ThumbnailProvider" "OFDQuickLookThumbnail"

    printf '\n'
    pass "quicklook build: OFDQuickLookPreview.appex OFDQuickLookThumbnail.appex"
    pass "quicklook output dir: $OUT_DIR"
else
    log "built bundles (run with --check to validate):"
    log "  $OUT_DIR/OFDQuickLookPreview.appex"
    log "  $OUT_DIR/OFDQuickLookThumbnail.appex"
fi
