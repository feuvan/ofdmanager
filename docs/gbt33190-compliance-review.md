# GB/T 33190-2016 Compliance Review

Source standard: `docs/33190-2016-gbt-cd-300.pdf`.

Review date: 2026-07-29.

Scope: the Rust core (`crates/ofd-core`) and CLI (`crates/ofd-cli`). This is a
code review against the standard, not a formal conformance certification.

## Executive Summary

Current status: **strong support for static fixed-layout parsing and rendering,
with partial document-management and interactive-feature compliance**.

The core now covers the difficult static appearance path used by real invoices,
certificates, annotations, and electronic seals:

- bounded ZIP/XML parsing with observable malformed-resource diagnostics;
- page, layer, object, resource, annotation, signature, outline, bookmark, and
  action models;
- paths, text, images, clipping, colors, patterns, gradients, composite vector
  resources, and raster/vector seal appearances;
- embedded-first font resolution, including bare and sfnt-wrapped CID-keyed CFF;
- signature protected-file digest verification; and
- golden-image regression over all checked-in fixtures, using strict rendering
  for warning-free inputs and an exact warning allowlist for malformed samples.

The largest remaining conformance gaps are:

- no XSD validation or complete enforcement of namespace, cardinality, required
  values, ID uniqueness, and strict `ST_Loc` case rules;
- `PageRes`, template `ZOrder`/area fallback, permissions, and view preferences;
- image masks/borders, CCITT/TIFF, and ICC/profile-aware color conversion;
- annotation flags and host-side execution of actions/audio/video;
- custom tags, extensions, versions, and attachments; and
- cryptographic signature authenticity/certificate validation and SHA-1 digest
  verification.

## Status Legend

- **OK**: implemented closely enough for the reviewed static requirement.
- **Partial**: useful support exists, but required behavior remains.
- **No**: not implemented.
- **N/A**: no direct runtime requirement.

## Safety and Portability Baseline

Untrusted documents are processed under finite defaults:

- ZIP archive, entry count, per-entry size, total uncompressed size, and
  declared/streamed compression ratio are bounded by `ContainerLimits`.
- XML entries, per-tree and package-wide DOM nodes, package graphic/model items,
  nesting depth, source text, delta/path expansion, `CGTransform` glyph slots,
  and repeated template-model expansion are bounded before or during model
  construction.
- Page surfaces, individual decoded images and their peak conversion working
  set, the decoded-image cache, JBIG2 structures/work, cumulative glyph/path/mask
  work, composite and pattern nesting/offscreen surfaces, Gouraud raster work,
  and recursive embedded-OFD seals are bounded by `RenderLimits`.
- SES ASN.1 traversal is iterative and bounded by depth and node count.
- Strict rendering reports missing or unusable fonts, images, and composite
  resources, rejects unimplemented image masks/borders, and propagates nested
  embedded-OFD seal warnings instead of silently accepting incomplete output.

`ofd-core` remains bytes-in/model-and-bitmap-out. It performs no file-system,
network, thread, or system-font discovery. The native CLI loads only the needed
font bytes and injects them into the core. The core's `fontdb` dependency enables
only `std`, not `fs`, `memmap`, or `fontconfig`.

Evidence: `container.rs`, `parser.rs`, `render.rs`, `ses.rs`, `fonts.rs`, and
`crates/ofd-cli/src/main.rs`.

## Chapter-by-Chapter Matrix

| Chapter | Standard area | Status | Notes |
| --- | --- | --- | --- |
| 1 | Scope | Partial | Parsing, static rendering, navigation metadata, and digest checking are covered; the full exchange/management surface is not. |
| 2 | Normative references | Partial | Namespaced XML is parsed structurally, but input is currently required to be UTF-8; GB18030 XML is not decoded. |
| 3 | Terms and definitions | N/A | No direct runtime requirement. |
| 4 | Abbreviations | N/A | No direct runtime requirement. |
| 5 | Overview and imaging model | Partial | The container-to-model-to-raster pipeline covers the main imaging model; interactive and management features remain host work. |
| 6 | File structure | Partial | ZIP ingestion and relative `ST_Loc` resolution work with safety limits. Exact schema/package validation and strict case-sensitive lookup are absent. |
| 7 | Basic structure | Partial | Core document/page/resource structures, outlines, bookmarks, and actions are modeled. Several management structures remain absent. |
| 8 | Page description | Partial | Boundary clipping, DrawParam precedence/inheritance, object alpha, clips, colors, and stroke styles render. Exact device-pixel line-width rules and profile colors remain. |
| 9 | Graphics | Partial | `S/M/L/Q/B/A/C`, fill rules, arc conversion, patterns, and gradients render. Some complex-paint behavior is approximate. |
| 10 | Images | Partial | JPEG/PNG/BMP/JBIG2, alpha, downsampling, and substitution render. Masks, borders, CCITT/TIFF, and profiles remain. |
| 11 | Text | Partial | Embedded fonts, CFF/CID handling, escapes, X/Y inheritance, directions, HScale, italic, glyph-slot deltas, and full span cardinalities are handled. Per-character coverage fallback is absent. |
| 12 | Video | Partial | Multimedia metadata and Movie actions are modeled; playback is intentionally a host/UI concern and is not implemented. |
| 13 | Composite objects | OK | `CompositeObject` and `CompositeGraphicUnit` parse/render with size clipping, alpha, inherited DrawParam, cycle detection, and nesting limits. |
| 14 | Actions | Partial | DO/PO/CLICK, Region, Goto, URI, GotoA, Sound, Movie, Dest, bookmarks, and outline actions are modeled; the core does not execute them. |
| 15 | Annotations | Partial | Static appearances render; annotation flags, metadata, parameters, and interaction semantics remain incomplete. |
| 16 | Custom tags | No | `CustomTags` and custom-tag files are not modeled. |
| 17 | Extensions | No | `Extensions` and extension data are not modeled. |
| 18 | Digital signatures | Partial | Signature metadata, relative locations, protected-file digests, seal extraction, vector/raster appearances, and stamp clipping work. Authenticity and SHA-1 remain. |
| 19 | Versions | No | Version descriptions and version file lists are not modeled. |
| 20 | Attachments | No | Attachment lists, metadata, and content access are not modeled. |
| Appendix A | Normative Schema | Partial | Rust types mirror much of the schema, including XSD boolean lexical forms, but full schema validation is not performed. |

