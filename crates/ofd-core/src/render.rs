//! Rasterizing renderer. Walks a [`Document`] page's layers in z-order and
//! paints them into an RGBA bitmap via `tiny-skia`. The same code path serves
//! on-screen display (bitmap → canvas) and image export.
//!
//! Coordinate flow: object-space (mm) → page-space (mm) via `Boundary` + `CTM`
//! → device pixels via `dpi/25.4`. Glyph outlines are baked into object-space mm
//! before the page transform is applied.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::rc::Rc;
use std::sync::Arc;

use tiny_skia::{
    FillRule as SkFillRule, FilterQuality, LineCap as SkLineCap, LineJoin as SkLineJoin, Mask,
    Paint, Path as SkPath, PathBuilder, Pattern as SkPattern, Pixmap, PixmapPaint, Shader,
    SpreadMode, Stroke, StrokeDash, Transform,
};
use ttf_parser::{Face, OutlineBuilder};

use crate::error::{OfdError, Result};
use crate::fonts::{FontResolver, ResolvedFont};
use crate::geom::{Matrix, Rect};
use crate::model::*;

/// Default text stem-darkening (fraction of font size). Tuned so CJK text
/// weight matches the reference renderer; see the golden fixtures.
pub const DEFAULT_STEM_DARKENING: f32 = 0.0;

/// A rendered page as tightly-packed, straight-alpha RGBA8 pixels in row-major
/// order. The color channels are not premultiplied, matching image encoders and
/// browser APIs such as `ImageData`/`putImageData`.
#[derive(Debug, Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Options controlling a render.
#[derive(Clone, Debug)]
pub struct RenderLimits {
    /// Maximum output surface area. At RGBA8 this default permits about 64 MiB.
    pub max_page_pixels: u64,
    /// Maximum decoded pixel count for one raster resource.
    pub max_image_pixels: u64,
    /// Decoder allocation budget for one raster resource.
    pub max_image_bytes: u64,
    /// Conservative peak working-set budget while decoding, converting, and
    /// resizing one raster resource. This includes coexisting straight-alpha,
    /// decoder-native, and premultiplied buffers.
    pub max_image_working_bytes: u64,
    /// Maximum cumulative raster pixels decoded during one top-level render.
    /// Cache hits are free; cache misses are charged before expensive decoding.
    pub max_raster_decode_pixels: u64,
    /// Maximum decoded RGBA bytes retained by a long-lived render session.
    pub max_decoded_image_cache_bytes: u64,
    /// Maximum JBIG2 segments, references, and cumulative symbol slots.
    pub max_jbig2_items: usize,
    /// Maximum cumulative pixels decoded across JBIG2 region segments.
    pub max_jbig2_decode_pixels: u64,
    /// Maximum nesting of CompositeGraphicUnit references.
    pub max_composite_depth: usize,
    /// Maximum number of `DrawParam/Relative` links followed per lookup.
    pub max_draw_param_depth: usize,
    /// Cumulative full-page pixels held by nested composite offscreens.
    pub max_composite_surface_pixels: u64,
    /// Maximum nesting of pattern paints through pattern cell content.
    pub max_pattern_depth: usize,
    /// Cumulative pixels held by nested pattern tile surfaces.
    pub max_pattern_surface_pixels: u64,
    /// Maximum recursive embedded-OFD seal appearances.
    pub max_embedded_ofd_depth: usize,
    /// Maximum graphic-object visits in one top-level render, including
    /// repeated composite/pattern expansion and embedded vector seals.
    pub max_rendered_objects: u64,
    /// Maximum cumulative text source/mapping slots outlined in one top-level
    /// render, including repeated resource expansion.
    pub max_rendered_glyphs: u64,
    /// Maximum cumulative path commands converted in one top-level render.
    pub max_rendered_path_commands: u64,
    /// Maximum cumulative pixels allocated for raster clip/boundary masks in one
    /// top-level render.
    pub max_mask_pixels: u64,
    /// Maximum cumulative Gouraud triangle/background/mask pixel visits in one
    /// top-level render.
    pub max_gouraud_raster_pixels: u64,
    /// Maximum cumulative axial/radial gradient pixels sampled in one
    /// top-level render.
    pub max_gradient_raster_pixels: u64,
    /// Maximum cumulative compressed bytes parsed from embedded-OFD seal
    /// appearances in one top-level render.
    pub max_embedded_ofd_bytes: u64,
    /// Maximum cumulative output pixels allocated for recursively rendered
    /// embedded-OFD seal pages in one top-level render.
    pub max_embedded_ofd_pixels: u64,
}

impl Default for RenderLimits {
    fn default() -> Self {
        Self {
            max_page_pixels: 16 * 1024 * 1024,
            max_image_pixels: 32 * 1024 * 1024,
            max_image_bytes: 128 * 1024 * 1024,
            max_image_working_bytes: 384 * 1024 * 1024,
            max_raster_decode_pixels: 128 * 1024 * 1024,
            max_decoded_image_cache_bytes: 256 * 1024 * 1024,
            max_jbig2_items: 100_000,
            max_jbig2_decode_pixels: 128 * 1024 * 1024,
            max_composite_depth: 64,
            max_draw_param_depth: 256,
            max_composite_surface_pixels: 64 * 1024 * 1024,
            max_pattern_depth: 32,
            max_pattern_surface_pixels: 64 * 1024 * 1024,
            max_embedded_ofd_depth: 8,
            max_rendered_objects: 1_000_000,
            max_rendered_glyphs: 10_000_000,
            max_rendered_path_commands: 10_000_000,
            max_mask_pixels: 4 * 1024 * 1024 * 1024,
            max_gouraud_raster_pixels: 512 * 1024 * 1024,
            max_gradient_raster_pixels: 512 * 1024 * 1024,
            max_embedded_ofd_bytes: 256 * 1024 * 1024,
            max_embedded_ofd_pixels: 128 * 1024 * 1024,
        }
    }
}

#[derive(Clone, Default)]
pub struct RenderOptions {
    /// Raw TTF/OTF bytes supplied by the host for declared-family lookup and
    /// deterministic fallback when a document's font is not embedded.
    /// `Arc` keeps large font files shared across option clones, render sessions,
    /// resolved font ids, and recursively rendered vector seals.
    pub fallback_fonts: Vec<Arc<Vec<u8>>>,
    /// Leave the page background transparent instead of filling it white. Used
    /// when rendering a vector seal's embedded OFD so it composites over the
    /// host page.
    pub transparent_background: bool,
    /// Stem-darkening for text, as a fraction of the font size. Filled glyphs
    /// are additionally stroked by this width so thin CJK strokes don't render
    /// lighter than system/commercial readers (which darken stems). 0 disables.
    pub text_stem_darkening: f32,
    /// Fail when a referenced font/image/composite cannot be rendered. The
    /// default remains best-effort for viewer use; validation and CLI `--strict`
    /// should enable this.
    pub strict: bool,
    /// Memory/work limits applied during rendering.
    pub limits: RenderLimits,
}

/// Render a single page to a bitmap at the given DPI.
///
/// This convenience entry point builds a short-lived [`RenderSession`]. Hosts
/// that render multiple pages or zoom levels should keep a session and call
/// [`RenderSession::render_page`] so fonts and decoded images are reused.
pub fn render_page(doc: &Document, page_index: usize, dpi: f32) -> Result<Bitmap> {
    render_page_with(doc, page_index, dpi, &RenderOptions::default())
}

/// Render a single page to a bitmap at the given DPI with explicit options.
pub fn render_page_with(
    doc: &Document,
    page_index: usize,
    dpi: f32,
    opts: &RenderOptions,
) -> Result<Bitmap> {
    RenderSession::new(doc, opts.clone()).render_page(page_index, dpi)
}

/// Long-lived renderer for one document.
///
/// The session owns all document-wide render state that is expensive or useful
/// to reuse: font resolution, resource indexes, and decoded image cache. It does
/// not own the document bytes/model; callers keep the parsed [`Document`] alive.
pub struct RenderSession<'a> {
    doc: &'a Document,
    fonts: FontResolver,
    draw_params: HashMap<u64, DrawParam>,
    images: HashMap<u64, &'a MultiMedia>,
    composites: HashMap<u64, &'a CompositeGraphicUnit>,
    color_spaces: HashMap<u64, &'a ColorSpace>,
    icc_transforms: HashMap<u64, Arc<moxcms::Transform8BitExecutor>>,
    default_color_space: Option<u64>,
    fallback_fonts: Vec<Arc<Vec<u8>>>,
    transparent_background: bool,
    stem_darkening: f32,
    strict: bool,
    limits: RenderLimits,
    decoded_images: HashMap<u64, Option<Arc<image::RgbaImage>>>,
    decoded_seals: HashMap<SealCacheKey, Arc<Pixmap>>,
    /// Shared byte accounting for decoded image and seal caches.
    decoded_image_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SealCacheKey {
    appearance: usize,
    dpi_bits: u32,
}

/// Work shared by a top-level page render and every nested resource expansion.
/// `Cell` keeps the accounting local and allocation-free while allowing child
/// render sessions for embedded OFD seals to share the same counters.
#[derive(Default)]
struct RenderBudget {
    rendered_objects: Cell<u64>,
    rendered_glyphs: Cell<u64>,
    rendered_path_commands: Cell<u64>,
    raster_decode_pixels: Cell<u64>,
    mask_pixels: Cell<u64>,
    gouraud_raster_pixels: Cell<u64>,
    gradient_raster_pixels: Cell<u64>,
    embedded_ofd_bytes: Cell<u64>,
    embedded_ofd_pixels: Cell<u64>,
}

impl RenderBudget {
    fn charge(counter: &Cell<u64>, amount: u64, limit: u64, resource: &str) -> Result<()> {
        let next = counter
            .get()
            .checked_add(amount)
            .ok_or_else(|| OfdError::ResourceLimit(format!("{resource} budget overflow")))?;
        if next > limit {
            return Err(OfdError::ResourceLimit(format!(
                "{resource} requires {next}; limit is {limit}"
            )));
        }
        counter.set(next);
        Ok(())
    }

    fn charge_object(&self, limit: u64) -> Result<()> {
        Self::charge(&self.rendered_objects, 1, limit, "rendered object count")
    }

    fn charge_glyphs(&self, glyphs: u64, limit: u64) -> Result<()> {
        Self::charge(&self.rendered_glyphs, glyphs, limit, "rendered glyph work")
    }

    fn charge_path_commands(&self, commands: u64, limit: u64) -> Result<()> {
        Self::charge(
            &self.rendered_path_commands,
            commands,
            limit,
            "rendered path-command work",
        )
    }

    fn charge_raster_decode(&self, pixels: u64, limit: u64) -> Result<()> {
        Self::charge(
            &self.raster_decode_pixels,
            pixels,
            limit,
            "raster decode pixels",
        )
    }

    fn charge_mask_pixels(&self, pixels: u64, limit: u64) -> Result<()> {
        Self::charge(&self.mask_pixels, pixels, limit, "render mask pixels")
    }

    fn charge_gouraud(&self, pixels: u64, limit: u64) -> Result<()> {
        Self::charge(
            &self.gouraud_raster_pixels,
            pixels,
            limit,
            "Gouraud raster work",
        )
    }

    fn charge_gradient(&self, pixels: u64, limit: u64) -> Result<()> {
        Self::charge(
            &self.gradient_raster_pixels,
            pixels,
            limit,
            "gradient raster work",
        )
    }

    fn charge_embedded_bytes(&self, bytes: u64, limit: u64) -> Result<()> {
        Self::charge(&self.embedded_ofd_bytes, bytes, limit, "embedded OFD bytes")
    }

    fn charge_embedded_pixels(&self, pixels: u64, limit: u64) -> Result<()> {
        Self::charge(
            &self.embedded_ofd_pixels,
            pixels,
            limit,
            "embedded OFD page pixels",
        )
    }
}

impl<'a> RenderSession<'a> {
    /// Build a reusable render session for a parsed document.
    pub fn new(doc: &'a Document, opts: RenderOptions) -> Self {
        let mut draw_params = HashMap::new();
        for draw_param in &doc.resources.draw_params {
            draw_params
                .entry(draw_param.id)
                .or_insert_with(|| draw_param.clone());
        }
        let mut images = HashMap::new();
        for image in &doc.resources.images {
            images.entry(image.id).or_insert(image);
        }
        let mut composites = HashMap::new();
        for composite in &doc.resources.composite_graphic_units {
            composites.entry(composite.id).or_insert(composite);
        }
        let mut color_spaces = HashMap::new();
        for color_space in &doc.resources.color_spaces {
            color_spaces.entry(color_space.id).or_insert(color_space);
        }
        let mut icc_transforms = HashMap::new();
        for (&id, color_space) in &color_spaces {
            if let Some(transform) = create_icc_transform(color_space) {
                icc_transforms.insert(id, transform);
            }
        }

        Self {
            doc,
            fonts: FontResolver::with_bundled(&doc.resources.fonts, &opts.fallback_fonts),
            draw_params,
            images,
            composites,
            color_spaces,
            icc_transforms,
            default_color_space: doc.default_color_space,
            fallback_fonts: opts.fallback_fonts,
            transparent_background: opts.transparent_background,
            stem_darkening: opts.text_stem_darkening.max(0.0),
            strict: opts.strict,
            limits: opts.limits,
            decoded_images: HashMap::new(),
            decoded_seals: HashMap::new(),
            decoded_image_bytes: 0,
        }
    }

    /// Render a page with this session, reusing cached document resources.
    pub fn render_page(&mut self, page_index: usize, dpi: f32) -> Result<Bitmap> {
        self.render_page_with_budget(page_index, dpi, Rc::new(RenderBudget::default()), false)
    }

    fn render_page_with_budget(
        &mut self,
        page_index: usize,
        dpi: f32,
        budget: Rc<RenderBudget>,
        embedded: bool,
    ) -> Result<Bitmap> {
        let doc = self.doc;
        let page = doc
            .pages
            .get(page_index)
            .ok_or_else(|| OfdError::Malformed(format!("no page {page_index}")))?;
        let area = page
            .area
            .unwrap_or(doc.page_area)
            .render_box()
            .ok_or_else(|| OfdError::Malformed("page has no area".into()))?;

        if !dpi.is_finite() || dpi <= 0.0 || !valid_rect(area) {
            return Err(OfdError::Malformed(format!(
                "invalid render geometry: area={area:?}, dpi={dpi}"
            )));
        }
        let scale = dpi / crate::geom::MM_PER_INCH;
        let width_f = (area.w * scale).round().max(1.0);
        let height_f = (area.h * scale).round().max(1.0);
        if width_f > u32::MAX as f32 || height_f > u32::MAX as f32 {
            return Err(OfdError::ResourceLimit(
                "render dimensions exceed u32 range".into(),
            ));
        }
        let width = width_f as u32;
        let height = height_f as u32;
        let pixels = u64::from(width) * u64::from(height);
        if pixels > self.limits.max_page_pixels {
            return Err(OfdError::ResourceLimit(format!(
                "page requires {pixels} pixels; limit is {}",
                self.limits.max_page_pixels
            )));
        }
        if embedded {
            budget.charge_embedded_pixels(pixels, self.limits.max_embedded_ofd_pixels)?;
        }

        let mut pixmap = Pixmap::new(width, height)
            .ok_or_else(|| OfdError::Malformed("invalid page size".into()))?;
        if !self.transparent_background {
            pixmap.fill(tiny_skia::Color::WHITE);
        }

        let frame = RenderFrame {
            // Page-space (mm) -> device (px). Object transforms are pre-concatenated.
            base: Transform::from_scale(scale, scale),
            // Offset so the page's PhysicalBox origin maps to (0,0).
            origin: (area.x, area.y),
            size: (width, height),
            dpi,
        };
        let mut ctx = RenderCtx {
            session: self,
            frame,
            composite_stack: Vec::new(),
            composite_surface_pixels: 0,
            pattern_depth: 0,
            pattern_surface_pixels: 0,
            budget,
        };

        // Paint layers in z-order: Background, then Body, then Foreground, Custom.
        for kind in [
            LayerKind::Background,
            LayerKind::Body,
            LayerKind::Foreground,
            LayerKind::Custom,
        ] {
            for layer in page.layers.iter().filter(|l| l.kind == kind) {
                let inherited = layer.draw_param.as_slice();
                for obj in &layer.objects {
                    ctx.paint_object(&mut pixmap, obj, inherited)?;
                }
            }
        }

        // Page annotations (watermarks, stamps) over the content.
        for annot in doc
            .annotations
            .iter()
            .filter(|annotation| annotation.page_id == page.id && annotation.visible)
        {
            for obj in &annot.objects {
                ctx.paint_object(&mut pixmap, obj, &[])?;
            }
        }

        // Electronic seal stamps placed on this page (drawn on top).
        for seal in doc.seals.iter().filter(|s| s.page_id == page.id) {
            ctx.paint_seal(&mut pixmap, seal)?;
        }

        Ok(Bitmap {
            width,
            height,
            rgba: pixmap_into_straight_rgba(pixmap),
        })
    }

    fn decoded_image_rgba(
        &mut self,
        resource_id: u64,
        budget: &RenderBudget,
    ) -> Result<Option<Arc<image::RgbaImage>>> {
        if !self.decoded_images.contains_key(&resource_id) {
            let decoded = match self.images.get(&resource_id) {
                Some(media) => match decode_image_rgba(media, &self.limits, budget) {
                    Ok(image) => Some(Arc::new(image)),
                    Err(error @ OfdError::ResourceLimit(_)) => return Err(error),
                    Err(_) => None,
                },
                None => None,
            };
            if let Some(image) = decoded.as_ref() {
                let bytes = u64::from(image.width())
                    .saturating_mul(u64::from(image.height()))
                    .saturating_mul(4);
                if self.reserve_decoded_cache(bytes) {
                    self.decoded_images.insert(resource_id, decoded.clone());
                }
            } else {
                self.decoded_images.insert(resource_id, None);
            }
            // An image larger than the cache budget is still returned for this
            // draw, but it is not retained after the caller drops the Arc.
            if !self.decoded_images.contains_key(&resource_id) {
                return Ok(decoded);
            }
        }
        Ok(self.decoded_images.get(&resource_id).and_then(Clone::clone))
    }

    fn reserve_decoded_cache(&mut self, bytes: u64) -> bool {
        if bytes > self.limits.max_decoded_image_cache_bytes {
            return false;
        }
        match self.decoded_image_bytes.checked_add(bytes) {
            Some(total) if total <= self.limits.max_decoded_image_cache_bytes => {
                self.decoded_image_bytes = total;
            }
            _ => {
                self.decoded_images.clear();
                self.decoded_seals.clear();
                self.decoded_image_bytes = bytes;
            }
        }
        true
    }
}

#[derive(Clone, Copy)]
struct RenderFrame {
    base: Transform,
    origin: (f32, f32),
    size: (u32, u32),
    /// Device resolution, propagated to recursively-rendered vector seals.
    dpi: f32,
}

struct RenderCtx<'s, 'a> {
    session: &'s mut RenderSession<'a>,
    frame: RenderFrame,
    composite_stack: Vec<u64>,
    composite_surface_pixels: u64,
    pattern_depth: usize,
    pattern_surface_pixels: u64,
    budget: Rc<RenderBudget>,
}

struct ColorPaintContext<'a> {
    common: &'a GraphicCommon,
    path: Option<&'a SkPath>,
    transform: Transform,
    fill_rule: Option<SkFillRule>,
}

#[derive(Clone, Copy)]
struct DrawParamSources<'a> {
    local: Option<u64>,
    area: Option<u64>,
    inherited: &'a [u64],
}

impl<'a> DrawParamSources<'a> {
    fn object(local: Option<u64>, inherited: &'a [u64]) -> Self {
        Self {
            local,
            area: None,
            inherited,
        }
    }

    fn clip(local: Option<u64>, area: Option<u64>, inherited: &'a [u64]) -> Self {
        Self {
            local,
            area,
            inherited,
        }
    }

    fn ids(self) -> impl Iterator<Item = u64> + 'a {
        self.local
            .into_iter()
            .chain(self.area)
            .chain(self.inherited.iter().copied())
    }
}

impl<'s, 'a> RenderCtx<'s, 'a> {
    /// Object placement transform (no object CTM): base ∘ translate(-pageOrigin)
    /// ∘ translate(boundary). The object's own CTM and a clip area's CTM are both
    /// applied on top of this shared frame.
    fn placement_transform(&self, boundary: Rect) -> Transform {
        self.frame
            .base
            .pre_translate(-self.frame.origin.0, -self.frame.origin.1)
            .pre_translate(boundary.x, boundary.y)
    }

    /// Object-space (mm) → device transform: placement ∘ CTM.
    fn object_transform(&self, common: &GraphicCommon) -> Transform {
        let c = common.ctm;
        let m = Transform::from_row(c.a, c.b, c.c, c.d, c.e, c.f);
        self.placement_transform(common.boundary).pre_concat(m)
    }

    /// Build the effective mask required by §8.5: the object's Boundary always
    /// clips, Areas in one Clip are unioned, and multiple Clips are intersected.
    fn object_mask(
        &mut self,
        common: &GraphicCommon,
        inherited_dps: &[u64],
    ) -> Result<Option<Mask>> {
        let mut result = self.new_mask()?;
        let Some(boundary) = tiny_skia::Rect::from_xywh(
            0.0,
            0.0,
            common.boundary.w.max(0.0),
            common.boundary.h.max(0.0),
        ) else {
            if self.session.strict {
                return Err(OfdError::Render(format!(
                    "graphic {} has invalid Boundary {:?}",
                    common.id, common.boundary
                )));
            }
            // Best-effort mode still honors the invalid/empty boundary by
            // clipping the object to nothing rather than drawing unbounded.
            return Ok(Some(result));
        };
        let mut boundary_path = PathBuilder::new();
        boundary_path.push_rect(boundary);
        if let Some(path) = boundary_path.finish() {
            // Boundary is in the containing coordinate system. The object's CTM
            // transforms its contents inside that fixed box, not the box itself.
            result.fill_path(
                &path,
                SkFillRule::Winding,
                true,
                self.placement_transform(common.boundary),
            );
        }

        for clip in &common.clips {
            let mut union = self.new_mask()?;
            for area in &clip.areas {
                self.add_clip_area(&mut union, common, area, inherited_dps)?;
            }
            intersect_masks(&mut result, &union);
        }
        Ok(Some(result))
    }

    fn add_clip_area(
        &mut self,
        union: &mut Mask,
        owner: &GraphicCommon,
        area: &ClipArea,
        inherited_dps: &[u64],
    ) -> Result<()> {
        let area_transform = self
            .object_transform(owner)
            .pre_concat(matrix_transform(area.ctm));
        let mut area_mask = self.new_mask()?;

        match &area.shape {
            ClipShape::Path(path) => {
                self.budget.charge_path_commands(
                    path.commands.len() as u64,
                    self.session.limits.max_rendered_path_commands,
                )?;
                let Some(shape) = sk_path(&path.commands) else {
                    return self.skip_or_error(format!(
                        "clip path {} has invalid or empty geometry",
                        path.common.id
                    ));
                };
                let transform = area_transform
                    .pre_translate(path.common.boundary.x, path.common.boundary.y)
                    .pre_concat(matrix_transform(path.common.ctm));
                if path.fill {
                    let rule = match path.fill_rule {
                        FillRule::NonZero => SkFillRule::Winding,
                        FillRule::EvenOdd => SkFillRule::EvenOdd,
                    };
                    area_mask.fill_path(&shape, rule, true, transform);
                }
                if path.stroke {
                    let sources = DrawParamSources::clip(
                        path.common.draw_param,
                        area.draw_param,
                        inherited_dps,
                    );
                    let stroke = self.stroke_for(&path.common, sources)?;
                    if let Some(outline) = shape.stroke(&stroke, 1.0) {
                        area_mask.fill_path(&outline, SkFillRule::Winding, true, transform);
                    }
                }
                self.clip_shape_to_boundary(&mut area_mask, path.common.boundary, area_transform)?;
            }
            ClipShape::Text(text) => {
                let Some(shape) = self.build_text_path(text)? else {
                    return Ok(());
                };
                let transform = area_transform
                    .pre_translate(text.common.boundary.x, text.common.boundary.y)
                    .pre_concat(matrix_transform(text.common.ctm));
                if text.fill {
                    area_mask.fill_path(&shape, SkFillRule::Winding, true, transform);
                }
                if text.stroke {
                    let sources = DrawParamSources::clip(
                        text.common.draw_param,
                        area.draw_param,
                        inherited_dps,
                    );
                    let stroke = self.stroke_for(&text.common, sources)?;
                    if let Some(outline) = shape.stroke(&stroke, 1.0) {
                        area_mask.fill_path(&outline, SkFillRule::Winding, true, transform);
                    }
                }
                self.clip_shape_to_boundary(&mut area_mask, text.common.boundary, area_transform)?;
            }
        }
        union_masks(union, &area_mask);
        Ok(())
    }

