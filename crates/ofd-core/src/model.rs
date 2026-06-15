//! In-memory object model for an OFD document, aligned with **GB/T 33190-2016**
//! ("电子文件存储与交换格式 版式文档" / OFD).
//!
//! The struct and field layout mirrors the standard's `CT_*` complex types so
//! the model is a faithful, self-documenting representation of the format. Some
//! fields are parsed and carried even though the current renderer does not yet
//! consume them (e.g. line cap/join, page bleed box, font weight) — they are
//! part of the standard and kept so the model stays complete.
//!
//! Coordinate values are in millimetres unless noted; OFD uses a top-left origin
//! with +X right and +Y down. The parser ([`crate::parser`]) produces these
//! types; the renderer ([`crate::render`]) consumes the subset it supports.

use crate::geom::{Matrix, Point, Rect};

/// The root `OFD` element: one package, one or more documents (`DocBody`).
#[derive(Debug, Default)]
pub struct OfdPackage {
    /// `OFD/@Version` — format version, e.g. "1.0" / "1.1".
    pub version: Option<String>,
    /// `OFD/@DocType` — usually "OFD".
    pub doc_type: Option<String>,
    pub documents: Vec<Document>,
}

/// A single document (`Document.xml` referenced by a `DocBody/DocRoot`).
#[derive(Debug, Default)]
pub struct Document {
    /// `CommonData/MaxUnitID` — largest object id in use.
    pub max_unit_id: u64,
    /// `CommonData/PageArea` — default page box used when a page omits its own.
    pub page_area: PageArea,
    /// `CommonData/DefaultCS` — default color space id.
    pub default_color_space: Option<u64>,
    pub pages: Vec<Page>,
    pub resources: Resources,
    pub outline: Vec<OutlineItem>,
    pub metadata: Metadata,
    /// Electronic seal stamps (GB/T 38540), rendered as appearance only.
    pub seals: Vec<Seal>,
    /// Page annotations (watermarks, stamps, highlights) drawn over content.
    pub annotations: Vec<Annotation>,
    /// Digital signatures / electronic seals (GB/T 33190 §18), modeled in full
    /// so they can be verified independently of rendering.
    pub signatures: Vec<Signature>,
    /// Non-fatal problems encountered while parsing (e.g. a referenced resource
    /// file that was missing or malformed). The document still parses, but these
    /// mean the render may be incomplete — callers can surface or fail on them.
    pub warnings: Vec<String>,
}

/// A signature entry (GB/T 33190 §18.2 `Signature.xml`).
#[derive(Debug, Clone)]
pub struct Signature {
    /// `Signatures/Signature/@ID`.
    pub id: String,
    /// `@Type`: a security seal (`Seal`, default) or a pure digital signature
    /// (`Sign`).
    pub sig_type: SignatureType,
    /// `SignedInfo/Provider/@ProviderName` — the signing component.
    pub provider: Option<String>,
    /// `SignedInfo/SignatureMethod` — signature algorithm OID (e.g. SM2-SM3).
    pub signature_method: Option<String>,
    /// `SignedInfo/SignatureDateTime`.
    pub signature_date_time: Option<String>,
    /// `SignedInfo/References` — the protected files and their digests.
    pub references: Vec<SignReference>,
    /// `SignedValue` — path to the signature-value file (CMS/SES).
    pub signed_value: Option<String>,
}

/// `Signature/@Type` (§18.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    /// Security electronic seal (carries a stamp picture).
    Seal,
    /// Pure digital signature (no visual seal).
    Sign,
}

/// One protected file and its digest (`References/Reference`, §18.2.2).
#[derive(Debug, Clone)]
pub struct SignReference {
    /// `@FileRef` — absolute in-package path of the protected file.
    pub file_ref: String,
    /// `References/@CheckMethod` — digest algorithm OID (default MD5).
    pub check_method: String,
    /// `CheckValue` — base64 of the file's binary digest.
    pub check_value: String,
}