## Detailed Findings

### Chapters 6-7: Container and Document Structure

Implemented:

- Bounded ZIP ingestion and normalized relative path resolution, including
  resolving resource `BaseLoc` against its containing resource-description file.
- `OFD.xml`, multiple `DocBody` values, `DocInfo`, `Document.xml`, CommonData,
  page areas, resources, templates, pages, layers, and page blocks.
- Fonts, multimedia, DrawParams, color spaces/palettes, and composite graphic
  units.
- Document/page/object actions, bookmarks, and recursive outlines with page
  destination resolution.
- Structured warnings for missing resources, invalid embedded fonts, unresolved
  references, and DrawParam/composite cycles.

Remaining:

- Namespace URI, required field, enum, cardinality, ID uniqueness, and positive
  box validation are incomplete.
- `read_normalized` deliberately accepts leading slash and case-insensitive
  fallback, which is useful for real-world files but is more permissive than
  strict `ST_Loc` semantics.
- `DocInfo` is partial; permissions and view preferences are not modeled.
- Page-specific `PageRes` is ignored.
- Template `ZOrder` and template area fallback are not implemented; referenced
  template layers are currently added beneath page content.
- Custom tags, extensions, versions, and attachments are absent.

Evidence: `container.rs`, `parser.rs`, and `model.rs`.

### Chapters 8-9: Page Description and Graphics

Implemented:

- Top-left millimetre coordinates, page-origin handling, object Boundary/CTM,
  visibility, alpha, and layer ordering.
- Automatic Boundary clipping for every graphic unit.
- Object attributes override the object's DrawParam; otherwise the layer
  DrawParam and its `Relative` chain supply values before standard defaults.
- Line width, cap, join, miter limit, dash offset, and dash pattern for both path
  and text strokes.
- Clip path and text shapes. Areas inside one Clip are unioned; multiple Clips
  are intersected. Clip geometry follows the owning object's transform.
- RGB/Gray/CMYK component scaling, `DefaultCS`, explicit ColorSpace, indexed
  palettes, color/object alpha, pattern cells, axial/radial gradients, and
  Gouraud/lattice shading.
- Abbreviated path operators `S`, `M`, `L`, `Q`, `B`, `A`, and `C`; elliptical
  arcs are converted to cubic Beziers.
- A stroked path with no resolvable fill color remains outline-only, avoiding the
  common anti-tamper-mark black-fill failure.

Remaining:

- OFD's exact zero-width/minimum-device-pixel stroke rules are approximated by a
  small positive raster width.
- ICC/profile-aware color conversion is absent.
- Complex paint behavior is implemented for the practical static subset but is
  not backed by a complete conformance fixture set for every mapping/extend case.

Evidence: `parser.rs`, `model.rs`, and `render.rs`.

### Chapter 10: Images

Implemented:

- JPEG, PNG, BMP, and JBIG2 decoding under dimension/allocation limits.
- Object alpha and automatic Boundary/clip masks.
- `Substitution` is used when the primary resource is unavailable.
- Large source images are resized to their device footprint before drawing.
- Missing and failed resources become strict-mode errors.

Remaining:

- `ImageMask` is parsed and reference-checked but not applied.
- Border fields are parsed but borders are not painted.
- CCITT/TIFF and embedded color profiles are not supported.

Evidence: `parser.rs` and `render.rs`.

### Chapter 11: Text

Implemented:

- Embedded-first font resolution; invalid embedded fonts emit warnings.
- Bare CFF wrapping and CID-to-GID charset mapping for both bare and already
  sfnt-wrapped CID-keyed CFF fonts.
- Declared-family lookup, aliases, deterministic/host-injected fallback fonts,
  and protection against applying producer glyph IDs to an unrelated substitute.
- `TextCode` formatting-whitespace removal, backslash-plus-four-hex-digit
  escapes, X/Y inheritance,
  and bounded `g N V` delta expansion.
