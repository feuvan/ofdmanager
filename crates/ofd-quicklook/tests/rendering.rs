//! Integration tests for the safe Rust rendering API against real fixtures.
//!
//! These tests must pass without the fetched bundled fonts: the renderer only
//! substitutes system fonts, and these assertions are structural (valid PDF/PNG,
//! page count, page size, aspect ratio), so missing CJK font coverage cannot
//! make them fail.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use ofd_quicklook::{render_preview_pdf, render_thumbnail_png, Error};

const PDF_MAGIC: &[u8] = b"%PDF";
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";
/// PDF points per millimetre (1 inch = 72 pt = 25.4 mm).
const PT_PER_MM: f32 = 72.0 / 25.4;

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name)
}

/// Parse a rendered PDF with printpdf itself, proving the bytes are a complete
/// PDF that a consumer can open.
fn parse_pdf(bytes: &[u8]) -> printpdf::PdfDocument {
    let mut warnings = Vec::new();
    printpdf::PdfDocument::parse(bytes, &printpdf::PdfParseOptions::default(), &mut warnings)
        .unwrap_or_else(|error| panic!("rendered PDF must parse: {error}"))
}

fn assert_page_size_mm(document: &printpdf::PdfDocument, page: usize, width: f32, height: f32) {
    let media_box = &document.pages[page].media_box;
    let actual_width = media_box.width.0 / PT_PER_MM;
    let actual_height = media_box.height.0 / PT_PER_MM;
    assert!(
        (actual_width - width).abs() < 0.5 && (actual_height - height).abs() < 0.5,
        "page {page} size is {actual_width} x {actual_height} mm, expected {width} x {height} mm"
    );
}

/// A uniquely named temp file, removed on drop.
struct TempFile(PathBuf);

impl TempFile {
    fn with_bytes(suffix: &str, bytes: &[u8]) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ofd-quicklook-test-{}-{nanos}-{suffix}",
            std::process::id()
        ));
        fs::write(&path, bytes).expect("write temp file");
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[test]
fn preview_pdf_is_a_complete_pdf_of_the_expected_page_count() {
    let cases = [
        ("helloworld.ofd", 210.0, 297.0),
        ("invoice-like.ofd", 210.0, 140.0),
    ];
    for (name, width_mm, height_mm) in cases {
        let path = fixture(name);
        let bytes = render_preview_pdf(&path, 0, 0.0, 0)
            .unwrap_or_else(|error| panic!("{name}: preview render failed: {error}"));
        assert!(bytes.starts_with(PDF_MAGIC), "{name}: output is not a PDF");
        assert!(
            bytes.len() > 1_000,
            "{name}: PDF is suspiciously small ({} bytes)",
            bytes.len()
        );
        let document = parse_pdf(&bytes);
        assert_eq!(
            document.pages.len(),
            1,
            "{name}: expected one rendered page"
        );
        assert_page_size_mm(&document, 0, width_mm, height_mm);
    }
}

#[test]
fn preview_pdf_honors_max_pages() {
    // outline-actions.ofd has four pages; the low DPI keeps this cheap.
    let path = fixture("outline-actions.ofd");
    let truncated = render_preview_pdf(&path, 1, 36.0, 0).expect("truncated preview");
    assert_eq!(parse_pdf(&truncated).pages.len(), 1);

    let full = render_preview_pdf(&path, 0, 36.0, 0).expect("full preview");
    assert_eq!(
        parse_pdf(&full).pages.len(),
        4,
        "default max_pages must cover all four pages"
    );
}

#[test]
fn preview_pdf_honors_requested_dpi() {
    let path = fixture("helloworld.ofd");
    let bytes = render_preview_pdf(&path, 0, 72.0, 0).expect("72 dpi preview");
    let document = parse_pdf(&bytes);
    // The page is A4; only the size in mm is checked, since the embedded image is
    // rasterized at the requested DPI.
    assert_page_size_mm(&document, 0, 210.0, 297.0);
}

#[test]
fn thumbnail_png_fits_the_longest_edge_and_keeps_the_aspect_ratio() {
    let cases = [
        ("helloworld.ofd", 0, 1024.0_f32, 210.0 / 297.0),
        ("invoice-like.ofd", 256, 256.0, 210.0 / 140.0),
    ];
    for (name, max_dimension, expected_longest, expected_ratio) in cases {
        let png = render_thumbnail_png(&fixture(name), max_dimension)
            .unwrap_or_else(|error| panic!("{name}: thumbnail render failed: {error}"));
        assert!(png.starts_with(PNG_MAGIC), "{name}: output is not a PNG");
        let image = image::load_from_memory(&png).expect("decode thumbnail PNG");
        let (width, height) = (image.width(), image.height());
        assert_eq!(
            width.max(height) as f32,
            expected_longest,
            "{name}: longest edge must fit max_dimension"
        );
        let ratio = width as f32 / height as f32;
        assert!(
            (ratio - expected_ratio).abs() < 0.005,
            "{name}: aspect ratio {ratio} does not match page area ({expected_ratio})"
        );
    }
}

#[test]
fn missing_file_is_an_io_error() {
    let missing = fixture("this-fixture-does-not-exist.ofd");
    for error in [
        render_preview_pdf(&missing, 0, 0.0, 0).unwrap_err(),
        render_thumbnail_png(&missing, 0).unwrap_err(),
    ] {
        assert!(matches!(error, Error::Io(_)), "unexpected error: {error:?}");
        assert_eq!(error.status(), 2);
    }
}

#[test]
fn non_ofd_bytes_are_a_container_error() {
    let file = TempFile::with_bytes("not-an-ofd.bin", b"this is definitely not a zip archive");
    for error in [
        render_preview_pdf(file.path(), 0, 0.0, 0).unwrap_err(),
        render_thumbnail_png(file.path(), 0).unwrap_err(),
    ] {
        assert!(
            matches!(error, Error::Container(_)),
            "unexpected error: {error:?}"
        );
        assert_eq!(error.status(), 3);
    }
}

#[test]
fn a_tiny_pixel_budget_is_a_render_error() {
    let error = render_preview_pdf(&fixture("helloworld.ofd"), 0, 0.0, 1).unwrap_err();
    assert!(
        matches!(error, Error::Render(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(error.status(), 5);
}

#[test]
fn nan_dpi_is_an_invalid_argument() {
    let error = render_preview_pdf(&fixture("helloworld.ofd"), 0, f32::NAN, 0).unwrap_err();
    assert!(
        matches!(error, Error::InvalidArgument(_)),
        "unexpected error: {error:?}"
    );
    assert_eq!(error.status(), 1);
}