/// `CT_PageArea` — the boxes describing a page's geometry, all in mm.
#[derive(Debug, Default, Clone, Copy)]
pub struct PageArea {
    /// `PhysicalBox` — the physical page size. Required by the standard.
    pub physical_box: Option<Rect>,
    /// `ApplicationBox` — the area available to content.
    pub application_box: Option<Rect>,
    /// `ContentBox` — the area actually containing content.
    pub content_box: Option<Rect>,
    /// `BleedBox` — the bleed area for printing.
    pub bleed_box: Option<Rect>,
}

impl PageArea {
    /// The effective render box: physical box, falling back to application box.
    pub fn render_box(&self) -> Option<Rect> {
        self.physical_box.or(self.application_box)
    }
}

/// A page annotation's drawable appearance. The contained objects' coordinates
/// have already had the appearance origin baked in, so they render like any
/// other page object.
#[derive(Debug, Clone)]
pub struct Annotation {
    /// Object id of the page this annotation belongs to (matches [`Page::id`]).
    pub page_id: u64,
    /// `Annot/@Type` — Stamp / Watermark / Link / Path / Highlight, etc.
    pub annot_type: String,
    pub objects: Vec<GraphicObject>,
}

/// An electronic seal's visual appearance and placement. Crypto verification is
/// out of scope; this carries only what's needed to draw the stamp.
#[derive(Debug, Clone)]
pub struct Seal {
    /// Object id of the page to stamp (matches [`Page::id`]).
    pub page_id: u64,
    /// Stamp box on that page, in mm.
    pub boundary: Rect,
    /// The stamp face, extracted from the SES seal structure.
    pub appearance: SealAppearance,
}

/// The picture a SES seal carries (GB/T 38540 `SES_ESPictureInfo`).
#[derive(Debug, Clone)]
pub enum SealAppearance {
    /// A raster picture (PNG/JPEG/BMP) to decode and blit.
    Raster { format: ImageFormat, data: Vec<u8> },
    /// A vector seal: an embedded OFD package whose first page is the stamp
    /// face, rendered recursively over a transparent background.
    Ofd(Vec<u8>),
}

/// Document metadata drawn from `DocInfo` / `CustomDatas`.
#[derive(Debug, Default, Clone)]
pub struct Metadata {
    pub title: Option<String>,
    pub author: Option<String>,
    pub subject: Option<String>,
    pub creator: Option<String>,
    pub creator_version: Option<String>,
    pub creation_date: Option<String>,
    pub doc_id: Option<String>,
}

/// A bookmark/outline entry (`CT_OutlineElem`) pointing at a page.
#[derive(Debug, Clone)]
pub struct OutlineItem {
    pub title: String,
    pub page_index: Option<usize>,
    pub children: Vec<OutlineItem>,
}

/// A single page (`CT_Page` / `Page.xml`).
#[derive(Debug, Default)]
pub struct Page {
    /// Object id of this page (referenced by `StampAnnot.PageRef`).
    pub id: u64,
    /// Per-page area override; falls back to the document default.
    pub area: Option<PageArea>,
    pub layers: Vec<Layer>,
}

/// `Layer/@Type` — z-order band for a layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LayerKind {
    Background,
    #[default]
    Body,
    Foreground,
    Custom,
}

/// A drawing layer (`CT_Layer`) holding graphic objects in paint order.
#[derive(Debug, Default, Clone)]
pub struct Layer {
    pub id: u64,
    pub kind: LayerKind,
    /// `Layer/@DrawParam` — default style for objects on this layer.
    pub draw_param: Option<u64>,
    pub objects: Vec<GraphicObject>,
}

/// Shared resources (`Res`), referenced by id from objects.
#[derive(Debug, Default)]
pub struct Resources {
    pub fonts: Vec<Font>,
    pub images: Vec<MultiMedia>,
    pub draw_params: Vec<DrawParam>,
    pub color_spaces: Vec<ColorSpace>,
}

/// A font resource (`CT_Font`).
#[derive(Debug, Clone, Default)]
pub struct Font {
    pub id: u64,
    /// `@FontName`.
    pub font_name: String,
    /// `@FamilyName`.
    pub family_name: Option<String>,
    /// `@Charset` — e.g. "unicode", "prc".
    pub charset: Option<String>,
    pub italic: bool,
    pub bold: bool,
    pub serif: bool,
    pub fixed_width: bool,
    /// Embedded font file bytes (`FontFile`). `None` => must substitute.
    pub data: Option<Vec<u8>>,
}

