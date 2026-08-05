# Agent guide — OFD Manager

Guidance for coding agents working in this repository. (`CLAUDE.md` is identical.)

## What this is

A cross-platform **OFD** (Open Fixed-layout Document, GB/T 33190-2016 — the Chinese
national PDF-equivalent) parser, renderer, and manager. The strategy is to write
the hard, valuable part — OFD parsing and rendering — once in Rust as a portable
core, then ship it to desktop (Tauri), mobile, and web.

Current state: the Rust core (`ofd-core`) and a CLI (`ofd-cli`) exist and render
real-world invoices/certificates faithfully. The desktop/mobile/web shells are
not built yet.

## Workspace layout

```
crates/ofd-core/        # THE core: bytes in → model + RGBA bitmap out. No FS/threads/UI.
  src/model.rs          #   object model, mirrors GB/T 33190 CT_* types
  src/parser.rs         #   roxmltree XML → model (pages, layers, objects, resources, signatures)
  src/render.rs         #   tiny-skia rasterizer (text, path, image, clip, seal, annotation)
  src/fonts.rs          #   font resolution: embedded-first + alias table + bundled fallbacks
  src/cff.rs            #   bare-CFF → synthesized OTTO wrapper
  src/ses.rs            #   GB/T 38540 electronic-seal (SES) ASN.1 structured decode
  src/sign.rs           #   signature file-digest verification (SM3/SHA-256/MD5)
  src/container.rs      #   ZIP container reader
  src/geom.rs           #   mm/affine geometry
  tests/render_fixtures.rs  # golden-image regression
crates/ofd-cli/         # render / verify commands
fixtures/               # sample .ofd + golden page PNGs (<name>-<page>.png)
docs/33190-2016-*.pdf   # the GB/T 33190 standard (authoritative reference)
scripts/fetch-fonts.sh  # fetch bundled CJK fonts (gitignored)
```

## Build, test, run

```bash
cargo build --workspace
cargo test  --workspace                 # unit + doc + golden regression
cargo test -p ofd-core --test render_fixtures   # golden only
cargo run -p ofd-cli -- render <in.ofd> <out.png> --dpi 300 [--page I] [--region x,y,w,h] [--strict]
cargo run -p ofd-cli -- verify <in.ofd>          # signature digest integrity
scripts/fetch-fonts.sh                  # optional: deterministic CJK fonts (47MB, gitignored)
```

To eyeball a render: produce a PNG with `ofd-cli render … --dpi 300` and open it.
`--region x,y,w,h` crops (device pixels) for close inspection.

## Non-negotiable rules

1. **`ofd-core` stays pure.** Bytes in, model/bitmap out. No filesystem, network,
   threads, or UI in the core — the host injects bytes. This is what keeps the
   crate portable to `wasm32` later. Don't add `std::fs` / threads to it.
2. **Implement to the standard, not to a fixture.** The authoritative spec is
   `docs/33190-2016-gbt-cd-300.pdf` (and GB/T 38540 for seals). When something
   renders wrong, find the root cause and align with the standard. Do **not**
   special-case a sample file or branch on a vendor-only attribute (e.g. the
   non-standard `TransFlag`) to make one file look right.
3. **Inspect OFD content with XML parsing, never regex.** When diagnosing, use a
   real XML parser (Python `xml.etree`, or roxmltree). OFD XML is namespaced
   (`xmlns="http://www.ofdspec.org/2016"`); match by local name.
4. **Embedded fonts are always preferred and used.** Resolution order: embedded
   (parse-validated, with bare-CFF wrapping) → declared family + alias table →
   bundled deterministic CJK fonts → system. If an embedded font can't be used,
   emit a parse warning; never silently swap it without one.
5. **Keep the golden regression green.** `tests/render_fixtures.rs` renders every
   fixture page at 96 DPI and perceptually compares pages that have a
   `<name>-<page>.png` golden (mean grayscale diff must stay < 0.10). Add fixtures
   + goldens for new capabilities; don't weaken the threshold to pass.
6. **No non-standard rendering by default.** e.g. text stem-darkening is not in
   GB/T 33190; it's off by default (opt-in `--stem`). Faithful-to-outline wins.
7. **Add tests for new logic** (unit tests next to the code; a fixture for
   visible features) and run the full suite before claiming done.

## Key domain facts (so you don't relearn them)

