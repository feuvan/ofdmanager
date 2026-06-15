//! Rasterizing renderer. Walks a [`Document`] page's layers in z-order and
//! paints them into an RGBA bitmap via `tiny-skia`. The same code path serves
//! on-screen display (bitmap → canvas) and image export.
//!
//! Coordinate flow: object-space (mm) → page-space (mm) via `Boundary` + `CTM`
//! → device pixels via `dpi/25.4`. Glyph outlines are baked into object-space mm
//! before the page transform is applied.

use std::collections::HashMap;

use tiny_skia::{
    FillRule as SkFillRule, FilterQuality, GradientStop, LineCap as SkLineCap,
    LineJoin as SkLineJoin, LinearGradient as SkLinearGradient, Mask, Paint, Path as SkPath,
    PathBuilder, Pattern as SkPattern, Pixmap, PixmapPaint, Point as SkPoint,
    RadialGradient as SkRadialGradient, Shader, SpreadMode, Stroke, StrokeDash, Transform,
};
use ttf_parser::{Face, OutlineBuilder};

use crate::error::{OfdError, Result};
use crate::fonts::FontResolver;
use crate::geom::{Matrix, Rect};
use crate::model::*;

/// Default text stem-darkening (fraction of font size). Tuned so CJK text
/// weight matches the reference renderer; see the golden fixtures.
pub const DEFAULT_STEM_DARKENING: f32 = 0.0;

/// A rendered page as tightly-packed RGBA8 pixels (row-major, premultiplied
/// alpha matching `tiny-skia`'s output), suitable for `putImageData`.
#[derive(Debug, Clone)]
pub struct Bitmap {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Options controlling a render.
#[derive(Clone, Default)]
pub struct RenderOptions {
    /// Raw TTF/OTF bytes of deterministic fallback fonts (e.g. the Windows core
    /// CJK fonts). Preferred over system fonts when a document's font is not
    /// embedded, so non-embedded text matches major implementations.
    pub fallback_fonts: Vec<Vec<u8>>,
    /// Leave the page background transparent instead of filling it white. Used
    /// when rendering a vector seal's embedded OFD so it composites over the
    /// host page.
    pub transparent_background: bool,
    /// Stem-darkening for text, as a fraction of the font size. Filled glyphs
    /// are additionally stroked by this width so thin CJK strokes don't render
    /// lighter than system/commercial readers (which darken stems). 0 disables.
    pub text_stem_darkening: f32,
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
    color_spaces: HashMap<u64, ColorSpace>,
    default_color_space: Option<u64>,
    fallback_fonts: Vec<Vec<u8>>,
    transparent_background: bool,
    stem_darkening: f32,
    decoded_images: HashMap<u64, Option<image::RgbaImage>>,
}

impl<'a> RenderSession<'a> {
    /// Build a reusable render session for a parsed document.
    pub fn new(doc: &'a Document, opts: RenderOptions) -> Self {
        Self {
            doc,
            fonts: FontResolver::with_bundled(&doc.resources.fonts, &opts.fallback_fonts),
            draw_params: doc
                .resources
                .draw_params
                .iter()
                .map(|d| (d.id, d.clone()))
                .collect(),
            images: doc.resources.images.iter().map(|m| (m.id, m)).collect(),
            color_spaces: doc
                .resources
                .color_spaces
                .iter()
                .map(|c| (c.id, c.clone()))
                .collect(),
            default_color_space: doc.default_color_space,
            fallback_fonts: opts.fallback_fonts,
            transparent_background: opts.transparent_background,
            stem_darkening: opts.text_stem_darkening.max(0.0),
            decoded_images: HashMap::new(),
        }
    }

    /// Render a page with this session, reusing cached document resources.
    pub fn render_page(&mut self, page_index: usize, dpi: f32) -> Result<Bitmap> {
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

        let scale = dpi / crate::geom::MM_PER_INCH;
        let width = (area.w * scale).round().max(1.0) as u32;
        let height = (area.h * scale).round().max(1.0) as u32;

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
        };

        // Paint layers in z-order: Background, then Body, then Foreground, Custom.
        for kind in [
            LayerKind::Background,
            LayerKind::Body,
            LayerKind::Foreground,
            LayerKind::Custom,
        ] {
            for layer in page.layers.iter().filter(|l| l.kind == kind) {
                for obj in &layer.objects {
                    ctx.paint_object(&mut pixmap, obj);
                }
            }
        }

        // Page annotations (watermarks, stamps) over the content.
        for annot in doc.annotations.iter().filter(|a| a.page_id == page.id) {
            for obj in &annot.objects {
                ctx.paint_object(&mut pixmap, obj);
            }
        }

        // Electronic seal stamps placed on this page (drawn on top).
        for seal in doc.seals.iter().filter(|s| s.page_id == page.id) {
            ctx.paint_seal(&mut pixmap, seal);
        }

        Ok(Bitmap {
            width,
            height,
            rgba: pixmap.take(),
        })
    }

