//! Host-side rendering used by the Quick Look FFI layer.
//!
//! This is the testable, safe-Rust half of the crate. It loads an OFD file from
//! disk, resolves substitute fonts from the system font database, and produces
//! the two artifacts Quick Look needs:
//!
//! * a multi-page image-based PDF (Finder spacebar preview), and
//! * a page-1 PNG (Finder thumbnail).
//!
//! The logic mirrors the desktop app's `load_document` / `load_fallback_fonts` /
//! `export_loaded_pdf` / `encode_png`, minus Tauri and minus the app's bundled
//! fonts: a sandboxed Quick Look extension can read system fonts but not the
//! host app's resources, so `fontdb` system fonts are the only substitution
//! source. The core itself stays bytes-in/model-out; all file access is here.

use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use image::ImageEncoder;
use ofd_core::container::ContainerLimits;
use ofd_core::geom::{Rect, MM_PER_INCH};
use ofd_core::model::{Document, Font};
use ofd_core::render::{RenderLimits, RenderOptions, RenderSession};
use ofd_core::OfdPackage;
use printpdf::{
    ImageCompression, ImageOptimizationOptions, Mm, Op, PdfDocument, PdfPage, PdfSaveOptions,
    RawImage, RawImageData, RawImageFormat, XObjectTransform,
};

/// Default number of pages in a preview PDF when the caller passes `0`.
pub const DEFAULT_MAX_PAGES: u32 = 32;
/// Upper sanity clamp for a caller-supplied page limit.
pub const MAX_MAX_PAGES: u32 = 256;
/// Default preview DPI when the caller passes `0`.
pub const DEFAULT_DPI: f32 = 144.0;
/// Lowest DPI a caller may request (after clamping).
pub const MIN_DPI: f32 = 36.0;
/// Highest DPI a caller may request (after clamping).
pub const MAX_DPI: f32 = 600.0;
/// Default total raster pixel budget when the caller passes `0`.
pub const DEFAULT_MAX_PIXELS: u64 = 50_000_000;
/// Default thumbnail longest-edge limit when the caller passes `0`.
pub const DEFAULT_MAX_DIMENSION: u32 = 1024;
/// Lowest thumbnail longest-edge limit (after clamping).
pub const MIN_MAX_DIMENSION: u32 = 32;
/// Highest thumbnail longest-edge limit (after clamping).
pub const MAX_MAX_DIMENSION: u32 = 4096;

/// A preview PDF is never rasterized below this DPI; below it the preview would
/// be useless, so the caller gets a render error instead (mirrors the desktop
/// batch-export logic).
const MIN_PDF_DPI: f32 = 72.0;
/// JPEG quality for the page images embedded in the preview PDF.
const PDF_JPEG_QUALITY: f32 = 0.9;
/// Upper bound on distinct family names queried against the system font database.
const MAX_FONT_FAMILY_QUERIES: usize = 4096;
/// Upper bound on injected fallback faces.
const MAX_FALLBACK_FONT_FACES: usize = 256;
/// Upper bound on injected fallback font bytes (inclusive).
const MAX_FALLBACK_FONT_BYTES: u64 = 256 * 1024 * 1024;

/// Family names tried against the system font database, in order. Mirrors the
/// desktop app's list (sans the bundled fonts, which a sandboxed appex cannot
/// read). [`ofd_core::fonts`] applies its own aliases on top of these.
const COMMON_FALLBACK_FAMILIES: &[&str] = &[
    "Songti SC",
    "STSong",
    "SimSun",
    "PingFang SC",
    "Heiti SC",
    "STHeiti",
    "Microsoft YaHei",
    "Noto Sans CJK SC",
    "Noto Serif CJK SC",
    "Source Han Sans SC",
    "Source Han Serif SC",
    "WenQuanYi Micro Hei",
    "Arial Unicode MS",
    "STKaiti",
    "Kaiti SC",
    "STFangsong",
    "Fangsong SC",
];

/// A Quick Look render failure, tagged with the C ABI status code it maps to.
#[derive(Debug)]
pub enum Error {
    /// Status 1: null/empty/non-UTF-8 path, NaN DPI, ...
    InvalidArgument(String),
    /// Status 2: the file could not be opened or read.
    Io(String),
    /// Status 3: the bytes are not a valid OFD/ZIP container.
    Container(String),
    /// Status 4: the package/document contains no pages.
    NoPages(String),
    /// Status 5: rasterization failed or the document exceeds the pixel budget.
    Render(String),
    /// Status 6: the PDF encoder failed to produce output.
    Pdf(String),
    /// Status 7: the PNG encoder failed.
    Png(String),
    /// Status 8: panic caught at the FFI boundary or any other failure.
    Other(String),
}

