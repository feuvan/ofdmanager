# GB/T 33190-2016 Compliance Review

Source standard: `docs/33190-2016-gbt-cd-300.pdf`.

Review date: 2026-06-15.

Scope: current Rust core (`crates/ofd-core`) and CLI (`crates/ofd-cli`). This is
a code review against the standard, not a formal conformance certification. The
current implementation is best described as a practical parser/renderer for a
useful OFD subset, not a complete GB/T 33190 implementation.

## Executive Summary

Current status: **partially compliant for core fixed-layout rendering**.

Strong coverage:

- ZIP container ingestion, `OFD.xml`, `Document.xml`, resource files, page files.
- Core page rendering pipeline: page/object coordinate spaces, layers, page
  blocks, path/text/image objects, basic colors, alpha, path clipping, embedded
  fonts, image decoding, static annotation appearances, and seal appearances.
- Signature metadata parsing and protected-file digest checking.
- Golden-image regression tests over fixture OFDs.

Largest gaps:

- No full Schema/XSD validation and no strict enforcement of required fields,
  namespace URI, ID uniqueness, path case-sensitivity, or many default/error
  rules.
- Many chapter-7 document structures are ignored: permissions, actions,
  view preferences, bookmarks, outlines, custom tags, extensions, versions, and
  attachments.
- Several rendering semantics are incomplete: object-boundary clipping, line
  cap/join/dash/miter rendering, color spaces/palettes/indexed colors/profiles,
  pattern and gradient colors, text clipping, image masks/borders/substitution,
  text direction/HScale/style transforms, path arcs, and XML region paths.
- No interactive action model, no video/audio playback, no composite object
  resources, and no cryptographic signature authenticity verification.

## Status Legend

- **OK**: implemented closely enough for the reviewed requirement.
- **Partial**: relevant support exists, but notable required behavior is missing.
- **No**: not implemented.
- **N/A**: terminology or scope text with no direct runtime requirement.

## Chapter-by-Chapter Matrix

| Chapter | Standard area | Status | Notes |
| --- | --- | --- | --- |
| 1 | Scope | Partial | The project targets parsing, rendering, and signature digest checks, but not the whole storage/reading/exchange/management surface. |
| 2 | Normative references | Partial | XML is parsed with `roxmltree`; XML bytes are converted with `String::from_utf8`, so non-UTF-8 GB18030 XML is not supported. |
| 3 | Terms and definitions | N/A | No runtime requirement. |
| 4 | Abbreviations | N/A | No runtime requirement. |
| 5 | Overview and imaging model | Partial | The core has a container -> model -> raster pipeline and renders text/path/image objects. Complex color, interactions, and some object semantics are missing. |
| 6 | File structure | Partial | Reads OFD as ZIP and requires `OFD.xml`; does not validate ZIP 6.2.0 conformance or exact package organization. `read_normalized` tolerates leading slash and case differences, which is useful but not strict ST_Loc behavior. |
| 7 | Basic structure | Partial | Main document/page/resource subsets are parsed. Many optional-but-standard structures are ignored, and required-field/schema validation is mostly absent. Details below. |
| 8 | Page description | Partial | Coordinate spaces and CTM are implemented. Draw params, colors, clipping, and graphic-unit semantics are incomplete. Details below. |
| 9 | Graphics | Partial | Basic abbreviated paths `M/L/Q/B/C` and fill rules are supported. `S`, `A`, XML region path descriptions, some defaults, and stroke styles are missing. |
| 10 | Images | Partial | JPEG/PNG/BMP/JBIG2 render. Substitution, masks, borders, TIFF/CCITT, and color-profile behavior are not rendered. |
| 11 | Text | Partial | Embedded/substituted fonts, glyph outlines, TextCode, DeltaX/Y, and basic CGTransform are implemented. HScale, read/char direction, weight/italic transforms, full CGTransform cases, TextCode escape rules, and some defaults are incomplete. |
| 12 | Video | No | Video resources may be parsed as multimedia metadata, but Movie actions/playback are absent. |
| 13 | Composite objects | No | PageBlock grouping exists, but `CompositeObject` and `CompositeGraphicUnit` resources are not parsed/rendered. |
| 14 | Actions | No | Document/page/object/outline actions, destinations, URI, attachment, sound, and movie actions are not modeled. |
| 15 | Annotations | Partial | Annotation list/files and static appearances are rendered. Annotation flags, metadata, interaction, parameters, link behavior, and visibility semantics are incomplete. |
| 16 | Custom tags | No | `CustomTags` entry and files are ignored. |
| 17 | Extensions | No | `Extensions` entry and extension data are ignored. |
| 18 | Digital signatures | Partial | Signature list/files and reference digests are parsed; digest integrity verification exists. No SignedValue authenticity, certificate validation, SHA1, or full stamp clipping. Some relative-path handling is likely incomplete. |
| 19 | Versions | No | `Versions` and version file lists are ignored. |
| 20 | Attachments | No | Attachment lists and attachment metadata/content are ignored. |
| Appendix A | Normative Schema | Partial | The implementation mirrors parts of the schema in Rust structs, but does not run XSD validation or enforce all schema cardinalities/defaults. |