    fn decoded_image_rgba(&mut self, resource_id: u64) -> Option<&image::RgbaImage> {
        if !self.decoded_images.contains_key(&resource_id) {
            let decoded = self
                .images
                .get(&resource_id)
                .and_then(|media| decode_image_rgba(media));
            self.decoded_images.insert(resource_id, decoded);
        }
        self.decoded_images
            .get(&resource_id)
            .and_then(|rgba| rgba.as_ref())
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

    /// Build a clip mask for an object's `Clips`, in the same placement frame as
    /// its content. Returns `None` when the object has no clip (draw unmasked).
    fn clip_mask(&self, common: &GraphicCommon) -> Option<Mask> {
        if common.clip.is_empty() {
            return None;
        }
        let mut mask = Mask::new(self.frame.size.0, self.frame.size.1)?;
        for (i, area) in common.clip.iter().enumerate() {
            let m = Transform::from_row(
                area.ctm.a, area.ctm.b, area.ctm.c, area.ctm.d, area.ctm.e, area.ctm.f,
            );
            // Per GB/T 33190 §8.5: clips use the object-space coordinate system,
            // and the object's CTM is the transform within object space — so the
            // clip goes through the object CTM, then the Area CTM (§8.4) further.
            let transform = self.object_transform(common).pre_concat(m);
            let mut b = PathBuilder::new();
            for cmd in &area.commands {
                push_cmd(&mut b, cmd);
            }
            let Some(path) = b.finish() else { continue };
            // First area establishes the region; further areas intersect it.
            if i == 0 {
                mask.fill_path(&path, SkFillRule::Winding, true, transform);
            } else {
                mask.intersect_path(&path, SkFillRule::Winding, true, transform);
            }
        }
        Some(mask)
    }

    fn paint_object(&mut self, pixmap: &mut Pixmap, obj: &GraphicObject) {
        // Objects marked `Visible="false"` are part of the document but must not
        // be drawn (GB/T 33190 §8.5).
        match obj {
            GraphicObject::Text(t) if t.common.visible => self.paint_text(pixmap, t),
            GraphicObject::Path(p) if p.common.visible => self.paint_path(pixmap, p),
            GraphicObject::Image(i) if i.common.visible => self.paint_image(pixmap, i),
            GraphicObject::Group(g) => {
                for o in g {
                    self.paint_object(pixmap, o);
                }
            }
            _ => {}
        }
    }

    fn paint_text(&mut self, pixmap: &mut Pixmap, t: &TextObject) {
        let Some(rf) = self.session.fonts.resolve(t.font_id) else {
            return;
        };
        let data = rf.data.clone();
        let Ok(face) = Face::parse(&data, rf.index) else {
            return;
        };
        let upem = face.units_per_em() as f32;
        if upem <= 0.0 || t.font_size <= 0.0 {
            return;
        }
        let gscale = t.font_size / upem; // font units -> object mm
        let h_scale = t.h_scale.max(0.0);
        let read_direction = normalize_direction(t.read_direction);
        let char_direction = normalize_direction(t.char_direction);
        let italic_shear = if t.italic { 0.21256 } else { 0.0 };

        let transform = self.object_transform(&t.common);
        let mask = self.clip_mask(&t.common);

        let mut builder = PathBuilder::new();
        let mut global_idx = 0usize;
        for run in &t.runs {
            let chars: Vec<char> = run.text.chars().collect();
            let mut pen_x = run.origin_x;
            let mut pen_y = run.origin_y;
            for (i, ch) in chars.iter().enumerate() {
                // Prefer the font cmap when it resolves. Fall back to CGTransform
                // for subsetted fonts, and honor one-to-many/many-to-one mappings.
                let glyphs = glyphs_for_char(&face, *ch, &t.cg_transforms, global_idx);
                let mut first_gid = None;
                let mut glyph_pen_x = pen_x;
                let mut glyph_pen_y = pen_y;
                for gid in glyphs {
                    first_gid.get_or_insert(gid);
                    if !ch.is_whitespace() {
                        let mut ob = GlyphOutline {
                            builder: &mut builder,
                            pen_x: glyph_pen_x,
                            pen_y: glyph_pen_y,
                            scale: gscale,
                            h_scale,
                            angle: char_direction as f32,
                            italic_shear,
                        };
                        face.outline_glyph(gid, &mut ob);
                    }
                    let adv = glyph_advance(gid, &face, gscale, t.font_size) * h_scale;
                    let (dx, dy) = read_advance_vector(adv, read_direction);
                    glyph_pen_x += dx;
                    glyph_pen_y += dy;
                }
                global_idx += 1;
                // Advance by the explicit OFD delta when present. When the
                // DeltaX list is shorter than the glyph count, repeat its last
                // value (producers emit a single advance for uniform-width CJK
                // runs and rely on the reader to extend it). Fall back to the
                // font's horizontal glyph advance only when the run has *no*
                // explicit positioning on either axis — for vertical text
                // (DeltaY present, DeltaX absent) the horizontal advance is 0.
                let fallback = if run.delta_x.is_empty() && run.delta_y.is_empty() {
                    let gid = first_gid;
                    let adv = gid
                        .map(|g| glyph_advance(g, &face, gscale, t.font_size))
                        .unwrap_or(t.font_size * 0.5);
                    let (dx, dy) = read_advance_vector(adv * h_scale, read_direction);
                    (dx, dy)
                } else {
                    (0.0, 0.0)
                };
                pen_x += advance(&run.delta_x, i, fallback.0);
                pen_y += advance(&run.delta_y, i, fallback.1);
            }
        }
        let Some(path) = builder.finish() else { return };
        let alpha = t.common.alpha;
        if t.fill {
            let fill = t
                .fill_color
                .clone()
                .or_else(|| self.dp_fill(t.common.draw_param))
                .unwrap_or_else(|| Color::BLACK.into());
            let stem_darkening = self.session.stem_darkening;
            let font_size = t.font_size;
            let weight = t.weight;
            self.with_paint_for_color(
                &fill,
                alpha,
                &t.common,
                Some(&path),
                transform,
                Some(SkFillRule::Winding),
                |paint| {
                    pixmap.fill_path(&path, paint, SkFillRule::Winding, transform, mask.as_ref());
                    // Stem-darkening: outline the filled glyphs with a hairline of the
                    // same ink so thin CJK strokes match heavier system rasterizers.
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
                    if weight > 400 {
                        let width = font_size * ((weight.saturating_sub(400)) as f32 / 3000.0);
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
            );
        }
        if t.stroke {
            let stroke = t
                .stroke_color
                .clone()
                .or_else(|| self.dp_stroke(t.common.draw_param))
                .unwrap_or_else(|| Color::BLACK.into());
            let sk = Stroke {
                width: t.common.line_width.max(0.01),
                ..Default::default()
            };
            self.with_paint_for_color(
                &stroke,
                alpha,
                &t.common,
                Some(&path),
                transform,
                None,
                |paint| {
                    pixmap.stroke_path(&path, paint, &sk, transform, mask.as_ref());
                },
            );
        }
    }

    fn paint_path(&mut self, pixmap: &mut Pixmap, p: &PathObject) {
        let mut b = PathBuilder::new();
        for cmd in &p.commands {
            push_cmd(&mut b, cmd);
        }
        let Some(path) = b.finish() else { return };
        let transform = self.object_transform(&p.common);
        let mask = self.clip_mask(&p.common);
        let alpha = p.common.alpha;

        if p.fill {
            // Resolve the fill color: explicit → draw param → black, but only
            // default to black when the path is fill-only. A `Fill="true"` path
            // with no color that is also stroked is an outline mark (e.g. the
            // ⊗ on invoices); black-filling it would hide the strokes.
            let color = p
                .fill_color
                .clone()
                .or_else(|| self.dp_fill(p.common.draw_param))
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
                    &p.common,
                    Some(&path),
                    transform,
                    Some(rule),
                    |paint| {
                        pixmap.fill_path(&path, paint, rule, transform, mask.as_ref());
                    },
                );
            }
        }
        if p.stroke {
            let stroke = p
                .stroke_color
                .clone()
                .or_else(|| self.dp_stroke(p.common.draw_param))
                .unwrap_or_else(|| Color::BLACK.into());
            let width = self
                .dp_line_width(p.common.draw_param)
                .unwrap_or(p.common.line_width)
                .max(0.01);
            let sk = self.stroke_for(&p.common, p.common.draw_param, width);
            self.with_paint_for_color(
                &stroke,
                alpha,
                &p.common,
                Some(&path),
                transform,
                None,
                |paint| {
                    pixmap.stroke_path(&path, paint, &sk, transform, mask.as_ref());
                },
            );
        }
    }

    fn paint_image(&mut self, pixmap: &mut Pixmap, im: &ImageObject) {
        // Image content is a unit square mapped onto the boundary (or by CTM).
        // Source pixels → object mm: scale by boundary size / image size.
        let obj = self.object_transform(&im.common);
        let mask = self.clip_mask(&im.common);
        let Some(rgba) = self.session.decoded_image_rgba(im.resource_id) else {
            return;
        };
        let (iw, ih) = (rgba.width() as f32, rgba.height() as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let to_obj = if im.common.ctm == Matrix::IDENTITY {
            Transform::from_scale(im.common.boundary.w / iw, im.common.boundary.h / ih)
        } else {
            // CTM maps the unit square to object space.
            Transform::from_scale(1.0 / iw, 1.0 / ih)
        };
        let transform = obj.pre_concat(to_obj);
        let paint = PixmapPaint {
            quality: tiny_skia::FilterQuality::Bilinear,
            ..Default::default()
        };

        // tiny-skia's draw_pixmap does not reliably downscale very large source
        // images (a full-page background can be ~3500px wide while its device
        // footprint is ~1000px). Pre-resize the source to its device footprint:
        // this both fixes that and improves downsampling quality.
        let scale_x = (transform.sx * transform.sx + transform.ky * transform.ky).sqrt();
        let scale_y = (transform.kx * transform.kx + transform.sy * transform.sy).sqrt();
        let tw = ((iw * scale_x).round() as u32).clamp(1, 8192);
        let th = ((ih * scale_y).round() as u32).clamp(1, 8192);

        if tw < rgba.width() || th < rgba.height() {
            let small =
                image::imageops::resize(rgba, tw, th, image::imageops::FilterType::Triangle);
            let Some(src) = rgba_to_pixmap(&small) else {
                return;
            };
            // Resized source → device: undo the source-pixel scaling baked above.
            let adj = transform.pre_concat(Transform::from_scale(iw / tw as f32, ih / th as f32));
            pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, adj, mask.as_ref());
        } else if let Some(src) = rgba_to_pixmap(&rgba) {
            pixmap.draw_pixmap(0, 0, src.as_ref(), &paint, transform, mask.as_ref());
        }
    }

    /// Draw an electronic seal's stamp face filling its box. Raster faces are
    /// decoded directly; vector (`ofd`) faces are rendered recursively over a
    /// transparent background so they composite onto the host page.
    fn paint_seal(&mut self, pixmap: &mut Pixmap, seal: &Seal) {
        let src = match &seal.appearance {
            SealAppearance::Raster { format, data } => decode_bytes(*format, data),
            SealAppearance::Ofd(bytes) => self.render_vector_seal(bytes),
        };
        let Some(src) = src else { return };
        let (iw, ih) = (src.width() as f32, src.height() as f32);
        if iw <= 0.0 || ih <= 0.0 {
            return;
        }
        let common = GraphicCommon {
            boundary: seal.boundary,
            ..Default::default()
        };
        let to_obj = Transform::from_scale(seal.boundary.w / iw, seal.boundary.h / ih);
        let transform = self.object_transform(&common).pre_concat(to_obj);
        pixmap.draw_pixmap(0, 0, src.as_ref(), &PixmapPaint::default(), transform, None);
    }

    /// Render a vector seal's embedded OFD (its first page) to a transparent
    /// pixmap at the host resolution.
    fn render_vector_seal(&self, ofd_bytes: &[u8]) -> Option<Pixmap> {
        let pkg = crate::parser::parse(ofd_bytes.to_vec()).ok()?;
        let doc = pkg.documents.first()?;
        let opts = RenderOptions {
            fallback_fonts: self.session.fallback_fonts.clone(),
            transparent_background: true,
            text_stem_darkening: self.session.stem_darkening,
            ..Default::default()
        };
        let bmp = render_page_with(doc, 0, self.frame.dpi, &opts).ok()?;
        let size = tiny_skia::IntSize::from_wh(bmp.width, bmp.height)?;
        Pixmap::from_vec(bmp.rgba, size)
    }

    fn with_paint_for_color<R, F>(
        &mut self,
        color: &OfdColor,
        alpha: u8,
        common: &GraphicCommon,
        path: Option<&SkPath>,
        transform: Transform,
        rule: Option<SkFillRule>,
        draw: F,
    ) -> R
    where
        F: for<'p> FnOnce(&Paint<'p>) -> R,
    {
        match color {
            OfdColor::Basic(c) => {
                let paint = solid(self.resolve_basic(c), alpha);
                draw(&paint)
            }
            OfdColor::Axial(g) => {
                let paint = self
                    .axial_shader(g, alpha, transform)
                    .map(shader_paint)
                    .unwrap_or_else(|| solid(Color::BLACK, alpha));
                draw(&paint)
            }
            OfdColor::Radial(g) => {
                let paint = self
                    .radial_shader(g, alpha, transform)
                    .map(shader_paint)
                    .unwrap_or_else(|| solid(Color::BLACK, alpha));
                draw(&paint)
            }
            OfdColor::Pattern(p) => {
                if let Some((tile, pat_transform)) = self.pattern_tile(p, common) {
                    let shader = SkPattern::new(
                        tile.as_ref(),
                        SpreadMode::Repeat,
                        FilterQuality::Bilinear,
                        alpha as f32 / 255.0,
                        pat_transform,
                    );
                    let paint = shader_paint(shader);
                    draw(&paint)
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    draw(&paint)
                }
            }
            OfdColor::Gouraud(g) => {
                if let Some((pm, shader_transform)) =
                    self.gouraud_pixmap(&g.points, g.back_color.as_ref(), alpha, path, rule, 0)
                {
                    let shader = SkPattern::new(
                        pm.as_ref(),
                        SpreadMode::Pad,
                        FilterQuality::Bilinear,
                        1.0,
                        shader_transform,
                    );
                    let paint = shader_paint(shader);
                    draw(&paint)
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    draw(&paint)
                }
            }
            OfdColor::LatticeGouraud(g) => {
                if let Some((pm, shader_transform)) = self.gouraud_pixmap(
                    &g.points,
                    g.back_color.as_ref(),
                    alpha,
                    path,
                    rule,
                    g.vertices_per_row,
                ) {
                    let shader = SkPattern::new(
                        pm.as_ref(),
                        SpreadMode::Pad,
                        FilterQuality::Bilinear,
                        1.0,
                        shader_transform,
                    );
                    let paint = shader_paint(shader);
                    draw(&paint)
                } else {
                    let paint = solid(Color::BLACK, alpha);
                    draw(&paint)
                }
            }
        }
    }