impl Error {
    /// The C ABI status code for this error (see `include/ofd_quicklook.h`).
    pub fn status(&self) -> i32 {
        match self {
            Self::InvalidArgument(_) => 1,
            Self::Io(_) => 2,
            Self::Container(_) => 3,
            Self::NoPages(_) => 4,
            Self::Render(_) => 5,
            Self::Pdf(_) => 6,
            Self::Png(_) => 7,
            Self::Other(_) => 8,
        }
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidArgument(message)
            | Self::Io(message)
            | Self::Container(message)
            | Self::NoPages(message)
            | Self::Render(message)
            | Self::Pdf(message)
            | Self::Png(message)
            | Self::Other(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

/// Render page images for the first `max_pages` pages into a complete,
/// image-based PDF (the Finder preview).
///
/// Knobs passed as `0` select the documented defaults; non-zero values are
/// honored after sanity clamping. The DPI is reduced when the rasterized page
/// area would exceed `max_pixels`, but never below 72 DPI (that is an error).
pub fn render_preview_pdf(
    path: &Path,
    max_pages: u32,
    dpi: f32,
    max_pixels: u64,
) -> Result<Vec<u8>, Error> {
    let max_pages = normalize_max_pages(max_pages) as usize;
    let dpi = normalize_dpi(dpi)?;
    let max_pixels = normalize_max_pixels(max_pixels);

    let loaded = LoadedDocument::load(path)?;
    let document = loaded.document()?;
    if document.pages.is_empty() {
        return Err(Error::NoPages("OFD document contains no pages".into()));
    }
    let page_count = document.pages.len().min(max_pages);
    let dpi = constrained_dpi(document, page_count, dpi, max_pixels)?;

    let options = RenderOptions {
        fallback_fonts: load_fallback_fonts(document),
        limits: RenderLimits {
            max_page_pixels: max_pixels,
            ..RenderLimits::default()
        },
        ..RenderOptions::default()
    };
    let mut renderer = RenderSession::new(document, options);
    let mut pdf = PdfDocument::new(&pdf_title(&loaded, document));
    let mut pages = Vec::with_capacity(page_count);

    for index in 0..page_count {
        let area = loaded.page_area(index)?;
        let bitmap = renderer
            .render_page(index, dpi)
            .map_err(|error| Error::Render(format!("cannot render page {}: {error}", index + 1)))?;
        // The bitmap size is the rounded device footprint; derive the exact
        // unit-square → millimetre scale from it so the image fills the page.
        let scale_x = (area.w * dpi / MM_PER_INCH) / bitmap.width as f32;
        let scale_y = (area.h * dpi / MM_PER_INCH) / bitmap.height as f32;
        let image = RawImage {
            pixels: RawImageData::U8(rgba_to_rgb(&bitmap.rgba)),
            width: bitmap.width as usize,
            height: bitmap.height as usize,
            data_format: RawImageFormat::RGB8,
            tag: Vec::new(),
        };
        let image_id = pdf.add_image(&image);
        pages.push(PdfPage::new(
            Mm(area.w),
            Mm(area.h),
            vec![Op::UseXobject {
                id: image_id,
                transform: XObjectTransform {
                    scale_x: Some(scale_x),
                    scale_y: Some(scale_y),
                    dpi: Some(dpi),
                    ..Default::default()
                },
            }],
        ));
    }

    pdf.with_pages(pages);
    let save_options = PdfSaveOptions {
        image_optimization: Some(ImageOptimizationOptions {
            quality: Some(PDF_JPEG_QUALITY),
            max_image_size: None,
            auto_optimize: Some(false),
            format: Some(ImageCompression::Jpeg),
            ..Default::default()
        }),
        ..Default::default()
    };
    let bytes = pdf.save(&save_options, &mut Vec::new());
    if !bytes.starts_with(b"%PDF") {
        return Err(Error::Pdf(
            "PDF encoder produced no valid PDF output".into(),
        ));
    }
    Ok(bytes)
}

/// Render page 1 to an RGBA PNG whose longest edge is at most `max_dimension`
/// pixels and whose aspect ratio matches the page area (the Finder thumbnail).
pub fn render_thumbnail_png(path: &Path, max_dimension: u32) -> Result<Vec<u8>, Error> {
    let max_dimension = normalize_max_dimension(max_dimension);

    let loaded = LoadedDocument::load(path)?;
    let document = loaded.document()?;
    if document.pages.is_empty() {
        return Err(Error::NoPages("OFD document contains no pages".into()));
    }
    let area = loaded.page_area(0)?;
    let dpi = thumbnail_dpi(area, max_dimension)?;

    let options = RenderOptions {
        fallback_fonts: load_fallback_fonts(document),
        ..RenderOptions::default()
    };
    let bitmap = RenderSession::new(document, options)
        .render_page(0, dpi)
        .map_err(|error| Error::Render(format!("cannot render page 1: {error}")))?;

    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &bitmap.rgba,
            bitmap.width,
            bitmap.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| Error::Png(format!("cannot encode thumbnail: {error}")))?;
    Ok(png)
}

/// A parsed OFD file plus its on-disk path (used for the PDF title fallback).
struct LoadedDocument {
    path: PathBuf,
    package: OfdPackage,
}

impl LoadedDocument {
    fn load(path: &Path) -> Result<Self, Error> {
        let bytes = read_limited(path)?;
        let package = ofd_core::open_with_limits(bytes, ContainerLimits::default())
            .map_err(|error| Error::Container(format!("cannot parse OFD package: {error}")))?;
        Ok(Self {
            path: path.to_path_buf(),
            package,
        })
    }

    fn document(&self) -> Result<&Document, Error> {
        self.package
            .documents
            .first()
            .ok_or_else(|| Error::NoPages("OFD package contains no documents".into()))
    }

    fn page_area(&self, page_index: usize) -> Result<Rect, Error> {
        let document = self.document()?;
        let page = document
            .pages
            .get(page_index)
            .ok_or_else(|| Error::Render(format!("no page {page_index}")))?;
        page.area
            .unwrap_or(document.page_area)
            .render_box()
            .ok_or_else(|| Error::Render(format!("page {} has no physical area", page_index + 1)))
    }
}

/// Read the OFD file, refusing archives larger than the container default limit
/// (the same bound [`ofd_core::open`] enforces).
fn read_limited(path: &Path) -> Result<Vec<u8>, Error> {
    let limits = ContainerLimits::default();
    let file = File::open(path)
        .map_err(|error| Error::Io(format!("cannot open {}: {error}", path.display())))?;
    let declared = file
        .metadata()
        .map_err(|error| Error::Io(format!("cannot read {} metadata: {error}", path.display())))?
        .len();
    if declared > limits.max_archive_bytes {
        return Err(Error::Container(format!(
            "document declares {declared} bytes; limit is {}",
            limits.max_archive_bytes
        )));
    }

    let mut bytes =
        Vec::with_capacity(usize::try_from(declared.min(1024 * 1024)).unwrap_or_default());
    file.take(limits.max_archive_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| Error::Io(format!("cannot read {}: {error}", path.display())))?;
    if bytes.len() as u64 > limits.max_archive_bytes {
        return Err(Error::Container(format!(
            "document exceeds the {} byte container limit",
            limits.max_archive_bytes
        )));
    }
    Ok(bytes)
}

/// The process-wide system font database, loaded lazily on first render.
///
/// Quick Look may ask one extension process to thumbnail many files, so the
/// (relatively expensive) font directory scan is done once and reused. The
/// `Database` is immutable after initialization and safe to share.
fn system_fonts() -> &'static fontdb::Database {
    static DATABASE: OnceLock<fontdb::Database> = OnceLock::new();
    DATABASE.get_or_init(|| {
        let mut database = fontdb::Database::new();
        database.load_system_fonts();
        database
    })
}

/// Collect raw font bytes for the document's declared families plus the common
/// CJK family list, capped at 256 faces / 256 MiB. Best effort: faces that
/// cannot be found or read are skipped (the renderer falls back further).
fn load_fallback_fonts(document: &Document) -> Vec<Arc<Vec<u8>>> {
    let database = system_fonts();
    let mut fonts: Vec<Arc<Vec<u8>>> = Vec::new();
    let mut font_bytes = 0u64;
    let mut queried = HashSet::new();
    let mut seen_faces = HashSet::new();

    let families = COMMON_FALLBACK_FAMILIES
        .iter()
        .copied()
        .chain(document.resources.fonts.iter().map(Font::family));
    for family in families {
        let key = family.trim().to_lowercase();
        if key.is_empty() || !queried.insert(key) {
            continue;
        }
        if queried.len() > MAX_FONT_FAMILY_QUERIES {
            break;
        }
        let Some(id) = database.query(&fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            ..Default::default()
        }) else {
            continue;
        };
        if !seen_faces.insert(id) {
            continue;
        }
        let data = database
            .with_face_data(id, |data, _| {
                fallback_font_fits(fonts.len(), font_bytes, data.len()).then(|| data.to_vec())
            })
            .flatten();
        if let Some(data) = data {
            push_fallback_font(&mut fonts, &mut font_bytes, data);
        }
    }
    fonts
}