## Detailed Findings

### Chapter 6 - File Structure

The container abstraction is a ZIP reader over injected bytes, matching the
standard's container model at a high level (`Container::open`,
`Container::read`). It also normalizes leading `/` and performs a case-insensitive
fallback when entry lookup fails.

Compliance risks:

- ST_Loc paths are specified as case-sensitive; the fallback in
  `Container::read_normalized` is permissive rather than strictly conforming.
- The code does not validate "one and only one `OFD.xml`", ZIP version, or
  recommended directory names.

Evidence: `crates/ofd-core/src/container.rs:15`, `:26`, `:38`.

### Chapter 7 - Basic Structure

Implemented:

- `OFD/@Version`, `OFD/@DocType`, multiple `DocBody` entries, `DocInfo`,
  `DocRoot`, and `Signatures`.
- `CommonData` subset: `MaxUnitID`, `PageArea`, `PublicRes`, `DocumentRes`,
  `TemplatePage`, `DefaultCS`.
- `Pages/Page`, page `Area`, page `Content/Layer`, `Layer/@Type`,
  `Layer/@DrawParam`, `PageBlock`, `TextObject`, `PathObject`, `ImageObject`.
- Resource files for fonts, multimedia, draw params, and color spaces.

Compliance risks:

- Namespace is matched only by local element name, so documents with wrong or
  missing namespace URI can be accepted.
- XML is assumed UTF-8; GB18030-encoded XML is not decoded.
- No schema validation for required fields, enum domains, ID uniqueness, positive
  `ST_Box` width/height, or unresolved references.
- `DocInfo` is only partially modeled. Missing fields include `Abstract`,
  `ModDate`, `DocUsage`, `Cover`, `Keywords`, and custom metadata.
- Document-level `Permissions`, `Actions`, `VPreferences`, `Bookmarks`,
  `Outlines`, `CustomTags`, `Extensions`, and `Attachments` are ignored.
- `outline` exists in the model but is always returned empty.
- Page `PageRes` is ignored.
- Template page `ZOrder` and template `Area` fallback are not implemented; parsed
  template layers are simply prepended before page layers.
- `CompositeObject` is not parsed.
- Resource `CompositeGraphicUnits` are not parsed.
- Color spaces are stored but mostly not used for color resolution; palettes and
  profiles are not parsed.

Evidence: `crates/ofd-core/src/parser.rs:18`, `:44`, `:56`, `:67`, `:92`,
`:111`, `:131`, `:154`, `:393`, `:410`, `:458`, `:637`;
`crates/ofd-core/src/model.rs:26`, `:146`, `:158`, `:196`;
`crates/ofd-core/src/parser.rs:805`.

### Chapter 8 - Page Description

Implemented:

- Page space uses millimetres and top-left origin.
- Object placement combines page origin, object boundary, and object CTM.
- Layer paint order is grouped by Background, Body, Foreground, then Custom.
- DrawParam inheritance through `Relative` is implemented for selected fields.
- Basic RGB/gray/CMYK-like value parsing and alpha are implemented.
- Path-based clipping exists.
- Object `Visible=false` is honored.

Compliance risks:

- Object `Boundary` is not used as an automatic clipping rectangle for text/path
  objects, although the standard says drawing outside the boundary is clipped.
- Stroke `Cap`, `Join`, `MiterLimit`, `DashOffset`, and `DashPattern` are parsed
  but not rendered. `tiny_skia::Stroke` is created mostly with defaults.
- Text stroke does not resolve `DrawParam` line width.
- Line-width special handling in the standard (0 = one device pixel, positive
  very small widths at least two pixels) is not implemented as specified.
- Color resolution ignores `ColorSpace`, `Index`, palettes, profiles, BPC scaling,
  and hexadecimal `#` values.
- Pattern, axial/radial/Gouraud/mesh gradient colors are not modeled.
- Clip semantics are incomplete: text clips are unsupported; `Area` entries are
  flattened and intersected, while the standard requires union within one `Clip`
  and intersection between multiple `Clip` objects.
- Clip `DrawParam` is ignored.
- Graphic-unit actions are ignored.