    fn clip_shape_to_boundary(
        &self,
        mask: &mut Mask,
        boundary: Rect,
        parent_transform: Transform,
    ) -> Result<()> {
        // Some legacy producers omit Boundary on clip child objects. The Area is
        // still well-defined in its owner's object space, so only apply the child
        // boundary when it has a positive extent.
        if boundary.w <= 0.0 || boundary.h <= 0.0 {
            return Ok(());
        }
        let rect = tiny_skia::Rect::from_xywh(0.0, 0.0, boundary.w, boundary.h)
            .ok_or_else(|| OfdError::Malformed("invalid clip-shape Boundary".into()))?;
        let mut builder = PathBuilder::new();
        builder.push_rect(rect);
        if let Some(path) = builder.finish() {
            mask.intersect_path(
                &path,
                SkFillRule::Winding,
                true,
                parent_transform.pre_translate(boundary.x, boundary.y),
            );
        }
        Ok(())
    }

    fn new_mask(&mut self) -> Result<Mask> {
        let pixels = u64::from(self.frame.size.0) * u64::from(self.frame.size.1);
        self.budget
            .charge_mask_pixels(pixels, self.session.limits.max_mask_pixels)?;
        Mask::new(self.frame.size.0, self.frame.size.1)
            .ok_or_else(|| OfdError::ResourceLimit("could not allocate render mask".into()))
    }

    fn paint_object(
        &mut self,
        pixmap: &mut Pixmap,
        obj: &GraphicObject,
        inherited_dps: &[u64],
    ) -> Result<()> {
        self.budget
            .charge_object(self.session.limits.max_rendered_objects)?;
        // Objects marked `Visible="false"` are part of the document but must not
        // be drawn (GB/T 33190 §8.5).
        match obj {
            GraphicObject::Text(t) if t.common.visible => {
                self.paint_text(pixmap, t, inherited_dps)?
            }
            GraphicObject::Path(p) if p.common.visible => {
                self.paint_path(pixmap, p, inherited_dps)?
            }
            GraphicObject::Image(i) if i.common.visible => {
                self.paint_image(pixmap, i, inherited_dps)?
            }
            GraphicObject::Group(g) => {
                for o in g {
                    self.paint_object(pixmap, o, inherited_dps)?;
                }
            }
            GraphicObject::Composite(co) if co.common.visible => {
                self.paint_composite(pixmap, co, inherited_dps)?
            }
            _ => {}
        }
        Ok(())
    }

    /// Draw a `CompositeObject` (§13): look up the referenced
    /// [`CompositeGraphicUnit`] and paint its child objects in a frame shifted by
    /// the composite's placement (page origin → composite boundary, then the
    /// composite's CTM). The unit's objects carry their own boundaries/CTMs in the
    /// unit's coordinate space, so they render normally on top of this frame.
    fn paint_composite(
        &mut self,
        pixmap: &mut Pixmap,
        co: &CompositeObject,
        inherited_dps: &[u64],
    ) -> Result<()> {
        if co.common.alpha == 0 {
            return Ok(());
        }
        if self.composite_stack.len() >= self.session.limits.max_composite_depth {
            return self.skip_or_error(format!(
                "composite nesting exceeds {}",
                self.session.limits.max_composite_depth
            ));
        }
        if self.composite_stack.contains(&co.resource_id) {
            return self.skip_or_error(format!(
                "cyclic CompositeGraphicUnit reference at id {}",
                co.resource_id
            ));
        }
        let Some(unit) = self.session.composites.get(&co.resource_id).copied() else {
            return self.skip_or_error(format!(
                "unresolved CompositeGraphicUnit id {}",
                co.resource_id
            ));
        };
        if !unit.width.is_finite()
            || !unit.height.is_finite()
            || unit.width <= 0.0
            || unit.height <= 0.0
        {
            return self.skip_or_error(format!(
                "CompositeGraphicUnit {} has invalid size {}x{}",
                unit.id, unit.width, unit.height
            ));
        }
        let Some(unit_rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, unit.width, unit.height) else {
            return self.skip_or_error(format!(
                "CompositeGraphicUnit {} has invalid size {}x{}",
                unit.id, unit.width, unit.height
            ));
        };
        let mut mask = self.object_mask(&co.common, inherited_dps)?;
        let surface_pixels = u64::from(self.frame.size.0) * u64::from(self.frame.size.1);
        let next_surface_pixels = self
            .composite_surface_pixels
            .checked_add(surface_pixels)
            .ok_or_else(|| OfdError::ResourceLimit("composite surface size overflow".into()))?;
        if next_surface_pixels > self.session.limits.max_composite_surface_pixels {
            return self.skip_or_error(format!(
                "nested composites require {next_surface_pixels} intermediate pixels; limit is {}",
                self.session.limits.max_composite_surface_pixels
            ));
        }
        self.composite_surface_pixels = next_surface_pixels;
        let mut offscreen = Pixmap::new(self.frame.size.0, self.frame.size.1).ok_or_else(|| {
            OfdError::ResourceLimit("could not allocate composite surface".into())
        })?;
        // Effective base for the unit's children: page→device with the page origin
        // and composite placement (boundary translate ∘ CTM) folded in, then a
        // zeroed origin so each child's own boundary translates from there.
        let placement = self.placement_transform(co.common.boundary);
        let c = co.common.ctm;
        let inner_base = placement.pre_concat(Transform::from_row(c.a, c.b, c.c, c.d, c.e, c.f));

        let mut unit_builder = PathBuilder::new();
        unit_builder.push_rect(unit_rect);
        if let Some(unit_path) = unit_builder.finish() {
            let mut unit_mask = self.new_mask()?;
            unit_mask.fill_path(&unit_path, SkFillRule::Winding, true, inner_base);
            if let Some(current) = mask.as_mut() {
                intersect_masks(current, &unit_mask);
            } else {
                mask = Some(unit_mask);
            }
        }

        let saved = self.frame;
        self.frame = RenderFrame {
            base: inner_base,
            origin: (0.0, 0.0),
            ..saved
        };
        self.composite_stack.push(co.resource_id);
        let mut composite_dps =
            Vec::with_capacity(inherited_dps.len() + usize::from(co.common.draw_param.is_some()));
        composite_dps.extend(co.common.draw_param);
        composite_dps.extend_from_slice(inherited_dps);
        let result = unit
            .objects
            .iter()
            .try_for_each(|o| self.paint_object(&mut offscreen, o, &composite_dps));
        self.composite_stack.pop();
        self.frame = saved;
        self.composite_surface_pixels -= surface_pixels;
        result?;

        let paint = PixmapPaint {
            opacity: co.common.alpha as f32 / 255.0,
            quality: FilterQuality::Bilinear,
            ..Default::default()
        };
        pixmap.draw_pixmap(
            0,
            0,
            offscreen.as_ref(),
            &paint,
            Transform::identity(),
            mask.as_ref(),
        );
        Ok(())
    }

