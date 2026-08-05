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
//! silently rot. Every fixture's exact parse-warning set is asserted. Clean
//! fixtures render in strict mode; allowlisted malformed real-world samples use
//! best-effort rendering so they stay useful without hiding new diagnostics.

use std::path::PathBuf;
use std::sync::Arc;

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
fn fallback_fonts() -> Vec<Arc<Vec<u8>>> {
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
    .map(Arc::new)
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

fn to_luma_thumb(img: image::DynamicImage) -> Vec<u8> {
    let small = img
        .resize_exact(THUMB, THUMB, FilterType::Triangle)
        .to_luma8();
    small.into_raw()
}

fn expected_warnings(stem: &str) -> &'static [&'static str] {
    match stem {
        "contains-jpeg" => &[
            "non-standard ST_Loc \"Doc_0/Res/Image_4.JPEG\" resolved as ZIP entry \"DOC_0/Res/Image_4.JPEG\"; paths are case-sensitive and use '/' separators",
            "non-standard ST_Loc \"Doc_0/Res/Image_8.JPEG\" resolved as ZIP entry \"DOC_0/Res/Image_8.JPEG\"; paths are case-sensitive and use '/' separators",
            "XML entry \"Doc_0/Document.xml\" has 1 unqualified element(s); first is <PhysicalBox>",
        ],
        "draw-param-ref" => &[
            "duplicate ST_ID 217 at Doc_0/TPLS/TPL_0/Content.xml <TextObject>; first declared at Doc_0/TPLS/TPL_0/Content.xml <TextObject>",
            "XML entry \"Doc_0/Annotation.xml\" has 2 unqualified element(s); first is <Parameters>",
            "Annotation Doc_0/Annotation.xml: Annot 300 missing required Creator",
            "Annotation Doc_0/Annotation.xml: Annot 300 missing required LastModDate",
            "unresolved ColorSpace resource id 57",
        ],
        "invoice-like" => &[
            "Document Doc_0/Document.xml: missing valid required CommonData/PageArea/PhysicalBox",
            "duplicate ST_ID 15 at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 7 at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <Layer>",
            "duplicate ST_ID 3 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/PublicRes.xml <DrawParam>",
            "duplicate ST_ID 8 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 9 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 10 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 11 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 12 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 13 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 19 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 22 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <PathObject>",
            "duplicate ST_ID 33 at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>; first declared at Doc_0/Tpls/Tpl_0/Content.xml <TextObject>",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 6952 missing required Creator",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 6952 missing required LastModDate",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 6956 missing required Creator",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 6956 missing required LastModDate",
        ],
        "multi-999" => &[
            "XML entry \"Doc_0/Annots/Page_0/Annotation.xml\" has 2 unqualified element(s); first is <Parameters>",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 7 missing required Creator",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 7 missing required LastModDate",
            "unresolved DrawParam resource id 4",
            "unresolved DrawParam resource id 602",
            "unresolved DrawParam resource id 768",
        ],
        "outline-actions" => &[
            "XML entry \"OFD.xml\" has 11 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <OFD>",
            "XML entry \"Doc_0/Document.xml\" has 40 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Document>",
            "XML entry \"Doc_0/PublicRes.xml\" has 9 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Res>",
            "XML entry \"Doc_0/DocumentRes.xml\" has 8 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Res>",
            "XML entry \"Doc_0/Pages/Page_0/Content.xml\" has 347 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Page>",
            "XML entry \"Doc_0/Pages/Page_1/Content.xml\" has 40 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Page>",
            "XML entry \"Doc_0/Pages/Page_2/Content.xml\" has 34 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Page>",
            "XML entry \"Doc_0/Pages/Page_3/Content.xml\" has 34 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Page>",
            "CommonData/MaxUnitID 10089 is smaller than declared ST_ID 11077",
        ],
        "path-clip" => &[
            "XML entry \"OFD.xml\" has 12 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <OFD>",
            "TemplatePage Doc_0/TPLS/TPL_0/Content.xml: malformed document: CGTransform code span 0..53 exceeds 44 source codes",
            "unresolved image resource id 0",
        ],
        "signout" => &[
            "Page Doc_0/Pages/Page_0/Content.xml: malformed document: CGTransform code span 0..1 exceeds 0 source codes",
            "XML entry \"Doc_0/Annots/Page_0/Annotation.xml\" has 1 unqualified element(s); first is <Remark>",
        ],
        "sample-1" => &[
            "XML entry \"Doc_0/Annots/Page_0/Annotation.xml\" has 2 unqualified element(s); first is <Parameters>",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 91 missing required Creator",
            "Annotation Doc_0/Annots/Page_0/Annotation.xml: Annot 91 missing required LastModDate",
        ],
        "v4-ride-right" => &[
            "PublicRes Doc_0/PublicRes.xml: unreadable: missing entry in OFD container: Doc_0/PublicRes.xml",
        ],
        "zsbk" => &[
            "TemplatePage Doc_0/TPLS/TPL_0/Content.xml: malformed document: CGTransform code span 0..1 exceeds 0 source codes",
            "TemplatePage Doc_0/TPLS/TPL_1/Content.xml: malformed document: CGTransform code span 0..1 exceeds 0 source codes",
        ],
        _ => &[],
    }
}

fn expected_strict_render_error(stem: &str) -> Option<&'static str> {
    match stem {
        "sample-1" => Some(
            "render error: vector seal contains 1 parse warning(s): unresolved ColorSpace resource id 4",
        ),
        _ => None,
    }
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
    let fonts = fallback_fonts();

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
        assert_eq!(
            doc.warnings,
            expected_warnings(&stem),
            "{stem}: parse warnings changed"
        );
        let expected_strict_error = expected_strict_render_error(&stem);
        if let Some(expected) = expected_strict_error {
            let strict_opts = RenderOptions {
                fallback_fonts: fonts.clone(),
                strict: true,
                ..Default::default()
            };
            let error = ofd_core::render::render_page_with(doc, 0, GOLDEN_DPI, &strict_opts)
                .expect_err("known malformed fixture must fail strict rendering");
            assert_eq!(error.to_string(), expected, "{stem}: strict error changed");
        }
        let opts = RenderOptions {
            fallback_fonts: fonts.clone(),
            // CLI --strict rejects parse warnings before rendering. Mirror that
            // contract here: strict-render valid fixtures, while exact parse or
            // nested-render diagnostics allowlist malformed viewer samples.
            strict: doc.warnings.is_empty() && expected_strict_error.is_none(),
            ..Default::default()
        };

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
            let golden_image =
                image::open(&golden).unwrap_or_else(|e| panic!("open {}: {e}", golden.display()));
            assert!(
                bmp.width.abs_diff(golden_image.width()) <= 1
                    && bmp.height.abs_diff(golden_image.height()) <= 1,
                "{stem} page {page}: dimensions {}x{} differ from golden {}x{}",
                bmp.width,
                bmp.height,
                golden_image.width(),
                golden_image.height()
            );
            let mine = thumb_from_rgba(&bmp.rgba, bmp.width, bmp.height);
            let gold = to_luma_thumb(golden_image);
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