Evidence: `crates/ofd-core/src/geom.rs:1`, `:41`;
`crates/ofd-core/src/render.rs:64`, `:99`, `:143`, `:162`, `:190`, `:296`;
`crates/ofd-core/src/parser.rs:476`, `:521`, `:707`, `:830`;
`crates/ofd-core/src/render.rs:427`, `:497`.

### Chapter 9 - Graphics

Implemented:

- Path objects with `Stroke`, `Fill`, `Rule`, `FillColor`, `StrokeColor`.
- Abbreviated path operators `M`, `L`, `Q`, `B`, `C`.
- NonZero and EvenOdd fill rules.

Compliance risks:

- Abbreviated operators `S` (subpath start) and `A` (arc) are not implemented.
- XML path/region descriptions (`Area`, `Move`, `Line`, `QuadraticBezier`,
  `CubicBezier`, `Arc`, `Close`) are not parsed.
- FillColor default for paths should be transparent; the renderer currently fills
  a fill-only path without a resolved fill color as black.
- Stroke cap/join/dash/miter styles are parsed but ignored at render time.

Evidence: `crates/ofd-core/src/parser.rs:585`, `:762`;
`crates/ofd-core/src/model.rs:454`, `:478`;
`crates/ofd-core/src/render.rs:296`, `:446`.

### Chapter 10 - Images

Implemented:

- Image objects parse `ResourceID`, `Substitution`, `ImageMask`, and `Border`.
- Multimedia resources parse type, format, and media file.
- Renderer decodes JPEG, PNG, BMP, and JBIG2; large images are pre-resized before
  drawing.

Compliance risks:

- `Substitution` and `ImageMask` are parsed but not used.
- Image borders and border dash/color/corner radii are parsed only partially and
  not rendered.
- TIFF/CCITT images are not decoded.
- No color profile handling.

Evidence: `crates/ofd-core/src/parser.rs:601`, `:685`, `:746`;
`crates/ofd-core/src/render.rs:337`, `:512`.

### Chapter 11 - Text

Implemented:

- Font resources parse `FontName`, `FamilyName`, `Charset`, `Serif`, `Bold`,
  `Italic`, `FixedWidth`, and `FontFile`.
- Embedded font resolution is preferred, including wrapped bare CFF support.
- Text objects parse `Font`, `Size`, `Stroke`, `Fill`, `HScale`,
  `ReadDirection`, `CharDirection`, `Weight`, `Italic`, colors, `CGTransform`,
  and `TextCode`.
- Renderer outlines glyphs and uses explicit `DeltaX`/`DeltaY`, with
  `CGTransform` fallback for subset fonts.

Compliance risks:

- `HScale`, `ReadDirection`, `CharDirection`, `Weight`, and `Italic` are parsed
  but not applied during rendering.
- TextCode omitted `X`/`Y` should inherit the previous TextCode coordinate; the
  parser currently defaults missing coordinates to 0.