    fn build_text_path(&mut self, t: &TextObject) -> Result<Option<SkPath>> {
        let mut source_slots = 0u64;
        for run in &t.runs {
            if !run.origin_x.is_finite()
                || !run.origin_y.is_finite()
                || run.delta_x.iter().any(|value| !value.is_finite())
                || run.delta_y.iter().any(|value| !value.is_finite())
            {
                return self.missing_or_invalid(format!(
                    "text object {} has non-finite positioning",
                    t.common.id
                ));
            }
            source_slots = source_slots
                .checked_add(run.text.chars().count() as u64)
                .ok_or_else(|| OfdError::ResourceLimit("text source-slot count overflow".into()))?;
        }
        let mapping_slots = t.cg_transforms.iter().try_fold(0u64, |total, transform| {
            let slots = transform.glyph_count.max(transform.glyphs.len()).max(1) as u64;
            total
                .checked_add(slots)
                .ok_or_else(|| OfdError::ResourceLimit("text mapping-slot count overflow".into()))
        })?;
        let glyph_work = source_slots
            .checked_add(mapping_slots)
            .ok_or_else(|| OfdError::ResourceLimit("text glyph-work count overflow".into()))?;
        self.budget
            .charge_glyphs(glyph_work, self.session.limits.max_rendered_glyphs)?;

        let Some(primary_font) = self
            .session
            .fonts
            .resolve_styled(t.font_id, t.weight, t.italic)
        else {
            return self.missing_or_invalid(format!("unresolved font id {}", t.font_id));
        };
        let primary_data = primary_font.data.clone();
        let Ok(primary_face) = Face::parse(&primary_data, primary_font.index) else {
            return self.missing_or_invalid(format!("font id {} is not parseable", t.font_id));
        };
        if primary_face.units_per_em() == 0
            || !t.font_size.is_finite()
            || t.font_size <= 0.0
            || !t.h_scale.is_finite()
            || t.h_scale < 0.0
        {
            return self.missing_or_invalid(format!(
                "text object {} has invalid font size {}",
                t.common.id, t.font_size
            ));
        }
        let h_scale = t.h_scale.max(0.0);
        let read_direction = normalize_direction(t.read_direction);
        let char_direction = normalize_direction(t.char_direction);
        let mut resolved_fonts = vec![primary_font.clone()];
        let mut resolved_font_indexes = HashMap::from([(font_face_key(&primary_font), 0usize)]);
        let mut positioned_runs = Vec::with_capacity(t.runs.len());
        let mut global_idx = 0usize;
        let mut covered_until = 0usize;
        let cg_index = cg_transform_index(&t.cg_transforms);
        for run in &t.runs {
            let chars: Vec<char> = run.text.chars().collect();
            // Resolve the run to a flat sequence of glyph slots. A CGTransform
            // (§11.4) maps a code span [CodePosition, +CodeCount) to GlyphCount
            // explicit glyphs — covering many-to-one (ligatures), one-to-many, and
            // many-to-many. Codes no transform covers map 1:1 through the font
            // cmap. `DeltaX`/`DeltaY` give one advance per *displayed glyph*, so
            // positioning iterates slots, not source characters.
            let slots = glyph_slots_for_run(
                &chars,
                global_idx,
                &mut covered_until,
                &cg_index,
                primary_font.trusted_glyph_ids,
                primary_font.cid_to_gid.as_deref(),
                |ch| {
                    let primary = cmap_slot(primary_face.glyph_index(ch), ch);
                    if primary.draw || ch.is_whitespace() {
                        return primary;
                    }
                    let Some(fallback) = self
                        .session
                        .fonts
                        .fallback_for_char_styled(t.font_id, t.weight, t.italic, ch)
                    else {
                        return primary;
                    };
                    let Ok(fallback_face) = Face::parse(&fallback.data, fallback.index) else {
                        return primary;
                    };
                    let Some(gid) = fallback_face.glyph_index(ch).filter(|glyph| glyph.0 != 0)
                    else {
                        return primary;
                    };
                    let key = font_face_key(&fallback);
                    let face_index = *resolved_font_indexes.entry(key).or_insert_with(|| {
                        let index = resolved_fonts.len();
                        resolved_fonts.push(fallback);
                        index
                    });
                    GlyphSlot {
                        gid,
                        draw: true,
                        face_index,
                    }
                },
            );
            global_idx = global_idx.saturating_add(chars.len());
            positioned_runs.push((run, slots));
        }

        let parsed_faces = resolved_fonts
            .iter()
            .map(|font| {
                Face::parse(&font.data, font.index).map_err(|_| {
                    OfdError::Render(format!("font id {} fallback is not parseable", t.font_id))
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let font_scales: Vec<f32> = parsed_faces
            .iter()
            .map(|face| t.font_size / face.units_per_em() as f32)
            .collect();
        let italic_shears: Vec<f32> = resolved_fonts
            .iter()
            .map(|font| if font.synthetic_italic { 0.21256 } else { 0.0 })
            .collect();

        let mut builder = PathBuilder::new();
        for (run, slots) in positioned_runs {
            let mut pen_x = run.origin_x;
            let mut pen_y = run.origin_y;
            for (k, slot) in slots.iter().enumerate() {
                let face = &parsed_faces[slot.face_index];
                let gscale = font_scales[slot.face_index];
                if slot.draw {
                    let mut ob = GlyphOutline {
                        builder: &mut builder,
                        pen_x,
                        pen_y,
                        scale: gscale,
                        h_scale,
                        angle: char_direction as f32,
                        italic_shear: italic_shears[slot.face_index],
                    };
                    face.outline_glyph(slot.gid, &mut ob);
                }
                // Advance by the explicit OFD delta. When the DeltaX list is
                // shorter than the glyph count, repeat its last value (producers
                // emit a single advance for uniform-width CJK runs and rely on the
                // reader to extend it). Fall back to the font's horizontal glyph
                // advance only when the run has *no* explicit positioning on either
                // axis — vertical text (DeltaY present, DeltaX absent) has X
                // advance 0.
                let fallback = if run.delta_x.is_empty() && run.delta_y.is_empty() {
                    let adv = glyph_advance(slot.gid, face, gscale, t.font_size) * h_scale;
                    read_advance_vector(adv, read_direction)
                } else {
                    (0.0, 0.0)
                };
                pen_x += advance(&run.delta_x, k, fallback.0);
                pen_y += advance(&run.delta_y, k, fallback.1);
            }
        }
        Ok(builder.finish())
    }

    fn paint_text(
        &mut self,
        pixmap: &mut Pixmap,
        t: &TextObject,
        inherited_dps: &[u64],
    ) -> Result<()> {
        let Some(path) = self.build_text_path(t)? else {
            return Ok(());
        };
        let transform = self.object_transform(&t.common);
        let mask = self.object_mask(&t.common, inherited_dps)?;
        let sources = DrawParamSources::object(t.common.draw_param, inherited_dps);
        let alpha = t.common.alpha;
        if t.fill {
            let fill = match t.fill_color.clone() {
                Some(color) => color,
                None => self
                    .dp_fill(sources)?
                    .unwrap_or_else(|| Color::BLACK.into()),
            };
            let stem_darkening = self.session.stem_darkening;
            let font_size = t.font_size;
            self.with_paint_for_color(
                &fill,
                alpha,
                ColorPaintContext {
                    common: &t.common,
                    path: Some(&path),
                    transform,
                    fill_rule: Some(SkFillRule::Winding),
                },
                |paint| {
                    pixmap.fill_path(&path, paint, SkFillRule::Winding, transform, mask.as_ref());
                    // Optional stem-darkening (opt-in, non-standard): outline the
                    // filled glyphs with a hairline of the same ink so thin CJK
                    // strokes can match heavier system rasterizers. The OFD
                    // `@Weight` is a font-selection hint, not a synthetic-bold
                    // instruction, so it is NOT emboldened here.
                    if stem_darkening > 0.0 {
                        let width = font_size * stem_darkening;
                        if width > 0.0 {
                            let sk = Stroke {
                                width,
                                line_join: SkLineJoin::Round,
                                ..Default::default()
                            };
                            pixmap.stroke_path(&path, paint, &sk, transform, mask.as_ref());
                        }
                    }
                },
            )?;
        }
        if t.stroke {
            let stroke = match t.stroke_color.clone() {
                Some(color) => Some(color),
                None => self.dp_stroke(sources)?,
            };
            if let Some(stroke) = stroke {
                let sk = self.stroke_for(&t.common, sources)?;
                self.with_paint_for_color(
                    &stroke,
                    alpha,
                    ColorPaintContext {
                        common: &t.common,
                        path: Some(&path),
                        transform,
                        fill_rule: None,
                    },
                    |paint| {
                        pixmap.stroke_path(&path, paint, &sk, transform, mask.as_ref());
                    },
                )?;
            }
        }
        Ok(())
    }

    fn paint_path(
        &mut self,
        pixmap: &mut Pixmap,
        p: &PathObject,
        inherited_dps: &[u64],
    ) -> Result<()> {
        self.budget.charge_path_commands(
            p.commands.len() as u64,
            self.session.limits.max_rendered_path_commands,
        )?;
        let Some(path) = sk_path(&p.commands) else {
            return self.skip_or_error(format!(
                "path object {} has invalid or empty geometry",
                p.common.id
            ));
        };
        let transform = self.object_transform(&p.common);
        let mask = self.object_mask(&p.common, inherited_dps)?;
        let sources = DrawParamSources::object(p.common.draw_param, inherited_dps);
        let alpha = p.common.alpha;

        if p.fill {
            // Resolve the fill color: explicit → draw param → black, but only
            // default to black when the path is fill-only. A `Fill="true"` path
            // with no color that is also stroked is an outline mark (e.g. the
            // ⊗ on invoices); black-filling it would hide the strokes.
            let color = match p.fill_color.clone() {
                Some(color) => Some(color),
                None => self.dp_fill(sources)?,
            }
            .or(if p.stroke {
                None
            } else {
                Some(Color::BLACK.into())
            });
            if let Some(fill) = color {
                let rule = match p.fill_rule {
                    FillRule::NonZero => SkFillRule::Winding,
                    FillRule::EvenOdd => SkFillRule::EvenOdd,
                };
                self.with_paint_for_color(
                    &fill,
                    alpha,
                    ColorPaintContext {
                        common: &p.common,
                        path: Some(&path),
                        transform,
                        fill_rule: Some(rule),
                    },
                    |paint| {
                        pixmap.fill_path(&path, paint, rule, transform, mask.as_ref());
                    },
                )?;
            }
        }
        if p.stroke {
            let stroke = match p.stroke_color.clone() {
                Some(color) => color,
                None => self
                    .dp_stroke(sources)?
                    .unwrap_or_else(|| Color::BLACK.into()),
            };
            let sk = self.stroke_for(&p.common, sources)?;
            self.with_paint_for_color(
                &stroke,
                alpha,
                ColorPaintContext {
                    common: &p.common,
                    path: Some(&path),
                    transform,
                    fill_rule: None,
                },
                |paint| {
                    pixmap.stroke_path(&path, paint, &sk, transform, mask.as_ref());
                },
            )?;
        }
        Ok(())
    }

    fn paint_image(
        &mut self,
        pixmap: &mut Pixmap,
        im: &ImageObject,
        inherited_dps: &[u64],
    ) -> Result<()> {
        // Image content is a unit square mapped onto the boundary (or by CTM).
        // Source pixels → object mm: scale by boundary size / image size.
        let obj = self.object_transform(&im.common);
        let object_mask = self.object_mask(&im.common, inherited_dps)?;
        let rgba = match self
            .session
            .decoded_image_rgba(im.resource_id, &self.budget)?
        {
            Some(image) => Some(image),
            None => match im.substitution {
                Some(id) => self.session.decoded_image_rgba(id, &self.budget)?,
                None => None,
            },
        };
        let Some(rgba) = rgba else {
            return self.skip_or_error(format!(
                "image resource {}{} is missing, unsupported, or failed to decode",
                im.resource_id,
                im.substitution
                    .map(|id| format!(" (substitution {id} also failed)"))
                    .unwrap_or_default()
            ));
        };
        let (iw, ih) = (rgba.width() as f32, rgba.height() as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return self.skip_or_error(format!(
                "image resource {} decoded with an empty size",
                im.resource_id
            ));
        }
        let image_mask = if let Some(mask_id) = im.image_mask {
            let Some(mask) = self.session.decoded_image_rgba(mask_id, &self.budget)? else {
                return self.skip_or_error(format!(
                    "image object {} mask resource {mask_id} is missing, unsupported, or failed to decode",
                    im.common.id
                ));
            };
            if mask.dimensions() != rgba.dimensions() {
                return self.skip_or_error(format!(
                    "image object {} mask resource {mask_id} is {}x{}, expected {}x{}",
                    im.common.id,
                    mask.width(),
                    mask.height(),
                    rgba.width(),
                    rgba.height()
                ));
            }
            let pixels = u64::from(mask.width()) * u64::from(mask.height());
            self.budget
                .charge_mask_pixels(pixels, self.session.limits.max_mask_pixels)?;
            if self.session.strict && !is_binary_image_mask(&mask) {
                return Err(OfdError::Render(format!(
                    "image object {} mask resource {mask_id} is not a black/white binary image",
                    im.common.id
                )));
            }
            Some(mask)
        } else {
            None
        };
        let to_obj = if im.common.ctm == Matrix::IDENTITY {
            Transform::from_scale(im.common.boundary.w / iw, im.common.boundary.h / ih)
        } else {
            // CTM maps the unit square to object space.
            Transform::from_scale(1.0 / iw, 1.0 / ih)
        };
        let transform = obj.pre_concat(to_obj);
        let paint = PixmapPaint {
            opacity: im.common.alpha as f32 / 255.0,
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        };

        // tiny-skia's draw_pixmap does not reliably downscale very large source
        // images (a full-page background can be ~3500px wide while its device
        // footprint is ~1000px). Pre-resize the source to its device footprint:
        // this both fixes that and improves downsampling quality.
        let scale_x = (transform.sx * transform.sx + transform.ky * transform.ky).sqrt();
        let scale_y = (transform.kx * transform.kx + transform.sy * transform.sy).sqrt();
        if !scale_x.is_finite() || !scale_y.is_finite() {
            return self.skip_or_error(format!(
                "image object {} has a non-finite transform",
                im.common.id
            ));
        }
        let mut tw = ((iw * scale_x).round() as u32).clamp(1, 8192);
        let mut th = ((ih * scale_y).round() as u32).clamp(1, 8192);
        let max_resize_pixels = self
            .session
            .limits
            .max_image_pixels
            .min(self.session.limits.max_image_bytes / 4)
            .min(self.session.limits.max_image_working_bytes / 12);
        if max_resize_pixels == 0 {
            return Err(OfdError::ResourceLimit(
                "image resize budget cannot hold one RGBA pixel".into(),
            ));
        }
        let target_pixels = u64::from(tw) * u64::from(th);
        if target_pixels > max_resize_pixels {
            let factor = (max_resize_pixels as f64 / target_pixels as f64).sqrt() as f32;
            tw = ((tw as f32 * factor).floor() as u32).max(1);
            th = ((th as f32 * factor).floor() as u32).max(1);
        }
        ensure_image_size(tw, th, &self.session.limits)?;

        if tw < rgba.width() || th < rgba.height() {
            let small =
                image::imageops::resize(&*rgba, tw, th, image::imageops::FilterType::Triangle);
            let small_mask = image_mask.as_ref().map(|mask| {
                image::imageops::resize(&**mask, tw, th, image::imageops::FilterType::Triangle)
            });
            let Some(src) = rgba_to_pixmap_masked(&small, small_mask.as_ref()) else {
                return Err(OfdError::ResourceLimit(
                    "could not allocate resized image surface".into(),
                ));
            };
            // Resized source → device: undo the source-pixel scaling baked above.
            let adj = transform.pre_concat(Transform::from_scale(iw / tw as f32, ih / th as f32));
            pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, adj, object_mask.as_ref());
        } else if let Some(src) = rgba_to_pixmap_masked(&rgba, image_mask.as_deref()) {
            pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, object_mask.as_ref());
        } else {
            return Err(OfdError::ResourceLimit(
                "could not allocate image surface".into(),
            ));
        }
        self.paint_image_border(pixmap, im, object_mask.as_ref())?;
        Ok(())
    }

    fn paint_image_border(
        &mut self,
        pixmap: &mut Pixmap,
        image: &ImageObject,
        mask: Option<&Mask>,
    ) -> Result<()> {
        let Some(border) = &image.border else {
            return Ok(());
        };
        if border.line_width == 0.0 {
            return Ok(());
        }
        if !border.line_width.is_finite()
            || border.line_width < 0.0
            || !border.horizontal_corner_radius.is_finite()
            || border.horizontal_corner_radius < 0.0
            || !border.vertical_corner_radius.is_finite()
            || border.vertical_corner_radius < 0.0
            || !border.dash_offset.is_finite()
        {
            return self.skip_or_error(format!(
                "image object {} has invalid Border geometry",
                image.common.id
            ));
        }
        let (width, height) = if image.common.ctm == Matrix::IDENTITY {
            (image.common.boundary.w, image.common.boundary.h)
        } else {
            (1.0, 1.0)
        };
        let Some(path) = rounded_rect_path(
            width,
            height,
            border.horizontal_corner_radius,
            border.vertical_corner_radius,
        ) else {
            return self.skip_or_error(format!(
                "image object {} has an invalid Border path",
                image.common.id
            ));
        };
        self.budget
            .charge_path_commands(10, self.session.limits.max_rendered_path_commands)?;
        let dash = if let Some(pattern) = &border.dash_pattern {
            match StrokeDash::new(pattern.clone(), border.dash_offset) {
                Some(dash) => Some(dash),
                None if self.session.strict => {
                    return Err(OfdError::Render(format!(
                        "image object {} has an invalid Border DashPattern",
                        image.common.id
                    )));
                }
                None => None,
            }
        } else {
            None
        };
        let stroke = Stroke {
            width: border.line_width,
            line_cap: SkLineCap::Butt,
            line_join: SkLineJoin::Miter,
            miter_limit: 4.234,
            dash,
        };
        let color = border.color.clone().unwrap_or_else(|| Color::BLACK.into());
        let transform = self.object_transform(&image.common);
        self.with_paint_for_color(
            &color,
            image.common.alpha,
            ColorPaintContext {
                common: &image.common,
                path: Some(&path),
                transform,
                fill_rule: None,
            },
            |paint| {
                pixmap.stroke_path(&path, paint, &stroke, transform, mask);
            },
        )
    }

    /// Draw an electronic seal's stamp face filling its box. Raster faces are
    /// decoded directly; vector (`ofd`) faces are rendered recursively over a
    /// transparent background so they composite onto the host page.
    fn paint_seal(&mut self, pixmap: &mut Pixmap, seal: &Seal) -> Result<()> {
        self.budget
            .charge_object(self.session.limits.max_rendered_objects)?;
        if !valid_rect(seal.boundary) {
            return self.skip_or_error(format!("seal has an invalid Boundary {:?}", seal.boundary));
        }
        if seal.clip.is_some_and(|clip| !valid_rect(clip)) {
            return self.skip_or_error(format!("seal has an invalid Clip {:?}", seal.clip));
        }
        let src = match self.decoded_seal(seal) {
            Ok(src) => src,
            Err(e) if self.session.strict => return Err(e),
            Err(_) => return Ok(()),
        };
        let (iw, ih) = (src.width() as f32, src.height() as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return self.skip_or_error("seal appearance decoded with an empty size".into());
        }
        let common = GraphicCommon {
            boundary: seal.boundary,
            ..Default::default()
        };
        let to_obj = Transform::from_scale(seal.boundary.w / iw, seal.boundary.h / ih);
        let transform = self.object_transform(&common).pre_concat(to_obj);

        // `StampAnnot/@Clip` (§18.2.3): the clip box is in the stamp's own
        // boundary-relative coordinates. For cross-page (骑缝) seals each page
        // clips the full seal to a different slice, so the pages reassemble it.
        let mask = if let Some(c) = seal.clip {
            let Some(rect) =
                tiny_skia::Rect::from_xywh(seal.boundary.x + c.x, seal.boundary.y + c.y, c.w, c.h)
            else {
                return self.skip_or_error("seal has an invalid stamp clip".into());
            };
            let mut m = self.new_mask()?;
            let mut pb = PathBuilder::new();
            pb.push_rect(rect);
            let Some(path) = pb.finish() else {
                return self.skip_or_error("seal stamp clip produced no path".into());
            };
            // The clip rect is already in absolute page mm.
            m.fill_path(
                &path,
                SkFillRule::Winding,
                true,
                self.placement_transform(Rect::new(0.0, 0.0, 0.0, 0.0)),
            );
            Some(m)
        } else {
            None
        };
        pixmap.draw_pixmap(
            0,
            0,
            src.as_ref().as_ref(),
            &PixmapPaint::default(),
            transform,
            mask.as_ref(),
        );
        Ok(())
    }

    fn decoded_seal(&mut self, seal: &Seal) -> Result<Arc<Pixmap>> {
        let dpi_bits = match seal.appearance.as_ref() {
            SealAppearance::Raster { .. } => 0,
            SealAppearance::Ofd(_) => self.frame.dpi.to_bits(),
        };
        let key = SealCacheKey {
            appearance: Arc::as_ptr(&seal.appearance) as usize,
            dpi_bits,
        };
        if let Some(cached) = self.session.decoded_seals.get(&key) {
            return Ok(cached.clone());
        }

        let pixmap = match seal.appearance.as_ref() {
            SealAppearance::Raster { format, data } => {
                decode_bytes(*format, data, &self.session.limits)?
            }
            SealAppearance::Ofd(bytes) => self.render_vector_seal(bytes)?,
        };
        let bytes = u64::from(pixmap.width())
            .saturating_mul(u64::from(pixmap.height()))
            .saturating_mul(4);
        let pixmap = Arc::new(pixmap);
        if self.session.reserve_decoded_cache(bytes) {
            self.session.decoded_seals.insert(key, pixmap.clone());
        }
        Ok(pixmap)
    }

    /// Render a vector seal's embedded OFD (its first page) to a transparent
    /// pixmap at the host resolution.
    fn render_vector_seal(&self, ofd_bytes: &[u8]) -> Result<Pixmap> {
        if self.session.limits.max_embedded_ofd_depth == 0 {
            return Err(OfdError::ResourceLimit(
                "embedded OFD seal nesting limit exceeded".into(),
            ));
        }
        let byte_count = u64::try_from(ofd_bytes.len())
            .map_err(|_| OfdError::ResourceLimit("embedded OFD size overflow".into()))?;
        self.budget
            .charge_embedded_bytes(byte_count, self.session.limits.max_embedded_ofd_bytes)?;
        let pkg = crate::parser::parse(ofd_bytes.to_vec())?;
        let doc = pkg
            .documents
            .first()
            .ok_or_else(|| OfdError::Render("vector seal contains no document".into()))?;
        if self.session.strict && !doc.warnings.is_empty() {
            return Err(OfdError::Render(format!(
                "vector seal contains {} parse warning(s): {}",
                doc.warnings.len(),
                doc.warnings.join("; ")
            )));
        }
        let mut limits = self.session.limits.clone();
        limits.max_embedded_ofd_depth -= 1;
        let opts = RenderOptions {
            fallback_fonts: self.session.fallback_fonts.clone(),
            transparent_background: true,
            text_stem_darkening: self.session.stem_darkening,
            strict: self.session.strict,
            limits,
        };
        let mut child = RenderSession::new(doc, opts);
        let bmp = child.render_page_with_budget(0, self.frame.dpi, self.budget.clone(), true)?;
        let size = tiny_skia::IntSize::from_wh(bmp.width, bmp.height)
            .ok_or_else(|| OfdError::Render("vector seal has invalid dimensions".into()))?;
        Pixmap::from_vec(straight_rgba_into_premultiplied(bmp.rgba), size)
            .ok_or_else(|| OfdError::ResourceLimit("could not allocate vector seal surface".into()))
    }

    fn with_paint_for_color<R, F>(
        &mut self,
        color: &OfdColor,
        alpha: u8,
        context: ColorPaintContext<'_>,
        draw: F,
    ) -> Result<R>
    where
        F: for<'p> FnOnce(&Paint<'p>) -> R,
    {
        match color {
            OfdColor::Basic(c) => {
                let paint = solid(self.resolve_basic(c), alpha);
                Ok(draw(&paint))
            }
            OfdColor::Axial(g) => {
                let alpha = multiply_alpha(alpha, g.alpha);
                if let Some((pm, shader_transform)) =
                    self.axial_gradient_pixmap(g, alpha, context.path, context.transform)?
                {
                    let shader = SkPattern::new(
                        pm.as_ref(),
                        SpreadMode::Pad,
                        FilterQuality::Bilinear,
                        1.0,
                        shader_transform,
                    );
                    let paint = shader_paint(shader);
                    Ok(draw(&paint))
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    Ok(draw(&paint))
                }
            }
            OfdColor::Radial(g) => {
                let alpha = multiply_alpha(alpha, g.alpha);
                if let Some((pm, shader_transform)) =
                    self.radial_gradient_pixmap(g, alpha, context.path, context.transform)?
                {
                    let shader = SkPattern::new(
                        pm.as_ref(),
                        SpreadMode::Pad,
                        FilterQuality::Bilinear,
                        1.0,
                        shader_transform,
                    );
                    let paint = shader_paint(shader);
                    Ok(draw(&paint))
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    Ok(draw(&paint))
                }
            }
            OfdColor::Pattern(p) => {
                let alpha = multiply_alpha(alpha, p.alpha);
                if let Some((tile, pat_transform)) = self.pattern_tile(p, context.common)? {
                    let shader = SkPattern::new(
                        tile.as_ref(),
                        SpreadMode::Repeat,
                        FilterQuality::Bilinear,
                        alpha as f32 / 255.0,
                        pat_transform,
                    );
                    let paint = shader_paint(shader);
                    Ok(draw(&paint))
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    Ok(draw(&paint))
                }
            }
            OfdColor::Gouraud(g) => {
                let alpha = multiply_alpha(alpha, g.alpha);
                if let Some((pm, shader_transform)) = self.gouraud_pixmap(
                    &g.points,
                    g.back_color.as_ref().filter(|_| g.extend),
                    alpha,
                    context.path,
                    context.fill_rule,
                    0,
                )? {
                    let shader = SkPattern::new(
                        pm.as_ref(),
                        SpreadMode::Pad,
                        FilterQuality::Bilinear,
                        1.0,
                        shader_transform,
                    );
                    let paint = shader_paint(shader);
                    Ok(draw(&paint))
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    Ok(draw(&paint))
                }
            }
            OfdColor::LatticeGouraud(g) => {
                let alpha = multiply_alpha(alpha, g.alpha);
                if let Some((pm, shader_transform)) = self.gouraud_pixmap(
                    &g.points,
                    g.back_color.as_ref().filter(|_| g.extend),
                    alpha,
                    context.path,
                    context.fill_rule,
                    g.vertices_per_row,
                )? {
                    let shader = SkPattern::new(
                        pm.as_ref(),
                        SpreadMode::Pad,
                        FilterQuality::Bilinear,
                        1.0,
                        shader_transform,
                    );
                    let paint = shader_paint(shader);
                    Ok(draw(&paint))
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    Ok(draw(&paint))
                }
            }
        }
    }

    fn resolve_basic(&self, c: &BasicColor) -> Color {
        let cs_id = c.color_space.or(self.session.default_color_space);
        let color_space = cs_id.and_then(|id| self.session.color_spaces.get(&id).copied());
        let components = c.components.as_deref().or_else(|| {
            c.index
                .and_then(|index| color_space.and_then(|space| space.palette.get(index)))
                .map(Vec::as_slice)
        });
        if let Some((space, transform)) =
            color_space.zip(cs_id.and_then(|id| self.session.icc_transforms.get(&id)))
        {
            if let Some(color) = resolve_icc_color(
                components,
                c.alpha,
                space.kind,
                space.bits_per_component,
                transform,
            ) {
                return color;
            }
        }
        let (kind, bpc) = color_space
            .map(|space| (space.kind, space.bits_per_component))
            .unwrap_or((ColorSpaceKind::Rgb, 8));
        resolve_color_components(components, c.alpha, kind, bpc)
    }

    fn resolved_gradient_stops(&self, segments: &[GradientSegment]) -> Vec<ResolvedGradientStop> {
        if segments.is_empty() {
            return vec![
                ResolvedGradientStop {
                    position: 0.0,
                    color: Color::BLACK,
                },
                ResolvedGradientStop {
                    position: 1.0,
                    color: Color::BLACK,
                },
            ];
        }
        gradient_positions(segments)
            .into_iter()
            .zip(segments)
            .map(|(position, segment)| ResolvedGradientStop {
                position,
                color: self.resolve_basic(&segment.color),
            })
            .collect()
    }

    fn axial_gradient_pixmap(
        &mut self,
        gradient: &AxialGradient,
        alpha: u8,
        path: Option<&SkPath>,
        transform: Transform,
    ) -> Result<Option<(Pixmap, Transform)>> {
        let Some(path) = path else {
            return Ok(None);
        };
        if !gradient.start.x.is_finite()
            || !gradient.start.y.is_finite()
            || !gradient.end.x.is_finite()
            || !gradient.end.y.is_finite()
        {
            return self.missing_or_invalid("axial gradient has non-finite points".into());
        }
        let dx = gradient.end.x - gradient.start.x;
        let dy = gradient.end.y - gradient.start.y;
        let axis_length = dx.hypot(dy);
        if !axis_length.is_finite() || axis_length <= f32::EPSILON {
            return self.missing_or_invalid("axial gradient has a zero-length axis".into());
        }
        let unit = gradient
            .map_unit
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(axis_length);
        let stops = self.resolved_gradient_stops(&gradient.segments);
        self.raster_gradient(path, transform, |x, y| {
            let projected =
                ((x - gradient.start.x) * dx + (y - gradient.start.y) * dy) / axis_length;
            let raw = projected / axis_length;
            let phase = if gradient.map_type == GradientMapType::Direct {
                raw
            } else {
                projected / unit
            };
            gradient_parameter(raw, phase, gradient.map_type, gradient.extend)
                .map(|parameter| sample_gradient(&stops, parameter, alpha))
        })
    }

    fn radial_gradient_pixmap(
        &mut self,
        gradient: &RadialGradient,
        alpha: u8,
        path: Option<&SkPath>,
        transform: Transform,
    ) -> Result<Option<(Pixmap, Transform)>> {
        let Some(path) = path else {
            return Ok(None);
        };
        if !gradient.start_radius.is_finite()
            || !gradient.end_radius.is_finite()
            || gradient.start_radius < 0.0
            || gradient.end_radius < 0.0
            || !gradient.eccentricity.is_finite()
            || !(0.0..1.0).contains(&gradient.eccentricity)
            || !gradient.angle.is_finite()
            || !gradient.start.x.is_finite()
            || !gradient.start.y.is_finite()
            || !gradient.end.x.is_finite()
            || !gradient.end.y.is_finite()
        {
            return self.missing_or_invalid("radial gradient has invalid ellipse geometry".into());
        }
        let minor_ratio = (1.0 - gradient.eccentricity * gradient.eccentricity)
            .sqrt()
            .max(f32::EPSILON);
        let start = ellipse_space(
            gradient.start.x,
            gradient.start.y,
            gradient.angle,
            minor_ratio,
        );
        let end = ellipse_space(gradient.end.x, gradient.end.y, gradient.angle, minor_ratio);
        let center_distance =
            (gradient.end.x - gradient.start.x).hypot(gradient.end.y - gradient.start.y);
        let unit = gradient
            .map_unit
            .filter(|value| value.is_finite() && *value > 0.0)
            .unwrap_or(center_distance);
        let stops = self.resolved_gradient_stops(&gradient.segments);
        self.raster_gradient(path, transform, |x, y| {
            let point = ellipse_space(x, y, gradient.angle, minor_ratio);
            let raw = solve_radial_parameter(
                point,
                start,
                end,
                gradient.start_radius,
                gradient.end_radius,
            )?;
            let phase = if gradient.map_type == GradientMapType::Direct
                || !unit.is_finite()
                || unit <= f32::EPSILON
                || center_distance <= f32::EPSILON
            {
                raw
            } else {
                raw * center_distance / unit
            };
            gradient_parameter(raw, phase, gradient.map_type, gradient.extend)
                .map(|parameter| sample_gradient(&stops, parameter, alpha))
        })
    }

    fn raster_gradient(
        &mut self,
        path: &SkPath,
        transform: Transform,
        mut sample: impl FnMut(f32, f32) -> Option<Color>,
    ) -> Result<Option<(Pixmap, Transform)>> {
        let bounds = path.bounds();
        let scale = transform_max_scale(transform);
        if !scale.is_finite()
            || scale <= 0.0
            || !bounds.width().is_finite()
            || !bounds.height().is_finite()
            || bounds.width() <= 0.0
            || bounds.height() <= 0.0
        {
            return self.missing_or_invalid("gradient has invalid raster bounds".into());
        }
        let width = checked_surface_dimension(bounds.width(), scale, "gradient width")?;
        let height = checked_surface_dimension(bounds.height(), scale, "gradient height")?;
        ensure_image_size(width, height, &self.session.limits)?;
        let pixels = u64::from(width) * u64::from(height);
        self.budget
            .charge_gradient(pixels, self.session.limits.max_gradient_raster_pixels)?;
        let mut pixmap = Pixmap::new(width, height)
            .ok_or_else(|| OfdError::ResourceLimit("could not allocate gradient surface".into()))?;
        let pixels = pixmap.pixels_mut();
        for y in 0..height {
            for x in 0..width {
                let object_x = bounds.left() + (x as f32 + 0.5) / scale;
                let object_y = bounds.top() + (y as f32 + 0.5) / scale;
                if let Some(color) = sample(object_x, object_y) {
                    let index = (y * width + x) as usize;
                    pixels[index] = premul_pixel(color);
                }
            }
        }
        let shader_transform = Transform::from_translate(bounds.left(), bounds.top())
            .pre_scale(1.0 / scale, 1.0 / scale);
        Ok(Some((pixmap, shader_transform)))
    }

    fn pattern_tile(
        &mut self,
        p: &PatternColor,
        common: &GraphicCommon,
    ) -> Result<Option<(Pixmap, Transform)>> {
        if self.pattern_depth >= self.session.limits.max_pattern_depth {
            if self.session.strict {
                return Err(OfdError::Render(format!(
                    "pattern nesting exceeds {}",
                    self.session.limits.max_pattern_depth
                )));
            }
            return Ok(None);
        }
        let scale = self.frame.dpi / crate::geom::MM_PER_INCH;
        let tw = (p.x_step * scale).ceil().clamp(1.0, 4096.0) as u32;
        let th = (p.y_step * scale).ceil().clamp(1.0, 4096.0) as u32;
        let tile_pixels = u64::from(tw) * u64::from(th);
        let reflected_factor = match p.reflect {
            PatternReflect::Normal => 0,
            PatternReflect::Row | PatternReflect::Column => 2,
            PatternReflect::RowAndColumn => 4,
        };
        let reflection_pixels = tile_pixels
            .checked_mul(reflected_factor)
            .ok_or_else(|| OfdError::ResourceLimit("reflected pattern size overflow".into()))?;
        let nested_pixels = self
            .pattern_surface_pixels
            .checked_add(tile_pixels)
            .ok_or_else(|| OfdError::ResourceLimit("pattern tile size overflow".into()))?;
        let peak_pixels = nested_pixels
            .checked_add(reflection_pixels)
            .ok_or_else(|| OfdError::ResourceLimit("pattern surface size overflow".into()))?;
        if peak_pixels > self.session.limits.max_pattern_surface_pixels {
            if self.session.strict {
                return Err(OfdError::ResourceLimit(format!(
                    "nested/reflected patterns require {peak_pixels} tile pixels; limit is {}",
                    self.session.limits.max_pattern_surface_pixels
                )));
            }
            return Ok(None);
        }
        let mut tile = Pixmap::new(tw, th)
            .ok_or_else(|| OfdError::ResourceLimit("could not allocate pattern tile".into()))?;

        {
            let composite_stack = self.composite_stack.clone();
            let mut cell = RenderCtx {
                session: &mut *self.session,
                frame: RenderFrame {
                    base: Transform::from_scale(scale, scale),
                    origin: (0.0, 0.0),
                    size: (tw, th),
                    dpi: self.frame.dpi,
                },
                composite_stack,
                composite_surface_pixels: self.composite_surface_pixels,
                pattern_depth: self.pattern_depth + 1,
                pattern_surface_pixels: nested_pixels,
                budget: self.budget.clone(),
            };
            for obj in &p.cell_content {
                cell.paint_object(&mut tile, obj, &[])?;
            }
        }

        let pat_transform = match p.relative_to {
            PatternRelativeTo::Object => {
                matrix_transform(p.ctm).pre_scale(1.0 / scale, 1.0 / scale)
            }
            PatternRelativeTo::Page => {
                let object = self.object_transform(common);
                let page = self
                    .frame
                    .base
                    .pre_translate(-self.frame.origin.0, -self.frame.origin.1);
                object
                    .invert()
                    .unwrap_or_else(Transform::identity)
                    .pre_concat(page)
                    .pre_concat(matrix_transform(p.ctm))
                    .pre_scale(1.0 / scale, 1.0 / scale)
            }
        };
        let tile = reflected_pattern_tile(tile, p.reflect)?;
        Ok(Some((tile, pat_transform)))
    }

    fn gouraud_pixmap(
        &mut self,
        points: &[GouraudPoint],
        back: Option<&BasicColor>,
        alpha: u8,
        path: Option<&SkPath>,
        rule: Option<SkFillRule>,
        vertices_per_row: usize,
    ) -> Result<Option<(Pixmap, Transform)>> {
        let Some(path) = path else {
            return Ok(None);
        };
        let bounds = path.bounds();
        let scale = self.frame.dpi / crate::geom::MM_PER_INCH;
        let w = ((bounds.width() * scale).ceil() as u32).clamp(1, 4096);
        let h = ((bounds.height() * scale).ceil() as u32).clamp(1, 4096);
        ensure_image_size(w, h, &self.session.limits)?;
        let pixels = u64::from(w) * u64::from(h);
        let triangles = gouraud_triangle_count(points, vertices_per_row)?;
        // Background fill is one full-surface pass. A shape mask adds mask
        // rasterization plus alpha application; each triangle can cover the
        // whole surface in the worst case.
        let fixed_passes = if rule.is_some() { 3 } else { 1 };
        let passes = triangles
            .checked_add(fixed_passes)
            .ok_or_else(|| OfdError::ResourceLimit("Gouraud pass count overflow".into()))?;
        let work = pixels
            .checked_mul(passes)
            .ok_or_else(|| OfdError::ResourceLimit("Gouraud raster work overflow".into()))?;
        self.budget
            .charge_gouraud(work, self.session.limits.max_gouraud_raster_pixels)?;

        let mut pm = Pixmap::new(w, h)
            .ok_or_else(|| OfdError::ResourceLimit("could not allocate Gouraud surface".into()))?;
        let transparent = Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        };
        let bg = back
            .map(|color| self.resolve_basic(color))
            .unwrap_or(transparent);
        fill_gouraud_pixmap(
            &mut pm,
            points,
            GouraudRasterOptions {
                vertices_per_row,
                background: bg,
                alpha,
                origin: (bounds.left(), bounds.top()),
                scale,
            },
            |b| self.resolve_basic(b),
        );
        if let Some(rule) = rule {
            self.budget
                .charge_mask_pixels(pixels, self.session.limits.max_mask_pixels)?;
            let mut m = Mask::new(w, h)
                .ok_or_else(|| OfdError::ResourceLimit("could not allocate Gouraud mask".into()))?;
            let local =
                Transform::from_scale(scale, scale).pre_translate(-bounds.left(), -bounds.top());
            m.fill_path(path, rule, true, local);
            apply_alpha_mask(&mut pm, &m);
        }
        let shader_transform = Transform::from_translate(bounds.left(), bounds.top())
            .pre_scale(1.0 / scale, 1.0 / scale);
        Ok(Some((pm, shader_transform)))
    }

    fn stroke_for(&self, common: &GraphicCommon, sources: DrawParamSources<'_>) -> Result<Stroke> {
        let cap = match match common.cap {
            Some(cap) => cap,
            None => self
                .dp_resolve(sources, |d| d.cap)?
                .unwrap_or(LineCap::Butt),
        } {
            LineCap::Round => SkLineCap::Round,
            LineCap::Square => SkLineCap::Square,
            LineCap::Butt => SkLineCap::Butt,
        };
        let join = match match common.join {
            Some(join) => join,
            None => self
                .dp_resolve(sources, |d| d.join)?
                .unwrap_or(LineJoin::Miter),
        } {
            LineJoin::Round => SkLineJoin::Round,
            LineJoin::Bevel => SkLineJoin::Bevel,
            LineJoin::Miter => SkLineJoin::Miter,
        };
        let miter_limit = match common.miter_limit {
            Some(value) => value,
            None => self
                .dp_resolve(sources, |d| d.miter_limit)?
                .unwrap_or(3.528),
        };
        let dash_pattern = match common.dash_pattern.clone() {
            Some(value) => Some(value),
            None => self.dp_resolve(sources, |d| d.dash_pattern.clone())?,
        };
        let dash_offset = match common.dash_offset {
            Some(value) => value,
            None => self.dp_resolve(sources, |d| d.dash_offset)?.unwrap_or(0.0),
        };
        let width = match common.line_width {
            Some(value) => value,
            None => self.dp_line_width(sources)?.unwrap_or(0.353),
        }
        .max(0.0);
        Ok(Stroke {
            // tiny-skia does not implement OFD's device-pixel special case for
            // zero-width lines. Use a hairline-sized positive width here; output
            // pixel minimum handling remains a separate conformance item.
            width: width.max(0.01),
            miter_limit,
            line_cap: cap,
            line_join: join,
            dash: dash_pattern.and_then(|d| StrokeDash::new(d, dash_offset)),
        })
    }

    // ---- DrawParam inheritance --------------------------------------------

    fn dp_fill(&self, sources: DrawParamSources<'_>) -> Result<Option<OfdColor>> {
        self.dp_resolve(sources, |d| d.fill_color.clone())
    }
    fn dp_stroke(&self, sources: DrawParamSources<'_>) -> Result<Option<OfdColor>> {
        self.dp_resolve(sources, |d| d.stroke_color.clone())
    }
    fn dp_line_width(&self, sources: DrawParamSources<'_>) -> Result<Option<f32>> {
        self.dp_resolve(sources, |d| d.line_width)
    }