fn fallback_font_fits(font_count: usize, total: u64, data_len: usize) -> bool {
    let Ok(bytes) = u64::try_from(data_len) else {
        return false;
    };
    font_count < MAX_FALLBACK_FONT_FACES
        && total
            .checked_add(bytes)
            .is_some_and(|next| next <= MAX_FALLBACK_FONT_BYTES)
}

fn push_fallback_font(fonts: &mut Vec<Arc<Vec<u8>>>, total: &mut u64, data: Vec<u8>) {
    if !fallback_font_fits(fonts.len(), *total, data.len()) {
        return;
    }
    let Ok(bytes) = u64::try_from(data.len()) else {
        return;
    };
    *total += bytes;
    fonts.push(Arc::new(data));
}

/// `0` selects the default; anything else is clamped to a sane range.
fn normalize_max_pages(max_pages: u32) -> u32 {
    if max_pages == 0 {
        DEFAULT_MAX_PAGES
    } else {
        max_pages.clamp(1, MAX_MAX_PAGES)
    }
}

/// `0` selects the default; anything else is clamped. NaN is invalid.
fn normalize_dpi(dpi: f32) -> Result<f32, Error> {
    if dpi.is_nan() {
        return Err(Error::InvalidArgument("dpi must not be NaN".into()));
    }
    if dpi == 0.0 {
        Ok(DEFAULT_DPI)
    } else {
        Ok(dpi.clamp(MIN_DPI, MAX_DPI))
    }
}