    fn resolve_basic(&self, c: &BasicColor) -> Color {
        let cs_id = c.color_space.or(self.session.default_color_space);
        if let Some(index) = c.index {
            if let Some(cs) = cs_id.and_then(|id| self.session.color_spaces.get(&id)) {
                if let Some(mut p) = cs.palette.get(index).copied() {
                    p.a = ((p.a as u16 * c.alpha as u16) / 255) as u8;
                    return p;
                }
            }
        }
        let (kind, bpc) = cs_id
            .and_then(|id| self.session.color_spaces.get(&id))
            .map(|cs| (cs.kind, cs.bits_per_component))
            .unwrap_or((ColorSpaceKind::Rgb, 8));
        resolve_basic_color(c, kind, bpc)
    }

    fn gradient_stops(&self, segments: &[GradientSegment], alpha: u8) -> Vec<GradientStop> {
        if segments.is_empty() {
            return vec![
                GradientStop::new(0.0, tiny_color(Color::BLACK, alpha)),
                GradientStop::new(1.0, tiny_color(Color::BLACK, alpha)),
            ];
        }
        let len = segments.len();
        segments
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let fallback = if len == 1 {
                    0.0
                } else {
                    i as f32 / (len - 1) as f32
                };
                GradientStop::new(
                    s.position.unwrap_or(fallback).clamp(0.0, 1.0),
                    tiny_color(self.resolve_basic(&s.color), alpha),
                )
            })
            .collect()
    }

    fn axial_shader(
        &self,
        g: &AxialGradient,
        alpha: u8,
        _transform: Transform,
    ) -> Option<Shader<'static>> {
        let mut end = SkPoint::from_xy(g.end.x, g.end.y);
        let mode = spread_mode(g.map_type);
        if g.map_type != GradientMapType::Direct {
            if let Some(unit) = g.map_unit.filter(|u| *u > 0.0) {
                let dx = g.end.x - g.start.x;
                let dy = g.end.y - g.start.y;
                let len = (dx * dx + dy * dy).sqrt();
                if len > 0.0 {
                    end =
                        SkPoint::from_xy(g.start.x + dx / len * unit, g.start.y + dy / len * unit);
                }
            }
        }
        SkLinearGradient::new(
            SkPoint::from_xy(g.start.x, g.start.y),
            end,
            self.gradient_stops(&g.segments, alpha),
            mode,
            Transform::identity(),
        )
    }

    fn radial_shader(
        &self,
        g: &RadialGradient,
        alpha: u8,
        _transform: Transform,
    ) -> Option<Shader<'static>> {
        let mut end = g.end;
        let mut radius = g.end_radius.max(0.0);
        let mode = spread_mode(g.map_type);
        if g.map_type != GradientMapType::Direct {
            if let Some(unit) = g.map_unit.filter(|u| *u > 0.0) {
                let dx = g.end.x - g.start.x;
                let dy = g.end.y - g.start.y;
                let len = (dx * dx + dy * dy).sqrt();
                if len > 0.0 {
                    end = crate::geom::Point::new(
                        g.start.x + dx / len * unit,
                        g.start.y + dy / len * unit,
                    );
                }
            }
        }
        let mut shader_transform = Transform::identity();
        if g.eccentricity > 0.0 && g.eccentricity < 1.0 {
            let sy = (1.0 - g.eccentricity * g.eccentricity).sqrt().max(0.001);
            shader_transform = shader_transform
                .pre_translate(g.start.x, g.start.y)
                .pre_rotate(g.angle)
                .pre_scale(1.0, sy)
                .pre_rotate(-g.angle)
                .pre_translate(-g.start.x, -g.start.y);
            radius = g.end_radius.max(g.start_radius);
        }
        SkRadialGradient::new(
            SkPoint::from_xy(g.start.x, g.start.y),
            SkPoint::from_xy(end.x, end.y),
            radius,
            self.gradient_stops(&g.segments, alpha),
            mode,
            shader_transform,
        )
    }

    fn pattern_tile(
        &mut self,
        p: &PatternColor,
        common: &GraphicCommon,
    ) -> Option<(Pixmap, Transform)> {
        let scale = self.frame.dpi / crate::geom::MM_PER_INCH;
        let tw = (p.x_step * scale).ceil().max(1.0).min(4096.0) as u32;
        let th = (p.y_step * scale).ceil().max(1.0).min(4096.0) as u32;
        let mut tile = Pixmap::new(tw, th)?;

        {
            let mut cell = RenderCtx {
                session: &mut *self.session,
                frame: RenderFrame {
                    base: Transform::from_scale(scale, scale).pre_concat(matrix_transform(p.ctm)),
                    origin: (0.0, 0.0),
                    size: (tw, th),
                    dpi: self.frame.dpi,
                },
            };
            for obj in &p.cell_content {
                cell.paint_object(&mut tile, obj);
            }
        }

        let pat_transform = match p.relative_to {
            PatternRelativeTo::Object => Transform::from_scale(1.0 / scale, 1.0 / scale),
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
                    .pre_scale(1.0 / scale, 1.0 / scale)
            }
        };
        let tile = reflected_pattern_tile(tile, p.reflect);
        Some((tile, pat_transform))
    }

    fn gouraud_pixmap(
        &self,
        points: &[GouraudPoint],
        back: Option<&BasicColor>,
        alpha: u8,
        path: Option<&SkPath>,
        rule: Option<SkFillRule>,
        vertices_per_row: usize,
    ) -> Option<(Pixmap, Transform)> {
        let path = path?;
        let bounds = path.bounds();
        let scale = self.frame.dpi / crate::geom::MM_PER_INCH;
        let w = ((bounds.width() * scale).ceil() as u32).clamp(1, 4096);
        let h = ((bounds.height() * scale).ceil() as u32).clamp(1, 4096);
        let mut pm = Pixmap::new(w, h)?;
        let bg = back.map(|b| self.resolve_basic(b)).unwrap_or(Color {
            r: 0,
            g: 0,
            b: 0,
            a: 0,
        });
        fill_gouraud_pixmap(
            &mut pm,
            points,
            vertices_per_row,
            bg,
            alpha,
            bounds.left(),
            bounds.top(),
            scale,
            |b| self.resolve_basic(b),
        );
        if let Some(rule) = rule {
            let mut m = Mask::new(w, h)?;
            let local =
                Transform::from_scale(scale, scale).pre_translate(-bounds.left(), -bounds.top());
            m.fill_path(path, rule, true, local);
            apply_alpha_mask(&mut pm, &m);
        }
        let shader_transform = Transform::from_translate(bounds.left(), bounds.top())
            .pre_scale(1.0 / scale, 1.0 / scale);
        Some((pm, shader_transform))
    }

    fn stroke_for(&self, common: &GraphicCommon, dp: Option<u64>, width: f32) -> Stroke {
        let cap = match self.dp_resolve(dp, |d| d.cap).unwrap_or(common.cap) {
            LineCap::Round => SkLineCap::Round,
            LineCap::Square => SkLineCap::Square,
            LineCap::Butt => SkLineCap::Butt,
        };
        let join = match self.dp_resolve(dp, |d| d.join).unwrap_or(common.join) {
            LineJoin::Round => SkLineJoin::Round,
            LineJoin::Bevel => SkLineJoin::Bevel,
            LineJoin::Miter => SkLineJoin::Miter,
        };
        let miter_limit = self
            .dp_resolve(dp, |d| d.miter_limit)
            .or(common.miter_limit)
            .unwrap_or(3.528);
        let dash_pattern = common
            .dash_pattern
            .clone()
            .or_else(|| self.dp_resolve(dp, |d| d.dash_pattern.clone()));
        let dash_offset = common
            .dash_offset
            .or_else(|| self.dp_resolve(dp, |d| d.dash_offset))
            .unwrap_or(0.0);
        Stroke {
            width,
            miter_limit,
            line_cap: cap,
            line_join: join,
            dash: dash_pattern.and_then(|d| StrokeDash::new(d, dash_offset)),
        }
    }

    // ---- DrawParam inheritance --------------------------------------------

    fn dp_fill(&self, id: Option<u64>) -> Option<OfdColor> {
        self.dp_resolve(id, |d| d.fill_color.clone())
    }
    fn dp_stroke(&self, id: Option<u64>) -> Option<OfdColor> {
        self.dp_resolve(id, |d| d.stroke_color.clone())
    }
    fn dp_line_width(&self, id: Option<u64>) -> Option<f32> {
        self.dp_resolve(id, |d| d.line_width)
    }

    /// Walk the `Relative` chain until the field is found.
    fn dp_resolve<T>(&self, id: Option<u64>, pick: impl Fn(&DrawParam) -> Option<T>) -> Option<T> {
        let mut cur = id;
        let mut guard = 0;
        while let Some(i) = cur {
            let d = self.session.draw_params.get(&i)?;
            if let Some(v) = pick(d) {
                return Some(v);
            }
            cur = d.relative;
            guard += 1;
            if guard > 16 {
                break;
            }
        }
        None
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
/// one-to-many, and many-to-one mappings.
fn glyphs_for_char(
    face: &Face,
    ch: char,
    transforms: &[CgTransform],
    idx: usize,
) -> Vec<ttf_parser::GlyphId> {
    if let Some(ids) = cg_transform_glyph_ids(transforms, idx) {
        let gids: Vec<_> = ids
            .into_iter()
            .filter(|g| *g != 0)
            .map(ttf_parser::GlyphId)
            .collect();
        if !gids.is_empty() {
            return gids;
        }
    }
    face.glyph_index(ch)
        .filter(|g| g.0 != 0)
        .into_iter()
        .collect()
}

fn cg_transform_glyph_ids(transforms: &[CgTransform], idx: usize) -> Option<Vec<u16>> {
    for cg in transforms {
        if idx < cg.code_position || idx >= cg.code_position + cg.code_count {
            continue;
        }
        let offset = idx - cg.code_position;
        let glyph_count = cg.glyph_count.max(cg.glyphs.len()).max(1);
        let ids = if cg.code_count == 1 {
            cg.glyphs.iter().copied().take(glyph_count).collect()
        } else if glyph_count == 1 {
            if offset == 0 {
                cg.glyphs.first().copied().into_iter().collect()
            } else {
                Vec::new()
            }
        } else if cg.code_count == glyph_count {
            cg.glyphs.get(offset).copied().into_iter().collect()
        } else {
            let start = offset.saturating_mul(glyph_count) / cg.code_count;
            let end = ((offset + 1).saturating_mul(glyph_count) / cg.code_count).max(start + 1);
            cg.glyphs
                .iter()
                .copied()
                .skip(start)
                .take(end - start)
                .collect()
        };
        return Some(ids);
    }
    None
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
    let a = ((c.a as u16 * alpha as u16) / 255) as u8;
    tiny_skia::Color::from_rgba8(c.r, c.g, c.b, a)
}

fn shader_paint<'p>(shader: Shader<'p>) -> Paint<'p> {
    Paint {
        shader,
        anti_alias: true,
        ..Default::default()
    }
}