    /// Walk each source's `Relative` chain in precedence order until the field
    /// is found: object-local DrawParam, clip Area DrawParam when applicable,
    /// then the containing layer/composite defaults.
    fn dp_resolve<T>(
        &self,
        sources: DrawParamSources<'_>,
        pick: impl Fn(&DrawParam) -> Option<T>,
    ) -> Result<Option<T>> {
        let mut depth = 0usize;
        let mut exhausted = HashSet::new();
        for root in sources.ids() {
            if exhausted.contains(&root) {
                continue;
            }
            let mut cur = Some(root);
            let mut chain = Vec::new();
            let mut seen = HashSet::new();
            while let Some(i) = cur {
                if exhausted.contains(&i) {
                    break;
                }
                if depth >= self.session.limits.max_draw_param_depth {
                    if self.session.strict {
                        return Err(OfdError::ResourceLimit(format!(
                            "DrawParam Relative lookup exceeds {} links",
                            self.session.limits.max_draw_param_depth
                        )));
                    }
                    return Ok(None);
                }
                depth += 1;
                if !seen.insert(i) {
                    if self.session.strict {
                        return Err(OfdError::Render(format!(
                            "DrawParam Relative cycle contains id {i}"
                        )));
                    }
                    break;
                }
                let Some(d) = self.session.draw_params.get(&i) else {
                    if self.session.strict {
                        return Err(OfdError::Render(format!("unresolved DrawParam id {i}")));
                    }
                    break;
                };
                chain.push(i);
                if let Some(v) = pick(d) {
                    return Ok(Some(v));
                }
                cur = d.relative;
            }
            exhausted.extend(chain);
        }
        Ok(None)
    }

    fn skip_or_error(&self, message: String) -> Result<()> {
        if self.session.strict {
            Err(OfdError::Render(message))
        } else {
            Ok(())
        }
    }

    fn missing_or_invalid<T>(&self, message: String) -> Result<Option<T>> {
        if self.session.strict {
            Err(OfdError::Render(message))
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Copy)]
struct ResolvedGradientStop {
    position: f32,
    color: Color,
}

fn gradient_positions(segments: &[GradientSegment]) -> Vec<f32> {
    if segments.is_empty() {
        return Vec::new();
    }
    if segments.len() == 1 {
        return vec![segments[0].position.unwrap_or(0.0).clamp(0.0, 1.0)];
    }
    let mut positions: Vec<Option<f32>> = segments
        .iter()
        .map(|segment| {
            segment
                .position
                .filter(|value| value.is_finite())
                .map(|value| value.clamp(0.0, 1.0))
        })
        .collect();
    positions[0].get_or_insert(0.0);
    let last = positions.len() - 1;
    positions[last].get_or_insert(1.0);

    let mut anchor = 0usize;
    while anchor < last {
        let next = ((anchor + 1)..=last)
            .find(|index| positions[*index].is_some())
            .unwrap_or(last);
        let start = positions[anchor].unwrap_or(0.0);
        let end = positions[next].unwrap_or(1.0).max(start);
        positions[next] = Some(end);
        let intervals = (next - anchor) as f32;
        for (offset, position) in positions[(anchor + 1)..next].iter_mut().enumerate() {
            let ratio = (offset + 1) as f32 / intervals;
            *position = Some(start + (end - start) * ratio);
        }
        anchor = next;
    }
    positions
        .into_iter()
        .map(|position| position.unwrap_or(0.0))
        .collect()
}

fn gradient_parameter(raw: f32, phase: f32, map_type: GradientMapType, extend: u8) -> Option<f32> {
    if !raw.is_finite() || !phase.is_finite() {
        return None;
    }
    if raw < 0.0 && extend & 1 == 0 {
        return None;
    }
    if raw > 1.0 && extend & 2 == 0 {
        return None;
    }
    match map_type {
        GradientMapType::Direct => Some(raw.clamp(0.0, 1.0)),
        GradientMapType::Repeat => {
            let repeated = phase.rem_euclid(1.0);
            if repeated <= f32::EPSILON && phase > 0.0 {
                Some(1.0)
            } else {
                Some(repeated)
            }
        }
        GradientMapType::Reflect => {
            let reflected = phase.rem_euclid(2.0);
            Some(if reflected <= 1.0 {
                reflected
            } else {
                2.0 - reflected
            })
        }
    }
}

fn sample_gradient(stops: &[ResolvedGradientStop], parameter: f32, alpha: u8) -> Color {
    let Some(first) = stops.first().copied() else {
        return Color {
            r: 0,
            g: 0,
            b: 0,
            a: alpha,
        };
    };
    let parameter = parameter.clamp(0.0, 1.0);
    let mut color = if parameter <= first.position {
        first.color
    } else if let Some(index) = stops.iter().position(|stop| stop.position >= parameter) {
        let left = stops[index - 1];
        let right = stops[index];
        let width = right.position - left.position;
        if width <= f32::EPSILON {
            right.color
        } else {
            lerp_color(left.color, right.color, (parameter - left.position) / width)
        }
    } else {
        stops.last().copied().unwrap_or(first).color
    };
    color.a = multiply_alpha(color.a, alpha);
    color
}

fn lerp_color(left: Color, right: Color, ratio: f32) -> Color {
    let ratio = ratio.clamp(0.0, 1.0);
    let channel =
        |a: u8, b: u8| (f32::from(a) + (f32::from(b) - f32::from(a)) * ratio).round() as u8;
    Color {
        r: channel(left.r, right.r),
        g: channel(left.g, right.g),
        b: channel(left.b, right.b),
        a: channel(left.a, right.a),
    }
}

fn ellipse_space(x: f32, y: f32, angle: f32, minor_ratio: f32) -> (f32, f32) {
    let radians = angle.to_radians();
    let (sin, cos) = radians.sin_cos();
    (cos * x + sin * y, (-sin * x + cos * y) / minor_ratio)
}

fn solve_radial_parameter(
    point: (f32, f32),
    start: (f32, f32),
    end: (f32, f32),
    start_radius: f32,
    end_radius: f32,
) -> Option<f32> {
    let qx = point.0 - start.0;
    let qy = point.1 - start.1;
    let dx = end.0 - start.0;
    let dy = end.1 - start.1;
    let dr = end_radius - start_radius;
    let a = dx * dx + dy * dy - dr * dr;
    let b = -2.0 * (qx * dx + qy * dy + start_radius * dr);
    let c = qx * qx + qy * qy - start_radius * start_radius;
    let mut roots = [None, None];
    if a.abs() <= 1.0e-6 {
        if b.abs() <= 1.0e-6 {
            return None;
        }
        roots[0] = Some(-c / b);
    } else {
        let discriminant = b * b - 4.0 * a * c;
        if discriminant < 0.0 || !discriminant.is_finite() {
            return None;
        }
        let root = discriminant.sqrt();
        roots = [Some((-b - root) / (2.0 * a)), Some((-b + root) / (2.0 * a))];
    }

    let mut best: Option<(f32, f32)> = None;
    for parameter in roots
        .into_iter()
        .flatten()
        .filter(|value| value.is_finite())
    {
        if start_radius + parameter * dr < -1.0e-4 {
            continue;
        }
        let distance = if parameter < 0.0 {
            -parameter
        } else if parameter > 1.0 {
            parameter - 1.0
        } else {
            0.0
        };
        if best.is_none_or(|(best_distance, _)| distance < best_distance) {
            best = Some((distance, parameter));
        }
    }
    best.map(|(_, parameter)| parameter)
}

fn transform_max_scale(transform: Transform) -> f32 {
    let sum = f64::from(transform.sx).powi(2)
        + f64::from(transform.kx).powi(2)
        + f64::from(transform.ky).powi(2)
        + f64::from(transform.sy).powi(2);
    let determinant = f64::from(transform.sx) * f64::from(transform.sy)
        - f64::from(transform.kx) * f64::from(transform.ky);
    let discriminant = (sum * sum - 4.0 * determinant * determinant).max(0.0);
    ((sum + discriminant.sqrt()) * 0.5).sqrt() as f32
}

fn checked_surface_dimension(length: f32, scale: f32, label: &str) -> Result<u32> {
    let dimension = f64::from(length) * f64::from(scale);
    if !dimension.is_finite() || dimension > f64::from(u32::MAX) {
        return Err(OfdError::ResourceLimit(format!(
            "{label} exceeds the supported dimension range"
        )));
    }
    Ok(dimension.ceil().max(1.0) as u32)
}

fn valid_rect(rect: Rect) -> bool {
    rect.x.is_finite()
        && rect.y.is_finite()
        && rect.w.is_finite()
        && rect.h.is_finite()
        && rect.w > 0.0
        && rect.h > 0.0
}

fn sk_path(commands: &[PathCommand]) -> Option<SkPath> {
    let mut builder = PathBuilder::new();
    for command in commands {
        if !path_command_is_finite(command) {
            return None;
        }
        push_cmd(&mut builder, command);
    }
    builder.finish()
}

fn rounded_rect_path(width: f32, height: f32, radius_x: f32, radius_y: f32) -> Option<SkPath> {
    if !width.is_finite()
        || !height.is_finite()
        || !radius_x.is_finite()
        || !radius_y.is_finite()
        || width <= 0.0
        || height <= 0.0
        || radius_x < 0.0
        || radius_y < 0.0
    {
        return None;
    }
    let radius_x = radius_x.min(width / 2.0);
    let radius_y = radius_y.min(height / 2.0);
    let mut builder = PathBuilder::new();
    if radius_x == 0.0 || radius_y == 0.0 {
        builder.push_rect(tiny_skia::Rect::from_xywh(0.0, 0.0, width, height)?);
        return builder.finish();
    }

    // Cubic approximation of four quarter ellipses, clockwise from the top
    // edge. The maximum radial error is below 0.03%.
    const KAPPA: f32 = 0.552_284_8;
    builder.move_to(radius_x, 0.0);
    builder.line_to(width - radius_x, 0.0);
    builder.cubic_to(
        width - radius_x + KAPPA * radius_x,
        0.0,
        width,
        radius_y - KAPPA * radius_y,
        width,
        radius_y,
    );
    builder.line_to(width, height - radius_y);
    builder.cubic_to(
        width,
        height - radius_y + KAPPA * radius_y,
        width - radius_x + KAPPA * radius_x,
        height,
        width - radius_x,
        height,
    );
    builder.line_to(radius_x, height);
    builder.cubic_to(
        radius_x - KAPPA * radius_x,
        height,
        0.0,
        height - radius_y + KAPPA * radius_y,
        0.0,
        height - radius_y,
    );
    builder.line_to(0.0, radius_y);
    builder.cubic_to(
        0.0,
        radius_y - KAPPA * radius_y,
        radius_x - KAPPA * radius_x,
        0.0,
        radius_x,
        0.0,
    );
    builder.close();
    builder.finish()
}

fn path_command_is_finite(command: &PathCommand) -> bool {
    match *command {
        PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => {
            x.is_finite() && y.is_finite()
        }
        PathCommand::CubicTo {
            x1,
            y1,
            x2,
            y2,
            x,
            y,
        } => [x1, y1, x2, y2, x, y].into_iter().all(f32::is_finite),
        PathCommand::QuadTo { x1, y1, x, y } => [x1, y1, x, y].into_iter().all(f32::is_finite),
        PathCommand::Close => true,
    }
}

fn union_masks(target: &mut Mask, other: &Mask) {
    for (target, other) in target.data_mut().iter_mut().zip(other.data()) {
        *target = (*target).max(*other);
    }
}

fn intersect_masks(target: &mut Mask, other: &Mask) {
    for (target, other) in target.data_mut().iter_mut().zip(other.data()) {
        *target = ((*target as u16 * *other as u16) / 255) as u8;
    }
}

/// Append a parsed OFD path command to a `tiny-skia` path builder.
fn push_cmd(b: &mut PathBuilder, cmd: &PathCommand) {
    match *cmd {
        PathCommand::MoveTo { x, y } => b.move_to(x, y),
        PathCommand::LineTo { x, y } => b.line_to(x, y),
        PathCommand::CubicTo {
            x1,
            y1,
            x2,
            y2,
            x,
            y,
        } => b.cubic_to(x1, y1, x2, y2, x, y),
        PathCommand::QuadTo { x1, y1, x, y } => b.quad_to(x1, y1, x, y),
        PathCommand::Close => b.close(),
    }
}

/// Resolve glyph ids for a character position, honoring CGTransform's one-to-one,
/// one-to-many, and many-to-one mappings. For CID-keyed fonts, the CGTransform
/// ids are CIDs and are mapped to GIDs via `cid_to_gid`.
/// One positioned glyph in a text run: the glyph to outline and whether it should
/// be drawn (a `.notdef`/whitespace slot still consumes a `DeltaX` advance).
struct GlyphSlot {
    gid: ttf_parser::GlyphId,
    draw: bool,
    /// Index into the text object's resolved face list. Explicit producer glyph
    /// ids always use face 0; cmap-mapped substitute glyphs may select another
    /// injected face for per-character coverage.
    face_index: usize,
}

fn cmap_slot(gid: Option<ttf_parser::GlyphId>, ch: char) -> GlyphSlot {
    let gid = gid
        .filter(|glyph| glyph.0 != 0)
        .unwrap_or(ttf_parser::GlyphId(0));
    GlyphSlot {
        gid,
        draw: gid.0 != 0 && !ch.is_whitespace(),
        face_index: 0,
    }
}

fn font_face_key(font: &ResolvedFont) -> (usize, u32) {
    (Arc::as_ptr(&font.data) as usize, font.index)
}

/// The CGTransform span that *starts* at code index `idx` (§11.4). Spans are
/// non-overlapping and code-boundary aligned, so the layout always lands on a
/// span's start; matching `CodePosition == idx` is sufficient.
#[cfg(test)]
fn cg_span_at(transforms: &[CgTransform], idx: usize) -> Option<&CgTransform> {
    transforms.iter().find(|cg| cg.code_position == idx)
}

/// Index transform starts once per text object. A linear scan for every source
/// code makes a large but otherwise valid text object quadratic to render.
fn cg_transform_index(transforms: &[CgTransform]) -> HashMap<usize, &CgTransform> {
    let mut index = HashMap::with_capacity(transforms.len());
    for transform in transforms {
        index.entry(transform.code_position).or_insert(transform);
    }
    index
}

/// Resolve one `TextCode` run while retaining code spans consumed by a
/// `CGTransform` that started in an earlier run. `CodePosition`/`CodeCount`
/// address the concatenated source codes of the whole `TextObject`, so a span
/// may legally cross a `TextCode` boundary.
fn glyph_slots_for_run(
    chars: &[char],
    run_start: usize,
    covered_until: &mut usize,
    transforms: &HashMap<usize, &CgTransform>,
    trusted_glyph_ids: bool,
    cid_to_gid: Option<&HashMap<u16, u16>>,
    mut cmap: impl FnMut(char) -> GlyphSlot,
) -> Vec<GlyphSlot> {
    let mut slots = Vec::new();
    let mut index = covered_until.saturating_sub(run_start).min(chars.len());
    while index < chars.len() && slots.len() < crate::parser::MAX_TEXT_SLOTS {
        let source_index = run_start.saturating_add(index);
        // Explicit producer glyph ids only make sense against the producer's
        // embedded/exact face. Generic substitutes map the source characters.
        if trusted_glyph_ids {
            if let Some(transform) = transforms.get(&source_index).copied() {
                let remaining = crate::parser::MAX_TEXT_SLOTS - slots.len();
                slots.extend(cg_span_slots(transform, cid_to_gid, remaining));
                let span_end = source_index.saturating_add(transform.code_count.max(1));
                *covered_until = (*covered_until).max(span_end);
                index = span_end.saturating_sub(run_start).min(chars.len());
                continue;
            }
        }
        if source_index >= *covered_until {
            slots.push(cmap(chars[index]));
        }
        index += 1;
    }
    slots
}

/// Expand a CGTransform span to its glyph slots, preserving the slot count
/// (= `GlyphCount`) so per-glyph `DeltaX` advances stay aligned. CID-keyed CFFs
/// map the explicit glyph ids through the charset CID→GID table; a `0`/`.notdef`
/// glyph keeps its slot (for the advance) but is not drawn.
fn cg_span_slots(
    cg: &CgTransform,
    cid_to_gid: Option<&HashMap<u16, u16>>,
    max_slots: usize,
) -> Vec<GlyphSlot> {
    let count = cg
        .glyph_count
        .min(max_slots)
        .min(crate::parser::MAX_TEXT_SLOTS);
    (0..count)
        .map(|k| {
            let id = cg.glyphs.get(k).copied().unwrap_or(0);
            let gid = cid_to_gid.and_then(|m| m.get(&id).copied()).unwrap_or(id);
            GlyphSlot {
                gid: ttf_parser::GlyphId(gid),
                draw: gid != 0,
                face_index: 0,
            }
        })
        .collect()
}

/// Compute the advance after glyph `i` along one axis.
///
/// Priority: explicit delta at `i` -> repeat the last delta (short list) ->
/// caller-provided fallback.
fn advance(deltas: &[f32], i: usize, fallback: f32) -> f32 {
    if let Some(d) = deltas.get(i) {
        *d
    } else if let Some(last) = deltas.last() {
        *last
    } else {
        fallback
    }
}

fn glyph_advance(gid: ttf_parser::GlyphId, face: &Face, gscale: f32, font_size: f32) -> f32 {
    face.glyph_hor_advance(gid)
        .map(|adv| adv as f32 * gscale)
        .unwrap_or(font_size * 0.5)
}

fn normalize_direction(d: Direction) -> u16 {
    match d.0.rem_euclid(360) {
        45..=134 => 90,
        135..=224 => 180,
        225..=314 => 270,
        _ => 0,
    }
}

fn read_advance_vector(advance: f32, direction: u16) -> (f32, f32) {
    match direction {
        90 => (0.0, advance),
        180 => (-advance, 0.0),
        270 => (0.0, -advance),
        _ => (advance, 0.0),
    }
}

fn tiny_color(c: Color, alpha: u8) -> tiny_skia::Color {
    let a = multiply_alpha(c.a, alpha);
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, a)
}

fn multiply_alpha(left: u8, right: u8) -> u8 {
    ((u16::from(left) * u16::from(right)) / 255) as u8
}

fn shader_paint<'p>(shader: Shader<'p>) -> Paint<'p> {
    Paint {
        shader,
        anti_alias: true,
        ..Default::default()
    }
}

fn matrix_transform(m: Matrix) -> Transform {
    Transform::from_row(m.a, m.b, m.c, m.d, m.e, m.f)
}

fn create_icc_transform(color_space: &ColorSpace) -> Option<Arc<moxcms::Transform8BitExecutor>> {
    let profile = color_space.profile.as_ref()?;
    let source = moxcms::ColorProfile::new_from_slice(&profile.data).ok()?;
    let source_layout = match color_space.kind {
        ColorSpaceKind::Gray => moxcms::Layout::Gray,
        ColorSpaceKind::Rgb => moxcms::Layout::Rgb,
        // moxcms uses the four-channel RGBA memory layout for CMYK profiles;
        // the fourth byte is interpreted as K from the profile color space.
        ColorSpaceKind::Cmyk => moxcms::Layout::Rgba,
    };
    source
        .create_transform_8bit(
            source_layout,
            &moxcms::ColorProfile::new_srgb(),
            moxcms::Layout::Rgb,
            moxcms::TransformOptions::default(),
        )
        .ok()
}

fn resolve_icc_color(
    components: Option<&[f32]>,
    alpha: u8,
    kind: ColorSpaceKind,
    bits_per_component: u8,
    transform: &Arc<moxcms::Transform8BitExecutor>,
) -> Option<Color> {
    let components = components?;
    let channels = match kind {
        ColorSpaceKind::Gray => 1,
        ColorSpaceKind::Rgb => 3,
        ColorSpaceKind::Cmyk => 4,
    };
    let source: Vec<u8> = (0..channels)
        .map(|index| {
            scale_color_component(
                components.get(index).copied().unwrap_or(0.0),
                bits_per_component,
            )
        })
        .collect();
    let mut destination = [0u8; 3];
    transform.transform(&source, &mut destination).ok()?;
    Some(Color {
        r: destination[0],
        g: destination[1],
        b: destination[2],
        a: alpha,
    })
}

#[cfg(test)]
fn resolve_basic_color(color: &BasicColor, kind: ColorSpaceKind, bpc: u8) -> Color {
    resolve_color_components(color.components.as_deref(), color.alpha, kind, bpc)
}

fn resolve_color_components(
    components: Option<&[f32]>,
    alpha: u8,
    kind: ColorSpaceKind,
    bpc: u8,
) -> Color {
    let comps = components.unwrap_or(&[]);
    let mut c = match kind {
        ColorSpaceKind::Gray => {
            let v = scale_color_component(comps.first().copied().unwrap_or(0.0), bpc);
            Color::rgb(v, v, v)
        }
        ColorSpaceKind::Rgb => Color::rgb(
            scale_color_component(comps.first().copied().unwrap_or(0.0), bpc),
            scale_color_component(comps.get(1).copied().unwrap_or(0.0), bpc),
            scale_color_component(comps.get(2).copied().unwrap_or(0.0), bpc),
        ),
        ColorSpaceKind::Cmyk => {
            let bpc = bpc.clamp(1, 16);
            let max = ((1u32 << bpc as u32) - 1).max(1) as f32;
            let norm = |i: usize| comps.get(i).copied().unwrap_or(0.0).clamp(0.0, max) / max;
            let (cy, m, ye, k) = (norm(0), norm(1), norm(2), norm(3));
            Color::rgb(
                (255.0 * (1.0 - cy) * (1.0 - k)).round() as u8,
                (255.0 * (1.0 - m) * (1.0 - k)).round() as u8,
                (255.0 * (1.0 - ye) * (1.0 - k)).round() as u8,
            )
        }
    };
    c.a = alpha;
    c
}

fn scale_color_component(value: f32, bits_per_component: u8) -> u8 {
    let bits_per_component = bits_per_component.clamp(1, 16);
    let max = ((1u32 << bits_per_component as u32) - 1).max(1) as f32;
    if bits_per_component == 8 {
        value.clamp(0.0, 255.0).round() as u8
    } else {
        (value.clamp(0.0, max) / max * 255.0).round() as u8
    }
}

fn reflected_pattern_tile(tile: Pixmap, reflect: PatternReflect) -> Result<Pixmap> {
    if reflect == PatternReflect::Normal {
        return Ok(tile);
    }
    let w = tile.width();
    let h = tile.height();
    let out_w = if matches!(
        reflect,
        PatternReflect::Column | PatternReflect::RowAndColumn
    ) {
        w.saturating_mul(2)
    } else {
        w
    };
    let out_h = if matches!(reflect, PatternReflect::Row | PatternReflect::RowAndColumn) {
        h.saturating_mul(2)
    } else {
        h
    };
    let mut out = Pixmap::new(out_w.max(1), out_h.max(1)).ok_or_else(|| {
        OfdError::ResourceLimit("could not allocate reflected pattern tile".into())
    })?;
    let src = tile.pixels();
    let dst = out.pixels_mut();
    for y in 0..out_h {
        for x in 0..out_w {
            let sx = if matches!(
                reflect,
                PatternReflect::Column | PatternReflect::RowAndColumn
            ) && x >= w
            {
                out_w - 1 - x
            } else {
                x
            };
            let sy = if matches!(reflect, PatternReflect::Row | PatternReflect::RowAndColumn)
                && y >= h
            {
                out_h - 1 - y
            } else {
                y
            };
            let dst_idx = (y * out_w + x) as usize;
            let src_idx = (sy.min(h - 1) * w + sx.min(w - 1)) as usize;
            dst[dst_idx] = src[src_idx];
        }
    }
    Ok(out)
}

struct GouraudRasterOptions {
    vertices_per_row: usize,
    background: Color,
    alpha: u8,
    origin: (f32, f32),
    scale: f32,
}

fn gouraud_triangle_count(points: &[GouraudPoint], vertices_per_row: usize) -> Result<u64> {
    let triangles = if vertices_per_row >= 2 {
        let rows = points.len() / vertices_per_row;
        if rows >= 2 {
            rows.checked_sub(1)
                .and_then(|rows| rows.checked_mul(vertices_per_row - 1))
                .and_then(|cells| cells.checked_mul(2))
                .ok_or_else(|| OfdError::ResourceLimit("Gouraud triangle count overflow".into()))?
        } else {
            free_gouraud_triangle_count(points)
        }
    } else {
        free_gouraud_triangle_count(points)
    };
    u64::try_from(triangles)
        .map_err(|_| OfdError::ResourceLimit("Gouraud triangle count overflow".into()))
}

fn free_gouraud_triangle_count(points: &[GouraudPoint]) -> usize {
    if points.len() < 3 {
        return 0;
    }
    let mut count = 1usize;
    let mut previous = [0usize, 1, 2];
    let mut index = 3usize;
    while index < points.len() {
        let Some((triangle, next)) =
            next_free_gouraud_triangle(previous, index, points[index].edge_flag, points.len())
        else {
            break;
        };
        count = count.saturating_add(1);
        previous = triangle;
        index = next;
    }
    count
}

/// Figure 40: flag 0 starts a new three-vertex triangle, flag 1 reuses the
/// previous Vb-Vc edge, and flag 2 reuses the previous Va-Vc edge.
fn next_free_gouraud_triangle(
    previous: [usize; 3],
    index: usize,
    edge_flag: Option<u8>,
    len: usize,
) -> Option<([usize; 3], usize)> {
    match edge_flag.unwrap_or(0) {
        1 if index < len => Some(([previous[1], previous[2], index], index + 1)),
        2 if index < len => Some(([previous[0], previous[2], index], index + 1)),
        _ if index.checked_add(2)? < len => Some(([index, index + 1, index + 2], index + 3)),
        _ => None,
    }
}

