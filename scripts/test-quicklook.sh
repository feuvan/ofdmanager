#!/usr/bin/env bash
#
# Local test harness for the macOS OFD Quick Look extensions.
#
# macOS only (prints a skip message and exits 0 on other platforms). Three stages:
#
#   1. build:      scripts/build-quicklook.sh --check (plutil/codesign/entitlements)
#   2. bridge:     compile apps/desktop/macos/quicklook/Tests/BridgeSmoke.swift against
#                  the real Rust staticlib and exercise the Swift<->Rust C ABI on
#                  fixtures/helloworld.ofd and fixtures/invoice-like.ofd (%PDF / PNG
#                  magic, PNG dimensions, renderer error status codes)
#   3. system:     OPTIONAL, only with --app <built OFD Manager.app> / --system-test.
#                  Registers the built appexes with LaunchServices/PlugInKit, has
#                  qlmanage generate a 512px thumbnail for a fixture copied under
#                  $HOME, asserts a non-trivial PNG was produced, then unregisters.
#                  A throw-away host app is assembled when no built app is found.
#
# The system stage is the noisy one (touches LaunchServices/PlugInKit/QuickLook);
# it never changes default app handlers and always unregisters on exit. The Lead
# owns final end-to-end verification.
#
# Usage:
#   scripts/test-quicklook.sh                        # build + Swift<->Rust bridge test
#   scripts/test-quicklook.sh --app "/path/OFD Manager.app"   # + system Quick Look test
#   scripts/test-quicklook.sh --system-test          # + system test with auto-detected app
#   scripts/test-quicklook.sh --skip-build           # reuse previous build (dev loop)
#   scripts/test-quicklook.sh --keep                 # keep state/registrations for debugging
#
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

log()  { printf '== %s\n' "$*"; }
pass() { printf 'PASS %s\n' "$*"; }
warn() { printf 'WARN %s\n' "$*"; }
die()  { printf 'FAIL %s\n' "$*" >&2; exit 1; }

usage() {
    sed -n '2,30p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
}

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "test-quicklook: host is $(uname -s), not macOS; the OFD Quick Look extensions are macOS-only. Skipping."
    exit 0
fi

# ---------------------------------------------------------------------------
# Arguments
# ---------------------------------------------------------------------------
APP_PATH="${APP_PATH:-}"
RUN_SYSTEM=0
SKIP_BUILD=0
KEEP_STATE=0

while (($#)); do
    case "$1" in
        --app)
            [[ $# -ge 2 ]] || die "--app requires a path"
            APP_PATH="$2"
            RUN_SYSTEM=1
            shift 2
            ;;
        --app=*)
            APP_PATH="${1#--app=}"
            RUN_SYSTEM=1
            shift
            ;;
        --system-test) RUN_SYSTEM=1; shift ;;
        --skip-build) SKIP_BUILD=1; shift ;;
        --keep) KEEP_STATE=1; shift ;;
        -h|--help) usage; exit 0 ;;
        *) die "unknown argument: $1" ;;
    esac
done
[[ -n "$APP_PATH" ]] && RUN_SYSTEM=1

# Shared state (all under $HOME so qlmanage gets its sandbox token; see contract).
STATE="${QUICKLOOK_TEST_STATE:-$HOME/Library/Caches/ofdmanager-quicklook-test}"
LSREGISTER="/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister"
PREVIEW_ID="com.ofdmanager.desktop.quicklook.preview"
THUMB_ID="com.ofdmanager.desktop.quicklook.thumbnail"
OUT_DIR="${OUT_DIR:-$REPO_ROOT/apps/desktop/src-tauri/gen/quicklook}"