impl Font {
    /// Best family name to match a substitute against.
    pub fn family(&self) -> &str {
        self.family_name.as_deref().unwrap_or(&self.font_name)
    }
}

/// Multimedia resource kind (`MultiMedia/@Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaKind {
    #[default]
    Image,
    Audio,
    Video,
}

/// An embedded media resource (`CT_MultiMedia`).
#[derive(Debug, Clone)]
pub struct MultiMedia {
    pub id: u64,
    pub kind: MediaKind,
    pub format: ImageFormat,
    pub data: Vec<u8>,
}

/// Raster image encodings an OFD may reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Bmp,
    /// Scanned-document codecs not yet decoded; rendered as a placeholder.
    Jbig2,
    Ccitt,
    Unknown,
}

/// Line cap style (`@Cap`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineCap {
    #[default]
    Butt,
    Round,
    Square,
}

/// Line join style (`@Join`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LineJoin {
    #[default]
    Miter,
    Round,
    Bevel,
}

/// A reusable style block (`CT_DrawParam`) supporting inheritance via `Relative`.
#[derive(Debug, Clone, Default)]
pub struct DrawParam {
    pub id: u64,
    pub relative: Option<u64>,
    pub line_width: Option<f32>,
    pub cap: Option<LineCap>,
    pub join: Option<LineJoin>,
    pub miter_limit: Option<f32>,
    pub dash_offset: Option<f32>,
    pub dash_pattern: Option<Vec<f32>>,
    pub fill_color: Option<OfdColor>,
    pub stroke_color: Option<OfdColor>,
}

/// Color space type (`ColorSpace/@Type`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorSpaceKind {
    Gray,
    Rgb,
    Cmyk,
}

/// A color space (`CT_ColorSpace`).
#[derive(Debug, Clone)]
pub struct ColorSpace {
    pub id: u64,
    pub kind: ColorSpaceKind,
    /// `@BitsPerComponent` — 1, 2, 4, 8 (default 8).
    pub bits_per_component: u8,
    /// Indexed palette entries, if any (`Palette/CV`).
    pub palette: Vec<Color>,
}

/// A color expression as defined by `CT_Color`: either a basic color resolved
/// through a color space/palette, or one of OFD's complex paint types.
#[derive(Debug, Clone)]
pub enum OfdColor {
    Basic(BasicColor),
    Pattern(PatternColor),
    Axial(AxialGradient),
    Radial(RadialGradient),
    Gouraud(GouraudGradient),
    LatticeGouraud(LatticeGouraudGradient),
}

impl From<Color> for OfdColor {
    fn from(value: Color) -> Self {
        OfdColor::Basic(BasicColor {
            components: Some(vec![value.r as f32, value.g as f32, value.b as f32]),
            index: None,
            color_space: None,
            alpha: value.a,
        })
    }
}