fn fill_gouraud_pixmap(
    pm: &mut Pixmap,
    points: &[GouraudPoint],
    options: GouraudRasterOptions,
    mut resolve: impl FnMut(&BasicColor) -> Color,
) {
    pm.fill(tiny_color(options.background, options.alpha));
    let mut verts = Vec::new();
    for p in points {
        verts.push(GouraudVertex {
            x: (p.x - options.origin.0) * options.scale,
            y: (p.y - options.origin.1) * options.scale,
            color: resolve(&p.color),
            edge_flag: p.edge_flag,
        });
    }
    if options.vertices_per_row >= 2 {
        fill_lattice_gouraud(pm, &verts, options.vertices_per_row, options.alpha);
    } else {
        fill_free_gouraud(pm, &verts, options.alpha);
    }
}

#[derive(Clone, Copy)]
struct GouraudVertex {
    x: f32,
    y: f32,
    color: Color,
    edge_flag: Option<u8>,
}

fn fill_lattice_gouraud(pm: &mut Pixmap, verts: &[GouraudVertex], per_row: usize, alpha: u8) {
    let rows = verts.len() / per_row;
    if rows < 2 {
        fill_free_gouraud(pm, verts, alpha);
        return;
    }
    for row in 0..rows - 1 {
        for col in 0..per_row - 1 {
            let i = row * per_row + col;
            let a = verts[i];
            let b = verts[i + 1];
            let c = verts[i + per_row];
            let d = verts[i + per_row + 1];
            fill_triangle(pm, a, b, c, alpha);
            fill_triangle(pm, b, d, c, alpha);
        }
    }
}

fn fill_free_gouraud(pm: &mut Pixmap, verts: &[GouraudVertex], alpha: u8) {
    if verts.len() < 3 {
        return;
    }
    fill_triangle(pm, verts[0], verts[1], verts[2], alpha);
    let mut previous = [0usize, 1, 2];
    let mut index = 3usize;
    while index < verts.len() {
        let Some((triangle, next)) =
            next_free_gouraud_triangle(previous, index, verts[index].edge_flag, verts.len())
        else {
            break;
        };
        fill_triangle(
            pm,
            verts[triangle[0]],
            verts[triangle[1]],
            verts[triangle[2]],
            alpha,
        );
        previous = triangle;
        index = next;
    }
}

fn fill_triangle(pm: &mut Pixmap, a: GouraudVertex, b: GouraudVertex, c: GouraudVertex, alpha: u8) {
    let area = edge(a.x, a.y, b.x, b.y, c.x, c.y);
    if area.abs() < 0.0001 {
        return;
    }
    let min_x = a.x.min(b.x).min(c.x).floor().max(0.0) as u32;
    let max_x = a.x.max(b.x).max(c.x).ceil().min(pm.width() as f32) as u32;
    let min_y = a.y.min(b.y).min(c.y).floor().max(0.0) as u32;
    let max_y = a.y.max(b.y).max(c.y).ceil().min(pm.height() as f32) as u32;
    let width = pm.width();
    let pixels = pm.pixels_mut();
    for y in min_y..max_y {
        for x in min_x..max_x {
            let px = x as f32 + 0.5;
            let py = y as f32 + 0.5;
            let w0 = edge(b.x, b.y, c.x, c.y, px, py) / area;
            let w1 = edge(c.x, c.y, a.x, a.y, px, py) / area;
            let w2 = edge(a.x, a.y, b.x, b.y, px, py) / area;
            if w0 >= -0.001 && w1 >= -0.001 && w2 >= -0.001 {
                let color = interpolate_color(a.color, b.color, c.color, w0, w1, w2, alpha);
                pixels[(y * width + x) as usize] = premul_pixel(color);
            }
        }
    }
}

fn edge(ax: f32, ay: f32, bx: f32, by: f32, px: f32, py: f32) -> f32 {
    (px - ax) * (by - ay) - (py - ay) * (bx - ax)
}

fn interpolate_color(a: Color, b: Color, c: Color, w0: f32, w1: f32, w2: f32, alpha: u8) -> Color {
    let mix = |av: u8, bv: u8, cv: u8| -> u8 {
        (av as f32 * w0 + bv as f32 * w1 + cv as f32 * w2)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    let mut out = Color {
        r: mix(a.r, b.r, c.r),
        g: mix(a.g, b.g, c.g),
        b: mix(a.b, b.b, c.b),
        a: mix(a.a, b.a, c.a),
    };
    out.a = ((out.a as u16 * alpha as u16) / 255) as u8;
    out
}

fn apply_alpha_mask(pm: &mut Pixmap, mask: &Mask) {
    for (px, coverage) in pm.pixels_mut().iter_mut().zip(mask.data().iter().copied()) {
        let scale = coverage as u16;
        let a = ((px.alpha() as u16 * scale) / 255) as u8;
        *px = tiny_skia::PremultipliedColorU8::from_rgba(
            ((px.red() as u16 * scale) / 255).min(a as u16) as u8,
            ((px.green() as u16 * scale) / 255).min(a as u16) as u8,
            ((px.blue() as u16 * scale) / 255).min(a as u16) as u8,
            a,
        )
        .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
    }
}

fn premul_pixel(c: Color) -> tiny_skia::PremultipliedColorU8 {
    tiny_skia::PremultipliedColorU8::from_rgba(
        ((c.r as u16 * c.a as u16) / 255) as u8,
        ((c.g as u16 * c.a as u16) / 255) as u8,
        ((c.b as u16 * c.a as u16) / 255) as u8,
        c.a,
    )
    .unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT)
}

fn pixmap_into_straight_rgba(pixmap: Pixmap) -> Vec<u8> {
    let mut rgba = pixmap.take();
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        if alpha == 0 {
            pixel[..3].fill(0);
            continue;
        }
        for channel in &mut pixel[..3] {
            let straight = (u16::from(*channel) * 255 + alpha / 2) / alpha;
            *channel = straight.min(255) as u8;
        }
    }
    rgba
}

fn straight_rgba_into_premultiplied(mut rgba: Vec<u8>) -> Vec<u8> {
    for pixel in rgba.chunks_exact_mut(4) {
        let alpha = u16::from(pixel[3]);
        for channel in &mut pixel[..3] {
            *channel = ((u16::from(*channel) * alpha + 127) / 255) as u8;
        }
    }
    rgba
}

/// A solid-color paint at the given object alpha (0..=255).
fn solid(c: Color, alpha: u8) -> Paint<'static> {
    let a = ((c.a as u16 * alpha as u16) / 255) as u8;
    Paint {
        shader: Shader::SolidColor(tiny_skia::Color::from_rgba8(c.r, c.g, c.b, a)),
        anti_alias: true,
        ..Default::default()
    }
}

/// Decode an image resource to straight-alpha RGBA under explicit allocation
/// and pixel limits.
fn decode_image_rgba(
    media: &MultiMedia,
    limits: &RenderLimits,
    budget: &RenderBudget,
) -> Result<image::RgbaImage> {
    decode_rgba_inner(media.format, &media.data, limits, Some(budget))
}

fn decode_rgba(
    format: ImageFormat,
    data: &[u8],
    limits: &RenderLimits,
) -> Result<image::RgbaImage> {
    decode_rgba_inner(format, data, limits, None)
}

fn decode_rgba_inner(
    format: ImageFormat,
    data: &[u8],
    limits: &RenderLimits,
    budget: Option<&RenderBudget>,
) -> Result<image::RgbaImage> {
    match format {
        ImageFormat::Jbig2 => decode_jbig2(data, limits, budget),
        ImageFormat::Ccitt => Err(OfdError::Render(
            "CCITT image decoding is not implemented".into(),
        )),
        _ => {
            let reader = image::ImageReader::new(Cursor::new(data))
                .with_guessed_format()
                .map_err(|e| OfdError::Render(format!("cannot identify raster image: {e}")))?;
            let (width, height) = reader
                .into_dimensions()
                .map_err(|e| OfdError::Render(format!("cannot read raster dimensions: {e}")))?;
            ensure_image_size(width, height, limits)?;
            if let Some(budget) = budget {
                budget.charge_raster_decode(
                    u64::from(width) * u64::from(height),
                    limits.max_raster_decode_pixels,
                )?;
            }

            let mut reader = image::ImageReader::new(Cursor::new(data))
                .with_guessed_format()
                .map_err(|e| OfdError::Render(format!("cannot identify raster image: {e}")))?;
            let mut decode_limits = image::Limits::default();
            decode_limits.max_image_width = Some(width);
            decode_limits.max_image_height = Some(height);
            decode_limits.max_alloc = Some(limits.max_image_bytes);
            reader.limits(decode_limits);
            let image = reader
                .decode()
                .map_err(|e| OfdError::Render(format!("raster image decode failed: {e}")))?
                .to_rgba8();
            ensure_image_size(image.width(), image.height(), limits)?;
            Ok(image)
        }
    }
}

fn ensure_image_size(width: u32, height: u32, limits: &RenderLimits) -> Result<()> {
    let pixels = u64::from(width) * u64::from(height);
    if width == 0 || height == 0 {
        return Err(OfdError::Render("image has zero dimensions".into()));
    }
    if pixels > limits.max_image_pixels {
        return Err(OfdError::ResourceLimit(format!(
            "image requires {pixels} pixels; limit is {}",
            limits.max_image_pixels
        )));
    }
    let rgba_bytes = pixels.saturating_mul(4);
    if rgba_bytes > limits.max_image_bytes {
        return Err(OfdError::ResourceLimit(format!(
            "decoded image requires at least {rgba_bytes} bytes; limit is {}",
            limits.max_image_bytes
        )));
    }
    let working_bytes = pixels
        .checked_mul(12)
        .ok_or_else(|| OfdError::ResourceLimit("image working-set size overflow".into()))?;
    if working_bytes > limits.max_image_working_bytes {
        return Err(OfdError::ResourceLimit(format!(
            "image conversion may require {working_bytes} working bytes; limit is {}",
            limits.max_image_working_bytes
        )));
    }
    Ok(())
}

/// Decode a JBIG2 bilevel image (e.g. invoice QR codes, scanned B&W pages) to
/// black-on-white RGBA. Foreground pixels (value 1) are black.
fn decode_jbig2(
    data: &[u8],
    limits: &RenderLimits,
    budget: Option<&RenderBudget>,
) -> Result<image::RgbaImage> {
    const FILE_MAGIC: &[u8] = &[0x97, 0x4a, 0x42, 0x32, 0x0d, 0x0a, 0x1a, 0x0a];
    let work = preflight_jbig2(data, limits)?;
    if let Some(budget) = budget {
        budget.charge_raster_decode(work, limits.max_raster_decode_pixels)?;
    }
    let pages = if data.starts_with(FILE_MAGIC) {
        justbig2::decode(data)
    } else {
        justbig2::decode_embedded(data)
    }
    .map_err(|e| OfdError::Render(format!("JBIG2 decode failed: {e}")))?;
    let page = pages
        .into_iter()
        .find(|p| p.width > 0 && p.height > 0)
        .ok_or_else(|| OfdError::Render("JBIG2 contains no non-empty page".into()))?;
    ensure_image_size(page.width, page.height, limits)?;

    let mut img = image::RgbaImage::new(page.width, page.height);
    for (y, row) in img.enumerate_rows_mut() {
        for (x, _, px) in row {
            let on = page.get_pixel(x, y) != 0;
            *px = if on {
                image::Rgba([0, 0, 0, 255])
            } else {
                image::Rgba([255, 255, 255, 255])
            };
        }
    }
    Ok(img)
}

#[derive(Debug)]
struct Jbig2SegmentMeta {
    number: u32,
    kind: u8,
    references: Vec<u32>,
    data_length: u32,
}

#[derive(Clone, Copy)]
struct Jbig2PageBudget {
    width: u32,
    height: u32,
    striped: bool,
}

#[derive(Default)]
struct Jbig2Preflight {
    pages: Vec<Jbig2PageBudget>,
    current_page: Option<usize>,
    symbol_slots: HashMap<u32, u64>,
    total_page_pixels: u64,
    total_symbol_slots: u64,
    decode_pixels: u64,
}

/// Validate all allocation-driving JBIG2 declarations before entering
/// `justbig2`. That decoder allocates page and region buffers while parsing, so
/// checking the returned dimensions afterwards is too late for untrusted input.
fn preflight_jbig2(data: &[u8], limits: &RenderLimits) -> Result<u64> {
    use justbig2::header::{FileHeader, Organization};

    let (organization, mut cursor) = if data.starts_with(&justbig2::header::MAGIC) {
        let Some((header, consumed)) = FileHeader::parse(data)
            .map_err(|e| OfdError::Render(format!("invalid JBIG2 header: {e}")))?
        else {
            return Err(OfdError::Render("truncated JBIG2 header".into()));
        };
        if header
            .n_pages
            .is_some_and(|count| count as usize > limits.max_jbig2_items)
        {
            return Err(OfdError::ResourceLimit(format!(
                "JBIG2 declares more than {} pages",
                limits.max_jbig2_items
            )));
        }
        (header.organization, consumed)
    } else {
        (Organization::Sequential, 0)
    };

    let mut state = Jbig2Preflight::default();
    let mut segment_count = 0usize;
    let mut reference_count = 0usize;

    match organization {
        Organization::Sequential => {
            while cursor < data.len() {
                let (meta, header_len) =
                    parse_jbig2_segment_header(&data[cursor..], limits.max_jbig2_items)?;
                segment_count = segment_count.checked_add(1).ok_or_else(|| {
                    OfdError::ResourceLimit("JBIG2 segment count overflow".into())
                })?;
                if segment_count > limits.max_jbig2_items {
                    return Err(OfdError::ResourceLimit(format!(
                        "JBIG2 exceeds {} segments",
                        limits.max_jbig2_items
                    )));
                }
                reference_count = reference_count
                    .checked_add(meta.references.len())
                    .ok_or_else(|| {
                        OfdError::ResourceLimit("JBIG2 reference count overflow".into())
                    })?;
                if reference_count > limits.max_jbig2_items {
                    return Err(OfdError::ResourceLimit(format!(
                        "JBIG2 exceeds {} segment references",
                        limits.max_jbig2_items
                    )));
                }
                cursor = cursor
                    .checked_add(header_len)
                    .ok_or_else(|| OfdError::ResourceLimit("JBIG2 input offset overflow".into()))?;
                let unknown_length = meta.data_length == u32::MAX;
                let body_len = if unknown_length {
                    data.len().saturating_sub(cursor)
                } else {
                    meta.data_length as usize
                };
                let end = cursor.checked_add(body_len).ok_or_else(|| {
                    OfdError::ResourceLimit("JBIG2 segment length overflow".into())
                })?;
                let body = data
                    .get(cursor..end)
                    .ok_or_else(|| OfdError::Render("truncated JBIG2 segment body".into()))?;
                inspect_jbig2_segment(&meta, body, limits, &mut state)?;
                cursor = end;
                if meta.kind == 51 || unknown_length {
                    break;
                }
            }
        }
        Organization::RandomAccess => {
            let mut headers = Vec::new();
            loop {
                let (meta, header_len) =
                    parse_jbig2_segment_header(&data[cursor..], limits.max_jbig2_items)?;
                segment_count = segment_count.checked_add(1).ok_or_else(|| {
                    OfdError::ResourceLimit("JBIG2 segment count overflow".into())
                })?;
                if segment_count > limits.max_jbig2_items {
                    return Err(OfdError::ResourceLimit(format!(
                        "JBIG2 exceeds {} segments",
                        limits.max_jbig2_items
                    )));
                }
                reference_count = reference_count
                    .checked_add(meta.references.len())
                    .ok_or_else(|| {
                        OfdError::ResourceLimit("JBIG2 reference count overflow".into())
                    })?;
                if reference_count > limits.max_jbig2_items {
                    return Err(OfdError::ResourceLimit(format!(
                        "JBIG2 exceeds {} segment references",
                        limits.max_jbig2_items
                    )));
                }
                cursor = cursor
                    .checked_add(header_len)
                    .ok_or_else(|| OfdError::ResourceLimit("JBIG2 input offset overflow".into()))?;
                let eof = meta.kind == 51;
                headers.push(meta);
                if eof {
                    break;
                }
            }
            for meta in headers {
                if meta.data_length == u32::MAX {
                    return Err(OfdError::Render(
                        "random-access JBIG2 has an indeterminate segment length".into(),
                    ));
                }
                let end = cursor
                    .checked_add(meta.data_length as usize)
                    .ok_or_else(|| {
                        OfdError::ResourceLimit("JBIG2 segment length overflow".into())
                    })?;
                let body = data
                    .get(cursor..end)
                    .ok_or_else(|| OfdError::Render("truncated JBIG2 segment body".into()))?;
                inspect_jbig2_segment(&meta, body, limits, &mut state)?;
                cursor = end;
            }
        }
    }
    state
        .decode_pixels
        .checked_add(state.total_page_pixels)
        .ok_or_else(|| OfdError::ResourceLimit("JBIG2 cumulative decode work overflow".into()))
}

fn parse_jbig2_segment_header(
    data: &[u8],
    max_references: usize,
) -> Result<(Jbig2SegmentMeta, usize)> {
    if data.len() < 6 {
        return Err(OfdError::Render("truncated JBIG2 segment header".into()));
    }
    let read_u32 = |at: usize| -> Option<u32> {
        Some(u32::from_be_bytes([
            *data.get(at)?,
            *data.get(at + 1)?,
            *data.get(at + 2)?,
            *data.get(at + 3)?,
        ]))
    };
    let number =
        read_u32(0).ok_or_else(|| OfdError::Render("truncated JBIG2 segment number".into()))?;
    if number == u32::MAX {
        return Err(OfdError::Render("invalid JBIG2 segment number".into()));
    }
    let flags = data[4];
    let rtscarf = data[5];
    let (reference_count, mut offset) = if rtscarf & 0xe0 == 0xe0 {
        let long = read_u32(5)
            .ok_or_else(|| OfdError::Render("truncated JBIG2 reference count".into()))?;
        let count = (long & 0x1fff_ffff) as usize;
        let retention_bytes = count
            .checked_add(1)
            .map(|n| n / 8)
            .ok_or_else(|| OfdError::ResourceLimit("JBIG2 reference count overflow".into()))?;
        let offset = 9usize
            .checked_add(retention_bytes)
            .ok_or_else(|| OfdError::ResourceLimit("JBIG2 segment header size overflow".into()))?;
        (count, offset)
    } else {
        ((rtscarf >> 5) as usize, 6)
    };
    if reference_count > max_references {
        return Err(OfdError::ResourceLimit(format!(
            "JBIG2 segment has {reference_count} references; limit is {max_references}"
        )));
    }

    let reference_size = if number <= 256 {
        1usize
    } else if number <= 65_536 {
        2
    } else {
        4
    };
    let page_size = if flags & 0x40 != 0 { 4usize } else { 1 };
    let refs_bytes = reference_count
        .checked_mul(reference_size)
        .ok_or_else(|| OfdError::ResourceLimit("JBIG2 reference size overflow".into()))?;
    let required = offset
        .checked_add(refs_bytes)
        .and_then(|n| n.checked_add(page_size))
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| OfdError::ResourceLimit("JBIG2 header size overflow".into()))?;
    if data.len() < required {
        return Err(OfdError::Render("truncated JBIG2 segment header".into()));
    }

    let mut references = Vec::with_capacity(reference_count);
    for _ in 0..reference_count {
        let value = match reference_size {
            1 => data[offset] as u32,
            2 => u16::from_be_bytes([data[offset], data[offset + 1]]) as u32,
            4 => u32::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]),
            _ => unreachable!(),
        };
        references.push(value);
        offset += reference_size;
    }
    offset += page_size;
    let data_length = u32::from_be_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ]);
    offset += 4;

    Ok((
        Jbig2SegmentMeta {
            number,
            kind: flags & 0x3f,
            references,
            data_length,
        },
        offset,
    ))
}

fn inspect_jbig2_segment(
    meta: &Jbig2SegmentMeta,
    body: &[u8],
    limits: &RenderLimits,
    state: &mut Jbig2Preflight,
) -> Result<()> {
    match meta.kind {
        0 => {
            if let Some((params, _)) = justbig2::symbol_dict::SymbolDictParams::parse(body) {
                ensure_jbig2_reference_slots(meta, limits, state)?;
                let new_slots = u64::from(params.sdnumnewsyms);
                state.total_symbol_slots = state
                    .total_symbol_slots
                    .checked_add(new_slots)
                    .ok_or_else(|| OfdError::ResourceLimit("JBIG2 symbol count overflow".into()))?;
                if state.total_symbol_slots > limits.max_jbig2_items as u64 {
                    return Err(OfdError::ResourceLimit(format!(
                        "JBIG2 exceeds {} cumulative symbol slots",
                        limits.max_jbig2_items
                    )));
                }
                state.symbol_slots.insert(meta.number, new_slots);
            }
        }
        48 => inspect_jbig2_page_info(body, limits, state)?,
        6 | 7 => {
            ensure_jbig2_reference_slots(meta, limits, state)?;
            inspect_jbig2_region(body, limits, state)?;
        }
        38 | 39 => inspect_jbig2_region(body, limits, state)?,
        _ => {}
    }
    Ok(())
}

fn ensure_jbig2_reference_slots(
    meta: &Jbig2SegmentMeta,
    limits: &RenderLimits,
    state: &Jbig2Preflight,
) -> Result<()> {
    let slots = meta.references.iter().try_fold(0u64, |total, reference| {
        total
            .checked_add(state.symbol_slots.get(reference).copied().unwrap_or(0))
            .ok_or_else(|| OfdError::ResourceLimit("JBIG2 referenced symbols overflow".into()))
    })?;
    if slots > limits.max_jbig2_items as u64 {
        return Err(OfdError::ResourceLimit(format!(
            "JBIG2 segment references {slots} symbol slots; limit is {}",
            limits.max_jbig2_items
        )));
    }
    Ok(())
}

fn inspect_jbig2_page_info(
    body: &[u8],
    limits: &RenderLimits,
    state: &mut Jbig2Preflight,
) -> Result<()> {
    if body.len() < 19 {
        return Err(OfdError::Render("truncated JBIG2 page information".into()));
    }
    let width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let declared_height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let striped = declared_height == u32::MAX;
    let height = if striped {
        let striping = i16::from_be_bytes([body[17], body[18]]);
        if striping < 0 {
            u32::from((striping & 0x7fff) as u16)
        } else {
            0x7fff
        }
    } else {
        declared_height
    };
    let page = Jbig2PageBudget {
        width,
        height: height.max(1),
        striped,
    };
    ensure_image_size(page.width, page.height, limits)?;
    let pixels = u64::from(page.width) * u64::from(page.height);
    let total = state
        .total_page_pixels
        .checked_add(pixels)
        .ok_or_else(|| OfdError::ResourceLimit("JBIG2 page pixel count overflow".into()))?;
    ensure_jbig2_total_page_pixels(total, limits)?;
    state.total_page_pixels = total;
    state.pages.push(page);
    state.current_page = Some(state.pages.len() - 1);
    Ok(())
}

fn inspect_jbig2_region(
    body: &[u8],
    limits: &RenderLimits,
    state: &mut Jbig2Preflight,
) -> Result<()> {
    if body.len() < 17 {
        return Err(OfdError::Render(
            "truncated JBIG2 region information".into(),
        ));
    }
    let width = u32::from_be_bytes([body[0], body[1], body[2], body[3]]);
    let height = u32::from_be_bytes([body[4], body[5], body[6], body[7]]);
    let y = u32::from_be_bytes([body[12], body[13], body[14], body[15]]);
    ensure_image_size(width, height, limits)?;
    let pixels = u64::from(width) * u64::from(height);
    state.decode_pixels = state
        .decode_pixels
        .checked_add(pixels)
        .ok_or_else(|| OfdError::ResourceLimit("JBIG2 decoded-region work overflow".into()))?;
    if state.decode_pixels > limits.max_jbig2_decode_pixels {
        return Err(OfdError::ResourceLimit(format!(
            "JBIG2 regions require {} decoded pixels; limit is {}",
            state.decode_pixels, limits.max_jbig2_decode_pixels
        )));
    }

    if let Some(index) = state.current_page {
        let mut page = state.pages[index];
        if page.striped {
            let required_height = y.checked_add(height).ok_or_else(|| {
                OfdError::ResourceLimit("JBIG2 striped page height overflow".into())
            })?;
            if required_height > page.height {
                page.height = required_height;
                charge_jbig2_page_growth(index, page, limits, state)?;
            }
        }
    }
    Ok(())
}

fn charge_jbig2_page_growth(
    page_index: usize,
    page: Jbig2PageBudget,
    limits: &RenderLimits,
    state: &mut Jbig2Preflight,
) -> Result<()> {
    ensure_image_size(page.width, page.height, limits)?;
    let previous_page = state
        .pages
        .get(page_index)
        .ok_or_else(|| OfdError::Render("JBIG2 current page is missing".into()))?;
    let previous = u64::from(previous_page.width) * u64::from(previous_page.height);
    let pixels = u64::from(page.width) * u64::from(page.height);
    let total = state
        .total_page_pixels
        .checked_sub(previous)
        .and_then(|n| n.checked_add(pixels))
        .ok_or_else(|| OfdError::ResourceLimit("JBIG2 page pixel count overflow".into()))?;
    ensure_jbig2_total_page_pixels(total, limits)?;
    state.total_page_pixels = total;
    state.pages[page_index] = page;
    Ok(())
}