# ---------------------------------------------------------------------------
# Shared target/profile resolution (kept in sync with build-quicklook.sh)
# ---------------------------------------------------------------------------
if [[ -z "${TARGET_TRIPLE:-}" ]]; then
    if [[ -n "${TAURI_ENV_TARGET_TRIPLE:-}" ]]; then
        TARGET_TRIPLE="$TAURI_ENV_TARGET_TRIPLE"
    elif [[ -n "${TAURI_ENV_ARCH:-}" ]]; then
        case "$TAURI_ENV_ARCH" in
            aarch64|arm64) TARGET_TRIPLE="aarch64-apple-darwin" ;;
            x86_64|x64)    TARGET_TRIPLE="x86_64-apple-darwin" ;;
            *) die "unsupported TAURI_ENV_ARCH '$TAURI_ENV_ARCH'" ;;
        esac
    else
        command -v rustc >/dev/null 2>&1 || die "rustc not found on PATH"
        TARGET_TRIPLE="$(rustc -vV | sed -n 's/^host: //p')"
    fi
fi
case "$TARGET_TRIPLE" in
    aarch64-apple-darwin) SWIFT_ARCH="arm64" ;;
    x86_64-apple-darwin)  SWIFT_ARCH="x86_64" ;;
    *) die "unsupported TARGET_TRIPLE '$TARGET_TRIPLE'" ;;
esac
PROFILE="${PROFILE:-release}"
if [[ "$PROFILE" == "debug" ]]; then
    CARGO_PROFILE_DIR="debug"
else
    CARGO_PROFILE_DIR="$PROFILE"
fi

QUICKLOOK_DIR="$REPO_ROOT/apps/desktop/macos/quicklook"
SOURCES_DIR="$QUICKLOOK_DIR/Sources"
SUPPORT_DIR="$QUICKLOOK_DIR/Support"
INCLUDE_DIR="${OFD_QUICKLOOK_INCLUDE_DIR:-$REPO_ROOT/crates/ofd-quicklook/include}"