/// A basic color. Components are kept in the OFD color-space domain and resolved
/// by the renderer using `ColorSpace`, `Index`, `Palette`, and `DefaultCS`.
#[derive(Debug, Clone)]
pub struct BasicColor {
    pub components: Option<Vec<f32>>,
    pub index: Option<usize>,
    pub color_space: Option<u64>,
    pub alpha: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GradientMapType {
    Direct,
    Repeat,
    Reflect,
}

impl Default for GradientMapType {
    fn default() -> Self {
        GradientMapType::Direct
    }
}

#[derive(Debug, Clone)]
pub struct GradientSegment {
    pub position: Option<f32>,
    pub color: BasicColor,
}

#[derive(Debug, Clone)]
pub struct AxialGradient {
    pub map_type: GradientMapType,
    pub map_unit: Option<f32>,
    pub extend: u8,
    pub start: Point,
    pub end: Point,
    pub segments: Vec<GradientSegment>,
}

#[derive(Debug, Clone)]
pub struct RadialGradient {
    pub map_type: GradientMapType,
    pub map_unit: Option<f32>,
    pub eccentricity: f32,
    pub angle: f32,
    pub start: Point,
    pub end: Point,
    pub start_radius: f32,
    pub end_radius: f32,
    pub extend: u8,
    pub segments: Vec<GradientSegment>,
}

#[derive(Debug, Clone)]
pub struct GouraudPoint {
    pub x: f32,
    pub y: f32,
    pub edge_flag: Option<u8>,
    pub color: BasicColor,
}

#[derive(Debug, Clone)]
pub struct GouraudGradient {
    pub extend: bool,
    pub points: Vec<GouraudPoint>,
    pub back_color: Option<BasicColor>,
}

#[derive(Debug, Clone)]
pub struct LatticeGouraudGradient {
    pub vertices_per_row: usize,
    pub extend: bool,
    pub points: Vec<GouraudPoint>,
    pub back_color: Option<BasicColor>,
}

/// OFD pattern color (`CT_Pattern`): a page block cell tiled across the target.
#[derive(Debug, Clone)]
pub struct PatternColor {
    pub width: f32,
    pub height: f32,
    pub x_step: f32,
    pub y_step: f32,
    pub reflect: PatternReflect,
    pub relative_to: PatternRelativeTo,
    pub ctm: Matrix,
    pub cell_content: Vec<GraphicObject>,
    pub thumbnail: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternReflect {
    Normal,
    Column,
    Row,
    RowAndColumn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatternRelativeTo {
    Object,
    Page,
}

/// An RGBA color, 8 bits per channel. Color values from any OFD color space are
/// resolved to RGBA at parse time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const BLACK: Color = Color {
        r: 0,
        g: 0,
        b: 0,
        a: 255,
    };

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        Color { r, g, b, a: 255 }
    }
}

/// Attributes shared by every graphic object (`CT_GraphicUnit`).
#[derive(Debug, Clone)]
pub struct GraphicCommon {
    pub id: u64,
    /// `@Boundary` — object box in mm.
    pub boundary: Rect,
    /// `@Name` — optional object name.
    pub name: Option<String>,
    /// `@Visible` — whether the object is drawn (default true).
    pub visible: bool,
    /// `@CTM` — object-to-page transform; identity if absent.
    pub ctm: Matrix,
    /// `@DrawParam` — referenced style block id.
    pub draw_param: Option<u64>,
    /// `@LineWidth` (mm).
    pub line_width: f32,
    pub cap: LineCap,
    pub join: LineJoin,
    pub miter_limit: Option<f32>,
    pub dash_offset: Option<f32>,
    pub dash_pattern: Option<Vec<f32>>,
    /// `@Alpha` — 0..=255 object opacity.
    pub alpha: u8,
    /// `Clips/Clip` — drawing is restricted to the intersection of these areas.
    pub clip: Vec<ClipArea>,
}

impl Default for GraphicCommon {
    fn default() -> Self {
        Self {
            id: 0,
            boundary: Rect::new(0.0, 0.0, 0.0, 0.0),
            name: None,
            visible: true,
            ctm: Matrix::IDENTITY,
            draw_param: None,
            line_width: 0.353, // ~1px at 72dpi in mm; overridden by document
            cap: LineCap::Butt,
            join: LineJoin::Miter,
            miter_limit: None,
            dash_offset: None,
            dash_pattern: None,
            alpha: 255,
            clip: Vec::new(),
        }
    }
}

/// A single clip region (`Clip/Area`): a path plus an optional `Area/@CTM` that
/// further transforms it within the object's coordinate space (§8.4).
#[derive(Debug, Clone)]
pub struct ClipArea {
    pub ctm: Matrix,
    pub commands: Vec<PathCommand>,
}

/// A drawable object within a layer.
#[derive(Debug, Clone)]
pub enum GraphicObject {
    Text(TextObject),
    Path(PathObject),
    Image(ImageObject),
    /// A `PageBlock` / composite grouping (templates, seal appearances).
    Group(Vec<GraphicObject>),
}

/// Text reading direction (`@ReadDirection`, degrees: 0/90/180/270).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Direction(pub u16);