fn ensure_jbig2_total_page_pixels(total: u64, limits: &RenderLimits) -> Result<()> {
    let max_total = limits
        .max_image_pixels
        .min(limits.max_image_bytes / 4)
        .min(limits.max_image_working_bytes / 12);
    if total > max_total {
        return Err(OfdError::ResourceLimit(format!(
            "JBIG2 pages require {total} cumulative pixels; limit is {max_total}"
        )));
    }
    Ok(())
}

/// Build a premultiplied `tiny-skia` pixmap from a straight-alpha RGBA image.
fn rgba_to_pixmap(rgba: &image::RgbaImage) -> Option<Pixmap> {
    rgba_to_pixmap_masked(rgba, None)
}

fn rgba_to_pixmap_masked(
    rgba: &image::RgbaImage,
    mask: Option<&image::RgbaImage>,
) -> Option<Pixmap> {
    let (w, h) = rgba.dimensions();
    if mask.is_some_and(|mask| mask.dimensions() != (w, h)) {
        return None;
    }
    let mut pm = Pixmap::new(w, h)?;
    for (index, (dst, px)) in pm.pixels_mut().iter_mut().zip(rgba.pixels()).enumerate() {
        let [r, g, b, a] = px.0;
        let mask_alpha = mask.map_or(255, |mask| {
            image_mask_alpha(mask.get_pixel(index as u32 % w, index as u32 / w))
        });
        let a = ((u16::from(a) * u16::from(mask_alpha)) / 255) as u8;
        *dst = tiny_skia::PremultipliedColorU8::from_rgba(
            ((r as u16 * a as u16) / 255) as u8,
            ((g as u16 * a as u16) / 255) as u8,
            ((b as u16 * a as u16) / 255) as u8,
            a,
        )?;
    }
    Some(pm)
}

fn image_mask_alpha(pixel: &image::Rgba<u8>) -> u8 {
    let [red, green, blue, alpha] = pixel.0;
    let luminance =
        (77 * u16::from(red) + 150 * u16::from(green) + 29 * u16::from(blue) + 128) / 256;
    ((luminance * u16::from(alpha)) / 255) as u8
}

fn is_binary_image_mask(mask: &image::RgbaImage) -> bool {
    mask.pixels().all(|pixel| {
        let [red, green, blue, _] = pixel.0;
        red == green && green == blue && matches!(red, 0 | 255)
    })
}

/// Decode image bytes straight to a pixmap (used for small seal pictures).
fn decode_bytes(format: ImageFormat, data: &[u8], limits: &RenderLimits) -> Result<Pixmap> {
    rgba_to_pixmap(&decode_rgba(format, data, limits)?)
        .ok_or_else(|| OfdError::ResourceLimit("could not allocate raster surface".into()))
}

/// Bridges `ttf-parser` glyph outlines into a `tiny-skia` path, placing points
/// at the current pen position in object-space mm (font y-up → page y-down).
struct GlyphOutline<'a> {
    builder: &'a mut PathBuilder,
    pen_x: f32,
    pen_y: f32,
    scale: f32,
    h_scale: f32,
    angle: f32,
    italic_shear: f32,
}

impl<'a> GlyphOutline<'a> {
    #[inline]
    fn map(&self, x: f32, y: f32) -> (f32, f32) {
        let gx = (x + self.italic_shear * y) * self.scale * self.h_scale;
        let gy = -y * self.scale;
        if self.angle == 0.0 {
            return (self.pen_x + gx, self.pen_y + gy);
        }
        let radians = self.angle.to_radians();
        let (sin, cos) = radians.sin_cos();
        (
            self.pen_x + cos * gx - sin * gy,
            self.pen_y + sin * gx + cos * gy,
        )
    }
}

impl<'a> OutlineBuilder for GlyphOutline<'a> {
    fn move_to(&mut self, x: f32, y: f32) {
        let (px, py) = self.map(x, y);
        self.builder.move_to(px, py);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (px, py) = self.map(x, y);
        self.builder.line_to(px, py);
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let (cx, cy) = self.map(x1, y1);
        let (px, py) = self.map(x, y);
        self.builder.quad_to(cx, cy, px, py);
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let (c1x, c1y) = self.map(x1, y1);
        let (c2x, c2y) = self.map(x2, y2);
        let (px, py) = self.map(x, y);
        self.builder.cubic_to(c1x, c1y, c2x, c2y, px, py);
    }
    fn close(&mut self) {
        self.builder.close();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geom::Rect;

    fn rect_path(common: GraphicCommon, fill_color: Option<OfdColor>) -> PathObject {
        let w = common.boundary.w;
        let h = common.boundary.h;
        PathObject {
            common,
            stroke: false,
            fill: true,
            fill_rule: FillRule::NonZero,
            fill_color,
            stroke_color: None,
            commands: vec![
                PathCommand::MoveTo { x: 0.0, y: 0.0 },
                PathCommand::LineTo { x: w, y: 0.0 },
                PathCommand::LineTo { x: w, y: h },
                PathCommand::LineTo { x: 0.0, y: h },
                PathCommand::Close,
            ],
        }
    }

    fn doc_with_objects(objects: Vec<GraphicObject>, resources: Resources) -> Document {
        Document {
            page_area: PageArea {
                physical_box: Some(Rect::new(0.0, 0.0, 20.0, 10.0)),
                ..Default::default()
            },
            resources,
            pages: vec![Page {
                id: 1,
                area: None,
                layers: vec![Layer {
                    id: 0,
                    kind: LayerKind::Body,
                    draw_param: None,
                    objects,
                }],
                actions: Vec::new(),
            }],
            ..Default::default()
        }
    }

    /// Build a minimal two-glyph TrueType font for deterministic cmap/outline
    /// fallback tests. Glyph 0 is empty; glyph 1 is a square mapped to `mapped`.
    fn test_font(family: &str, mapped: char) -> Arc<Vec<u8>> {
        test_font_with_style(family, mapped, 400, false)
    }

    fn test_font_with_style(family: &str, mapped: char, weight: u16, italic: bool) -> Arc<Vec<u8>> {
        fn push_u16(out: &mut Vec<u8>, value: u16) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        fn push_i16(out: &mut Vec<u8>, value: i16) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        fn push_u32(out: &mut Vec<u8>, value: u32) {
            out.extend_from_slice(&value.to_be_bytes());
        }
        fn checksum(bytes: &[u8]) -> u32 {
            bytes.chunks(4).fold(0u32, |sum, chunk| {
                let mut word = [0u8; 4];
                word[..chunk.len()].copy_from_slice(chunk);
                sum.wrapping_add(u32::from_be_bytes(word))
            })
        }
        fn utf16be(value: &str) -> Vec<u8> {
            value.encode_utf16().flat_map(u16::to_be_bytes).collect()
        }

        let mut head = vec![0u8; 54];
        head[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        head[4..8].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        head[12..16].copy_from_slice(&0x5F0F_3CF5u32.to_be_bytes());
        head[18..20].copy_from_slice(&1000u16.to_be_bytes());
        head[36..38].copy_from_slice(&0i16.to_be_bytes());
        head[38..40].copy_from_slice(&0i16.to_be_bytes());
        head[40..42].copy_from_slice(&800i16.to_be_bytes());
        head[42..44].copy_from_slice(&800i16.to_be_bytes());
        head[46..48].copy_from_slice(&8u16.to_be_bytes());
        head[48..50].copy_from_slice(&2i16.to_be_bytes());

        let mut hhea = vec![0u8; 36];
        hhea[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        hhea[4..6].copy_from_slice(&800i16.to_be_bytes());
        hhea[6..8].copy_from_slice(&(-200i16).to_be_bytes());
        hhea[10..12].copy_from_slice(&1000u16.to_be_bytes());
        hhea[18..20].copy_from_slice(&1i16.to_be_bytes());
        hhea[34..36].copy_from_slice(&2u16.to_be_bytes());

        let mut maxp = vec![0u8; 32];
        maxp[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        maxp[4..6].copy_from_slice(&2u16.to_be_bytes());
        maxp[6..8].copy_from_slice(&4u16.to_be_bytes());
        maxp[8..10].copy_from_slice(&1u16.to_be_bytes());

        let mut hmtx = Vec::new();
        for _ in 0..2 {
            push_u16(&mut hmtx, 1000);
            push_i16(&mut hmtx, 0);
        }

        let mut glyf = Vec::new();
        push_i16(&mut glyf, 1);
        for value in [0, 0, 800, 800] {
            push_i16(&mut glyf, value);
        }
        push_u16(&mut glyf, 3);
        push_u16(&mut glyf, 0);
        glyf.extend_from_slice(&[1, 1, 1, 1]);
        for value in [0, 800, 0, -800, 0, 0, 800, 0] {
            push_i16(&mut glyf, value);
        }
        let mut loca = Vec::new();
        for offset in [0u16, 0, (glyf.len() / 2) as u16] {
            push_u16(&mut loca, offset);
        }

        let code = u16::try_from(mapped as u32).expect("test font uses a BMP character");
        let mut cmap_format4 = Vec::new();
        push_u16(&mut cmap_format4, 4);
        push_u16(&mut cmap_format4, 32);
        push_u16(&mut cmap_format4, 0);
        push_u16(&mut cmap_format4, 4);
        push_u16(&mut cmap_format4, 4);
        push_u16(&mut cmap_format4, 1);
        push_u16(&mut cmap_format4, 0);
        push_u16(&mut cmap_format4, code);
        push_u16(&mut cmap_format4, 0xffff);
        push_u16(&mut cmap_format4, 0);
        push_u16(&mut cmap_format4, code);
        push_u16(&mut cmap_format4, 0xffff);
        push_u16(&mut cmap_format4, 1u16.wrapping_sub(code));
        push_u16(&mut cmap_format4, 1);
        push_u16(&mut cmap_format4, 0);
        push_u16(&mut cmap_format4, 0);
        let mut cmap = Vec::new();
        push_u16(&mut cmap, 0);
        push_u16(&mut cmap, 1);
        push_u16(&mut cmap, 3);
        push_u16(&mut cmap, 1);
        push_u32(&mut cmap, 12);
        cmap.extend_from_slice(&cmap_format4);

        let postscript = family.replace(' ', "");
        let family_bytes = utf16be(family);
        let postscript_bytes = utf16be(&postscript);
        let mut name = Vec::new();
        push_u16(&mut name, 0);
        push_u16(&mut name, 2);
        push_u16(&mut name, 30);
        for (name_id, length, offset) in [
            (1u16, family_bytes.len() as u16, 0u16),
            (
                6u16,
                postscript_bytes.len() as u16,
                family_bytes.len() as u16,
            ),
        ] {
            push_u16(&mut name, 3);
            push_u16(&mut name, 1);
            push_u16(&mut name, 0x0409);
            push_u16(&mut name, name_id);
            push_u16(&mut name, length);
            push_u16(&mut name, offset);
        }
        name.extend_from_slice(&family_bytes);
        name.extend_from_slice(&postscript_bytes);

        let mut os2 = vec![0u8; 78];
        os2[2..4].copy_from_slice(&500i16.to_be_bytes());
        os2[4..6].copy_from_slice(&weight.to_be_bytes());
        os2[6..8].copy_from_slice(&5u16.to_be_bytes());
        let mut selection = 0u16;
        if italic {
            selection |= 1;
        }
        if weight >= 700 {
            selection |= 1 << 5;
        }
        os2[62..64].copy_from_slice(&selection.to_be_bytes());

        let mut post = vec![0u8; 32];
        post[0..4].copy_from_slice(&0x0003_0000u32.to_be_bytes());
        if italic {
            post[4..8].copy_from_slice(&(-12i32 * 65_536).to_be_bytes());
        }

        let mut tables = vec![
            (*b"OS/2", os2),
            (*b"cmap", cmap),
            (*b"glyf", glyf),
            (*b"head", head),
            (*b"hhea", hhea),
            (*b"hmtx", hmtx),
            (*b"loca", loca),
            (*b"maxp", maxp),
            (*b"name", name),
            (*b"post", post),
        ];
        tables.sort_by_key(|(tag, _)| *tag);
        let directory_len = 12 + tables.len() * 16;
        let mut offsets = Vec::with_capacity(tables.len());
        let mut next_offset = directory_len;
        for (_, data) in &tables {
            offsets.push(next_offset);
            next_offset += (data.len() + 3) & !3;
        }

        let mut font = Vec::with_capacity(next_offset);
        push_u32(&mut font, 0x0001_0000);
        push_u16(&mut font, tables.len() as u16);
        push_u16(&mut font, 128);
        push_u16(&mut font, 3);
        push_u16(&mut font, 0);
        for ((tag, data), offset) in tables.iter().zip(&offsets) {
            font.extend_from_slice(tag);
            push_u32(&mut font, checksum(data));
            push_u32(&mut font, *offset as u32);
            push_u32(&mut font, data.len() as u32);
        }
        for (_, data) in tables {
            font.extend_from_slice(&data);
            while font.len() % 4 != 0 {
                font.push(0);
            }
        }
        Arc::new(font)
    }

    #[test]
    fn duplicate_resource_ids_keep_the_first_declaration() {
        let resources = Resources {
            draw_params: vec![
                DrawParam {
                    id: 1,
                    line_width: Some(1.0),
                    ..Default::default()
                },
                DrawParam {
                    id: 1,
                    line_width: Some(2.0),
                    ..Default::default()
                },
            ],
            images: vec![
                MultiMedia {
                    id: 2,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: vec![1],
                },
                MultiMedia {
                    id: 2,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: vec![2],
                },
            ],
            composite_graphic_units: vec![
                CompositeGraphicUnit {
                    id: 3,
                    width: 1.0,
                    height: 1.0,
                    objects: Vec::new(),
                },
                CompositeGraphicUnit {
                    id: 3,
                    width: 2.0,
                    height: 2.0,
                    objects: Vec::new(),
                },
            ],
            color_spaces: vec![
                ColorSpace {
                    id: 4,
                    kind: ColorSpaceKind::Gray,
                    bits_per_component: 1,
                    palette: Vec::new(),
                    profile: None,
                },
                ColorSpace {
                    id: 4,
                    kind: ColorSpaceKind::Rgb,
                    bits_per_component: 8,
                    palette: Vec::new(),
                    profile: None,
                },
            ],
            ..Default::default()
        };
        let doc = doc_with_objects(Vec::new(), resources);
        let session = RenderSession::new(&doc, RenderOptions::default());

        assert_eq!(session.draw_params[&1].line_width, Some(1.0));
        assert_eq!(session.images[&2].data, [1]);
        assert_eq!(session.composites[&3].width, 1.0);
        assert_eq!(session.color_spaces[&4].kind, ColorSpaceKind::Gray);
    }

    fn pixel(b: &Bitmap, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * b.width + x) * 4) as usize;
        b.rgba[idx..idx + 4].try_into().unwrap()
    }

    /// A 10mm x 10mm page with one page-filling black path, toggling visibility.
    fn one_path_doc(visible: bool) -> Document {
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
            visible,
            ..Default::default()
        };
        let path = rect_path(common, Some(Color::BLACK.into()));
        Document {
            page_area: PageArea {
                physical_box: Some(Rect::new(0.0, 0.0, 10.0, 10.0)),
                ..Default::default()
            },
            pages: vec![Page {
                id: 1,
                area: None,
                layers: vec![Layer {
                    id: 0,
                    kind: LayerKind::Body,
                    draw_param: None,
                    objects: vec![GraphicObject::Path(path)],
                }],
                actions: Vec::new(),
            }],
            ..Default::default()
        }
    }

    fn non_white(b: &Bitmap) -> usize {
        b.rgba
            .chunks_exact(4)
            .filter(|p| p[0] != 255 || p[1] != 255 || p[2] != 255)
            .count()
    }

    fn pixel_at_mm(bitmap: &Bitmap, x: f32, y: f32) -> [u8; 4] {
        let scale = 96.0 / crate::geom::MM_PER_INCH;
        pixel(bitmap, (x * scale) as u32, (y * scale) as u32)
    }

    #[test]
    fn visible_path_paints() {
        let b = render_page(&one_path_doc(true), 0, 96.0).unwrap();
        assert!(non_white(&b) > 100, "a visible path should paint");
    }

    #[test]
    fn invisible_path_is_skipped() {
        let b = render_page(&one_path_doc(false), 0, 96.0).unwrap();
        assert_eq!(non_white(&b), 0, "a Visible=false path must not paint");
    }

    #[test]
    fn invisible_annotation_appearance_is_skipped() {
        let object = GraphicObject::Path(rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                ..Default::default()
            },
            Some(Color::BLACK.into()),
        ));
        let mut document = doc_with_objects(Vec::new(), Resources::default());
        document.annotations.push(Annotation {
            page_id: 1,
            id: 2,
            annot_type: "Watermark".into(),
            creator: "test".into(),
            last_mod_date: "2026-07-29".into(),
            subtype: None,
            visible: false,
            print: true,
            no_zoom: false,
            no_rotate: false,
            read_only: true,
            remark: None,
            parameters: Vec::new(),
            appearance_boundary: Some(Rect::new(0.0, 0.0, 10.0, 10.0)),
            objects: vec![object],
        });