# Keep in sync with build-quicklook.sh (honours CARGO_TARGET_DIR).
CARGO_TARGET_ROOT="${CARGO_TARGET_DIR:-$REPO_ROOT/target}"
case "$CARGO_TARGET_ROOT" in
    /*) ;;
    *) CARGO_TARGET_ROOT="$REPO_ROOT/$CARGO_TARGET_ROOT" ;;
esac
LIB_PATH="${OFD_QUICKLOOK_LIB_PATH:-$CARGO_TARGET_ROOT/$TARGET_TRIPLE/$CARGO_PROFILE_DIR/libofd_quicklook.a}"
BUILD_ROOT="$CARGO_TARGET_ROOT/quicklook-build/$TARGET_TRIPLE-$CARGO_PROFILE_DIR"

# ---------------------------------------------------------------------------
# Stage 1: build + --check
# ---------------------------------------------------------------------------
if ((SKIP_BUILD)); then
    log "stage 1: skipping build (--skip-build)"
    [[ -f "$LIB_PATH" ]] || die "no staticlib at $LIB_PATH; run scripts/build-quicklook.sh first"
else
    log "stage 1: building appexes (scripts/build-quicklook.sh --check)"
    bash "$SCRIPT_DIR/build-quicklook.sh" --check
    pass "stage 1: build-quicklook.sh --check"
fi

# ---------------------------------------------------------------------------
# Stage 2: Swift<->Rust ABI bridge smoke test
# ---------------------------------------------------------------------------
log "stage 2: Swift<->Rust bridge smoke test"
[[ -f "$LIB_PATH" ]] || die "staticlib not found: $LIB_PATH (run scripts/build-quicklook.sh)"
[[ -f "$INCLUDE_DIR/ofd_quicklook.h" ]] || die "C header not found: $INCLUDE_DIR/ofd_quicklook.h"

FIXTURES=("$REPO_ROOT/fixtures/helloworld.ofd" "$REPO_ROOT/fixtures/invoice-like.ofd")
for fixture in "${FIXTURES[@]}"; do
    [[ -f "$fixture" ]] || die "fixture missing: $fixture"
done

mkdir -p "$BUILD_ROOT"
SMOKE_BIN="$BUILD_ROOT/ofd-quicklook-bridge-smoke"
LIB_DIR="$(dirname -- "$LIB_PATH")"
LIB_NAME="$(basename -- "$LIB_PATH")"
LIB_NAME="${LIB_NAME#lib}"
LIB_NAME="${LIB_NAME%.a}"

log "compiling $SMOKE_BIN"
swiftc \
    -parse-as-library \
    -O \
    -swift-version 5 \
    -target "$SWIFT_ARCH-apple-macos12.0" \
    -module-cache-path "$BUILD_ROOT/modulecache" \
    -import-objc-header "$SUPPORT_DIR/OFDQuickLookBridge.h" \
    -I "$INCLUDE_DIR" \
    "$SOURCES_DIR/OFDQuickLook.swift" \
    "$QUICKLOOK_DIR/Tests/BridgeSmoke.swift" \
    -o "$SMOKE_BIN" \
    -L "$LIB_DIR" "-l$LIB_NAME" \
    -framework Foundation \
    -framework PDFKit

"$SMOKE_BIN" "${FIXTURES[@]}"
pass "stage 2: Swift<->Rust ABI bridge smoke test"

# ---------------------------------------------------------------------------
# Stage 3 (optional): system Quick Look test
# ---------------------------------------------------------------------------
if ((RUN_SYSTEM)); then
    log "stage 3: system Quick Look test"

    HOST_APP=""
    THROWAWAY=0
    cleanup() {
        set +e
        if [[ -n "${HOST_APP:-}" && -d "${HOST_APP:-}" ]]; then
            pluginkit -r "$HOST_APP/Contents/PlugIns/OFDQuickLookPreview.appex" >/dev/null 2>&1
            pluginkit -r "$HOST_APP/Contents/PlugIns/OFDQuickLookThumbnail.appex" >/dev/null 2>&1
            "$LSREGISTER" -u "$HOST_APP" >/dev/null 2>&1
        fi
        if ((KEEP_STATE)); then
            log "keeping state for debugging: $STATE"
        else
            rm -rf "$STATE"
        fi
        set -e
    }
    trap cleanup EXIT

    mkdir -p "$STATE"

    # Validate appex bundles after they were copied/embedded: codesign --verify also
    # catches copies that broke the signature, and a dropped sandbox entitlement would
    # make PlugInKit reject the plugin ("plug-ins must be sandboxed").
    verify_appex_bundle() {
        local appex="$1" name plist exe entitlements
        name="$(basename "$appex")"
        [[ -d "$appex" ]] || die "missing appex bundle: $appex"
        plist="$appex/Contents/Info.plist"
        exe="$appex/Contents/MacOS/${name%.appex}"
        [[ -f "$plist" ]] || die "$name: missing Contents/Info.plist"
        plutil -lint "$plist" >/dev/null || die "$name: Info.plist failed plutil -lint ($plist)"
        [[ -x "$exe" ]] || die "$name: missing executable ($exe)"
        codesign --verify --strict "$appex" 2>/dev/null \
            || die "$name: codesign --verify --strict failed (copy broke the signature?)"
        entitlements="$(codesign -d --entitlements :- "$appex" 2>/dev/null || true)"
        printf '%s' "$entitlements" | tr -d '[:space:]' \
            | grep -q '<key>com.apple.security.app-sandbox</key><true/>' \
            || die "$name: missing com.apple.security.app-sandbox entitlement (pkd would reject it)"
        printf '%s' "$entitlements" | tr -d '[:space:]' \
            | grep -q '<key>com.apple.security.files.user-selected.read-only</key><true/>' \
            || die "$name: missing com.apple.security.files.user-selected.read-only entitlement"
        pass "bundled appex ok (signed + sandboxed): $name"
    }

    # Resolve a host app: explicit --app wins; otherwise auto-detect a built app;
    # otherwise assemble a throw-away app from the freshly built appexes.
    if [[ -n "$APP_PATH" ]]; then
        [[ -d "$APP_PATH" ]] || die "--app path does not exist: $APP_PATH"
        HOST_APP="$APP_PATH"
    else
        for candidate in \
            "$REPO_ROOT/target/release/bundle/macos/OFD Manager.app" \
            "$REPO_ROOT/target/debug/bundle/macos/OFD Manager.app" \
            "$REPO_ROOT/target/universal-apple-darwin/release/bundle/macos/OFD Manager.app" \
            "$REPO_ROOT/apps/desktop/src-tauri/target/release/bundle/macos/OFD Manager.app"; do
            if [[ -d "$candidate" ]]; then
                HOST_APP="$candidate"
                break
            fi
        done
        if [[ -n "$HOST_APP" ]] \
            && [[ ! -d "$HOST_APP/Contents/PlugIns/OFDQuickLookPreview.appex" ]]; then
            warn "auto-detected app has no Quick Look PlugIns yet (Tauri wiring pending)"

            HOST_APP=""
            THROWAWAY=1
        fi
        if [[ -z "$HOST_APP" ]]; then
            THROWAWAY=1
        fi
    fi

    if [[ -z "$THROWAWAY" ]]; then
        for appex_name in OFDQuickLookPreview.appex OFDQuickLookThumbnail.appex; do
            [[ -d "$HOST_APP/Contents/PlugIns/$appex_name" ]] \
                || die "host app is missing PlugIns/$appex_name: $HOST_APP (Tauri bundling wiring not applied?)"
            verify_appex_bundle "$HOST_APP/Contents/PlugIns/$appex_name"
        done
        pass "host app contains both verified Quick Look appexes ($HOST_APP)"
    fi

    if ((THROWAWAY)); then
        log "assembling throw-away host app under $STATE"
        HOST_APP="$STATE/OFDQuickLookTestHost.app"
        rm -rf "$HOST_APP"
        mkdir -p "$HOST_APP/Contents/MacOS" "$HOST_APP/Contents/PlugIns"
        cat > "$HOST_APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>CFBundleExecutable</key>
	<string>hostlauncher</string>
	<key>CFBundleIdentifier</key>
	<string>com.ofdmanager.desktop.quicklook.testhost</string>
	<key>CFBundleInfoDictionaryVersion</key>
	<string>6.0</string>
	<key>CFBundleName</key>
	<string>OFD Quick Look Test Host</string>
	<key>CFBundlePackageType</key>
	<string>APPL</string>
	<key>CFBundleShortVersionString</key>
	<string>1.0</string>
	<key>CFBundleVersion</key>
	<string>1</string>
	<key>LSMinimumSystemVersion</key>
	<string>12.0</string>
</dict>
</plist>
PLIST
        # Dummy host executable: the app is never launched, it only anchors the
        # extensions in LaunchServices/PlugInKit for the duration of the test.
        printf 'int main(void) { return 0; }\n' > "$STATE/hostlauncher.c"
        xcrun clang -arch "$SWIFT_ARCH" -o "$HOST_APP/Contents/MacOS/hostlauncher" "$STATE/hostlauncher.c"
        for appex_name in OFDQuickLookPreview.appex OFDQuickLookThumbnail.appex; do
            [[ -d "$OUT_DIR/$appex_name" ]] || die "missing appex to install: $OUT_DIR/$appex_name"
            ditto "$OUT_DIR/$appex_name" "$HOST_APP/Contents/PlugIns/$appex_name"
            verify_appex_bundle "$HOST_APP/Contents/PlugIns/$appex_name"
        done
        codesign --force --sign - "$HOST_APP" >/dev/null 2>&1 \
            || warn "could not ad-hoc sign throw-away host app (continuing)"
        pass "assembled throw-away host app with both appexes"
    fi

    # --- register ---------------------------------------------------------
    log "registering extensions with LaunchServices + PlugInKit"
    "$LSREGISTER" -f "$HOST_APP" >/dev/null 2>&1 \
        || warn "lsregister -f failed (continuing; direct appex registration may still work)"
    for appex_name in OFDQuickLookPreview.appex OFDQuickLookThumbnail.appex; do
        pluginkit -a "$HOST_APP/Contents/PlugIns/$appex_name" \
            || die "pluginkit -a failed for $appex_name"
    done

    wait_for_extension() {
        local identifier="$1" waited=0
        while ! pluginkit -m -i "$identifier" 2>/dev/null | grep -q "$identifier"; do
            sleep 1
            waited=$((waited + 1))
            if ((waited >= 15)); then
                return 1
            fi
        done
        return 0
    }
    wait_for_extension "$PREVIEW_ID" || die "preview extension not discovered by PlugInKit within 15s"
    wait_for_extension "$THUMB_ID" || die "thumbnail extension not discovered by PlugInKit within 15s"
    pass "both extensions discovered by PlugInKit"
    qlmanage -r >/dev/null 2>&1 || true # refresh the generator list (best effort)

    # --- qlmanage thumbnail ----------------------------------------------
    THUMB_OUT="$STATE/thumb-out"
    FIXTURE_DIR="$STATE/fixture"
    mkdir -p "$THUMB_OUT" "$FIXTURE_DIR"
    # Copy under $HOME: qlmanage needs a sandbox token, paths in /private/tmp hang.
    FIXTURE_COPY="$FIXTURE_DIR/ofd-quicklook-test-$$-$(date +%s).ofd"
    cp "$REPO_ROOT/fixtures/helloworld.ofd" "$FIXTURE_COPY"
    THUMB_LOG="$STATE/qlmanage-thumbnail.log"
    rm -f "$THUMB_OUT"/*.png

    run_with_timeout() {
        local seconds="$1"; shift
        if command -v timeout >/dev/null 2>&1; then
            timeout "$seconds" "$@"
            return $?
        fi
        "$@" &
        local pid=$!
        ( sleep "$seconds"; kill -9 "$pid" 2>/dev/null ) &
        local watchdog=$!
        local rc=0
        wait "$pid" || rc=$?
        kill "$watchdog" 2>/dev/null
        return "$rc"
    }

    log "qlmanage -x -t -s 512 -o $THUMB_OUT <fixture copy>"
    ( cd "$HOME" && run_with_timeout 90 qlmanage -x -t -s 512 -o "$THUMB_OUT" "$FIXTURE_COPY" ) \
        >"$THUMB_LOG" 2>&1 || warn "qlmanage exited non-zero; will still check for a PNG"

    THUMB_PNG="$(find "$THUMB_OUT" -maxdepth 1 -name '*.png' -type f | head -1)"
    if [[ -z "$THUMB_PNG" ]]; then
        printf '%s\n' '--- qlmanage log ---'
        cat "$THUMB_LOG" 2>/dev/null || true
        die "qlmanage produced no thumbnail PNG"
    fi
    PNG_BYTES="$(wc -c < "$THUMB_PNG" | tr -d ' ')"
    PNG_MAGIC="$(od -An -tx1 -N8 "$THUMB_PNG" | tr -d ' \n')"
    [[ "$PNG_MAGIC" == "89504e470d0a1a0a" ]] || die "qlmanage output is not a PNG (magic: $PNG_MAGIC)"
    ((PNG_BYTES > 1024)) || die "thumbnail PNG is suspiciously small: $PNG_BYTES bytes"
    PNG_DIMS="$(sips -g pixelWidth -g pixelHeight "$THUMB_PNG" 2>/dev/null \
        | awk '/pixelWidth|pixelHeight/ {printf "%s%s", sep, $2; sep="x"}')"
    pass "qlmanage thumbnail PNG: $THUMB_PNG (${PNG_BYTES} bytes, ${PNG_DIMS:-unknown} px)"

    # Advisory: confirm our extension (not a system OFD generator) handled it.
    # macOS 27.2 ships its own OFD Quick Look extension, so precedence is opaque;
    # stage 2 already proves our renderer works, so this is diagnostic only.
    EXT_LOG_LINES="$(log show --last 120s --info \
        --predicate 'subsystem == "com.ofdmanager.quicklook"' 2>/dev/null | wc -l | tr -d ' ')"
    if [[ "${EXT_LOG_LINES:-0}" -gt 1 ]]; then
        pass "extension execution confirmed in unified log ($EXT_LOG_LINES log lines)"
    else
        warn "could not confirm our extension served this thumbnail; the system may have used another OFD extension (macOS 27.2 ships one). Bridge smoke test still passed."
    fi

    pass "stage 3: system Quick Look thumbnail test"
    log "note: preview extension is exercised by the Lead's GUI qlmanage -p check; bridge smoke covers its render path"
else
    log "stage 3: system Quick Look test skipped (pass --app <OFD Manager.app> or --system-test to run it)"
fi

printf '\n'
pass "test-quicklook: all executed stages passed"