- Text escape handling for `\` plus four hex digits is not implemented.
- Full CGTransform semantics are incomplete: one-to-many, many-to-one,
  many-to-many, and `GlyphCount` are not fully modeled.
- Text `StrokeColor` default should be transparent; when `Stroke=true`, the
  renderer falls back to black.
- The renderer falls back to font horizontal advance when DeltaX/DeltaY are both
  empty; this may be desirable for real-world files but should be reconciled with
  the standard text that absent DeltaX/DeltaY means no offset on that axis.

Evidence: `crates/ofd-core/src/parser.rs:538`, `:550`, `:567`;
`crates/ofd-core/src/fonts.rs:87`, `:107`, `:133`;
`crates/ofd-core/src/cff.rs:1`;
`crates/ofd-core/src/render.rs:206`, `:219`, `:472`;
`crates/ofd-core/src/model.rs:396`, `:430`, `:441`.

### Chapters 12-14 - Video, Composite Objects, Actions

Video:

- Multimedia resources can be classified as Audio/Video, but there is no action
  model or playback surface.

Composite objects:

- `PageBlock` grouping is implemented, but standard `CompositeObject` nodes and
  `CompositeGraphicUnit` resources are absent.

Actions:

- No support for `DO`, `PO`, or `CLICK` events.
- No support for `Goto`, `URI`, `GotoA`, `Sound`, or `Movie`.
- No `Dest`, bookmark target, or region action handling.

Evidence: `crates/ofd-core/src/model.rs:230`, `:386`;
`crates/ofd-core/src/parser.rs:458`, `:685`.

### Chapter 15 - Annotations

Implemented:

- `Annotations` entry is parsed from `Document.xml`.
- Annotation list pages and per-page annotation files are followed.
- `Annot/@Type` and static `Appearance` page blocks are parsed and drawn over
  page content.

Compliance risks:

- Annotation metadata and flags are ignored: `ID`, `Creator`, `LastModDate`,
  `Subtype`, `Visible`, `Print`, `NoZoom`, `NoRotate`, `ReadOnly`, `Remark`, and
  `Parameters`.
- Link annotations are not interactive; only static appearance can render.
- Annotation `Visible=false` is not honored at the annotation level.
- No print/nozoom/norotate behavior.

Evidence: `crates/ofd-core/src/parser.rs:154`, `:205`;
`crates/ofd-core/src/render.rs:113`.

### Chapters 16-17 - Custom Tags and Extensions

No implementation was found for `CustomTags`, custom tag files, `Extensions`, or
extension data.

Evidence: no parser/model symbols beyond a metadata comment; `rg` found no
functional parsing for these structures.

### Chapter 18 - Digital Signatures

Implemented:

- `Signatures.xml` and `Signature.xml` are parsed.
- `Seal` vs `Sign` type is modeled.
- Provider name, signature method, signature time, references, check values, and
  signed value path are modeled.
- Protected file digests can be verified using MD5, SM3, and SHA-256.
- Seal appearances can be rendered from SES picture data when extractable.

Compliance risks:

- The actual `SignedValue` cryptographic signature is not verified; no SM2/CMS
  verification or certificate-chain validation.
- Appendix schema lists MD5/SHA1 for `CheckMethod`; SHA1 is not supported.
- `StampAnnot/@Clip` is ignored.
- `Signature.xml` path handling appears not to resolve relative paths against the
  `Signatures.xml` directory, which ST_Loc normally requires for relative paths.
- Seal parsing relies on GB/T 38540/GM/T 0031 structures, but that standard is not
  present under `docs/` for this review.

Evidence: `crates/ofd-core/src/parser.rs:147`, `:283`;
`crates/ofd-core/src/sign.rs:1`, `:62`, `:107`;
`crates/ofd-core/src/ses.rs:1`;
`crates/ofd-cli/src/main.rs:25`, `:67`.

### Chapters 19-20 - Versions and Attachments

No implementation was found for:

- `Versions` entries in `OFD.xml`.
- `DocVersion` files and version file lists.
- `Attachments` entry in `Document.xml`.
- Attachment metadata or embedded attachment file access.

Evidence: `crates/ofd-core/src/parser.rs:25` only reads `DocInfo`, `DocRoot`,
and `Signatures` from each `DocBody`; `crates/ofd-core/src/parser.rs:67` does
not read `Attachments`, and no attachment/version model exists.

## Test and Verification Coverage

Existing tests provide useful regression coverage but do not prove standard
conformance:

- Fixture smoke rendering and perceptual golden comparison at 96 DPI.
- Parser unit tests for abbreviated path basics, relative path joining, deltas,
  rectangles, and floats.
- Rendering unit tests for `Visible=false`.
- Signature unit tests for SM3 and method resolution.
- SES unit tests for structural picture extraction.
- Bare CFF unit test.

Gaps in tests mirror many implementation gaps: no conformance fixtures for
gradients, pattern colors, palette/indexed colors, page resources, outlines,
actions, attachments, versions, path arcs, XML region paths, image masks,
annotation flags, template ZOrder, object boundary clipping, or full text
direction/CGTransform behavior.

Evidence: `crates/ofd-core/tests/render_fixtures.rs:74`;
`crates/ofd-core/src/parser.rs:937`;
`crates/ofd-core/src/render.rs:608`;
`crates/ofd-core/src/sign.rs:150`;
`crates/ofd-core/src/ses.rs:162`;
`crates/ofd-core/tests/bare_cff.rs`.

## Recommended Implementation Order

1. Add a strict/conformance mode separate from permissive real-world parsing:
   namespace checks, required fields, ID/reference validation, ST_Loc rules,
   schema cardinalities, and structured warnings/errors.
2. Fix visible rendering deviations in the core subset:
   object-boundary clipping, path FillColor default, text StrokeColor default,
   line cap/join/dash/miter rendering, PageRes, template ZOrder, template area
   fallback, relative signature paths, and annotation visibility.
3. Complete text semantics:
   HScale, ReadDirection, CharDirection, X/Y inheritance, text escapes, style
   transforms, and full CGTransform cases.
4. Complete image and path primitives:
   image masks, borders, substitution, TIFF/CCITT, path `S`/`A`, and XML region
   paths.
5. Implement standard color machinery:
   ColorSpace/Index/palette/profile/BPC, patterns, and gradients.
6. Add structural features:
   outlines/bookmarks, actions, attachments, versions, custom tags, extensions,
   composite objects, video/audio metadata and host-facing hooks.
7. Add full signature authenticity verification behind an optional feature,
   preserving the current lightweight digest check.

