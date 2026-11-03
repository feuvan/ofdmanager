//! Fixture-based regression tests for `<TextCode>` whitespace handling.
//!
//! GB/T 33190 §11.3 asks producers to hex-escape significant spaces, but
//! real-world OFD files go both ways and the layout data settles the intent:
//!
//! * literal U+0020 spaces are **glyph slots** — the producer gives each one its
//!   own `DeltaX` advance (airline itinerary invoices, `invoice-like` /
//!   `draw-param-ref` below), and
//! * serializer formatting whitespace (a pretty-printed newline plus its
//!   indentation) is **not** a slot — a `CGTransform`/`DeltaX` sequence is only
//!   internally consistent when it is ignored (see `z.ofd` upstream).
//!
//! Dropping a literal space collapses glyphs onto each other and shifts the rest
//! of the run, so this is pinned here against real producer files rather than
//! only against hand-written unit XML.

use std::path::PathBuf;

use ofd_core::model::{GraphicObject, TextObject};

fn parse_fixture(name: &str) -> ofd_core::OfdPackage {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures")
        .join(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {name}: {e}"));
    ofd_core::open(bytes).unwrap_or_else(|e| panic!("parse {name}: {e}"))
}

/// Every `TextRun` on every page of the first document, in paint order.
fn all_text_objects(pkg: &ofd_core::OfdPackage) -> Vec<&TextObject> {
    fn collect<'a>(objects: &'a [GraphicObject], out: &mut Vec<&'a TextObject>) {
        for object in objects {
            match object {
                GraphicObject::Text(text) => out.push(text),
                GraphicObject::Group(inner) => collect(inner, out),
                GraphicObject::Path(_) | GraphicObject::Image(_) | GraphicObject::Composite(_) => {}
            }
        }
    }

    let document = pkg.documents.first().expect("document");
    let mut out = Vec::new();
    for page in &document.pages {
        for layer in &page.layers {
            collect(&layer.objects, &mut out);
        }
    }
    out
}

fn run_texts<'a>(objects: &'a [&'a TextObject]) -> Vec<&'a str> {
    objects
        .iter()
        .flat_map(|text| text.runs.iter().map(|run| run.text.as_str()))
        .collect()
}

#[test]
fn literal_interior_spaces_are_glyph_slots() {
    // `收款人:李蓉蓉;    复核人:周欣铭;    ` — the four-space run before
    // `复核人` is a layout slot with its own DeltaX entries; dropping it makes
    // `复核人` overlap `;`.
    let pkg = parse_fixture("invoice-like.ofd");
    let objects = all_text_objects(&pkg);
    let texts = run_texts(&objects);
    assert!(
        texts.contains(&"收款人:李蓉蓉;    复核人:周欣铭;    "),
        "remark row lost its literal spaces: {texts:?}"
    );

    // A TextCode made only of spaces is content too (blank placeholder), not
    // formatting to strip away.
    assert!(
        texts.contains(&"    "),
        "space-only TextCode was stripped: {texts:?}"
    );

    // Page footer of the `draw-param-ref` sample: the leading spaces are glyph
    // slots used to place the page number.
    let pkg = parse_fixture("draw-param-ref.ofd");
    let objects = all_text_objects(&pkg);
    let texts = run_texts(&objects);
    assert!(
        texts.contains(&"  4            1"),
        "footer spacing was stripped: {texts:?}"
    );

    // Latin run: the space between "OFD" and "Reader" is a slot as well.
    let pkg = parse_fixture("helloworld.ofd");
    let objects = all_text_objects(&pkg);
    let texts = run_texts(&objects);
    assert!(
        texts.contains(&"你好呀，OFD Reader&Writer！"),
        "helloworld run lost its space: {texts:?}"
    );
}

#[test]
fn formatting_whitespace_never_becomes_a_glyph_slot() {
    // XML serializer formatting (CR/LF/TAB) must be gone before glyph slots are
    // counted; no real fixture should keep any. Unit tests in `parser.rs` cover
    // the pretty-printed `"\r\n  text"` case that no checked-in fixture has.
    for name in [
        "invoice-like.ofd",
        "draw-param-ref.ofd",
        "helloworld.ofd",
        "path-clip.ofd",
        "signout.ofd",
        "zsbk.ofd",
    ] {
        let pkg = parse_fixture(name);
        let objects = all_text_objects(&pkg);
        for text in run_texts(&objects) {
            assert!(
                !text.contains(['\r', '\n', '\t']),
                "{name}: formatting whitespace survived in {text:?}"
            );
        }
    }
}
