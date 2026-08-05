//! Regression: zsbk's bare-CFF embedded font (AdobeSongStd-Light, id 766) must
//! be usable via the synthesised OTTO wrapper — no parse warning, and it
//! resolves to a face ttf-parser can outline.
use std::path::PathBuf;
fn fdir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

#[test]
fn bare_cff_font_is_usable() {
    let pkg = ofd_core::open(std::fs::read(fdir().join("zsbk.ofd")).unwrap()).unwrap();
    let doc = &pkg.documents[0];

    // No embedded font should be reported unparseable anymore.
    assert!(
        !doc.warnings.iter().any(|w| w.contains("failed to parse")),
        "unexpected font warnings: {:?}",
        doc.warnings
    );

    // Font 766 (bare CFF) resolves to a parseable face.
    let mut r = ofd_core::fonts::FontResolver::new(&doc.resources.fonts);
    let rf = r.resolve(766).expect("font 766 resolves");
    let face = ttf_parser::Face::parse(&rf.data, rf.index).expect("wrapped CFF parses");
    assert!(face.number_of_glyphs() > 0);

    // Re-processing an already wrapped CID-keyed CFF must retain its CID→GID
    // mapping; otherwise explicit OFD glyph ids address the wrong outlines.
    let original_map = rf.cid_to_gid.as_ref().expect("CID-keyed CFF map");
    let prepared = ofd_core::cff::usable_font(&rf.data).expect("wrapped CFF remains usable");
    assert_eq!(
        prepared.cid_to_gid.as_ref().map(|m| m.len()),
        Some(original_map.len())
    );
}