/// Text object (`CT_Text`). Glyph positions come from explicit per-glyph
/// `DeltaX`/`DeltaY` advances rather than font metrics — this is essential for
/// faithful CJK layout.
#[derive(Debug, Clone)]
pub struct TextObject {
    pub common: GraphicCommon,
    /// `@Font` — referenced font resource id.
    pub font_id: u64,
    /// `@Size` — font size in mm.
    pub font_size: f32,
    /// `@Stroke` — whether glyph outlines are stroked (default false).
    pub stroke: bool,
    /// `@Fill` — whether glyphs are filled (default true).
    pub fill: bool,
    /// `@HScale` — horizontal scale factor (default 1.0).
    pub h_scale: f32,
    /// `@ReadDirection` / `@CharDirection`.
    pub read_direction: Direction,
    pub char_direction: Direction,
    /// `@Weight` (100..900) and `@Italic`.
    pub weight: u16,
    pub italic: bool,
    pub fill_color: Option<OfdColor>,
    pub stroke_color: Option<OfdColor>,
    /// Explicit character→glyph mappings. Subsetted embedded fonts often lack a
    /// usable cmap, so OFD supplies glyph ids directly via `CGTransform`.
    pub cg_transforms: Vec<CgTransform>,
    pub runs: Vec<TextRun>,
}

/// Maps a span of text-code positions to explicit glyph ids (`CGTransform`).
#[derive(Debug, Clone)]
pub struct CgTransform {
    /// Index of the first character (over the object's concatenated TextCodes).
    pub code_position: usize,
    /// Number of characters this mapping covers.
    pub code_count: usize,
    /// Glyph ids to use for those characters.
    pub glyphs: Vec<u16>,
    /// Number of glyph ids in this transform. Kept so one-to-many and
    /// many-to-one mappings remain explicit even when producers include padding.
    pub glyph_count: usize,
}

/// A run of characters (`TextCode`) with an origin and explicit advances.
#[derive(Debug, Default, Clone)]
pub struct TextRun {
    pub text: String,
    /// Origin of the first glyph (mm, object space).
    pub origin_x: f32,
    pub origin_y: f32,
    /// Horizontal advances applied between successive glyphs (mm).
    pub delta_x: Vec<f32>,
    /// Vertical advances applied between successive glyphs (mm).
    pub delta_y: Vec<f32>,
}

/// Fill rule for a path (`@Rule`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FillRule {
    #[default]
    NonZero,
    EvenOdd,
}

/// Path object (`CT_Path`) carrying parsed sub-paths.
#[derive(Debug, Clone)]
pub struct PathObject {
    pub common: GraphicCommon,
    /// `@Stroke` — whether the path is stroked (default true).
    pub stroke: bool,
    /// `@Fill` — whether the path is filled (default false).
    pub fill: bool,
    pub fill_rule: FillRule,
    /// Explicit fill/stroke colors. `None` => inherit from the object's draw
    /// param, or (for stroke) the document default of black.
    pub fill_color: Option<OfdColor>,
    pub stroke_color: Option<OfdColor>,
    pub commands: Vec<PathCommand>,
}

/// A single path command parsed from `AbbreviatedData` (coordinates in mm).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathCommand {
    MoveTo {
        x: f32,
        y: f32,
    },
    LineTo {
        x: f32,
        y: f32,
    },
    /// Cubic Bézier (`B`).
    CubicTo {
        x1: f32,
        y1: f32,
        x2: f32,
        y2: f32,
        x: f32,
        y: f32,
    },
    /// Quadratic Bézier (`Q`).
    QuadTo {
        x1: f32,
        y1: f32,
        x: f32,
        y: f32,
    },
    /// Close subpath (`C`).
    Close,
}

/// Image object (`CT_Image`) referencing a [`MultiMedia`] resource.
#[derive(Debug, Clone)]
pub struct ImageObject {
    pub common: GraphicCommon,
    /// `@ResourceID` — the image to draw.
    pub resource_id: u64,
    /// `@Substitution` — fallback image id.
    pub substitution: Option<u64>,
    /// `@ImageMask` — id of an image used as a stencil mask.
    pub image_mask: Option<u64>,
    /// `Border` — optional image border.
    pub border: Option<ImageBorder>,
}

/// An image object's border (`CT_Image/Border`).
#[derive(Debug, Clone)]
pub struct ImageBorder {
    pub line_width: f32,
    pub horizontal_corner_radius: f32,
    pub vertical_corner_radius: f32,
    pub color: Option<OfdColor>,
}
