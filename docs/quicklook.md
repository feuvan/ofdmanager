# Quick Look on macOS

Native Finder integration for `.ofd` files: press Space on an OFD document to get a
rendered preview, and see page thumbnails in Finder icon/Quick Look views. Both are
implemented as macOS Quick Look **app extensions** (`.appex`) that are bundled inside
the Tauri desktop app.

This document covers the architecture, the Tauri bundling wiring, and how to build and
verify the extensions. Rust core rendering behaviour is documented in the repo
[AGENTS.md](../AGENTS.md) and [docs/gbt33190-compliance-review.md](gbt33190-compliance-review.md).

## Support matrix

| Item | Value |
| --- | --- |
| Platform | macOS only (extensions require macOS 12+; the app itself is unchanged) |
| Preview extension | bundle id `com.ofdmanager.desktop.quicklook.preview`, `OFDQuickLookPreview.appex`, principal class `PreviewProvider` |
| Thumbnail extension | bundle id `com.ofdmanager.desktop.quicklook.thumbnail`, `OFDQuickLookThumbnail.appex`, principal class `ThumbnailProvider` |
| Supported UTIs | `public.ofd` (declared system-wide by macOS 27.2+) and `com.ofdmanager.ofd` (exported by the host app; fallback on older macOS) |
| Preview output | image-based multi-page PDF, first 32 pages at 144 DPI by default |
| Thumbnail output | RGBA PNG of page 1, longest edge sized to the requested thumbnail (64–2048 px; renderer default 1024 when unspecified) |
| Sandbox | both appexes are App Sandboxed with `com.apple.security.files.user-selected.read-only` |
| Fonts | the sandboxed extensions load **system fonts** only; they cannot read the host app's bundled font resources |
| Installation | app must be in a LaunchServices-indexed location (e.g. `/Applications`) and launched once so the extensions register |

## Architecture

```
             ┌─────────────────────────────────────────────┐
  Finder ───▶│ OFD Manager.app (Tauri host app)            │
  Quick Look │  Contents/PlugIns/OFDQuickLookPreview.appex │──▶ Swift QLPreviewProvider
  / Finder   │  Contents/PlugIns/OFDQuickLookThumbnail.appex│──▶ Swift QLThumbnailProvider
             │  Contents/Info.plist  (UTI declarations)     │
             └─────────────────────────────────────────────┘
                                │ FFI (C ABI, staticlib)
                                ▼
             crates/ofd-quicklook: libofd_quicklook.a
             (ofd_quicklook_render_preview_pdf /
              ofd_quicklook_render_thumbnail_png)
                                │
                                ▼
             crates/ofd-core: OFD parsing + rendering (pure Rust)
```

- **`crates/ofd-quicklook`** — a Rust `staticlib` (+ `rlib` for tests) exposing a small C
  ABI (`include/ofd_quicklook.h`): render a preview PDF and a thumbnail PNG from a file
  path. It reuses `ofd-core` for parsing/rendering, builds the PDF with `printpdf`
  (JPEG-compressed page images) and the thumbnail with `image`; fonts come from
  `fontdb`'s system font database. No panics cross the FFI boundary.
- **`apps/desktop/macos/quicklook`** — the Swift sources for both extensions, the
  `Info.plist`/entitlements templates, and a tiny FFI bridge. The preview is a
  data-based preview (`QLIsDataBasedPreview = true`, `QLPreviewReply` returning the PDF
  data); the thumbnail returns a `QLThumbnailReply` pointing at a PNG in the appex's
  temporary directory.
- **`scripts/build-quicklook.sh`** — builds the staticlib for the requested target,
  compiles and links both appexes with `swiftc`, ad-hoc (or identity) signs them with
  the sandbox entitlements, and assembles them in
  `apps/desktop/src-tauri/gen/quicklook/`. `--check` additionally verifies plists,
  signatures, entitlements and extension points.
- **`scripts/test-quicklook.sh`** — local end-to-end harness (build + Swift↔Rust bridge
  smoke test + throw-away PlugInKit registration under `~/Library/Caches` and a
  `qlmanage` thumbnail run). It never changes default app handlers.

## Tauri bundling wiring

macOS-specific Tauri config lives in
[`apps/desktop/src-tauri/tauri.macos.conf.json`](../apps/desktop/src-tauri/tauri.macos.conf.json)
(paths in this section are repo-root relative). Tauri merges it over
`tauri.conf.json` for macOS builds only, so Windows/Linux bundling is untouched.

- `build.beforeBundleCommand` runs `bash ../../scripts/build-quicklook.sh` after the
  Rust build and before bundling. Tauri runs the hook with its cwd set to the frontend
  directory (`apps/desktop`) and exports `TAURI_ENV_ARCH` / `TAURI_ENV_PLATFORM` /
  `TAURI_ENV_DEBUG`; the script uses `TAURI_ENV_ARCH` to build the appexes for the same
  architecture as the app (important for the `x86_64-apple-darwin` release build on an
  arm64 runner). The script itself resolves paths from its own location.
- `bundle.macOS.files` maps each appex to its destination **inside
  `Contents/`** — here `PlugIns/OFDQuickLookPreview.appex` and
  `PlugIns/OFDQuickLookThumbnail.appex`. Source paths resolve against the Tauri
  config directory (`apps/desktop/src-tauri`, the same convention as
  `bundle.resources`), hence the `gen/quicklook/...` values. (Do not prefix them with
  `src-tauri/`: that resolves to `src-tauri/src-tauri/gen/...` and the bundler fails
  with "Failed to copy ... does not exist".)