fn spread_mode(map_type: GradientMapType) -> SpreadMode {
    match map_type {
        GradientMapType::Direct => SpreadMode::Pad,
        GradientMapType::Repeat => SpreadMode::Repeat,
        GradientMapType::Reflect => SpreadMode::Reflect,
    }
}

fn matrix_transform(m: Matrix) -> Transform {
    Transform::from_row(m.a, m.b, m.c, m.d, m.e, m.f)
}

fn resolve_basic_color(color: &BasicColor, kind: ColorSpaceKind, bpc: u8) -> Color {
    let comps = color.components.as_deref().unwrap_or(&[]);
    let scale = |v: f32| -> u8 {
        let bpc = bpc.clamp(1, 16);
        let max = ((1u32 << bpc as u32) - 1).max(1) as f32;
        if bpc == 8 {
            v.clamp(0.0, 255.0).round() as u8
        } else {
            (v.clamp(0.0, max) / max * 255.0).round() as u8
        }
    };
    let mut c = match kind {
        ColorSpaceKind::Gray => {
            let v = scale(comps.first().copied().unwrap_or(0.0));
            Color::rgb(v, v, v)
        }
        ColorSpaceKind::Rgb => Color::rgb(
            scale(comps.first().copied().unwrap_or(0.0)),
            scale(comps.get(1).copied().unwrap_or(0.0)),
            scale(comps.get(2).copied().unwrap_or(0.0)),
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
    c.a = color.alpha;
    c
}

fn reflected_pattern_tile(tile: Pixmap, reflect: PatternReflect) -> Pixmap {
    if reflect == PatternReflect::Normal {
        return tile;
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
    let Some(mut out) = Pixmap::new(out_w.max(1), out_h.max(1)) else {
        return tile;
    };
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
    out
}

fn fill_gouraud_pixmap(
    pm: &mut Pixmap,
    points: &[GouraudPoint],
    vertices_per_row: usize,
    back: Color,
    alpha: u8,
    origin_x: f32,
    origin_y: f32,
    scale: f32,
    mut resolve: impl FnMut(&BasicColor) -> Color,
) {
    pm.fill(tiny_color(back, alpha));
    let mut verts = Vec::new();
    for p in points {
        verts.push(GouraudVertex {
            x: (p.x - origin_x) * scale,
            y: (p.y - origin_y) * scale,
            color: resolve(&p.color),
            edge_flag: p.edge_flag,
        });
    }
    if vertices_per_row >= 2 {
        fill_lattice_gouraud(pm, &verts, vertices_per_row, alpha);
    } else {
        fill_free_gouraud(pm, &verts, alpha);
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
    let mut prev = [verts[0], verts[1], verts[2]];
    for v in verts.iter().copied().skip(3) {
        let tri = match v.edge_flag.unwrap_or(0) {
            1 => [prev[0], prev[2], v],
            2 => [prev[1], prev[2], v],
            _ => [prev[1], prev[2], v],
        };
        fill_triangle(pm, tri[0], tri[1], tri[2], alpha);
        prev = tri;
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

/// A solid-color paint at the given object alpha (0..=255).
fn solid(c: Color, alpha: u8) -> Paint<'static> {
    let a = ((c.a as u16 * alpha as u16) / 255) as u8;
    Paint {
        shader: Shader::SolidColor(tiny_skia::Color::from_rgba8(c.r, c.g, c.b, a)),
        anti_alias: true,
        ..Default::default()
    }
}

/// Decode an image resource to straight-alpha RGBA.
fn decode_image_rgba(media: &MultiMedia) -> Option<image::RgbaImage> {
    decode_rgba(media.format, &media.data)
}

fn decode_rgba(format: ImageFormat, data: &[u8]) -> Option<image::RgbaImage> {
    match format {
        ImageFormat::Jbig2 => decode_jbig2(data),
        ImageFormat::Ccitt => None, // not yet supported
        _ => Some(image::load_from_memory(data).ok()?.to_rgba8()),
    }
}

/// Decode a JBIG2 bilevel image (e.g. invoice QR codes, scanned B&W pages) to
/// black-on-white RGBA. Foreground pixels (value 1) are black.
fn decode_jbig2(data: &[u8]) -> Option<image::RgbaImage> {
    const FILE_MAGIC: &[u8] = &[0x97, 0x4a, 0x42, 0x32, 0x0d, 0x0a, 0x1a, 0x0a];
    let pages = if data.starts_with(FILE_MAGIC) {
        justbig2::decode(data)
    } else {
        justbig2::decode_embedded(data)
    }
    .ok()?;
    let page = pages.into_iter().find(|p| p.width > 0 && p.height > 0)?;

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
    Some(img)
}

/// Build a premultiplied `tiny-skia` pixmap from a straight-alpha RGBA image.
fn rgba_to_pixmap(rgba: &image::RgbaImage) -> Option<Pixmap> {
    let (w, h) = rgba.dimensions();
    let mut pm = Pixmap::new(w, h)?;
    for (dst, px) in pm.pixels_mut().iter_mut().zip(rgba.pixels()) {
        let [r, g, b, a] = px.0;
        *dst = tiny_skia::PremultipliedColorU8::from_rgba(
            ((r as u16 * a as u16) / 255) as u8,
            ((g as u16 * a as u16) / 255) as u8,
            ((b as u16 * a as u16) / 255) as u8,
            a,
        )?;
    }
    Some(pm)
}

/// Decode image bytes straight to a pixmap (used for small seal pictures).
fn decode_bytes(format: ImageFormat, data: &[u8]) -> Option<Pixmap> {
    rgba_to_pixmap(&decode_rgba(format, data)?)
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
            }],
            ..Default::default()
        }
    }

    fn pixel(b: &Bitmap, x: u32, y: u32) -> [u8; 4] {
        let idx = ((y * b.width + x) * 4) as usize;
        b.rgba[idx..idx + 4].try_into().unwrap()
    }

    /// A 10mm x 10mm page with one page-filling black path, toggling visibility.
    fn one_path_doc(visible: bool) -> Document {
        let mut common = GraphicCommon::default();
        common.boundary = Rect::new(0.0, 0.0, 10.0, 10.0);
        common.visible = visible;
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
    fn render_session_reuses_state_across_pages() {
        let doc = one_path_doc(true);
        let mut session = RenderSession::new(&doc, RenderOptions::default());
        let first = session.render_page(0, 96.0).unwrap();
        let second = session.render_page(0, 96.0).unwrap();
        assert_eq!((first.width, first.height), (second.width, second.height));
        assert_eq!(first.rgba, second.rgba);
    }

    #[test]
    fn palette_index_resolves_through_color_space() {
        let mut common = GraphicCommon::default();
        common.boundary = Rect::new(0.0, 0.0, 20.0, 10.0);
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
                palette: vec![Color::rgb(255, 0, 0), Color::rgb(0, 180, 0)],
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
    fn axial_gradient_transitions_across_path() {
        let mut common = GraphicCommon::default();
        common.boundary = Rect::new(0.0, 0.0, 20.0, 10.0);
        let path = rect_path(
            common,
            Some(OfdColor::Axial(AxialGradient {
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
    fn pattern_tiles_cell_content() {
        let mut cell_common = GraphicCommon::default();
        cell_common.boundary = Rect::new(0.0, 0.0, 2.0, 2.0);
        let cell = rect_path(cell_common, Some(Color::BLACK.into()));
        let mut common = GraphicCommon::default();
        common.boundary = Rect::new(0.0, 0.0, 20.0, 10.0);
        let path = rect_path(
            common,
            Some(OfdColor::Pattern(PatternColor {
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
            0,
            Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0,
            },
            255,
            0.0,
            0.0,
            1.0,
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
    fn cg_transform_handles_many_to_one_and_one_to_many() {
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
        assert_eq!(cg_transform_glyph_ids(&transforms, 0).unwrap(), vec![42]);
        assert!(cg_transform_glyph_ids(&transforms, 1).unwrap().is_empty());
        assert_eq!(cg_transform_glyph_ids(&transforms, 2).unwrap(), vec![7, 8]);
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
}