/// `0` selects the default; anything else is honored as-is.
fn normalize_max_pixels(max_pixels: u64) -> u64 {
    if max_pixels == 0 {
        DEFAULT_MAX_PIXELS
    } else {
        max_pixels
    }
}

/// `0` selects the default; anything else is clamped to a sane range.
fn normalize_max_dimension(max_dimension: u32) -> u32 {
    if max_dimension == 0 {
        DEFAULT_MAX_DIMENSION
    } else {
        max_dimension.clamp(MIN_MAX_DIMENSION, MAX_MAX_DIMENSION)
    }
}

/// Reduce the requested DPI so the first `page_count` pages fit in
/// `max_pixels`, mirroring the desktop export path. The result is floored (to
/// avoid drifting over the budget) and must not fall below [`MIN_PDF_DPI`].
fn constrained_dpi(
    document: &Document,
    page_count: usize,
    requested_dpi: f32,
    max_pixels: u64,
) -> Result<f32, Error> {
    let mut total_area = 0.0_f64;
    for page in document.pages.iter().take(page_count) {
        let area = page
            .area
            .unwrap_or(document.page_area)
            .render_box()
            .ok_or_else(|| Error::Render("a page has no physical area".into()))?;
        total_area += f64::from(area.w) * f64::from(area.h);
    }
    let pixels = total_area * f64::from(requested_dpi / MM_PER_INCH).powi(2);
    if !pixels.is_finite() {
        return Err(Error::Render(
            "page geometry is not representable at the requested dpi".into(),
        ));
    }
    if pixels <= max_pixels as f64 {
        return Ok(requested_dpi);
    }

    let constrained = f64::from(requested_dpi) * (max_pixels as f64 / pixels).sqrt();
    if !constrained.is_finite() || constrained < f64::from(MIN_PDF_DPI) {
        return Err(Error::Render(format!(
            "document is too large to preview within the {} megapixel limit",
            max_pixels / 1_000_000
        )));
    }
    Ok((constrained as f32).floor())
}