- Explicit per-displayed-glyph positioning, last-delta repetition, horizontal
  metric fallback only when both axes omit deltas, and vertical-axis behavior.
- `CGTransform` one-to-one, one-to-many, many-to-one, and many-to-many spans with
  preserved `.notdef` slots, `GlyphCount` alignment, and spans crossing
  `TextCode` run boundaries.
- HScale, ReadDirection, CharDirection, italic shear, fill/stroke paints, and
  complete stroke styles.
- Text glyph outlines can form real clip masks.

Remaining:

- A substitute face is selected per declared font, not per character. If that
  face lacks a character, the renderer leaves its slot undrawn rather than
  searching another fallback or painting tofu.
- `Weight` is retained as a font-selection hint and is not synthesized into
  artificial bold outlines.

Evidence: `fonts.rs`, `cff.rs`, `parser.rs`, `render.rs`, `bare_cff.rs`, and
`tests/text_clip.rs`.

### Chapters 12-14: Media, Composite Objects, and Actions

Composite vector resources are fully part of the static render model. Their
content is clipped by the unit dimensions and the referencing object's Boundary
and Clips, rendered through the composite CTM, and then composited using the
object alpha. DrawParam inheritance, missing references, cycles, nesting depth,
and intermediate surface allocation are handled explicitly.

Actions are data, not page paint. The parser retains all standard event and
behavior forms, including explicit Region segment geometry and destinations.
Executing navigation, opening a URI/attachment, or playing audio/video belongs
to a desktop/mobile/web host that does not yet exist.

Evidence: `model.rs`, `parser.rs`, `render.rs`, and `tests/actions.rs`.

### Chapter 15: Annotations

Annotation list/files and static appearance objects are parsed and painted over
page content. Remaining work includes annotation `Visible`, `Print`, `NoZoom`,
`NoRotate`, `ReadOnly`, creator/date/remark metadata, parameters, and link/host
interaction semantics.

Evidence: `parser.rs` and `render.rs`.

### Chapter 18: Digital Signatures

Implemented:

- Signature lists/descriptions, `Seal` versus `Sign`, provider, method, time,
  protected references, and signed-value location.
- Relative `BaseLoc`, `Seal/BaseLoc`, and `SignedValue` path resolution.
- MD5, SM3, and SHA-256 protected-file digest checking.
- Structured SES picture extraction and raster or embedded-OFD seal rendering.
- Required SES header-shape validation before accepting a seal picture.
- The CLI fails verification when signature metadata could not be parsed; a
  broken signature list is not reported as an unsigned document.

Remaining:

- The actual signature value and certificate chain are not authenticated.
- SHA-1 `CheckMethod` is not implemented.
- Full GB/T 38540 cryptographic validation is out of scope for the current SES
  appearance decoder.

Evidence: `parser.rs`, `ses.rs`, `sign.rs`, and `crates/ofd-cli/src/main.rs`.

## Test and Verification Coverage

The current verification baseline includes:

- `cargo test --workspace`: 101 core unit tests, action/CFF/text-clip integration
  tests, golden regression, and doc tests.
- Golden rendering at exactly 96 DPI across 15 fixtures and 38 pages, with 30
  perceptual comparisons. Dimensions may differ from a golden by at most one
  pixel per axis.
- Exact parse-warning and nested strict-render diagnostic allowlists for every
  fixture. Warning-free inputs render in strict mode; explicitly allowlisted
  malformed samples exercise bounded best-effort rendering, matching the CLI
  contract that `--strict` rejects incomplete or malformed output.
- Focused tests for ZIP/streamed-ratio limits, resource-relative paths, XML
  booleans, delta/glyph cardinality, cross-run glyph spans, arcs, action regions,
  Boundary/clip semantics, DrawParam precedence, image alpha, composite
  alpha/cycles, strict missing resources, page/package/model/work limits, image
  conversion peak memory, font trust, missing cmap slots, shared/validated seal
  appearances, signature parse failures, substitution references, and CID CFF
  mapping.
- `cargo test -p ofd-core --no-default-features`, all-feature workspace checking,
  warning-free Clippy, formatting, and Rustdoc builds.

Tests demonstrate regression coverage, not formal standard certification. The
largest fixture gaps follow the remaining implementation gaps: PageRes,
template ZOrder/area fallback, image masks/borders/CCITT, annotation flags,
custom tags/extensions, versions, attachments, and signature authenticity.

## Recommended Next Work

1. Add a separate conformance-validation layer for namespace, required values,
   cardinalities, IDs/references, box validity, and strict case-sensitive paths.
2. Implement PageRes and complete template ZOrder/area behavior.
3. Render ImageMask and Border, then add CCITT/TIFF and profile-aware colors.
4. Model and enforce annotation flags and metadata in host-facing APIs.
5. Add attachments, versions, custom tags, extensions, permissions, and view
   preferences.
6. Add SHA-1 digest support and optional signature/certificate authenticity
   verification.
7. Build host shells that execute navigation, URI, attachment, audio, and video
   actions while keeping `ofd-core` pure.