- [`apps/desktop/src-tauri/Info.plist`](../apps/desktop/src-tauri/Info.plist) is merged
  by Tauri into the generated app `Info.plist` (user keys win). It declares:
  - `CFBundleDocumentTypes`: extension `ofd`, role `Viewer`, rank `Default`,
    `LSItemContentTypes = [public.ofd, com.ofdmanager.ofd]`;
  - `UTExportedTypeDeclarations` for `com.ofdmanager.ofd` (conforms to `public.data`
    and `public.composite-content`, tags `.ofd` / `application/ofd`). On macOS versions
    before 27.2 this is the UTI other apps and the system use; on 27.2+ the system
    already declares `public.ofd` and both extensions claim it.
- No signing identity is configured: CI and release builds produce ad-hoc-signed
  appexes (the extension must be sandboxed — that is what PlugInKit requires). A nested
  signed appex is accepted inside an unsigned/non-notarized host app for local
  installation; distribution outside the App Store would still need notarization.

## Build

```bash
# Build + self-check only the extension bundles (macOS; other platforms skip).
# Output: apps/desktop/src-tauri/gen/quicklook/{OFDQuickLookPreview,OFDQuickLookThumbnail}.appex
scripts/build-quicklook.sh --check

# Build the full desktop app; the hook builds the appexes and embeds them.
cd apps/desktop
npx tauri build --bundles app
# => ../../target/release/bundle/macos/OFD Manager.app
```

The release workflow builds macOS app bundles for both `aarch64-apple-darwin` and
`x86_64-apple-darwin`; `TAURI_ENV_ARCH` makes the hook build matching appexes, so no
extra release steps are needed.

## Verify

```bash
APP="target/release/bundle/macos/OFD Manager.app"   # run from the repo root

# 1. Both extensions are embedded
ls "$APP/Contents/PlugIns"

# 2. The host Info.plist declares the UTIs / document type
plutil -p "$APP/Contents/Info.plist" | grep -A 8 -E 'CFBundleDocumentTypes|UTExportedTypeDeclarations'

# 3. Appex signatures and sandbox entitlements
for appex in OFDQuickLookPreview OFDQuickLookThumbnail; do
  codesign --verify --strict "$APP/Contents/PlugIns/$appex.appex"
  codesign -d --entitlements :- "$APP/Contents/PlugIns/$appex.appex"
done

# 4. System Quick Look (optional; install + launch the app once first)
qlmanage -p invoice.ofd                      # interactive preview panel
qlmanage -x -t -s 512 -o out invoice.ofd     # thumbnail; keep file + out under $HOME
```

> `qlmanage -x -t` hangs on paths under `/private/tmp` ("No sandbox token") — copy the
> OFD file and the output directory under `$HOME` first.

To check that PlugInKit discovered the extensions, launch the app once, then inspect the
extension elections (this only lists registrations; use System Settings to change them):

```bash
pluginkit -m -i com.ofdmanager.desktop.quicklook.preview
pluginkit -m -i com.ofdmanager.desktop.quicklook.thumbnail
```

## Extension precedence on macOS 27.2+

macOS 27.2 ships its own OFD Quick Look providers (`com.apple.OFDPreview` for preview,
`com.apple.OFDThumbnail` for thumbnails). If both the system extension and the OFD
Manager extension are enabled, the system chooses which one to use, and the choice is
not guaranteed to favour the app extension. Users can adjust it in **System Settings >
General > Login Items & Extensions** (Quick Look section), or with `pluginkit`
elections:

```bash
# Prefer the OFD Manager preview extension
pluginkit -e use -i com.ofdmanager.desktop.quicklook.preview
pluginkit -e ignore -i com.apple.OFDPreview

# Restore the system extension
pluginkit -e default -i com.apple.OFDPreview
```

Elections affect the current user; the same precedence rules apply to thumbnails
(`com.ofdmanager.desktop.quicklook.thumbnail` vs `com.apple.OFDThumbnail`).

## Troubleshooting

- **No preview / thumbnail after installing** — make sure the app is in `/Applications`
  (or another LaunchServices-indexed location) and has been launched at least once;
  PlugInKit registers extensions on app launch. Re-launch after replacing the app.
- **Old page content after updating the app** — Quick Look caches thumbnails; clear the
  cache (`qlmanage -r cache`) or touch the file.
- **Preview shows a generic placeholder** — the extension failed to render (damaged or
  unsupported OFD) and Quick Look falls back to a generic preview. The appex logs the
  reason with subsystem `com.ofdmanager.quicklook`; inspect with
  `log show --predicate 'subsystem == "com.ofdmanager.quicklook"' --last 5m`.
- **Layout differs from the app** — the sandboxed extension renders with system fonts
  only (the app's bundled fallback fonts are not readable from the appex), so documents
  that do not embed their fonts can look slightly different in Quick Look.

## CI

`.github/workflows/ci.yml` has a `quicklook-macos` job that runs
`cargo test -p ofd-quicklook`, `cargo clippy -p ofd-quicklook`, verifies formatting,
runs `scripts/build-quicklook.sh --check`, and validates the Tauri wiring (JSON/plist
config, UTI declarations, appex source paths, appex signatures and sandbox
entitlements). It does not fetch the bundled fallback fonts — the crate's tests must
pass without them.
