use std::path::PathBuf;

use ofd_core::geom::{Matrix, Rect};
use ofd_core::model::*;
use ofd_core::render::{render_page_with, RenderOptions};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

#[test]
fn text_shape_rasterizes_into_clip_mask() {
    let package = ofd_core::open(std::fs::read(fixtures_dir().join("zsbk.ofd")).unwrap()).unwrap();
    let source_font = package.documents[0]
        .resources
        .fonts
        .iter()
        .find(|font| font.id == 766)
        .expect("fixture embedded font")
        .clone();
    let prepared = ofd_core::cff::usable_font(source_font.data.as_deref().unwrap()).unwrap();
    let face = ttf_parser::Face::parse(&prepared.data, 0).unwrap();
    let cid_map = prepared.cid_to_gid.as_ref().expect("CID-keyed font");
    let (&cid, _) = cid_map
        .iter()
        .find(|(_, gid)| {
            **gid != 0
                && face
                    .glyph_bounding_box(ttf_parser::GlyphId(**gid))
                    .is_some_and(|bounds| bounds.width() > 0 && bounds.height() > 0)
        })
        .expect("font has an outlined glyph");

    let clip_text = TextObject {
        common: GraphicCommon {
            boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
            ..Default::default()
        },
        font_id: 766,
        font_size: 8.0,
        stroke: false,
        fill: true,
        h_scale: 1.0,
        read_direction: Direction(0),
        char_direction: Direction(0),
        weight: 400,
        italic: false,
        fill_color: None,
        stroke_color: None,
        cg_transforms: vec![CgTransform {
            code_position: 0,
            code_count: 1,
            glyphs: vec![cid],
            glyph_count: 1,
        }],
        runs: vec![TextRun {
            text: "x".into(),
            origin_x: 3.0,
            origin_y: 8.0,
            delta_x: Vec::new(),
            delta_y: Vec::new(),
        }],
    };

    let common = GraphicCommon {
        boundary: Rect::new(0.0, 0.0, 20.0, 10.0),
        clips: vec![Clip {
            areas: vec![ClipArea {
                ctm: Matrix::IDENTITY,
                draw_param: None,
                shape: ClipShape::Text(Box::new(clip_text)),
            }],
        }],
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
            PathCommand::LineTo { x: 20.0, y: 0.0 },
            PathCommand::LineTo { x: 20.0, y: 10.0 },
            PathCommand::LineTo { x: 0.0, y: 10.0 },
            PathCommand::Close,
        ],
    };
    let document = Document {
        page_area: PageArea {
            physical_box: Some(Rect::new(0.0, 0.0, 20.0, 10.0)),
            ..Default::default()
        },
        pages: vec![Page {
            id: 1,
            area: None,
            layers: vec![Layer {
                id: 1,
                kind: LayerKind::Body,
                draw_param: None,
                objects: vec![GraphicObject::Path(path)],
            }],
            actions: Vec::new(),
        }],
        resources: Resources {
            fonts: vec![source_font],
            ..Default::default()
        },
        ..Default::default()
    };

    let bitmap = render_page_with(
        &document,
        0,
        96.0,
        &RenderOptions {
            strict: true,
            ..Default::default()
        },
    )
    .unwrap();
    let painted = bitmap
        .rgba
        .chunks_exact(4)
        .filter(|pixel| pixel[0] < 128)
        .count();
    assert!(painted > 5, "text outline should create a non-empty clip");
    assert!(
        painted < (bitmap.width * bitmap.height / 2) as usize,
        "clip should restrict the page-filling path to the glyph outline"
    );
}
