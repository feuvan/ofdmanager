//! Action / outline parsing against a real OFD sample.
//!
//! `outline-actions.ofd` (from the ofdrw reference test corpus) carries an
//! `Outlines` tree whose nodes each hold a `CLICK` `Goto/Dest` action — the
//! canonical way OFD bookmarks navigate. This verifies the §14 action parser and
//! §7 outline parser on real-world data: nodes parse, their goto-dest targets
//! resolve to page indices, and the action behavior is modeled.

use ofd_core::model::{ActionEvent, ActionKind, GotoTarget};

#[test]
fn outline_goto_actions_parse_and_resolve() {
    let bytes = std::fs::read("../../fixtures/outline-actions.ofd").unwrap();
    let pkg = ofd_core::parser::parse(bytes).unwrap();
    let doc = &pkg.documents[0];

    assert!(!doc.outline.is_empty(), "outline tree should be parsed");

    let mut goto_dest = 0;
    let mut resolved = 0;
    for node in &doc.outline {
        for action in &node.actions {
            assert_eq!(action.event, ActionEvent::Click);
            if let ActionKind::Goto(GotoTarget::Dest(d)) = &action.kind {
                goto_dest += 1;
                // The destination references a real page id in the document.
                assert!(doc.pages.iter().any(|p| p.id == d.page_id));
            }
        }
        if node.page_index.is_some() {
            resolved += 1;
        }
    }
    assert!(goto_dest > 0, "expected outline goto-dest actions");
    assert!(
        resolved > 0,
        "at least one outline node should resolve to a page index"
    );
}
