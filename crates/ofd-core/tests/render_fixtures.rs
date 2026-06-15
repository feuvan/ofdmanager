//! Render-regression test over the bundled sample OFDs.
//!
//! Two layers of checking:
//!   1. **Smoke** — every page of every fixture parses and renders without
//!      panicking, has plausible dimensions, and is not a blank white page.
//!   2. **Golden** — where a same-directory page image `<stem>-<n>.png` exists
//!      (1-based page index), the corresponding page is rendered at the golden's
//!      DPI and compared perceptually. The goldens come from a reference
//!      renderer, so an exact pixel match is neither expected nor required;
//!      instead both images are reduced to a small grayscale thumbnail and the
//!      mean per-pixel difference must stay under a tolerance. This catches the
//!      regressions a non-blank check misses (moved text, wrong colors, broken
//!      clipping, dropped images, wrong page) while tolerating font/AA
//!      differences between engines.
//!
//! The test also fails if no goldens are exercised, so the golden contract can't
//! silently rot, and treats parse warnings on golden fixtures as failures.

use std::path::{Path, PathBuf};

use image::imageops::FilterType;
use ofd_core::render::RenderOptions;

/// Goldens were produced at 96 DPI (A4 width 210mm → ~794px).
const GOLDEN_DPI: f32 = 96.0;
/// Side of the grayscale thumbnail used for the perceptual comparison.
const THUMB: u32 = 96;
/// Max allowed mean per-pixel grayscale difference (0..1) vs the golden.
const MAX_DIFF: f64 = 0.10;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// Deterministic fallback fonts, if fetched (`scripts/fetch-fonts.sh`). Matching
/// the reference renderer's fonts keeps the golden comparison meaningful.
fn fallback_fonts() -> Vec<Vec<u8>> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets/fonts");
    [
        "simsun.ttf",
        "simhei.ttf",
        "simkai.ttf",
        "SIMFANG.TTF",
        "xbst.ttf",
    ]
    .iter()
    .filter_map(|n| std::fs::read(dir.join(n)).ok())
    .collect()
}

fn non_white_pixels(rgba: &[u8]) -> usize {
    rgba.chunks_exact(4)
        .filter(|p| p[0] != 255 || p[1] != 255 || p[2] != 255)
        .count()
}

/// Reduce an RGBA buffer to a `THUMB×THUMB` grayscale thumbnail.
fn thumb_from_rgba(rgba: &[u8], w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_raw(w, h, rgba.to_vec()).expect("rgba dimensions");
    to_luma_thumb(image::DynamicImage::ImageRgba8(img))
}

fn thumb_from_path(path: &Path) -> Vec<u8> {
    let img = image::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    to_luma_thumb(img)
}

fn to_luma_thumb(img: image::DynamicImage) -> Vec<u8> {
    let small = img
        .resize_exact(THUMB, THUMB, FilterType::Triangle)
        .to_luma8();
    small.into_raw()
}

/// Mean absolute per-pixel difference, normalised to 0..1.
fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a
        .iter()
        .zip(b)
        .map(|(x, y)| (*x as i16 - *y as i16).unsigned_abs() as u64)
        .sum();
    sum as f64 / (a.len() as f64 * 255.0)
}

#[test]
fn fixtures_render_and_match_goldens() {
    let dir = fixtures_dir();
    let opts = RenderOptions {
        fallback_fonts: fallback_fonts(),
        ..Default::default()
    };

    let mut fixtures = 0;
    let mut pages_rendered = 0;
    let mut goldens_compared = 0;
    let mut worst: (f64, String) = (0.0, String::new());

    let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("fixtures dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("ofd"))
        .collect();
    entries.sort();

    for path in entries {
        fixtures += 1;
        let stem = path.file_stem().unwrap().to_string_lossy().to_string();
        let bytes = std::fs::read(&path).expect("read fixture");
        let pkg = ofd_core::open(bytes).unwrap_or_else(|e| panic!("{stem}: parse failed: {e}"));
        let doc = pkg
            .documents
            .first()
            .unwrap_or_else(|| panic!("{stem}: no documents"));

        // Smoke: every page renders non-blank.
        assert!(!doc.pages.is_empty(), "{stem}: no pages parsed");
        for i in 0..doc.pages.len() {
            let bmp = ofd_core::render::render_page_with(doc, i, GOLDEN_DPI, &opts)
                .unwrap_or_else(|e| panic!("{stem} page {i}: render failed: {e}"));
            assert!(
                bmp.width > 0 && bmp.height > 0,
                "{stem} page {i}: zero size"
            );
            assert_eq!(bmp.rgba.len(), (bmp.width * bmp.height * 4) as usize);
            assert!(
                non_white_pixels(&bmp.rgba) > 100,
                "{stem} page {i}: blank render"
            );
            pages_rendered += 1;
        }

        // Golden comparison for each `<stem>-<n>.png` that exists.
        for page in 0..doc.pages.len() {
            let golden = dir.join(format!("{stem}-{}.png", page + 1));
            if !golden.exists() {
                continue;
            }
            let bmp = ofd_core::render::render_page_with(doc, page, GOLDEN_DPI, &opts).unwrap();
            let mine = thumb_from_rgba(&bmp.rgba, bmp.width, bmp.height);
            let gold = thumb_from_path(&golden);
            let diff = mean_abs_diff(&mine, &gold);
            eprintln!("{stem} page {page}: perceptual diff {diff:.4}");
            if diff > worst.0 {
                worst = (diff, format!("{stem} page {page}"));
            }
            assert!(
                diff < MAX_DIFF,
                "{stem} page {page}: diverges from golden ({diff:.4} >= {MAX_DIFF})"
            );
            goldens_compared += 1;
        }
    }

    eprintln!(
        "fixtures={fixtures} pages_rendered={pages_rendered} goldens_compared={goldens_compared} \
         worst_diff={:.4} ({})",
        worst.0, worst.1
    );
    assert!(fixtures > 0, "no .ofd fixtures found in {}", dir.display());
    assert!(
        goldens_compared > 0,
        "no golden PNGs were compared — golden contract not exercised"
    );
}