        assert_eq!(non_white(&render_page(&document, 0, 96.0).unwrap()), 0);
        document.annotations[0].visible = true;
        assert!(non_white(&render_page(&document, 0, 96.0).unwrap()) > 100);
    }

    #[test]
    fn object_boundary_clips_overflowing_geometry() {
        let common = GraphicCommon {
            boundary: Rect::new(5.0, 2.0, 5.0, 5.0),
            ..Default::default()
        };
        let path = PathObject {
            common,
            stroke: false,
            fill: true,
            fill_rule: FillRule::NonZero,
            fill_color: Some(Color::BLACK.into()),
            stroke_color: None,
            commands: vec![
                PathCommand::MoveTo { x: 0.0, y: 0.0 },
                PathCommand::LineTo { x: 12.0, y: 0.0 },
                PathCommand::LineTo { x: 12.0, y: 8.0 },
                PathCommand::LineTo { x: 0.0, y: 8.0 },
                PathCommand::Close,
            ],
        };
        let bitmap = render_page(
            &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
            0,
            96.0,
        )
        .unwrap();
        assert!(pixel_at_mm(&bitmap, 7.0, 4.0)[0] < 20);
        assert!(pixel_at_mm(&bitmap, 12.0, 4.0)[0] > 240);
    }

    fn clip_rect(x: f32, width: f32) -> ClipArea {
        ClipArea {
            ctm: Matrix::IDENTITY,
            draw_param: None,
            shape: ClipShape::Path(Box::new(PathObject {
                common: GraphicCommon::default(),
                stroke: false,
                fill: true,
                fill_rule: FillRule::NonZero,
                fill_color: None,
                stroke_color: None,
                commands: vec![
                    PathCommand::MoveTo { x, y: 0.0 },
                    PathCommand::LineTo {
                        x: x + width,
                        y: 0.0,
                    },
                    PathCommand::LineTo {
                        x: x + width,
                        y: 10.0,
                    },
                    PathCommand::LineTo { x, y: 10.0 },
                    PathCommand::Close,
                ],
            })),
        }
    }

    #[test]
    fn clip_areas_union_and_clips_intersect() {
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
            clips: vec![
                Clip {
                    areas: vec![clip_rect(0.0, 5.0), clip_rect(10.0, 5.0)],
                },
                Clip {
                    areas: vec![clip_rect(2.0, 10.0)],
                },
            ],
            ..Default::default()
        };
        let path = rect_path(common, Some(Color::BLACK.into()));
        let bitmap = render_page(
            &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
            0,
            96.0,
        )
        .unwrap();
        assert!(pixel_at_mm(&bitmap, 3.0, 5.0)[0] < 20);
        assert!(pixel_at_mm(&bitmap, 7.0, 5.0)[0] > 240);
        assert!(pixel_at_mm(&bitmap, 11.0, 5.0)[0] < 20);
        assert!(pixel_at_mm(&bitmap, 14.0, 5.0)[0] > 240);
    }

    #[test]
    fn object_stroke_properties_override_draw_param() {
        let resources = Resources {
            draw_params: vec![DrawParam {
                id: 7,
                line_width: Some(8.0),
                cap: Some(LineCap::Round),
                join: Some(LineJoin::Round),
                ..Default::default()
            }],
            ..Default::default()
        };
        let doc = doc_with_objects(Vec::new(), resources);
        let mut session = RenderSession::new(&doc, RenderOptions::default());
        let ctx = RenderCtx {
            session: &mut session,
            frame: RenderFrame {
                base: Transform::identity(),
                origin: (0.0, 0.0),
                size: (10, 10),
                dpi: 96.0,
            },
            composite_stack: Vec::new(),
            composite_surface_pixels: 0,
            pattern_depth: 0,
            pattern_surface_pixels: 0,
            budget: Rc::new(RenderBudget::default()),
        };
        let common = GraphicCommon {
            draw_param: Some(7),
            line_width: Some(1.25),
            cap: Some(LineCap::Square),
            join: Some(LineJoin::Bevel),
            ..Default::default()
        };
        let stroke = ctx
            .stroke_for(&common, DrawParamSources::object(common.draw_param, &[]))
            .unwrap();
        assert_eq!(stroke.width, 1.25);
        assert_eq!(stroke.line_cap, SkLineCap::Square);
        assert_eq!(stroke.line_join, SkLineJoin::Bevel);
    }

    #[test]
    fn layer_fill_is_used_when_object_draw_param_omits_it() {
        let resources = Resources {
            draw_params: vec![DrawParam {
                id: 7,
                fill_color: Some(Color::rgb(220, 20, 30).into()),
                ..Default::default()
            }],
            ..Default::default()
        };
        let path = GraphicObject::Path(rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                ..Default::default()
            },
            None,
        ));
        let mut doc = doc_with_objects(vec![path], resources);
        doc.pages[0].layers[0].draw_param = Some(7);

        let bitmap = render_page(&doc, 0, 96.0).unwrap();
        let color = pixel_at_mm(&bitmap, 5.0, 5.0);
        assert!(
            color[0] > 180 && color[1] < 60 && color[2] < 70,
            "{color:?}"
        );
    }

    #[test]
    fn composite_children_fall_back_field_by_field_to_composite_then_layer() {
        let child = GraphicObject::Path(rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                ..Default::default()
            },
            None,
        ));
        let composite = |x, draw_param| {
            GraphicObject::Composite(CompositeObject {
                common: GraphicCommon {
                    boundary: Rect::new(x, 0.0, 10.0, 10.0),
                    draw_param: Some(draw_param),
                    ..Default::default()
                },
                resource_id: 9,
            })
        };
        let resources = Resources {
            draw_params: vec![
                DrawParam {
                    id: 1,
                    fill_color: Some(Color::rgb(20, 40, 220).into()),
                    ..Default::default()
                },
                DrawParam {
                    id: 2,
                    fill_color: Some(Color::rgb(220, 30, 20).into()),
                    ..Default::default()
                },
                DrawParam {
                    id: 3,
                    line_width: Some(2.0),
                    ..Default::default()
                },
            ],
            composite_graphic_units: vec![CompositeGraphicUnit {
                id: 9,
                width: 10.0,
                height: 10.0,
                objects: vec![child],
            }],
            ..Default::default()
        };
        let mut doc = doc_with_objects(vec![composite(0.0, 2), composite(10.0, 3)], resources);
        doc.pages[0].layers[0].draw_param = Some(1);

        let bitmap = render_page(&doc, 0, 96.0).unwrap();
        let from_composite = pixel_at_mm(&bitmap, 5.0, 5.0);
        let from_layer = pixel_at_mm(&bitmap, 15.0, 5.0);
        assert!(
            from_composite[0] > 180 && from_composite[2] < 70,
            "{from_composite:?}"
        );
        assert!(from_layer[2] > 180 && from_layer[0] < 70, "{from_layer:?}");
    }

    #[test]
    fn strict_render_reports_draw_param_depth_exhaustion() {
        let resources = Resources {
            draw_params: vec![
                DrawParam {
                    id: 1,
                    relative: Some(2),
                    ..Default::default()
                },
                DrawParam {
                    id: 2,
                    fill_color: Some(Color::BLACK.into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let path = GraphicObject::Path(rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                draw_param: Some(1),
                ..Default::default()
            },
            None,
        ));
        let options = RenderOptions {
            strict: true,
            limits: RenderLimits {
                max_draw_param_depth: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            render_page_with(&doc_with_objects(vec![path], resources), 0, 96.0, &options,),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn transparent_composite_does_not_paint() {
        let child_common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
            ..Default::default()
        };
        let child = GraphicObject::Path(rect_path(child_common, Some(Color::BLACK.into())));
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 0,
            ..Default::default()
        };
        let composite = GraphicObject::Composite(CompositeObject {
            common,
            resource_id: 9,
        });
        let resources = Resources {
            composite_graphic_units: vec![CompositeGraphicUnit {
                id: 9,
                width: 10.0,
                height: 10.0,
                objects: vec![child],
            }],
            ..Default::default()
        };
        let bitmap = render_page(&doc_with_objects(vec![composite], resources), 0, 96.0).unwrap();
        assert_eq!(non_white(&bitmap), 0);
    }

    #[test]
    fn invalid_composite_size_is_skipped_in_lax_mode_and_reported_in_strict_mode() {
        let composite = GraphicObject::Composite(CompositeObject {
            common: GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                ..Default::default()
            },
            resource_id: 9,
        });
        let resources = Resources {
            composite_graphic_units: vec![CompositeGraphicUnit {
                id: 9,
                width: 0.0,
                height: 10.0,
                objects: vec![GraphicObject::Path(rect_path(
                    GraphicCommon {
                        boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                        ..Default::default()
                    },
                    Some(Color::BLACK.into()),
                ))],
            }],
            ..Default::default()
        };
        let doc = doc_with_objects(vec![composite], resources);

        assert_eq!(non_white(&render_page(&doc, 0, 96.0).unwrap()), 0);
        let error = render_page_with(
            &doc,
            0,
            96.0,
            &RenderOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid size"));
    }

    #[test]
    fn composite_cycles_are_bounded_and_strictly_reported() {
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
            ..Default::default()
        };
        let composite = GraphicObject::Composite(CompositeObject {
            common: common.clone(),
            resource_id: 1,
        });
        let resources = Resources {
            composite_graphic_units: vec![CompositeGraphicUnit {
                id: 1,
                width: 10.0,
                height: 10.0,
                objects: vec![composite.clone()],
            }],
            ..Default::default()
        };
        let doc = doc_with_objects(vec![composite], resources);
        let options = RenderOptions {
            strict: true,
            ..Default::default()
        };
        let error = render_page_with(&doc, 0, 96.0, &options).unwrap_err();
        assert!(error.to_string().contains("cyclic CompositeGraphicUnit"));
    }

    #[test]
    fn image_alpha_is_applied() {
        let source = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
            alpha: 0,
            ..Default::default()
        };
        let image = GraphicObject::Image(ImageObject {
            common,
            resource_id: 4,
            substitution: None,
            image_mask: None,
            border: None,
        });
        let resources = Resources {
            images: vec![MultiMedia {
                id: 4,
                kind: MediaKind::Image,
                format: ImageFormat::Png,
                data: bytes.into_inner(),
            }],
            ..Default::default()
        };
        let bitmap = render_page(&doc_with_objects(vec![image], resources), 0, 96.0).unwrap();
        assert_eq!(non_white(&bitmap), 0);
    }

    #[test]
    fn tiff_image_decodes_under_normal_image_limits() {
        let source = image::RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 {
                image::Rgba([255, 0, 0, 255])
            } else {
                image::Rgba([0, 0, 255, 255])
            }
        });
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut bytes, image::ImageFormat::Tiff)
            .unwrap();

        let decoded = decode_rgba(
            ImageFormat::Tiff,
            &bytes.into_inner(),
            &RenderLimits::default(),
        )
        .unwrap();
        assert_eq!(decoded.dimensions(), (2, 1));
        assert_eq!(decoded.get_pixel(0, 0).0, [255, 0, 0, 255]);
        assert_eq!(decoded.get_pixel(1, 0).0, [0, 0, 255, 255]);
    }

    #[test]
    fn decoded_image_cache_obeys_its_total_byte_budget() {
        let source = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let encoded = bytes.into_inner();
        let resources = Resources {
            images: vec![
                MultiMedia {
                    id: 1,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encoded.clone(),
                },
                MultiMedia {
                    id: 2,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encoded,
                },
            ],
            ..Default::default()
        };
        let doc = doc_with_objects(Vec::new(), resources);
        let mut session = RenderSession::new(
            &doc,
            RenderOptions {
                limits: RenderLimits {
                    max_decoded_image_cache_bytes: 4,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let budget = RenderBudget::default();

        assert!(session.decoded_image_rgba(1, &budget).unwrap().is_some());
        assert!(session.decoded_images.contains_key(&1));
        assert!(session.decoded_image_rgba(2, &budget).unwrap().is_some());
        assert!(!session.decoded_images.contains_key(&1));
        assert!(session.decoded_images.contains_key(&2));
        assert_eq!(session.decoded_image_bytes, 4);
    }

    #[test]
    fn raster_decode_work_is_cumulative_across_cache_evictions() {
        let source = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let encoded = bytes.into_inner();
        let resources = Resources {
            images: vec![
                MultiMedia {
                    id: 1,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encoded.clone(),
                },
                MultiMedia {
                    id: 2,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encoded,
                },
            ],
            ..Default::default()
        };
        let doc = doc_with_objects(Vec::new(), resources);
        let mut session = RenderSession::new(
            &doc,
            RenderOptions {
                limits: RenderLimits {
                    max_decoded_image_cache_bytes: 4,
                    max_raster_decode_pixels: 2,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let budget = RenderBudget::default();

        assert!(session.decoded_image_rgba(1, &budget).unwrap().is_some());
        assert!(session.decoded_image_rgba(1, &budget).unwrap().is_some());
        assert!(session.decoded_image_rgba(2, &budget).unwrap().is_some());
        assert!(matches!(
            session.decoded_image_rgba(1, &budget),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn repeated_stamp_annotations_share_one_decoded_seal() {
        let source = image::RgbaImage::from_pixel(1, 1, image::Rgba([0, 0, 0, 255]));
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let appearance = Arc::new(SealAppearance::Raster {
            format: ImageFormat::Png,
            data: bytes.into_inner(),
        });
        let mut doc = doc_with_objects(Vec::new(), Resources::default());
        doc.seals = vec![
            Seal {
                page_id: 1,
                boundary: Rect::new(0.0, 0.0, 5.0, 5.0),
                clip: None,
                appearance: appearance.clone(),
            },
            Seal {
                page_id: 1,
                boundary: Rect::new(5.0, 0.0, 5.0, 5.0),
                clip: None,
                appearance,
            },
        ];
        let mut session = RenderSession::new(&doc, RenderOptions::default());
        session.render_page(0, 96.0).unwrap();
        assert_eq!(session.decoded_seals.len(), 1);
        assert_eq!(session.decoded_image_bytes, 4);
    }

    #[test]
    fn invalid_seal_geometry_is_skipped_in_lax_mode_and_reported_in_strict_mode() {
        let mut doc = doc_with_objects(Vec::new(), Resources::default());
        doc.seals.push(Seal {
            page_id: 1,
            boundary: Rect::new(0.0, 0.0, 0.0, 5.0),
            clip: None,
            appearance: Arc::new(SealAppearance::Raster {
                format: ImageFormat::Unknown,
                data: Vec::new(),
            }),
        });

        assert_eq!(non_white(&render_page(&doc, 0, 96.0).unwrap()), 0);
        let error = render_page_with(
            &doc,
            0,
            96.0,
            &RenderOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid Boundary"));
    }

    #[test]
    fn strict_render_reports_missing_image_resource() {
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
            ..Default::default()
        };
        let image = GraphicObject::Image(ImageObject {
            common,
            resource_id: 404,
            substitution: None,
            image_mask: None,
            border: None,
        });
        let options = RenderOptions {
            strict: true,
            ..Default::default()
        };
        let error = render_page_with(
            &doc_with_objects(vec![image], Resources::default()),
            0,
            96.0,
            &options,
        )
        .unwrap_err();
        assert!(error.to_string().contains("image resource 404"));
    }

    #[test]
    fn image_mask_controls_alpha_and_border_is_painted() {
        let encode = |image: image::RgbaImage| {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let source = encode(image::RgbaImage::from_pixel(
            2,
            1,
            image::Rgba([255, 0, 0, 255]),
        ));
        let mask = encode(image::RgbaImage::from_fn(2, 1, |x, _| {
            if x == 0 {
                image::Rgba([255, 255, 255, 255])
            } else {
                image::Rgba([0, 0, 0, 255])
            }
        }));
        let common = GraphicCommon {
            boundary: Rect::new(2.0, 1.0, 16.0, 8.0),
            ..Default::default()
        };
        let strict = RenderOptions {
            strict: true,
            ..Default::default()
        };
        let masked = GraphicObject::Image(ImageObject {
            common,
            resource_id: 1,
            substitution: None,
            image_mask: Some(2),
            border: Some(ImageBorder {
                line_width: 0.8,
                horizontal_corner_radius: 1.0,
                vertical_corner_radius: 1.0,
                dash_offset: 0.0,
                dash_pattern: None,
                color: Some(Color::BLACK.into()),
            }),
        });
        let resources = Resources {
            images: vec![
                MultiMedia {
                    id: 1,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: source,
                },
                MultiMedia {
                    id: 2,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: mask,
                },
            ],
            ..Default::default()
        };
        let bitmap =
            render_page_with(&doc_with_objects(vec![masked], resources), 0, 96.0, &strict).unwrap();
        let visible = pixel_at_mm(&bitmap, 5.0, 5.0);
        let hidden = pixel_at_mm(&bitmap, 15.0, 5.0);
        let border = pixel_at_mm(&bitmap, 2.1, 5.0);
        assert!(visible[0] > 220 && visible[1] < 30, "{visible:?}");
        assert!(hidden[0] > 240 && hidden[1] > 240, "{hidden:?}");
        assert!(border[0] < 200 && border[1] < 200, "{border:?}");
    }

    #[test]
    fn strict_image_mask_requires_matching_dimensions() {
        let encode = |image: image::RgbaImage| {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let image = GraphicObject::Image(ImageObject {
            common: GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                ..Default::default()
            },
            resource_id: 1,
            substitution: None,
            image_mask: Some(2),
            border: None,
        });
        let resources = Resources {
            images: vec![
                MultiMedia {
                    id: 1,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encode(image::RgbaImage::from_pixel(
                        2,
                        1,
                        image::Rgba([0, 0, 0, 255]),
                    )),
                },
                MultiMedia {
                    id: 2,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encode(image::RgbaImage::from_pixel(
                        1,
                        1,
                        image::Rgba([128, 128, 128, 255]),
                    )),
                },
            ],
            ..Default::default()
        };
        let error = render_page_with(
            &doc_with_objects(vec![image], resources),
            0,
            96.0,
            &RenderOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("expected 2x1"));
    }

    #[test]
    fn strict_image_mask_requires_binary_pixels() {
        let encode = |image: image::RgbaImage| {
            let mut bytes = Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(image)
                .write_to(&mut bytes, image::ImageFormat::Png)
                .unwrap();
            bytes.into_inner()
        };
        let object = GraphicObject::Image(ImageObject {
            common: GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                ..Default::default()
            },
            resource_id: 1,
            substitution: None,
            image_mask: Some(2),
            border: None,
        });
        let resources = Resources {
            images: vec![
                MultiMedia {
                    id: 1,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encode(image::RgbaImage::from_pixel(
                        1,
                        1,
                        image::Rgba([0, 0, 0, 255]),
                    )),
                },
                MultiMedia {
                    id: 2,
                    kind: MediaKind::Image,
                    format: ImageFormat::Png,
                    data: encode(image::RgbaImage::from_pixel(
                        1,
                        1,
                        image::Rgba([128, 128, 128, 255]),
                    )),
                },
            ],
            ..Default::default()
        };
        let error = render_page_with(
            &doc_with_objects(vec![object], resources),
            0,
            96.0,
            &RenderOptions {
                strict: true,
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("not a black/white binary image"));
    }

    #[test]
    fn page_pixel_limit_rejects_oversized_surface() {
        let options = RenderOptions {
            limits: RenderLimits {
                max_page_pixels: 10,
                ..Default::default()
            },
            ..Default::default()
        };
        let error = render_page_with(&one_path_doc(true), 0, 96.0, &options).unwrap_err();
        assert!(matches!(error, OfdError::ResourceLimit(_)));
    }

    #[test]
    fn rendered_object_budget_bounds_repeated_expansion_work() {
        let invisible = |id| {
            GraphicObject::Path(rect_path(
                GraphicCommon {
                    id,
                    boundary: Rect::new(0.0, 0.0, 1.0, 1.0),
                    visible: false,
                    ..Default::default()
                },
                Some(Color::BLACK.into()),
            ))
        };
        let options = RenderOptions {
            limits: RenderLimits {
                max_rendered_objects: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        let error = render_page_with(
            &doc_with_objects(vec![invisible(1), invisible(2)], Resources::default()),
            0,
            96.0,
            &options,
        )
        .unwrap_err();
        assert!(matches!(error, OfdError::ResourceLimit(_)));
    }

    #[test]
    fn path_command_budget_bounds_one_large_object() {
        let error = render_page_with(
            &one_path_doc(true),
            0,
            96.0,
            &RenderOptions {
                limits: RenderLimits {
                    max_rendered_path_commands: 4,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, OfdError::ResourceLimit(_)));
    }

    #[test]
    fn glyph_budget_is_checked_before_font_resolution_or_outline_allocation() {
        let text = GraphicObject::Text(TextObject {
            common: GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 10.0, 10.0),
                ..Default::default()
            },
            font_id: 404,
            font_size: 3.0,
            stroke: false,
            fill: true,
            h_scale: 1.0,
            read_direction: Direction(0),
            char_direction: Direction(0),
            weight: 400,
            italic: false,
            fill_color: Some(Color::BLACK.into()),
            stroke_color: None,
            cg_transforms: Vec::new(),
            runs: vec![TextRun {
                text: "a".into(),
                ..Default::default()
            }],
        });
        let error = render_page_with(
            &doc_with_objects(vec![text], Resources::default()),
            0,
            96.0,
            &RenderOptions {
                limits: RenderLimits {
                    max_rendered_glyphs: 0,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, OfdError::ResourceLimit(_)));
    }

    #[test]
    fn substitute_font_selection_honors_text_weight_and_italic() {
        let normal = test_font_with_style("Styled Test", 'A', 400, false);
        let bold = test_font_with_style("Styled Test", 'A', 700, false);
        let italic = test_font_with_style("Styled Test", 'A', 400, true);
        let resource = Font {
            id: 41,
            font_name: "Styled Test".into(),
            ..Default::default()
        };
        let mut resolver = FontResolver::with_bundled(
            std::slice::from_ref(&resource),
            &[normal.clone(), bold.clone(), italic.clone()],
        );

        let regular_face = resolver.resolve_styled(41, 400, false).unwrap();
        assert!(Arc::ptr_eq(&regular_face.data, &normal));
        assert!(!regular_face.synthetic_italic);

        let bold_face = resolver.resolve_styled(41, 700, false).unwrap();
        assert!(Arc::ptr_eq(&bold_face.data, &bold));
        assert!(!bold_face.synthetic_italic);

        let italic_face = resolver.resolve_styled(41, 400, true).unwrap();
        assert!(Arc::ptr_eq(&italic_face.data, &italic));
        assert!(!italic_face.synthetic_italic);

        let mut upright_only = FontResolver::with_bundled(&[resource], &[normal]);
        assert!(
            upright_only
                .resolve_styled(41, 400, true)
                .unwrap()
                .synthetic_italic
        );
    }

    #[test]
    fn substitute_font_selection_honors_ct_font_style_hints() {
        let normal = test_font_with_style("Resource Style", 'A', 400, false);
        let bold_italic = test_font_with_style("Resource Style", 'A', 700, true);
        let resource = Font {
            id: 41,
            font_name: "Resource Style".into(),
            charset: Some("unicode".into()),
            italic: true,
            bold: true,
            serif: true,
            fixed_width: false,
            ..Default::default()
        };
        let mut resolver = FontResolver::with_bundled(&[resource], &[normal, bold_italic.clone()]);
        let selected = resolver.resolve(41).unwrap();
        assert!(Arc::ptr_eq(&selected.data, &bold_italic));
        assert!(!selected.synthetic_italic);
    }

    #[test]
    fn missing_substitute_glyph_uses_a_per_character_fallback_face() {
        let primary_data = test_font("Primary Test", 'A');
        let fallback_data = test_font("Fallback Test", '中');
        let fallback_fonts = vec![primary_data.clone(), fallback_data];
        let font_resource = Font {
            id: 42,
            font_name: "Primary Test".into(),
            ..Default::default()
        };
        let mut probe =
            FontResolver::with_bundled(std::slice::from_ref(&font_resource), &fallback_fonts);
        let primary = probe.resolve(42).unwrap();
        let primary_face = Face::parse(&primary.data, primary.index).unwrap();
        assert!(primary_face.glyph_index('中').is_none());
        let fallback = probe
            .fallback_for_char_styled(42, 400, false, '中')
            .unwrap();
        assert!(Face::parse(&fallback.data, fallback.index)
            .unwrap()
            .glyph_index('中')
            .is_some());

        let text = GraphicObject::Text(TextObject {
            common: GraphicCommon {
                id: 1,
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                ..Default::default()
            },
            font_id: 42,
            font_size: 6.0,
            stroke: false,
            fill: true,
            h_scale: 1.0,
            read_direction: Direction(0),
            char_direction: Direction(0),
            weight: 400,
            italic: false,
            fill_color: Some(Color::BLACK.into()),
            stroke_color: None,
            cg_transforms: Vec::new(),
            runs: vec![TextRun {
                text: "中".into(),
                origin_x: 2.0,
                origin_y: 7.0,
                delta_x: vec![6.0],
                delta_y: Vec::new(),
            }],
        });
        let doc = doc_with_objects(
            vec![text],
            Resources {
                fonts: vec![font_resource],
                ..Default::default()
            },
        );
        let non_white = |bitmap: &Bitmap| {
            bitmap
                .rgba
                .chunks_exact(4)
                .filter(|pixel| pixel[..3] != [255, 255, 255])
                .count()
        };
        let primary_only = render_page_with(
            &doc,
            0,
            96.0,
            &RenderOptions {
                fallback_fonts: vec![primary_data],
                ..Default::default()
            },
        )
        .unwrap();
        let with_fallback = render_page_with(
            &doc,
            0,
            96.0,
            &RenderOptions {
                fallback_fonts,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(non_white(&primary_only), 0);
        assert!(non_white(&with_fallback) > 0);
    }

    #[test]
    fn full_page_mask_work_is_cumulatively_limited() {
        let error = render_page_with(
            &one_path_doc(true),
            0,
            96.0,
            &RenderOptions {
                limits: RenderLimits {
                    max_mask_pixels: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(matches!(error, OfdError::ResourceLimit(_)));
    }

    #[test]
    fn image_working_set_limit_accounts_for_coexisting_conversion_buffers() {
        let limits = RenderLimits {
            max_image_pixels: 1,
            max_image_bytes: 4,
            max_image_working_bytes: 11,
            ..Default::default()
        };
        let error = ensure_image_size(1, 1, &limits).unwrap_err();
        assert!(matches!(error, OfdError::ResourceLimit(_)));
        assert!(error.to_string().contains("working bytes"));
    }

    #[test]
    fn non_finite_public_path_geometry_is_not_forwarded_to_tiny_skia() {
        assert!(sk_path(&[
            PathCommand::MoveTo { x: 0.0, y: 0.0 },
            PathCommand::LineTo {
                x: f32::NAN,
                y: 1.0,
            },
        ])
        .is_none());
    }

    #[test]
    fn embedded_ofd_budgets_are_cumulative_and_preallocate_safe() {
        let budget = Rc::new(RenderBudget::default());
        budget.charge_embedded_bytes(3, 5).unwrap();
        assert!(matches!(
            budget.charge_embedded_bytes(3, 5),
            Err(OfdError::ResourceLimit(_))
        ));

        let doc = doc_with_objects(Vec::new(), Resources::default());
        let mut session = RenderSession::new(
            &doc,
            RenderOptions {
                limits: RenderLimits {
                    max_embedded_ofd_pixels: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        assert!(matches!(
            session.render_page_with_budget(0, 96.0, budget, true),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn render_session_reuses_state_across_pages() {
        let doc = one_path_doc(true);
        let mut session = RenderSession::new(&doc, RenderOptions::default());
        let first = session.render_page(0, 96.0).unwrap();
        let second = session.render_page(0, 96.0).unwrap();
        assert_eq!((first.width, first.height), (second.width, second.height));
        assert_eq!(first.rgba, second.rgba);
    }

    #[test]
    fn transparent_bitmap_exposes_straight_alpha_rgba() {
        let path = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                ..Default::default()
            },
            Some(OfdColor::Basic(BasicColor {
                components: Some(vec![255.0, 0.0, 0.0]),
                index: None,
                color_space: None,
                alpha: 128,
            })),
        );
        let bitmap = render_page_with(
            &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
            0,
            96.0,
            &RenderOptions {
                transparent_background: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            pixel(&bitmap, bitmap.width / 2, bitmap.height / 2),
            [255, 0, 0, 128]
        );
    }

    #[test]
    fn palette_index_resolves_through_color_space() {
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
            ..Default::default()
        };
        let path = rect_path(
            common,
            Some(OfdColor::Basic(BasicColor {
                components: None,
                index: Some(1),
                color_space: Some(7),
                alpha: 255,
            })),
        );
        let resources = Resources {
            color_spaces: vec![ColorSpace {
                id: 7,
                kind: ColorSpaceKind::Rgb,
                bits_per_component: 8,
                palette: vec![vec![255.0, 0.0, 0.0], vec![0.0, 180.0, 0.0]],
                profile: None,
            }],
            ..Default::default()
        };
        let b = render_page(
            &doc_with_objects(vec![GraphicObject::Path(path)], resources),
            0,
            96.0,
        )
        .unwrap();
        let p = pixel(&b, b.width / 2, b.height / 2);
        assert!(
            p[1] > p[0] && p[1] > p[2],
            "palette index should paint green, got {p:?}"
        );
    }

    #[test]
    fn explicit_color_value_takes_precedence_over_palette_index() {
        let path = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                ..Default::default()
            },
            Some(OfdColor::Basic(BasicColor {
                components: Some(vec![255.0, 0.0, 0.0]),
                index: Some(0),
                color_space: Some(7),
                alpha: 255,
            })),
        );
        let resources = Resources {
            color_spaces: vec![ColorSpace {
                id: 7,
                kind: ColorSpaceKind::Rgb,
                bits_per_component: 8,
                palette: vec![vec![0.0, 255.0, 0.0]],
                profile: None,
            }],
            ..Default::default()
        };
        let bitmap = render_page(
            &doc_with_objects(vec![GraphicObject::Path(path)], resources),
            0,
            96.0,
        )
        .unwrap();
        let actual = pixel(&bitmap, bitmap.width / 2, bitmap.height / 2);
        assert!(
            actual[0] > 240 && actual[1] < 10,
            "expected Value red, got {actual:?}"
        );
    }

    #[test]
    fn icc_profile_converts_palette_components_to_srgb() {
        let source_profile = moxcms::ColorProfile::new_adobe_rgb();
        let profile_data = source_profile.encode().unwrap();
        let mut expected = [0u8; 3];
        source_profile
            .create_transform_8bit(
                moxcms::Layout::Rgb,
                &moxcms::ColorProfile::new_srgb(),
                moxcms::Layout::Rgb,
                moxcms::TransformOptions::default(),
            )
            .unwrap()
            .transform(&[64, 128, 192], &mut expected)
            .unwrap();
        assert_ne!(expected, [64, 128, 192]);

        let path = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                ..Default::default()
            },
            Some(OfdColor::Basic(BasicColor {
                components: None,
                index: Some(0),
                color_space: Some(7),
                alpha: 255,
            })),
        );
        let resources = Resources {
            color_spaces: vec![ColorSpace {
                id: 7,
                kind: ColorSpaceKind::Rgb,
                bits_per_component: 8,
                palette: vec![vec![64.0, 128.0, 192.0]],
                profile: Some(IccProfile {
                    location: "Profiles/adobe.icc".into(),
                    data: Arc::new(profile_data),
                }),
            }],
            ..Default::default()
        };
        let bitmap = render_page(
            &doc_with_objects(vec![GraphicObject::Path(path)], resources),
            0,
            96.0,
        )
        .unwrap();
        let actual = pixel(&bitmap, bitmap.width / 2, bitmap.height / 2);
        assert_eq!(&actual[..3], &expected);
    }

    #[test]
    fn missing_gradient_positions_are_distributed_between_known_neighbors() {
        let segment = |position| GradientSegment {
            position,
            color: BasicColor {
                components: Some(vec![0.0, 0.0, 0.0]),
                index: None,
                color_space: None,
                alpha: 255,
            },
        };
        let positions = gradient_positions(&[
            segment(None),
            segment(None),
            segment(Some(0.6)),
            segment(None),
            segment(None),
        ]);
        assert_eq!(positions, vec![0.0, 0.3, 0.6, 0.8, 1.0]);
    }

    #[test]
    fn gradient_extend_bits_control_each_end_of_the_original_domain() {
        for extend in 0..=3 {
            assert_eq!(
                gradient_parameter(-0.25, -0.25, GradientMapType::Direct, extend),
                (extend & 1 != 0).then_some(0.0),
                "start extension for Extend={extend}"
            );
            assert_eq!(
                gradient_parameter(1.25, 1.25, GradientMapType::Direct, extend),
                (extend & 2 != 0).then_some(1.0),
                "end extension for Extend={extend}"
            );
        }
    }

    #[test]
    fn repeat_and_reflect_do_not_escape_an_unextended_gradient_domain() {
        assert_eq!(
            gradient_parameter(0.75, 1.75, GradientMapType::Repeat, 0),
            Some(0.75)
        );
        assert_eq!(
            gradient_parameter(0.75, 1.75, GradientMapType::Reflect, 0),
            Some(0.25)
        );
        assert_eq!(
            gradient_parameter(1.25, 1.25, GradientMapType::Repeat, 0),
            None
        );
        assert_eq!(
            gradient_parameter(-0.25, -0.25, GradientMapType::Reflect, 0),
            None
        );
        assert_eq!(
            gradient_parameter(1.25, 1.25, GradientMapType::Repeat, 2),
            Some(0.25)
        );
        assert_eq!(
            gradient_parameter(-0.25, -0.25, GradientMapType::Reflect, 1),
            Some(0.25)
        );
    }

    #[test]
    fn axial_gradient_transitions_across_path() {
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
            ..Default::default()
        };
        let path = rect_path(
            common,
            Some(OfdColor::Axial(AxialGradient {
                alpha: 255,
                map_type: GradientMapType::Direct,
                map_unit: None,
                extend: 3,
                start: crate::geom::Point::new(0.0, 0.0),
                end: crate::geom::Point::new(20.0, 0.0),
                segments: vec![
                    GradientSegment {
                        position: Some(0.0),
                        color: BasicColor {
                            components: Some(vec![255.0, 0.0, 0.0]),
                            index: None,
                            color_space: None,
                            alpha: 255,
                        },
                    },
                    GradientSegment {
                        position: Some(1.0),
                        color: BasicColor {
                            components: Some(vec![0.0, 0.0, 255.0]),
                            index: None,
                            color_space: None,
                            alpha: 255,
                        },
                    },
                ],
            })),
        );
        let b = render_page(
            &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
            0,
            96.0,
        )
        .unwrap();
        let left = pixel(&b, 2, b.height / 2);
        let right = pixel(&b, b.width - 3, b.height / 2);
        assert!(
            left[0] > left[2],
            "left side should be red-ish, got {left:?}"
        );
        assert!(
            right[2] > right[0],
            "right side should be blue-ish, got {right:?}"
        );
    }

    #[test]
    fn complex_gradient_alpha_multiplies_the_segment_alpha() {
        let red = BasicColor {
            components: Some(vec![255.0, 0.0, 0.0]),
            index: None,
            color_space: None,
            alpha: 128,
        };
        let path = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                alpha: 128,
                ..Default::default()
            },
            Some(OfdColor::Axial(AxialGradient {
                alpha: 128,
                map_type: GradientMapType::Direct,
                map_unit: None,
                extend: 3,
                start: crate::geom::Point::new(0.0, 0.0),
                end: crate::geom::Point::new(20.0, 0.0),
                segments: vec![
                    GradientSegment {
                        position: Some(0.0),
                        color: red.clone(),
                    },
                    GradientSegment {
                        position: Some(1.0),
                        color: red,
                    },
                ],
            })),
        );
        let bitmap = render_page_with(
            &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
            0,
            96.0,
            &RenderOptions {
                transparent_background: true,
                ..Default::default()
            },
        )
        .unwrap();
        let actual = pixel(&bitmap, bitmap.width / 2, bitmap.height / 2);
        assert!(
            actual[0] > 240 && actual[1] < 10,
            "straight RGBA expected: {actual:?}"
        );
        assert!(
            (30..=33).contains(&actual[3]),
            "three half-alpha factors: {actual:?}"
        );
    }

    #[test]
    fn radial_gradient_honors_nonzero_start_radius_and_extend() {
        let segment = |position, components| GradientSegment {
            position: Some(position),
            color: BasicColor {
                components: Some(components),
                index: None,
                color_space: None,
                alpha: 255,
            },
        };
        let make_path = |extend| {
            rect_path(
                GraphicCommon {
                    boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                    ..Default::default()
                },
                Some(OfdColor::Radial(RadialGradient {
                    alpha: 255,
                    map_type: GradientMapType::Direct,
                    map_unit: None,
                    eccentricity: 0.0,
                    angle: 0.0,
                    start: crate::geom::Point::new(10.0, 5.0),
                    end: crate::geom::Point::new(10.0, 5.0),
                    start_radius: 3.0,
                    end_radius: 8.0,
                    extend,
                    segments: vec![
                        segment(0.0, vec![255.0, 0.0, 0.0]),
                        segment(1.0, vec![0.0, 0.0, 255.0]),
                    ],
                })),
            )
        };

        let no_extend = render_page(
            &doc_with_objects(
                vec![GraphicObject::Path(make_path(0))],
                Resources::default(),
            ),
            0,
            96.0,
        )
        .unwrap();
        let center = pixel_at_mm(&no_extend, 10.0, 5.0);
        assert!(
            center[..3].iter().all(|channel| *channel > 245),
            "start-radius hole: {center:?}"
        );
        let ring = pixel_at_mm(&no_extend, 14.0, 5.0);
        assert!(
            ring[0] > ring[2],
            "point inside the radial domain: {ring:?}"
        );

        let extend_start = render_page(
            &doc_with_objects(
                vec![GraphicObject::Path(make_path(1))],
                Resources::default(),
            ),
            0,
            96.0,
        )
        .unwrap();
        let center = pixel_at_mm(&extend_start, 10.0, 5.0);
        assert!(
            center[0] > 240 && center[1] < 15 && center[2] < 15,
            "extended start color: {center:?}"
        );
    }

    #[test]
    fn pattern_tiles_cell_content() {
        let cell_common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 2.0, 2.0),
            ..Default::default()
        };
        let cell = rect_path(cell_common, Some(Color::BLACK.into()));
        let common = GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
            ..Default::default()
        };
        let path = rect_path(
            common,
            Some(OfdColor::Pattern(PatternColor {
                alpha: 255,
                width: 2.0,
                height: 2.0,
                x_step: 4.0,
                y_step: 4.0,
                reflect: PatternReflect::Normal,
                relative_to: PatternRelativeTo::Object,
                ctm: Matrix::IDENTITY,
                cell_content: vec![GraphicObject::Path(cell)],
                thumbnail: None,
            })),
        );
        let b = render_page(
            &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
            0,
            96.0,
        )
        .unwrap();
        let blackish = b
            .rgba
            .chunks_exact(4)
            .filter(|p| p[0] < 40 && p[1] < 40 && p[2] < 40)
            .count();
        let whiteish = b
            .rgba
            .chunks_exact(4)
            .filter(|p| p[0] > 220 && p[1] > 220 && p[2] > 220)
            .count();
        assert!(
            blackish > 20 && whiteish > 20,
            "pattern should create alternating painted and empty cells"
        );
    }

    #[test]
    fn complex_pattern_alpha_is_applied_to_the_complete_tiled_paint() {
        let cell = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                ..Default::default()
            },
            Some(Color::BLACK.into()),
        );
        let path = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                alpha: 128,
                ..Default::default()
            },
            Some(OfdColor::Pattern(PatternColor {
                alpha: 128,
                width: 20.0,
                height: 10.0,
                x_step: 20.0,
                y_step: 10.0,
                reflect: PatternReflect::Normal,
                relative_to: PatternRelativeTo::Object,
                ctm: Matrix::IDENTITY,
                cell_content: vec![GraphicObject::Path(cell)],
                thumbnail: None,
            })),
        );
        let bitmap = render_page_with(
            &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
            0,
            96.0,
            &RenderOptions {
                transparent_background: true,
                ..Default::default()
            },
        )
        .unwrap();
        let actual = pixel(&bitmap, bitmap.width / 2, bitmap.height / 2);
        assert_eq!(&actual[..3], &[0, 0, 0]);
        assert!(
            (62..=65).contains(&actual[3]),
            "two half-alpha factors: {actual:?}"
        );
    }

    #[test]
    fn pattern_ctm_transforms_the_tile_grid_without_transforming_cell_content() {
        let cell = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 1.0, 1.0),
                ..Default::default()
            },
            Some(Color::BLACK.into()),
        );
        let mut pattern = PatternColor {
            alpha: 255,
            width: 1.0,
            height: 1.0,
            x_step: 2.0,
            y_step: 2.0,
            reflect: PatternReflect::Normal,
            relative_to: PatternRelativeTo::Object,
            ctm: Matrix::IDENTITY,
            cell_content: vec![GraphicObject::Path(cell)],
            thumbnail: None,
        };
        let doc = doc_with_objects(Vec::new(), Resources::default());
        let mut session = RenderSession::new(&doc, RenderOptions::default());
        let scale = 96.0 / crate::geom::MM_PER_INCH;
        let mut ctx = RenderCtx {
            session: &mut session,
            frame: RenderFrame {
                base: Transform::from_scale(scale, scale),
                origin: (0.0, 0.0),
                size: (100, 100),
                dpi: 96.0,
            },
            composite_stack: Vec::new(),
            composite_surface_pixels: 0,
            pattern_depth: 0,
            pattern_surface_pixels: 0,
            budget: Rc::new(RenderBudget::default()),
        };
        let (identity_tile, identity_transform) = ctx
            .pattern_tile(&pattern, &GraphicCommon::default())
            .unwrap()
            .unwrap();
        pattern.ctm = Matrix::new(2.0, 0.0, 0.0, 2.0, 0.0, 0.0);
        let (scaled_tile, scaled_transform) = ctx
            .pattern_tile(&pattern, &GraphicCommon::default())
            .unwrap()
            .unwrap();

        assert_eq!(
            (identity_tile.width(), identity_tile.height()),
            (scaled_tile.width(), scaled_tile.height())
        );
        assert_eq!(identity_tile.data(), scaled_tile.data());
        assert!((scaled_transform.sx / identity_transform.sx - 2.0).abs() < 1.0e-5);
        assert!((scaled_transform.sy / identity_transform.sy - 2.0).abs() < 1.0e-5);
    }

    #[test]
    fn pattern_nesting_obeys_the_render_limit() {
        let doc = doc_with_objects(Vec::new(), Resources::default());
        let mut session = RenderSession::new(
            &doc,
            RenderOptions {
                strict: true,
                limits: RenderLimits {
                    max_pattern_depth: 1,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut ctx = RenderCtx {
            session: &mut session,
            frame: RenderFrame {
                base: Transform::identity(),
                origin: (0.0, 0.0),
                size: (10, 10),
                dpi: 96.0,
            },
            composite_stack: Vec::new(),
            composite_surface_pixels: 0,
            pattern_depth: 1,
            pattern_surface_pixels: 0,
            budget: Rc::new(RenderBudget::default()),
        };
        let pattern = PatternColor {
            alpha: 255,
            width: 1.0,
            height: 1.0,
            x_step: 1.0,
            y_step: 1.0,
            reflect: PatternReflect::Normal,
            relative_to: PatternRelativeTo::Object,
            ctm: Matrix::IDENTITY,
            cell_content: Vec::new(),
            thumbnail: None,
        };
        assert!(matches!(
            ctx.pattern_tile(&pattern, &GraphicCommon::default()),
            Err(OfdError::Render(_))
        ));
    }

    #[test]
    fn reflected_pattern_peak_surface_obeys_the_render_limit() {
        let doc = doc_with_objects(Vec::new(), Resources::default());
        let mut session = RenderSession::new(
            &doc,
            RenderOptions {
                strict: true,
                limits: RenderLimits {
                    // At 96 DPI a 1 mm step makes a 4x4 (16 pixel) base tile.
                    // Row+column reflection also allocates an 8x8 output while
                    // that base tile is live, for an 80 pixel peak.
                    max_pattern_surface_pixels: 16,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let mut ctx = RenderCtx {
            session: &mut session,
            frame: RenderFrame {
                base: Transform::identity(),
                origin: (0.0, 0.0),
                size: (10, 10),
                dpi: 96.0,
            },
            composite_stack: Vec::new(),
            composite_surface_pixels: 0,
            pattern_depth: 0,
            pattern_surface_pixels: 0,
            budget: Rc::new(RenderBudget::default()),
        };
        let pattern = PatternColor {
            alpha: 255,
            width: 1.0,
            height: 1.0,
            x_step: 1.0,
            y_step: 1.0,
            reflect: PatternReflect::RowAndColumn,
            relative_to: PatternRelativeTo::Object,
            ctm: Matrix::IDENTITY,
            cell_content: Vec::new(),
            thumbnail: None,
        };
        assert!(matches!(
            ctx.pattern_tile(&pattern, &GraphicCommon::default()),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn gouraud_triangle_interpolates_colors() {
        let Some(mut pm) = Pixmap::new(20, 20) else {
            panic!("pixmap")
        };
        let points = vec![
            GouraudPoint {
                x: 0.0,
                y: 0.0,
                edge_flag: None,
                color: BasicColor {
                    components: Some(vec![255.0, 0.0, 0.0]),
                    index: None,
                    color_space: None,
                    alpha: 255,
                },
            },
            GouraudPoint {
                x: 20.0,
                y: 0.0,
                edge_flag: None,
                color: BasicColor {
                    components: Some(vec![0.0, 255.0, 0.0]),
                    index: None,
                    color_space: None,
                    alpha: 255,
                },
            },
            GouraudPoint {
                x: 0.0,
                y: 20.0,
                edge_flag: None,
                color: BasicColor {
                    components: Some(vec![0.0, 0.0, 255.0]),
                    index: None,
                    color_space: None,
                    alpha: 255,
                },
            },
        ];
        fill_gouraud_pixmap(
            &mut pm,
            &points,
            GouraudRasterOptions {
                vertices_per_row: 0,
                background: Color {
                    r: 0,
                    g: 0,
                    b: 0,
                    a: 0,
                },
                alpha: 255,
                origin: (0.0, 0.0),
                scale: 1.0,
            },
            |b| resolve_basic_color(b, ColorSpaceKind::Rgb, 8),
        );
        let center = pm.pixel(5, 5).unwrap();
        assert!(center.alpha() > 0, "triangle interior should be painted");
        assert!(
            center.red() > 0 && center.green() > 0 && center.blue() > 0,
            "interior should interpolate RGB"
        );
    }

    #[test]
    fn free_gouraud_edge_flags_follow_figure_40() {
        let previous = [0, 1, 2];
        assert_eq!(
            next_free_gouraud_triangle(previous, 3, Some(0), 6),
            Some(([3, 4, 5], 6))
        );
        assert_eq!(
            next_free_gouraud_triangle(previous, 3, Some(1), 4),
            Some(([1, 2, 3], 4))
        );
        assert_eq!(
            next_free_gouraud_triangle(previous, 3, Some(2), 4),
            Some(([0, 2, 3], 4))
        );
        assert_eq!(next_free_gouraud_triangle(previous, 3, None, 5), None);
    }

    #[test]
    fn gouraud_back_color_is_used_only_when_extend_is_true() {
        let color = |r, g, b| BasicColor {
            components: Some(vec![r, g, b]),
            index: None,
            color_space: None,
            alpha: 255,
        };
        let make_path = |extend| {
            rect_path(
                GraphicCommon {
                    boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                    ..Default::default()
                },
                Some(OfdColor::Gouraud(GouraudGradient {
                    alpha: 255,
                    extend,
                    points: vec![
                        GouraudPoint {
                            x: 0.0,
                            y: 0.0,
                            edge_flag: Some(0),
                            color: color(255.0, 0.0, 0.0),
                        },
                        GouraudPoint {
                            x: 5.0,
                            y: 0.0,
                            edge_flag: None,
                            color: color(255.0, 0.0, 0.0),
                        },
                        GouraudPoint {
                            x: 0.0,
                            y: 5.0,
                            edge_flag: None,
                            color: color(255.0, 0.0, 0.0),
                        },
                    ],
                    back_color: Some(color(0.0, 255.0, 0.0)),
                })),
            )
        };
        let without_extend = render_page(
            &doc_with_objects(
                vec![GraphicObject::Path(make_path(false))],
                Resources::default(),
            ),
            0,
            96.0,
        )
        .unwrap();
        let outside = pixel_at_mm(&without_extend, 18.0, 8.0);
        assert!(
            outside[..3].iter().all(|channel| *channel > 245),
            "no back color: {outside:?}"
        );

        let with_extend = render_page(
            &doc_with_objects(
                vec![GraphicObject::Path(make_path(true))],
                Resources::default(),
            ),
            0,
            96.0,
        )
        .unwrap();
        let outside = pixel_at_mm(&with_extend, 18.0, 8.0);
        assert!(
            outside[1] > 240 && outside[0] < 15 && outside[2] < 15,
            "extended back color: {outside:?}"
        );
    }

    #[test]
    fn gouraud_raster_work_obeys_the_cumulative_limit() {
        let color = |r, g, b| BasicColor {
            components: Some(vec![r, g, b]),
            index: None,
            color_space: None,
            alpha: 255,
        };
        let mut path = rect_path(
            GraphicCommon {
                boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
                ..Default::default()
            },
            None,
        );
        path.fill_color = Some(OfdColor::Gouraud(GouraudGradient {
            alpha: 255,
            extend: false,
            points: vec![
                GouraudPoint {
                    x: 0.0,
                    y: 0.0,
                    edge_flag: None,
                    color: color(255.0, 0.0, 0.0),
                },
                GouraudPoint {
                    x: 20.0,
                    y: 0.0,
                    edge_flag: None,
                    color: color(0.0, 255.0, 0.0),
                },
                GouraudPoint {
                    x: 0.0,
                    y: 10.0,
                    edge_flag: None,
                    color: color(0.0, 0.0, 255.0),
                },
            ],
            back_color: None,
        }));
        let options = RenderOptions {
            limits: RenderLimits {
                max_gouraud_raster_pixels: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(matches!(
            render_page_with(
                &doc_with_objects(vec![GraphicObject::Path(path)], Resources::default()),
                0,
                96.0,
                &options,
            ),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn cg_transform_spans_expand_to_glyph_slots() {
        // A many-to-one (2 codes → 1 glyph) span at code 0, and a one-to-many
        // (1 code → 2 glyphs) span at code 2.
        let transforms = vec![
            CgTransform {
                code_position: 0,
                code_count: 2,
                glyphs: vec![42],
                glyph_count: 1,
            },
            CgTransform {
                code_position: 2,
                code_count: 1,
                glyphs: vec![7, 8],
                glyph_count: 2,
            },
        ];
        // Layout lands on span starts at code 0 and code 2; code 1 is absorbed.
        let s0 = cg_span_at(&transforms, 0).unwrap();
        assert_eq!(s0.code_count, 2);
        let slots0 = cg_span_slots(s0, None, usize::MAX);
        assert_eq!(slots0.len(), 1); // many-to-one: one glyph slot
        assert_eq!(slots0[0].gid.0, 42);

        assert!(cg_span_at(&transforms, 1).is_none()); // absorbed code, no span starts here

        let s2 = cg_span_at(&transforms, 2).unwrap();
        let slots2 = cg_span_slots(s2, None, usize::MAX);
        assert_eq!(slots2.len(), 2); // one-to-many: two glyph slots
        assert_eq!((slots2[0].gid.0, slots2[1].gid.0), (7, 8));

        let index = cg_transform_index(&transforms);
        assert_eq!(index.get(&0).unwrap().code_count, 2);
        assert!(!index.contains_key(&1));
        assert_eq!(index.get(&2).unwrap().glyph_count, 2);
    }

    #[test]
    fn cg_span_slots_preserve_count_and_map_cids() {
        // many-to-many (2 codes → 3 glyphs); a CID→GID table remaps ids and a
        // missing/zero glyph keeps its slot but is not drawn.
        let cg = CgTransform {
            code_position: 0,
            code_count: 2,
            glyphs: vec![94, 76, 88],
            glyph_count: 3,
        };
        let map: HashMap<u16, u16> = [(94u16, 940u16), (88, 0)].into_iter().collect();
        let slots = cg_span_slots(&cg, Some(&map), usize::MAX);
        assert_eq!(slots.len(), 3); // slot count == GlyphCount, for DeltaX alignment
        assert_eq!(slots[0].gid.0, 940); // remapped via CID→GID
        assert!(slots[0].draw && slots[1].draw);
        assert_eq!(slots[2].gid.0, 0);
        assert!(!slots[2].draw); // .notdef keeps its slot but is not drawn

        let explicit_count_wins = CgTransform {
            code_position: 0,
            code_count: 1,
            glyphs: vec![1, 2],
            glyph_count: 1,
        };
        assert_eq!(
            cg_span_slots(&explicit_count_wins, None, usize::MAX).len(),
            1
        );
        let zero_glyphs = CgTransform {
            glyph_count: 0,
            ..explicit_count_wins
        };
        assert!(cg_span_slots(&zero_glyphs, None, usize::MAX).is_empty());
    }

    #[test]
    fn missing_cmap_glyph_keeps_an_undrawn_position_slot() {
        let slot = cmap_slot(None, 'x');
        assert_eq!(slot.gid.0, 0);
        assert!(!slot.draw);
    }

    #[test]
    fn cg_span_expansion_obeys_the_render_limit() {
        let transform = CgTransform {
            code_position: 0,
            code_count: 1,
            glyphs: vec![1],
            glyph_count: usize::MAX,
        };
        assert_eq!(cg_span_slots(&transform, None, 8).len(), 8);
    }

    #[test]
    fn cg_span_crossing_text_codes_does_not_render_covered_codes_twice() {
        let transforms = vec![CgTransform {
            code_position: 0,
            code_count: 2,
            glyphs: vec![42],
            glyph_count: 1,
        }];
        let index = cg_transform_index(&transforms);
        let mut covered_until = 0;
        let mut cmap_calls = 0;

        let first = glyph_slots_for_run(&['a'], 0, &mut covered_until, &index, true, None, |_| {
            cmap_calls += 1;
            GlyphSlot {
                gid: ttf_parser::GlyphId(7),
                draw: true,
                face_index: 0,
            }
        });
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].gid.0, 42);
        assert_eq!(covered_until, 2);

        let second = glyph_slots_for_run(&['b'], 1, &mut covered_until, &index, true, None, |_| {
            cmap_calls += 1;
            GlyphSlot {
                gid: ttf_parser::GlyphId(8),
                draw: true,
                face_index: 0,
            }
        });
        assert!(second.is_empty());
        assert_eq!(cmap_calls, 0);

        let third = glyph_slots_for_run(&['c'], 2, &mut covered_until, &index, true, None, |_| {
            cmap_calls += 1;
            GlyphSlot {
                gid: ttf_parser::GlyphId(9),
                draw: true,
                face_index: 0,
            }
        });
        assert_eq!(third.len(), 1);
        assert_eq!(third[0].gid.0, 9);
        assert_eq!(cmap_calls, 1);
    }

    #[test]
    fn read_direction_vectors_follow_standard_angles() {
        assert_eq!(
            read_advance_vector(3.0, normalize_direction(Direction(0))),
            (3.0, 0.0)
        );
        assert_eq!(
            read_advance_vector(3.0, normalize_direction(Direction(90))),
            (0.0, 3.0)
        );
        assert_eq!(
            read_advance_vector(3.0, normalize_direction(Direction(180))),
            (-3.0, 0.0)
        );
        assert_eq!(
            read_advance_vector(3.0, normalize_direction(Direction(270))),
            (0.0, -3.0)
        );
    }

    fn embedded_jbig2_segment(number: u32, kind: u8, page: u8, body: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&number.to_be_bytes());
        data.push(kind);
        data.push(0); // no referred-to segments
        data.push(page);
        data.extend_from_slice(&(body.len() as u32).to_be_bytes());
        data.extend_from_slice(body);
        data
    }

    #[test]
    fn jbig2_dimensions_are_limited_before_decoder_allocation() {
        let mut page_info = vec![0u8; 19];
        page_info[0..4].copy_from_slice(&u32::MAX.to_be_bytes());
        page_info[4..8].copy_from_slice(&1u32.to_be_bytes());
        let data = embedded_jbig2_segment(1, 48, 1, &page_info);
        assert!(matches!(
            decode_jbig2(&data, &RenderLimits::default(), None),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn jbig2_reference_count_is_limited_before_header_allocation() {
        let limits = RenderLimits::default();
        let count = limits.max_jbig2_items as u32 + 1;
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.push(0); // SymbolDictionary
        data.extend_from_slice(&(0xe000_0000 | count).to_be_bytes());
        assert!(matches!(
            decode_jbig2(&data, &limits, None),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn jbig2_repeated_page_ids_still_share_the_total_pixel_budget() {
        let mut page_info = vec![0u8; 19];
        page_info[0..4].copy_from_slice(&1u32.to_be_bytes());
        page_info[4..8].copy_from_slice(&1u32.to_be_bytes());
        let mut data = embedded_jbig2_segment(1, 48, 1, &page_info);
        data.extend_from_slice(&embedded_jbig2_segment(2, 48, 1, &page_info));
        let limits = RenderLimits {
            max_image_pixels: 1,
            ..Default::default()
        };
        assert!(matches!(
            decode_jbig2(&data, &limits, None),
            Err(OfdError::ResourceLimit(_))
        ));
    }
}
