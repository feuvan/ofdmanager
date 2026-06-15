# OFD Manager

A cross-platform viewer and toolkit for **OFD** files — *Open Fixed-layout
Document* (GB/T 33190-2016), China's national fixed-layout document format used
for VAT e-invoices, e-tickets, certificates, and official documents.

The core idea: implement the hard part — faithful OFD **parsing and rendering** —
once in portable Rust, then ship it everywhere (desktop, mobile, web). Today the
Rust core and a command-line tool are working and render real-world documents
with high fidelity; the GUI apps are the next milestone.

> Status: **early but functional.** The rendering engine handles real invoices,
> air tickets, and certificates faithfully (validated against reference renders).
> Desktop/mobile/web shells are not built yet.

## What works

- **Rendering** of pages to images: text (with embedded fonts), vector paths,
  raster images, layers, templates, clipping, transparency.
- **Fonts**: uses the document's embedded fonts (TrueType, OpenType/CFF, and even
  bare-CFF); substitutes deterministic CJK fonts when a font isn't embedded, so
  output is consistent across machines. Correct CJK layout via OFD's explicit
  per-glyph positioning, including **vertical text**.
- **Electronic seals** (电子签章, GB/T 38540): renders both raster seals and vector
  seals (an embedded OFD stamp), decoded from the SES structure.
- **JBIG2 images**: invoice QR codes and scanned bilevel images.
- **Signatures**: models digital signatures and verifies **file-digest integrity**
  (SM3/SHA-256/MD5) — detects whether any protected file was tampered with.
- **Export**: render any page to PNG at any DPI.

Known gaps: full cryptographic signature *authenticity* (SM2 + certificate chain),
CCITT/G4 images, and a few advanced annotation blend modes are not implemented yet.

## Quick start

Requirements: a recent Rust toolchain.

```bash
# Render the first page of an OFD to a 300-DPI PNG
cargo run -p ofd-cli -- render invoice.ofd out.png --dpi 300

# Render a specific page, or crop a region (pixels) for a close look
cargo run -p ofd-cli -- render doc.ofd p2.png --dpi 300 --page 1
cargo run -p ofd-cli -- render doc.ofd crop.png --dpi 300 --region 0,0,800,400

# Check a signed document's integrity (are the protected files unmodified?)
cargo run -p ofd-cli -- verify invoice.ofd
```

For pixel-consistent CJK rendering of documents that don't embed their fonts,
fetch the bundled fallback fonts once (optional; ~47 MB, not committed):

```bash
scripts/fetch-fonts.sh
```

## Project layout

```
crates/
  ofd-core/     Portable rendering core (parse → model → bitmap). No I/O or UI.
  ofd-cli/      Command-line front-end: render, verify.
fixtures/       Sample OFD files and reference page images for regression tests.
docs/           The GB/T 33190 standard (the authoritative reference).
scripts/        Helper scripts (e.g. fetch-fonts.sh).
```

The core is intentionally pure (bytes in, image out, no filesystem or threads of
its own) so the same code can later compile to WebAssembly for an in-browser
viewer and link into native desktop/mobile apps.

## Building and testing

```bash
cargo build --workspace
cargo test  --workspace
```

The test suite includes a **golden-image regression**: every sample renders, and
pages with a reference image are compared perceptually to catch layout, color, or
font regressions.

## Roadmap

- Desktop app (Tauri v2 + React) wrapping the core
- OFD → PDF export
- Android / iOS apps and a pure-WebAssembly web viewer
- Full signature authenticity (SM2 signature + certificate validation)
- Local file management (browse, recents, favorites)

## Standards

- **GB/T 33190-2016** — OFD document format (parsing & rendering target)
- **GB/T 38540 / GM/T 0031** — electronic seal (SES) structure
- **GB/T 32905** — SM3 hash (signature digest verification)

## A note on fonts

Documents that embed their fonts render exactly. For documents that only *name* a
font (e.g. 宋体/SimSun) without embedding it, the result depends on available
fonts; the bundled fallback set (Windows core CJK fonts, fetched via the script)
gives deterministic, cross-platform output. Those fonts are not redistributed in
this repository.