- Coordinates: **top-left origin, +X right, +Y down, millimetres.** `px = mm/25.4*dpi`.
- Text is positioned by **explicit per-glyph `DeltaX`/`DeltaY`**, not font metrics.
  `DeltaX="g N V"` means N copies of V. The deltas are **per displayed glyph, not
  per source character** — with ligatures the glyph count differs from the char
  count, so positioning iterates glyph slots. A short delta list repeats its last
  value; an empty list falls back to the font's glyph advance — but only
  horizontally and only when there's no `DeltaY` (vertical = `DeltaY`, X adv 0).
- `TextCode` content: per §11.3 a **significant space must be hex-escaped**
  (`\0020`), so literal unescaped whitespace (e.g. a pretty-print `\r\n`) is
  formatting — strip it *before* unescaping, or it eats a `DeltaX` and shifts every
  `CGTransform/@CodePosition`.
- `CGTransform` (§11.4) is authoritative for the code span it covers: it maps
  `[CodePosition, +CodeCount)` to `GlyphCount` explicit glyph ids — handling
  many-to-one (ligatures), one-to-many, and many-to-many. The slot count must equal
  `GlyphCount` (keep `.notdef` slots, undrawn, for `DeltaX` alignment). Codes no
  transform covers map 1:1 through the cmap. CID-keyed CFFs remap the ids via the
  charset CID→GID table. **But glyph indices only index the font the producer
  used** — `ResolvedFont::trusted_glyph_ids` is false when the declared font is
  neither embedded nor found (generic fallback); there the indices are meaningless
  and the renderer maps by the real character instead. (No per-character coverage
  fallback yet: a non-embedded font whose family is missing and whose chars the
  chosen substitute lacks — e.g. Tamil "Latha" — renders nothing, not tofu.)
- An object's effective draw params come from its own `@DrawParam`, **then the
  containing Layer's `@DrawParam`** (layer = default style), walking the `Relative`
  chain. Clips are in object space and go through the object's CTM (§8.5).
- Path `Fill="true"` with no resolvable color is *not* a black fill when the path
  is also stroked (that's the ⊗ anti-tamper mark — outline only).
- `AbbreviatedData` operators (§9.3) are `S`/`M` (start/move), `L`, `Q`, `B`, `A`
  (elliptical arc — converted to cubics), `C` (close). Don't drop `S`/`A`.
- `CompositeObject` (`CT_Composite`, §13) references a `CompositeGraphicUnit`
  (`CT_VectorG`) resource by `@ResourceID`; the unit's `Content` objects live in
  the unit's own coordinate space and are drawn through the composite's
  boundary+CTM. Handwritten ink signatures are commonly carried this way (each
  stroke a `PathObject`), so a missing composite = missing signatures.
- Clips (`Clips/Clip/Area`, §8.4) are shaped by a `Path` (§9.1) **or** a `Text`
  object (§11.2) — both parsed (`ClipShape`). Path clips rasterise to a mask;
  text-shaped clips are modeled but not yet rasterised (no real samples found).
- Actions (`CT_Action`, §14) are interaction metadata, **not rendered**, but fully
  parsed: `@Event` (DO/PO/CLICK), optional `Region` (`CT_Region` outline of
  Move/Line/Quad/Cubic/Arc/Close segments), and a Goto/URI/GotoA/Sound/Movie
  behavior. They hang off graphic objects, pages, the document, and outline nodes.
  `Goto/Dest@PageID` and `Bookmarks` resolve navigation; outline tree is parsed too.
- Seals: `/Signs/Signature.xml` has `StampAnnot`s (page + box, plus an optional
  `@Clip`) and either a `Seal.esl` or the seal embedded in `SignedValue.dat`. The
  `BaseLoc`/`SignedValue` paths may be relative (resolve against their containing
  file). The SES picture is `ofd` (an embedded OFD, rendered recursively over a
  transparent bg) or a raster. `Type="Sign"` signatures have no visual seal.
  Cross-page **骑缝** seals use one seal with a different `StampAnnot/@Clip` slice
  per page — the clip is in the stamp's boundary-relative coordinates.
- Large source images: tiny-skia's `draw_pixmap` doesn't reliably downscale huge
  images — pre-resize to the device footprint before drawing.
- JBIG2 (invoice QR / scanned B&W) decodes via the `justbig2` crate.

## Gotchas

- This repo's git history was initialized late; check `git status` before assuming
  what's committed.
- Bundled fonts (`crates/ofd-core/assets/fonts/`) and `target/` are gitignored.
  The golden test and CLI use bundled fonts when present for determinism.
- The render is resolution-independent; the **golden test must stay at 96 DPI**
  (golden PNG dimensions), but render anything for human viewing at 300+ DPI.