/// DPI that makes the page's longest edge exactly `max_dimension` device pixels.
fn thumbnail_dpi(area: Rect, max_dimension: u32) -> Result<f32, Error> {
    if !area.w.is_finite() || !area.h.is_finite() || area.w <= 0.0 || area.h <= 0.0 {
        return Err(Error::Render(format!(
            "page has an invalid physical area: {} x {} mm",
            area.w, area.h
        )));
    }
    let longest_edge_mm = area.w.max(area.h);
    Ok(max_dimension as f32 * MM_PER_INCH / longest_edge_mm)
}

fn pdf_title(loaded: &LoadedDocument, document: &Document) -> String {
    document
        .metadata
        .title
        .clone()
        .filter(|title| !title.is_empty())
        .or_else(|| {
            loaded
                .path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "OFD document".to_string())
}

/// Compact straight-alpha RGBA to RGB for the PDF page image (page backgrounds
/// are opaque white, so the alpha channel carries no information).
fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.as_chunks::<4>().0 {
        rgb.extend_from_slice(&pixel[..3]);
    }
    rgb
}

#[cfg(test)]
mod tests {
    use super::*;
    use ofd_core::model::PageArea;

    fn doc_with_pages(areas: &[Rect]) -> Document {
        Document {
            pages: areas
                .iter()
                .map(|area| ofd_core::model::Page {
                    area: Some(PageArea {
                        physical_box: Some(*area),
                        ..PageArea::default()
                    }),
                    ..ofd_core::model::Page::default()
                })
                .collect(),
            ..Document::default()
        }
    }

    fn px_at(area: Rect, dpi: f32) -> (u32, u32) {
        let scale = dpi / MM_PER_INCH;
        (
            (area.w * scale).round().max(1.0) as u32,
            (area.h * scale).round().max(1.0) as u32,
        )
    }

    #[test]
    fn max_pages_zero_selects_default_and_others_clamp() {
        assert_eq!(normalize_max_pages(0), DEFAULT_MAX_PAGES);
        assert_eq!(normalize_max_pages(1), 1);
        assert_eq!(normalize_max_pages(7), 7);
        assert_eq!(normalize_max_pages(300), MAX_MAX_PAGES);
    }

    #[test]
    fn max_dimension_zero_selects_default_and_others_clamp() {
        assert_eq!(normalize_max_dimension(0), DEFAULT_MAX_DIMENSION);
        assert_eq!(normalize_max_dimension(10), MIN_MAX_DIMENSION);
        assert_eq!(normalize_max_dimension(500), 500);
        assert_eq!(normalize_max_dimension(99_999), MAX_MAX_DIMENSION);
    }

    #[test]
    fn dpi_zero_selects_default_and_others_clamp() {
        assert_eq!(normalize_dpi(0.0).unwrap(), DEFAULT_DPI);
        assert_eq!(normalize_dpi(10.0).unwrap(), MIN_DPI);
        assert_eq!(normalize_dpi(300.0).unwrap(), 300.0);
        assert_eq!(normalize_dpi(10_000.0).unwrap(), MAX_DPI);
        assert_eq!(normalize_dpi(f32::INFINITY).unwrap(), MAX_DPI);
        assert_eq!(normalize_dpi(f32::NEG_INFINITY).unwrap(), MIN_DPI);
        assert_eq!(
            normalize_dpi(f32::NAN).unwrap_err().status(),
            1,
            "NaN dpi is an invalid argument"
        );
    }

    #[test]
    fn max_pixels_zero_selects_default_and_others_are_honored() {
        assert_eq!(normalize_max_pixels(0), DEFAULT_MAX_PIXELS);
        assert_eq!(normalize_max_pixels(123), 123);
        assert_eq!(normalize_max_pixels(u64::MAX), u64::MAX);
    }

    #[test]
    fn dpi_is_kept_when_the_document_fits_the_pixel_budget() {
        let document = doc_with_pages(&[Rect::new(0.0, 0.0, 210.0, 297.0)]);
        let dpi = constrained_dpi(&document, 1, 300.0, DEFAULT_MAX_PIXELS).unwrap();
        assert_eq!(dpi, 300.0);
    }

    #[test]
    fn dpi_is_reduced_to_fit_the_pixel_budget() {
        let document = doc_with_pages(&[Rect::new(0.0, 0.0, 210.0, 297.0)]);
        let dpi = constrained_dpi(&document, 1, 600.0, 1_000_000).unwrap();
        assert!(dpi >= MIN_PDF_DPI, "dpi {dpi} must stay usable");
        assert!(dpi < 600.0);
        let (width, height) = px_at(Rect::new(0.0, 0.0, 210.0, 297.0), dpi);
        assert!(
            u64::from(width) * u64::from(height) <= 1_000_000,
            "{width}x{height} exceeds the budget"
        );
    }

    #[test]
    fn dpi_that_would_fall_below_the_usable_floor_is_an_error() {
        let document = doc_with_pages(&[Rect::new(0.0, 0.0, 210.0, 297.0)]);
        let error = constrained_dpi(&document, 1, 144.0, 1).unwrap_err();
        assert_eq!(error.status(), 5);
    }

    #[test]
    fn pixel_budget_only_counts_pages_that_are_actually_rendered() {
        // Ten A4 pages but only the first one is requested: the budget is
        // computed over the rendered page count, so 300 DPI still fits.
        let page = Rect::new(0.0, 0.0, 210.0, 297.0);
        let document = doc_with_pages(&[page; 10]);
        let dpi = constrained_dpi(&document, 1, 300.0, DEFAULT_MAX_PIXELS).unwrap();
        assert_eq!(dpi, 300.0);
        let dpi_all = constrained_dpi(&document, 10, 300.0, DEFAULT_MAX_PIXELS);
        assert!(
            dpi_all.is_err() || dpi_all.unwrap() < 300.0,
            "rendering all ten pages must not fit 300 dpi"
        );
    }

    #[test]
    fn thumbnail_dpi_fits_the_longest_edge_for_portrait_and_landscape() {
        for area in [
            Rect::new(0.0, 0.0, 210.0, 297.0),
            Rect::new(0.0, 0.0, 297.0, 210.0),
        ] {
            for max_dimension in [32, 200, 1024, 4096] {
                let dpi = thumbnail_dpi(area, max_dimension).unwrap();
                let (width, height) = px_at(area, dpi);
                assert_eq!(
                    width.max(height),
                    max_dimension,
                    "{area:?} at {max_dimension}"
                );
                let expected_ratio = area.w / area.h;
                let actual_ratio = width as f32 / height as f32;
                // Rounding to whole pixels is significant at tiny sizes, so the
                // tolerance scales with the target dimension.
                let tolerance = 2.0 / max_dimension as f32 + 0.005;
                assert!(
                    (actual_ratio - expected_ratio).abs() < tolerance,
                    "aspect ratio drifted: {actual_ratio} vs {expected_ratio} (tolerance {tolerance})"
                );
            }
        }
    }

    #[test]
    fn thumbnail_rejects_degenerate_page_areas() {
        for area in [
            Rect::new(0.0, 0.0, 0.0, 297.0),
            Rect::new(0.0, 0.0, -210.0, 297.0),
            Rect::new(0.0, 0.0, f32::NAN, 297.0),
            Rect::new(0.0, 0.0, f32::INFINITY, 297.0),
        ] {
            assert_eq!(thumbnail_dpi(area, 512).unwrap_err().status(), 5);
        }
    }

    #[test]
    fn opaque_rgba_is_compacted_for_pdf_images() {
        assert_eq!(
            rgba_to_rgb(&[1, 2, 3, 255, 4, 5, 6, 255]),
            vec![1, 2, 3, 4, 5, 6]
        );
        assert!(rgba_to_rgb(&[]).is_empty());
    }

    #[test]
    fn error_status_codes_match_the_c_abi() {
        let cases = [
            (Error::InvalidArgument(String::new()), 1),
            (Error::Io(String::new()), 2),
            (Error::Container(String::new()), 3),
            (Error::NoPages(String::new()), 4),
            (Error::Render(String::new()), 5),
            (Error::Pdf(String::new()), 6),
            (Error::Png(String::new()), 7),
            (Error::Other(String::new()), 8),
        ];
        for (error, expected) in cases {
            assert_eq!(error.status(), expected);
        }
        assert_eq!(
            Error::Render("cannot render".into()).to_string(),
            "cannot render"
        );
    }
}
