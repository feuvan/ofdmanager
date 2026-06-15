//! XML → model parser. Reads the container entries (`OFD.xml`,
//! `Document.xml`, per-page `Content.xml`, resource XML) using a read-only DOM
//! ([`roxmltree`]) and produces the [`crate::model`] types.
//!
//! Element names are matched by their *local* name (the `ofd:` namespace prefix
//! is resolved away by the XML parser), so producers that use different prefixes
//! still parse.

use std::collections::HashMap;

use roxmltree::{Document as XmlDoc, Node};

use crate::container::Container;
use crate::error::{OfdError, Result};
use crate::geom::{Matrix, Point, Rect};
use crate::model::*;

/// Parse a whole OFD package from raw file bytes.
pub fn parse(bytes: Vec<u8>) -> Result<OfdPackage> {
    let mut c = Container::open(bytes)?;
    let root_xml = read_str(&mut c, "OFD.xml")?;
    let xml = XmlDoc::parse(&root_xml)?;
    let ofd = xml.root_element();

    let mut documents = Vec::new();
    for doc_body in children(ofd, "DocBody") {
        let metadata = doc_body
            .children()
            .find(|n| local(n) == "DocInfo")
            .map(parse_doc_info)
            .unwrap_or_default();
        if let Some(doc_root) = child_text(doc_body, "DocRoot") {
            let signatures = child_text(doc_body, "Signatures");
            documents.push(parse_document(&mut c, &doc_root, signatures, metadata)?);
        }
    }
    Ok(OfdPackage {
        version: ofd.attribute("Version").map(|s| s.to_string()),
        doc_type: ofd.attribute("DocType").map(|s| s.to_string()),
        documents,
    })
}

fn parse_doc_info(node: Node) -> Metadata {
    Metadata {
        title: child_text(node, "Title"),
        author: child_text(node, "Author"),
        subject: child_text(node, "Subject"),
        creator: child_text(node, "Creator"),
        creator_version: child_text(node, "CreatorVersion"),
        creation_date: child_text(node, "CreationDate"),
        doc_id: child_text(node, "DocID"),
    }
}

fn parse_document(
    c: &mut Container,
    doc_root: &str,
    signatures: Option<String>,
    metadata: Metadata,
) -> Result<Document> {
    let dir = parent_dir(doc_root); // e.g. "Doc_0"
    let doc_xml = read_str(c, doc_root)?;
    let xml = XmlDoc::parse(&doc_xml)?;
    let root = xml.root_element();

    let common = child(root, "CommonData");
    let page_area = common
        .and_then(|cd| child(cd, "PageArea"))
        .map(parse_page_area)
        .unwrap_or_default();
    let max_unit_id = common
        .and_then(|cd| child_text(cd, "MaxUnitID"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let default_color_space = common
        .and_then(|cd| child_text(cd, "DefaultCS"))
        .and_then(|s| s.parse().ok());

    // Non-fatal parse problems collected here so callers can observe or fail.
    let mut warnings: Vec<String> = Vec::new();
    // Declared-but-absent media (id → file): common in invoice templates that
    // list every province's seal but ship only the used one. We only warn for
    // those actually referenced by an object (checked after pages parse).
    let mut missing_media: HashMap<u64, String> = HashMap::new();

    // Resources (PublicRes + DocumentRes), each path relative to the doc dir.
    // A referenced resource file that is missing or malformed is recorded as a
    // warning rather than silently dropped: the render may then be missing
    // fonts/images/styles, which we want to be observable.
    let mut resources = Resources::default();
    if let Some(cd) = common {
        for tag in ["PublicRes", "DocumentRes"] {
            for res_path in cd.children().filter(|n| local(n) == tag) {
                if let Some(p) = res_path.text() {
                    let full = join(&dir, p.trim());
                    match read_str(c, &full) {
                        Ok(s) => match XmlDoc::parse(&s) {
                            Ok(rx) => parse_resources(
                                c,
                                rx.root_element(),
                                &dir,
                                &mut resources,
                                &mut warnings,
                                &mut missing_media,
                            ),
                            Err(e) => warnings.push(format!("{tag} {full}: malformed xml: {e}")),
                        },
                        Err(e) => warnings.push(format!("{tag} {full}: unreadable: {e}")),
                    }
                }
            }
        }
    }

    // Template pages, keyed by their declared ID, parsed into reusable layers.
    let mut templates: HashMap<String, Vec<Layer>> = HashMap::new();
    if let Some(cd) = common {
        for tpl in cd.children().filter(|n| local(n) == "TemplatePage") {
            if let (Some(id), Some(base)) = (tpl.attribute("ID"), tpl.attribute("BaseLoc")) {
                let full = join(&dir, base);
                match read_str(c, &full).map_err(|e| e.to_string()).and_then(|s| {
                    XmlDoc::parse(&s)
                        .map(|px| parse_page_layers(px.root_element()))
                        .map_err(|e| e.to_string())
                }) {
                    Ok(layers) => {
                        templates.insert(id.to_string(), layers);
                    }
                    Err(e) => warnings.push(format!("TemplatePage {full}: {e}")),
                }
            }
        }
    }

    // Pages.
    let mut pages = Vec::new();
    if let Some(pages_node) = child(root, "Pages") {
        for page_node in pages_node.children().filter(|n| local(n) == "Page") {
            if let Some(base) = page_node.attribute("BaseLoc") {
                let id = page_node
                    .attribute("ID")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                let full = join(&dir, base);
                let s = read_str(c, &full)?;
                let px = XmlDoc::parse(&s)?;
                let mut page = parse_page(px.root_element(), &templates);
                page.id = id;
                pages.push(page);
            }
        }
    }

    // Signatures / electronic seals (referenced from OFD.xml's DocBody).
    let mut seals = Vec::new();
    let mut sig_models = Vec::new();
    if let Some(sig_path) = signatures {
        parse_signatures(c, &sig_path, &mut seals, &mut sig_models, &mut warnings);
    }

    // Page annotations (referenced from Document.xml).
    let mut annotations = Vec::new();
    if let Some(annots_ref) = child_text(root, "Annotations") {
        parse_annotations(c, &join(&dir, &annots_ref), &mut annotations, &mut warnings);
    }

    // Warn only for missing media that is actually referenced by an image
    // object (skipping the unused template declarations).
    if !missing_media.is_empty() {
        let mut referenced = std::collections::HashSet::new();
        for page in &pages {
            for layer in &page.layers {
                collect_image_refs(&layer.objects, &mut referenced);
            }
        }
        for a in &annotations {
            collect_image_refs(&a.objects, &mut referenced);
        }
        for id in referenced {
            if let Some(file) = missing_media.get(&id) {
                warnings.push(format!(
                    "image {file} (id {id}) referenced but missing from container"
                ));
            }
        }
    }

    Ok(Document {
        max_unit_id,
        page_area,
        default_color_space,
        pages,
        resources,
        outline: Vec::new(),
        metadata,
        seals,
        annotations,
        signatures: sig_models,
        warnings,
    })
}

/// Parse a `CT_PageArea`: PhysicalBox plus the optional Application/Content/Bleed
/// boxes (all mm).
fn parse_page_area(node: Node) -> PageArea {
    PageArea {
        physical_box: child_text(node, "PhysicalBox")
            .as_deref()
            .and_then(parse_rect),
        application_box: child_text(node, "ApplicationBox")
            .as_deref()
            .and_then(parse_rect),
        content_box: child_text(node, "ContentBox")
            .as_deref()
            .and_then(parse_rect),
        bleed_box: child_text(node, "BleedBox").as_deref().and_then(parse_rect),
    }
}

/// Parse page annotations: `Annotations.xml` → per-page `Annotation.xml` →
/// each `Annot`'s `Appearance` graphic objects, with the appearance origin
/// baked into each object's boundary so they render like page content.
fn parse_annotations(
    c: &mut Container,
    annotations_path: &str,
    out: &mut Vec<Annotation>,
    warnings: &mut Vec<String>,
) {
    let Ok(list_xml) = read_str(c, annotations_path) else {
        warnings.push(format!("Annotations {annotations_path}: unreadable"));
        return;
    };
    let Ok(list) = XmlDoc::parse(&list_xml) else {
        warnings.push(format!("Annotations {annotations_path}: malformed xml"));
        return;
    };
    let annots_dir = parent_dir(annotations_path);

    for page in list
        .root_element()
        .descendants()
        .filter(|n| local(n) == "Page")
    {
        let page_id = page
            .attribute("PageID")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let Some(file_loc) = child_text(page, "FileLoc") else {
            continue;
        };
        let path = join(&annots_dir, &file_loc);
        let Ok(annot_xml) = read_str(c, &path) else {
            warnings.push(format!("Annotation {path}: unreadable"));
            continue;
        };
        let Ok(adoc) = XmlDoc::parse(&annot_xml) else {
            warnings.push(format!("Annotation {path}: malformed xml"));
            continue;
        };

        for annot in adoc
            .root_element()
            .descendants()
            .filter(|n| local(n) == "Annot")
        {
            let Some(app) = child(annot, "Appearance") else {
                continue;
            };
            let annot_type = annot.attribute("Type").unwrap_or("").to_string();
            let (ox, oy) = app
                .attribute("Boundary")
                .and_then(parse_rect)
                .map(|r| (r.x, r.y))
                .unwrap_or((0.0, 0.0));
            let mut objects = Vec::new();
            for obj in app.children().filter(|n| n.is_element()) {
                if let Some(mut o) = parse_object(obj) {
                    offset_object(&mut o, ox, oy);
                    objects.push(o);
                }
            }
            if !objects.is_empty() {
                out.push(Annotation {
                    page_id,
                    annot_type,
                    objects,
                });
            }
        }
    }
}

/// Translate an object's placement by `(dx, dy)` mm (used to bake an
/// annotation appearance's origin into its contained objects).
fn offset_object(obj: &mut GraphicObject, dx: f32, dy: f32) {
    match obj {
        GraphicObject::Text(t) => {
            t.common.boundary.x += dx;
            t.common.boundary.y += dy;
        }
        GraphicObject::Path(p) => {
            p.common.boundary.x += dx;
            p.common.boundary.y += dy;
        }
        GraphicObject::Image(i) => {
            i.common.boundary.x += dx;
            i.common.boundary.y += dy;
        }
        GraphicObject::Group(g) => {
            for child in g {
                offset_object(child, dx, dy);
            }
        }
    }
}

/// Parse the signature list (`Signatures.xml`, §18.1) and each signature
/// description (`Signature.xml`, §18.2). Builds the full [`Signature`] model for
/// verification, and — for `Seal`-type signatures — the renderable stamp
/// appearances ([`Seal`]).
fn parse_signatures(
    c: &mut Container,
    signatures_path: &str,
    seals: &mut Vec<Seal>,
    sigs: &mut Vec<Signature>,
    warnings: &mut Vec<String>,
) {
    let Ok(list_xml) = read_str(c, signatures_path) else {
        warnings.push(format!("Signatures {signatures_path}: unreadable"));
        return;
    };
    let Ok(list) = XmlDoc::parse(&list_xml) else {
        warnings.push(format!("Signatures {signatures_path}: malformed xml"));
        return;
    };

    for sig in list
        .root_element()
        .descendants()
        .filter(|n| local(n) == "Signature")
    {
        let sig_type = match sig.attribute("Type") {
            Some(t) if t.eq_ignore_ascii_case("Sign") => SignatureType::Sign,
            _ => SignatureType::Seal,
        };
        let id = sig.attribute("ID").unwrap_or("").to_string();
        let Some(base) = sig.attribute("BaseLoc") else {
            continue;
        };
        let Ok(sig_xml) = read_str(c, base) else {
            warnings.push(format!("Signature {base}: unreadable"));
            continue;
        };
        let Ok(doc) = XmlDoc::parse(&sig_xml) else {
            warnings.push(format!("Signature {base}: malformed xml"));
            continue;
        };
        let root = doc.root_element();
        let signed_info = child(root, "SignedInfo");

        // References (the protected files and their digests).
        let mut references = Vec::new();
        if let Some(refs) = signed_info.and_then(|si| child(si, "References")) {
            let check_method = refs.attribute("CheckMethod").unwrap_or("MD5").to_string();
            for r in refs.children().filter(|n| local(n) == "Reference") {
                if let Some(file_ref) = r.attribute("FileRef") {
                    references.push(SignReference {
                        file_ref: file_ref.to_string(),
                        check_method: check_method.clone(),
                        check_value: child_text(r, "CheckValue").unwrap_or_default(),
                    });
                }
            }
        }
        let signed_value = root
            .descendants()
            .find(|n| local(n) == "SignedValue")
            .and_then(|n| n.text().map(|s| s.trim().to_string()));

        sigs.push(Signature {
            id,
            sig_type,
            provider: signed_info
                .and_then(|si| child(si, "Provider"))
                .and_then(|p| p.attribute("ProviderName"))
                .map(|s| s.to_string()),
            signature_method: signed_info.and_then(|si| child_text(si, "SignatureMethod")),
            signature_date_time: signed_info.and_then(|si| child_text(si, "SignatureDateTime")),
            references,
            signed_value: signed_value.clone(),
        });

        // Only `Seal`-type signatures carry a renderable stamp picture (§18.1):
        // from a standalone `Seal.esl` or embedded in `SignedValue.dat`.
        if sig_type == SignatureType::Sign {
            continue;
        }
        let seal_loc = root
            .descendants()
            .find(|n| local(n) == "Seal")
            .and_then(|s| child_text(s, "BaseLoc"));
        let appearance = [seal_loc, signed_value]
            .into_iter()
            .flatten()
            .filter_map(|loc| read_bytes(c, &loc).ok())
            .find_map(|bytes| extract_seal_appearance(&bytes));
        let Some(appearance) = appearance else {
            warnings.push(format!("Signature {base}: no renderable seal picture"));
            continue;
        };
        for stamp in root.descendants().filter(|n| local(n) == "StampAnnot") {
            let page_id = stamp
                .attribute("PageRef")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            if let Some(boundary) = stamp.attribute("Boundary").and_then(parse_rect) {
                seals.push(Seal {
                    page_id,
                    boundary,
                    appearance: appearance.clone(),
                });
            }
        }
    }
}

/// Map a structurally-decoded SES seal picture (GB/T 38540, see [`crate::ses`])
/// to a renderable appearance.
fn extract_seal_appearance(esl: &[u8]) -> Option<SealAppearance> {
    let pic = crate::ses::extract_seal_picture(esl)?;
    Some(match pic.kind.as_str() {
        "ofd" => SealAppearance::Ofd(pic.data),
        "png" => SealAppearance::Raster {
            format: ImageFormat::Png,
            data: pic.data,
        },
        "jpg" | "jpeg" => SealAppearance::Raster {
            format: ImageFormat::Jpeg,
            data: pic.data,
        },
        "bmp" => SealAppearance::Raster {
            format: ImageFormat::Bmp,
            data: pic.data,
        },
        _ => SealAppearance::Raster {
            format: ImageFormat::Unknown,
            data: pic.data,
        },
    })
}

/// Parse a single page, prepending any referenced template layers underneath.
fn parse_page(page: Node, templates: &HashMap<String, Vec<Layer>>) -> Page {
    let area = child(page, "Area").map(parse_page_area);

    let mut layers = Vec::new();
    // `<ofd:Template TemplateID="1"/>` pulls in a template page's content first.
    for tpl_ref in page.children().filter(|n| local(n) == "Template") {
        if let Some(id) = tpl_ref.attribute("TemplateID") {
            if let Some(tpl_layers) = templates.get(id) {
                layers.extend(tpl_layers.iter().cloned());
            }
        }
    }
    layers.extend(parse_page_layers(page));
    Page {
        id: 0,
        area,
        layers,
    }
}

/// Extract the drawing layers from a page or template page node.
fn parse_page_layers(page: Node) -> Vec<Layer> {
    let mut layers = Vec::new();
    if let Some(content) = child(page, "Content") {
        for layer_node in content.children().filter(|n| local(n) == "Layer") {
            let kind = match layer_node.attribute("Type") {
                Some("Background") => LayerKind::Background,
                Some("Foreground") => LayerKind::Foreground,
                Some("Custom") => LayerKind::Custom,
                _ => LayerKind::Body,
            };
            let mut objects = Vec::new();
            for obj in layer_node.children().filter(|n| n.is_element()) {
                if let Some(o) = parse_object(obj) {
                    objects.push(o);
                }
            }
            // A Layer's `@DrawParam` is the default style for objects on it;
            // objects without their own DrawParam inherit it (§8.x / Layer).
            let draw_param = layer_node
                .attribute("DrawParam")
                .and_then(|s| s.parse().ok());
            if let Some(dp) = draw_param {
                for o in &mut objects {
                    inherit_draw_param(o, dp);
                }
            }
            layers.push(Layer {
                id: attr_u64(layer_node, "ID"),
                kind,
                draw_param,
                objects,
            });
        }
    }
    layers
}

/// Set an object's DrawParam to `dp` when it has none of its own, recursing
/// into groups (objects inherit the containing layer's default style).
fn inherit_draw_param(obj: &mut GraphicObject, dp: u64) {
    let common = match obj {
        GraphicObject::Text(t) => &mut t.common,
        GraphicObject::Path(p) => &mut p.common,
        GraphicObject::Image(i) => &mut i.common,
        GraphicObject::Group(g) => {
            for c in g {
                inherit_draw_param(c, dp);
            }
            return;
        }
    };
    common.draw_param.get_or_insert(dp);
}

fn parse_object(node: Node) -> Option<GraphicObject> {
    match local(&node) {
        "TextObject" => Some(GraphicObject::Text(parse_text(node))),
        "PathObject" => Some(GraphicObject::Path(parse_path(node))),
        "ImageObject" => Some(GraphicObject::Image(parse_image(node))),
        "PageBlock" => Some(GraphicObject::Group(parse_page_block_children(node))),
        _ => None,
    }
}

fn parse_page_block_children(node: Node) -> Vec<GraphicObject> {
    let mut group = Vec::new();
    for child in node.children().filter(|n| n.is_element()) {
        if let Some(o) = parse_object(child) {
            group.push(o);
        }
    }
    group
}

fn parse_common(node: Node) -> GraphicCommon {
    GraphicCommon {
        id: attr_u64(node, "ID"),
        boundary: node
            .attribute("Boundary")
            .and_then(parse_rect)
            .unwrap_or(Rect::new(0.0, 0.0, 0.0, 0.0)),
        name: node.attribute("Name").map(|s| s.to_string()),
        visible: node
            .attribute("Visible")
            .map(|s| s != "false")
            .unwrap_or(true),
        ctm: node
            .attribute("CTM")
            .and_then(parse_matrix)
            .unwrap_or(Matrix::IDENTITY),
        draw_param: node.attribute("DrawParam").and_then(|s| s.parse().ok()),
        line_width: node
            .attribute("LineWidth")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.353),
        cap: parse_cap(node.attribute("Cap")),
        join: parse_join(node.attribute("Join")),
        miter_limit: node.attribute("MiterLimit").and_then(|s| s.parse().ok()),
        dash_offset: node.attribute("DashOffset").and_then(|s| s.parse().ok()),
        dash_pattern: node
            .attribute("DashPattern")
            .map(parse_floats)
            .filter(|v| !v.is_empty()),
        alpha: node
            .attribute("Alpha")
            .and_then(|s| s.parse::<f32>().ok())
            .map(|a| a.clamp(0.0, 255.0) as u8)
            .unwrap_or(255),
        clip: parse_clips(node),
    }
}

fn parse_cap(s: Option<&str>) -> LineCap {
    match s {
        Some("Round") => LineCap::Round,
        Some("Square") => LineCap::Square,
        _ => LineCap::Butt,
    }
}

fn parse_join(s: Option<&str>) -> LineJoin {
    match s {
        Some("Round") => LineJoin::Round,
        Some("Bevel") => LineJoin::Bevel,
        _ => LineJoin::Miter,
    }
}

/// Parse an object's `Clips/Clip/Area` regions (path-based areas only; text
/// clips are uncommon and not yet supported).
fn parse_clips(node: Node) -> Vec<ClipArea> {
    let mut out = Vec::new();
    let Some(clips) = child(node, "Clips") else {
        return out;
    };
    for clip in clips.children().filter(|n| local(n) == "Clip") {
        for area in clip.children().filter(|n| local(n) == "Area") {
            let ctm = area
                .attribute("CTM")
                .and_then(parse_matrix)
                .unwrap_or(Matrix::IDENTITY);
            if let Some(path) = child(area, "Path") {
                let abbr = child_text(path, "AbbreviatedData").unwrap_or_default();
                out.push(ClipArea {
                    ctm,
                    commands: parse_abbreviated_data(&abbr),
                });
            }
        }
    }
    out
}

fn parse_text(node: Node) -> TextObject {
    let mut runs = Vec::new();
    let mut last_x = 0.0;
    let mut last_y = 0.0;
    for tc in node.children().filter(|n| local(n) == "TextCode") {
        let origin_x = tc
            .attribute("X")
            .and_then(|s| s.parse().ok())
            .unwrap_or(last_x);
        let origin_y = tc
            .attribute("Y")
            .and_then(|s| s.parse().ok())
            .unwrap_or(last_y);
        last_x = origin_x;
        last_y = origin_y;
        runs.push(TextRun {
            text: decode_text_code(tc.text().unwrap_or("")),
            origin_x,
            origin_y,
            delta_x: parse_deltas(tc.attribute("DeltaX").unwrap_or("")),
            delta_y: parse_deltas(tc.attribute("DeltaY").unwrap_or("")),
        });
    }
    let mut cg_transforms = Vec::new();
    for cg in node.children().filter(|n| local(n) == "CGTransform") {
        let glyphs: Vec<u16> = child(cg, "Glyphs")
            .and_then(|g| g.text())
            .map(|s| {
                s.split([' ', ',', '\t'])
                    .filter(|t| !t.is_empty())
                    .filter_map(|t| t.parse::<u16>().ok())
                    .collect()
            })
            .unwrap_or_default();
        cg_transforms.push(CgTransform {
            code_position: cg
                .attribute("CodePosition")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            code_count: cg
                .attribute("CodeCount")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1),
            glyph_count: cg
                .attribute("GlyphCount")
                .and_then(|s| s.parse().ok())
                .unwrap_or_else(|| glyphs.len().max(1)),
            glyphs,
        });
    }

    TextObject {
        common: parse_common(node),
        font_id: attr_u64(node, "Font"),
        font_size: node
            .attribute("Size")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        stroke: node
            .attribute("Stroke")
            .map(|s| s == "true")
            .unwrap_or(false),
        fill: node.attribute("Fill").map(|s| s != "false").unwrap_or(true),
        h_scale: node
            .attribute("HScale")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0),
        read_direction: Direction(attr_u16(node, "ReadDirection", 0)),
        char_direction: Direction(attr_u16(node, "CharDirection", 0)),
        weight: attr_u16(node, "Weight", 400),
        italic: node
            .attribute("Italic")
            .map(|s| s == "true")
            .unwrap_or(false),
        fill_color: inline_color(node, "FillColor"),
        stroke_color: inline_color(node, "StrokeColor"),
        cg_transforms,
        runs,
    }
}

fn parse_path(node: Node) -> PathObject {
    let abbr = child_text(node, "AbbreviatedData").unwrap_or_default();
    PathObject {
        common: parse_common(node),
        stroke: node
            .attribute("Stroke")
            .map(|s| s != "false")
            .unwrap_or(true),
        fill: node.attribute("Fill").map(|s| s == "true").unwrap_or(false),
        fill_rule: match node.attribute("Rule") {
            Some("Even-Odd") | Some("EvenOdd") => FillRule::EvenOdd,
            _ => FillRule::NonZero,
        },
        fill_color: inline_color(node, "FillColor"),
        stroke_color: inline_color(node, "StrokeColor"),
        commands: parse_abbreviated_data(&abbr),
    }
}

fn parse_image(node: Node) -> ImageObject {
    ImageObject {
        common: parse_common(node),
        resource_id: attr_u64(node, "ResourceID"),
        substitution: node.attribute("Substitution").and_then(|s| s.parse().ok()),
        image_mask: node.attribute("ImageMask").and_then(|s| s.parse().ok()),
        border: child(node, "Border").map(|b| ImageBorder {
            line_width: b
                .attribute("LineWidth")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.353),
            horizontal_corner_radius: b
                .attribute("HorizonalCornerRadius")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            vertical_corner_radius: b
                .attribute("VerticalCornerRadius")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0),
            color: inline_color(b, "BorderColor"),
        }),
    }
}

// ---- Resources -------------------------------------------------------------

/// Collect the resource ids referenced by image objects (recursing into groups).
fn collect_image_refs(objects: &[GraphicObject], out: &mut std::collections::HashSet<u64>) {
    for o in objects {
        match o {
            GraphicObject::Image(i) => {
                out.insert(i.resource_id);
            }
            GraphicObject::Group(g) => collect_image_refs(g, out),
            _ => {}
        }
    }
}

fn parse_resources(
    c: &mut Container,
    res: Node,
    dir: &str,
    out: &mut Resources,
    warnings: &mut Vec<String>,
    missing_media: &mut HashMap<u64, String>,
) {
    let res_base = res.attribute("BaseLoc").unwrap_or("Res");
    for group in res.children().filter(|n| n.is_element()) {
        match local(&group) {
            "Fonts" => {
                for f in group.children().filter(|n| local(n) == "Font") {
                    let id = attr_u64(f, "ID");
                    let name = f.attribute("FontName").unwrap_or("");
                    // Embedded fonts are always preferred and used. Read the
                    // FontFile and validate it parses; warn (but keep the bytes)
                    // when it does not, so the substitution is observable.
                    let data = child_text(f, "FontFile").and_then(|file| {
                        let path = join(dir, &join(res_base, &file));
                        match read_bytes(c, &path) {
                            Ok(bytes) => {
                                if crate::cff::usable_font(&bytes).is_none() {
                                    warnings.push(format!(
                                        "embedded font {path} (id {id}, {name}) failed to parse — substituting"
                                    ));
                                }
                                Some(bytes)
                            }
                            Err(e) => {
                                warnings.push(format!("FontFile {path} (id {id}, {name}): {e}"));
                                None
                            }
                        }
                    });
                    out.fonts.push(Font {
                        id,
                        font_name: f.attribute("FontName").unwrap_or("").to_string(),
                        family_name: f.attribute("FamilyName").map(|s| s.to_string()),
                        charset: f.attribute("Charset").map(|s| s.to_string()),
                        italic: f.attribute("Italic").map(|s| s == "true").unwrap_or(false),
                        bold: f.attribute("Bold").map(|s| s == "true").unwrap_or(false),
                        serif: f.attribute("Serif").map(|s| s == "true").unwrap_or(false),
                        fixed_width: f
                            .attribute("FixedWidth")
                            .map(|s| s == "true")
                            .unwrap_or(false),
                        data,
                    });
                }
            }
            "MultiMedias" => {
                for m in group.children().filter(|n| local(n) == "MultiMedia") {
                    let id = attr_u64(m, "ID");
                    let kind = match m.attribute("Type") {
                        Some("Audio") => MediaKind::Audio,
                        Some("Video") => MediaKind::Video,
                        _ => MediaKind::Image,
                    };
                    if let Some(file) = child_text(m, "MediaFile") {
                        let format = guess_format(m.attribute("Format"), &file);
                        let path = join(dir, &join(res_base, &file));
                        match read_bytes(c, &path) {
                            Ok(data) => out.images.push(MultiMedia {
                                id,
                                kind,
                                format,
                                data,
                            }),
                            // Record but don't warn yet — many templates declare
                            // media they don't ship. Warn later only if referenced.
                            Err(_) => {
                                missing_media.insert(id, file);
                            }
                        }
                    }
                }
            }
            "DrawParams" => {
                for d in group.children().filter(|n| local(n) == "DrawParam") {
                    out.draw_params.push(DrawParam {
                        id: attr_u64(d, "ID"),
                        relative: d.attribute("Relative").and_then(|s| s.parse().ok()),
                        line_width: d.attribute("LineWidth").and_then(|s| s.parse().ok()),
                        cap: d.attribute("Cap").map(|s| parse_cap(Some(s))),
                        join: d.attribute("Join").map(|s| parse_join(Some(s))),
                        miter_limit: d.attribute("MiterLimit").and_then(|s| s.parse().ok()),
                        dash_offset: d.attribute("DashOffset").and_then(|s| s.parse().ok()),
                        dash_pattern: d
                            .attribute("DashPattern")
                            .map(parse_floats)
                            .filter(|v| !v.is_empty()),
                        fill_color: inline_color(d, "FillColor"),
                        stroke_color: inline_color(d, "StrokeColor"),
                    });
                }
            }
            "ColorSpaces" => {
                for cs in group.children().filter(|n| local(n) == "ColorSpace") {
                    let kind = match cs.attribute("Type") {
                        Some("GRAY") => ColorSpaceKind::Gray,
                        Some("CMYK") => ColorSpaceKind::Cmyk,
                        _ => ColorSpaceKind::Rgb,
                    };
                    out.color_spaces.push(ColorSpace {
                        id: attr_u64(cs, "ID"),
                        kind,
                        bits_per_component: cs
                            .attribute("BitsPerComponent")
                            .and_then(|s| s.parse().ok())
                            .unwrap_or(8),
                        palette: child(cs, "Palette")
                            .map(|p| {
                                p.children()
                                    .filter(|n| local(n) == "CV")
                                    .filter_map(|cv| cv.text())
                                    .map(|text| {
                                        let basic = BasicColor {
                                            components: Some(parse_color_components(text)),
                                            index: None,
                                            color_space: Some(attr_u64(cs, "ID")),
                                            alpha: 255,
                                        };
                                        resolve_basic_color_static(
                                            &basic,
                                            kind,
                                            cs.attribute("BitsPerComponent")
                                                .and_then(|s| s.parse().ok())
                                                .unwrap_or(8),
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                    });
                }
            }
            _ => {}
        }
    }
}

fn resolve_basic_color_static(color: &BasicColor, kind: ColorSpaceKind, bpc: u8) -> Color {
    let comps = color.components.as_deref().unwrap_or(&[]);
    let scale = |v: f32| -> u8 {
        let max = ((1u32 << bpc.min(16) as u32) - 1).max(1) as f32;
        if bpc == 8 {
            v.clamp(0.0, 255.0) as u8
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
            let max = ((1u32 << bpc.min(16) as u32) - 1).max(1) as f32;
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

fn guess_format(fmt: Option<&str>, file: &str) -> ImageFormat {
    let hint = fmt
        .map(|s| s.to_ascii_uppercase())
        .unwrap_or_else(|| file.rsplit('.').next().unwrap_or("").to_ascii_uppercase());
    match hint.as_str() {
        "JPG" | "JPEG" => ImageFormat::Jpeg,
        "PNG" => ImageFormat::Png,
        "BMP" => ImageFormat::Bmp,
        "JB2" | "JBIG2" | "GBIG2" => ImageFormat::Jbig2,
        "CCITT" | "TIFF" | "FAX" => ImageFormat::Ccitt,
        _ => ImageFormat::Unknown,
    }
}

// ---- AbbreviatedData path parsing ------------------------------------------

/// Parse OFD `AbbreviatedData` (e.g. `"M 0 0 L 10 0 L 10 10 C ... B"`).
pub fn parse_abbreviated_data(s: &str) -> Vec<PathCommand> {
    let mut out = Vec::new();
    let mut nums: Vec<f32> = Vec::new();
    let mut op: Option<char> = None;

    let flush = |op: char, nums: &[f32], out: &mut Vec<PathCommand>| match op {
        'M' if nums.len() >= 2 => out.push(PathCommand::MoveTo {
            x: nums[0],
            y: nums[1],
        }),
        'L' if nums.len() >= 2 => out.push(PathCommand::LineTo {
            x: nums[0],
            y: nums[1],
        }),
        'B' if nums.len() >= 6 => out.push(PathCommand::CubicTo {
            x1: nums[0],
            y1: nums[1],
            x2: nums[2],
            y2: nums[3],
            x: nums[4],
            y: nums[5],
        }),
        'Q' if nums.len() >= 4 => out.push(PathCommand::QuadTo {
            x1: nums[0],
            y1: nums[1],
            x: nums[2],
            y: nums[3],
        }),
        'C' => out.push(PathCommand::Close),
        _ => {}
    };

    for tok in s
        .split([' ', ',', '\n', '\r', '\t'])
        .filter(|t| !t.is_empty())
    {
        if let Ok(n) = tok.parse::<f32>() {
            nums.push(n);
        } else if let Some(ch) = tok.chars().next() {
            if let Some(prev) = op.take() {
                flush(prev, &nums, &mut out);
            }
            nums.clear();
            op = Some(ch);
            if ch == 'C' {
                // Close takes no operands; emit immediately.
                flush('C', &nums, &mut out);
                op = None;
            }
        }
    }
    if let Some(prev) = op {
        flush(prev, &nums, &mut out);
    }
    out
}

// ---- Small helpers ---------------------------------------------------------

fn read_str(c: &mut Container, path: &str) -> Result<String> {
    let bytes = c.read_normalized(path)?;
    String::from_utf8(bytes).map_err(|e| OfdError::Xml(format!("non-utf8 xml in {path}: {e}")))
}

fn read_bytes(c: &mut Container, path: &str) -> Result<Vec<u8>> {
    c.read_normalized(path)
}

fn local<'a>(n: &Node<'a, 'a>) -> &'a str {
    n.tag_name().name()
}

fn child<'a>(parent: Node<'a, 'a>, name: &str) -> Option<Node<'a, 'a>> {
    parent.children().find(|n| local(n) == name)
}

fn children<'a>(parent: Node<'a, 'a>, name: &'a str) -> impl Iterator<Item = Node<'a, 'a>> {
    parent.children().filter(move |n| local(n) == name)
}

fn child_text(parent: Node, name: &str) -> Option<String> {
    child(parent, name)
        .and_then(|n| n.text())
        .map(|s| s.trim().to_string())
}

/// Parse a `CT_Color` child element (`FillColor`/`StrokeColor`/`BorderColor`).
/// Basic colors are resolved later by the renderer so `ColorSpace`, `Index`,
/// palettes, and the document `DefaultCS` can all participate.
fn inline_color(parent: Node, name: &str) -> Option<OfdColor> {
    let node = child(parent, name)?;
    parse_color_node(node)
}

fn parse_color_node(node: Node) -> Option<OfdColor> {
    for child_node in node.children().filter(|n| n.is_element()) {
        match local(&child_node) {
            "Pattern" => return parse_pattern(child_node).map(OfdColor::Pattern),
            "AxialShd" => return parse_axial(child_node).map(OfdColor::Axial),
            "RadialShd" => return parse_radial(child_node).map(OfdColor::Radial),
            "GouraudShd" => return Some(OfdColor::Gouraud(parse_gouraud(child_node))),
            "LaGouraudShd" | "LaGourandShd" => {
                return Some(OfdColor::LatticeGouraud(parse_lattice_gouraud(child_node)))
            }
            _ => {}
        }
    }
    Some(OfdColor::Basic(parse_basic_color(node)))
}

fn parse_basic_color(node: Node) -> BasicColor {
    BasicColor {
        components: node.attribute("Value").map(parse_color_components),
        index: node.attribute("Index").and_then(|s| s.parse().ok()),
        color_space: node.attribute("ColorSpace").and_then(|s| s.parse().ok()),
        alpha: node
            .attribute("Alpha")
            .and_then(|s| s.parse::<f32>().ok())
            .map(|a| a.clamp(0.0, 255.0) as u8)
            .unwrap_or(255),
    }
}

fn parse_pattern(node: Node) -> Option<PatternColor> {
    let width = node.attribute("Width").and_then(|s| s.parse().ok())?;
    let height = node.attribute("Height").and_then(|s| s.parse().ok())?;
    let x_step = node
        .attribute("XStep")
        .and_then(|s| s.parse().ok())
        .filter(|v| *v >= width)
        .unwrap_or(width);
    let y_step = node
        .attribute("YStep")
        .and_then(|s| s.parse().ok())
        .filter(|v| *v >= height)
        .unwrap_or(height);
    let reflect = match node.attribute("ReflectMethod") {
        Some("Column") => PatternReflect::Column,
        Some("Row") => PatternReflect::Row,
        Some("RowAndColumn") => PatternReflect::RowAndColumn,
        _ => PatternReflect::Normal,
    };
    let relative_to = match node.attribute("RelativeTo") {
        Some("Page") => PatternRelativeTo::Page,
        _ => PatternRelativeTo::Object,
    };
    let ctm = node
        .attribute("CTM")
        .and_then(parse_matrix)
        .unwrap_or(Matrix::IDENTITY);
    let cell_content = child(node, "CellContent")
        .map(parse_page_block_children)
        .unwrap_or_default();
    let thumbnail = child_text(node, "Thumbnail").and_then(|s| s.parse().ok());
    Some(PatternColor {
        width,
        height,
        x_step,
        y_step,
        reflect,
        relative_to,
        ctm,
        cell_content,
        thumbnail,
    })
}

fn parse_axial(node: Node) -> Option<AxialGradient> {
    Some(AxialGradient {
        map_type: parse_map_type(node.attribute("MapType")),
        map_unit: node.attribute("MapUnit").and_then(|s| s.parse().ok()),
        extend: attr_u16(node, "Extend", 0).min(3) as u8,
        start: node.attribute("StartPoint").and_then(parse_point)?,
        end: node.attribute("EndPoint").and_then(parse_point)?,
        segments: parse_segments(node),
    })
}

fn parse_radial(node: Node) -> Option<RadialGradient> {
    Some(RadialGradient {
        map_type: parse_map_type(node.attribute("MapType")),
        map_unit: node.attribute("MapUnit").and_then(|s| s.parse().ok()),
        eccentricity: node
            .attribute("Eccentricity")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        angle: node
            .attribute("Angle")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        start: node.attribute("StartPoint").and_then(parse_point)?,
        end: node.attribute("EndPoint").and_then(parse_point)?,
        start_radius: node
            .attribute("StartRadius")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        end_radius: node.attribute("EndRadius").and_then(|s| s.parse().ok())?,
        extend: attr_u16(node, "Extend", 0).min(3) as u8,
        segments: parse_segments(node),
    })
}

fn parse_gouraud(node: Node) -> GouraudGradient {
    GouraudGradient {
        extend: node
            .attribute("Extend")
            .map(|s| s == "1" || s == "true")
            .unwrap_or(false),
        points: parse_gouraud_points(node),
        back_color: child(node, "BackColor").map(parse_basic_color),
    }
}

fn parse_lattice_gouraud(node: Node) -> LatticeGouraudGradient {
    LatticeGouraudGradient {
        vertices_per_row: node
            .attribute("VerticesPerRow")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        extend: node
            .attribute("Extend")
            .map(|s| s == "1" || s == "true")
            .unwrap_or(false),
        points: parse_gouraud_points(node),
        back_color: child(node, "BackColor").map(parse_basic_color),
    }
}

fn parse_gouraud_points(node: Node) -> Vec<GouraudPoint> {
    node.children()
        .filter(|n| local(n) == "Point")
        .map(|p| GouraudPoint {
            x: p.attribute("X").and_then(|s| s.parse().ok()).unwrap_or(0.0),
            y: p.attribute("Y").and_then(|s| s.parse().ok()).unwrap_or(0.0),
            edge_flag: p.attribute("EdgeFlag").and_then(|s| s.parse().ok()),
            color: child(p, "Color")
                .map(parse_basic_color)
                .unwrap_or_else(|| BasicColor {
                    components: Some(vec![0.0, 0.0, 0.0]),
                    index: None,
                    color_space: None,
                    alpha: 255,
                }),
        })
        .collect()
}

fn parse_segments(node: Node) -> Vec<GradientSegment> {
    node.children()
        .filter(|n| local(n) == "Segment")
        .map(|s| GradientSegment {
            position: s.attribute("Position").and_then(|v| v.parse().ok()),
            color: child(s, "Color")
                .map(parse_basic_color)
                .unwrap_or_else(|| BasicColor {
                    components: Some(vec![0.0, 0.0, 0.0]),
                    index: None,
                    color_space: None,
                    alpha: 255,
                }),
        })
        .collect()
}

fn parse_map_type(s: Option<&str>) -> GradientMapType {
    match s {
        Some("Repeat") => GradientMapType::Repeat,
        Some("Reflect") => GradientMapType::Reflect,
        _ => GradientMapType::Direct,
    }
}

fn parse_point<S: AsRef<str>>(s: S) -> Option<Point> {
    let v = parse_floats(s.as_ref());
    if v.len() >= 2 {
        Some(Point::new(v[0], v[1]))
    } else {
        None
    }
}

fn parse_color_components(s: &str) -> Vec<f32> {
    s.split([' ', ',', '\t'])
        .filter(|t| !t.is_empty())
        .filter_map(parse_color_component)
        .collect()
}

fn parse_color_component(s: &str) -> Option<f32> {
    if let Some(hex) = s.strip_prefix('#') {
        u32::from_str_radix(hex, 16).ok().map(|v| v as f32)
    } else {
        s.parse().ok()
    }
}

/// Decode OFD TextCode backslash hex escapes (`\4E2D`) while leaving literal
/// text untouched. Invalid escapes are preserved verbatim.
fn decode_text_code(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        let mut hex = String::new();
        for _ in 0..4 {
            match chars.peek().copied().filter(|c| c.is_ascii_hexdigit()) {
                Some(c) => {
                    hex.push(c);
                    chars.next();
                }
                None => break,
            }
        }
        if hex.len() == 4 {
            if let Ok(cp) = u32::from_str_radix(&hex, 16) {
                if let Some(decoded) = char::from_u32(cp) {
                    out.push(decoded);
                    continue;
                }
            }
        }
        out.push('\\');
        out.push_str(&hex);
    }
    out
}

/// Parse an unsigned id attribute, defaulting to 0.
fn attr_u64(node: Node, name: &str) -> u64 {
    node.attribute(name)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

/// Parse a small unsigned attribute with a fallback default.
fn attr_u16(node: Node, name: &str, default: u16) -> u16 {
    node.attribute(name)
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

/// Parse an OFD delta list (`DeltaX`/`DeltaY`), expanding the `g` run-length
/// operator: `g N V` yields `N` copies of `V`. Example: `"g 8 3.175 1.6 g 4 3"`
/// expands to eight `3.175`s, then `1.6`, then four `3`s.
fn parse_deltas(s: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let mut toks = s
        .split([' ', ',', '\t', '\n', '\r'])
        .filter(|t| !t.is_empty());
    while let Some(tok) = toks.next() {
        if tok == "g" || tok == "G" {
            let count = toks
                .next()
                .and_then(|t| t.parse::<f32>().ok())
                .unwrap_or(0.0) as usize;
            let value = toks
                .next()
                .and_then(|t| t.parse::<f32>().ok())
                .unwrap_or(0.0);
            out.extend(std::iter::repeat(value).take(count));
        } else if let Ok(v) = tok.parse::<f32>() {
            out.push(v);
        }
    }
    out
}

fn parse_floats(s: &str) -> Vec<f32> {
    s.split([' ', ',', '\t'])
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse().ok())
        .collect()
}

fn parse_rect<S: AsRef<str>>(s: S) -> Option<Rect> {
    let v = parse_floats(s.as_ref());
    if v.len() >= 4 {
        Some(Rect::new(v[0], v[1], v[2], v[3]))
    } else {
        None
    }
}

fn parse_matrix(s: &str) -> Option<Matrix> {
    let v = parse_floats(s);
    if v.len() >= 6 {
        Some(Matrix::new(v[0], v[1], v[2], v[3], v[4], v[5]))
    } else {
        None
    }
}

/// Resolve a path relative to a directory, normalising `.`/`..` and slashes.
fn join(dir: &str, rel: &str) -> String {
    let rel = rel.trim();
    if rel.starts_with('/') {
        return rel.trim_start_matches('/').to_string();
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in dir.split('/').chain(rel.split('/')) {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn parent_dir(path: &str) -> String {
    match path.rfind('/') {
        Some(i) => path[..i].to_string(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abbreviated_data_basic() {
        let cmds = parse_abbreviated_data("M 0 0 L 10 0 L 10 10 C");
        assert_eq!(
            cmds,
            vec![
                PathCommand::MoveTo { x: 0.0, y: 0.0 },
                PathCommand::LineTo { x: 10.0, y: 0.0 },
                PathCommand::LineTo { x: 10.0, y: 10.0 },
                PathCommand::Close,
            ]
        );
    }

    #[test]
    fn abbreviated_data_bezier() {
        let cmds = parse_abbreviated_data("M 0 0 B 1 1 2 2 3 3");
        assert_eq!(cmds.len(), 2);
        assert_eq!(
            cmds[1],
            PathCommand::CubicTo {
                x1: 1.0,
                y1: 1.0,
                x2: 2.0,
                y2: 2.0,
                x: 3.0,
                y: 3.0
            }
        );
    }

    #[test]
    fn join_resolves_relative() {
        assert_eq!(
            join("Doc_0", "Pages/Page_0/Content.xml"),
            "Doc_0/Pages/Page_0/Content.xml"
        );
        assert_eq!(join("Doc_0", "Res/img.png"), "Doc_0/Res/img.png");
        assert_eq!(join("Doc_0/Pages", "../Res/x"), "Doc_0/Res/x");
        assert_eq!(join("Doc_0", "/OFD.xml"), "OFD.xml");
    }

    #[test]
    fn deltas_expand_g_runs() {
        assert_eq!(parse_deltas("3 3 3 3 1.5"), vec![3.0, 3.0, 3.0, 3.0, 1.5]);
        assert_eq!(parse_deltas("g 2 3.175"), vec![3.175, 3.175]);
        assert_eq!(
            parse_deltas("g 8 3.175 1.6 g 4 3.175"),
            vec![
                3.175, 3.175, 3.175, 3.175, 3.175, 3.175, 3.175, 3.175, 1.6, 3.175, 3.175, 3.175,
                3.175
            ]
        );
    }

    #[test]
    fn rect_and_floats() {
        assert_eq!(
            parse_rect("0 0 210 297"),
            Some(Rect::new(0.0, 0.0, 210.0, 297.0))
        );
        assert_eq!(parse_floats("3 3 1.5"), vec![3.0, 3.0, 1.5]);
    }

    #[test]
    fn text_code_inherits_position_and_decodes_hex() {
        let xml = XmlDoc::parse(
            r#"<TextObject ID="1" Font="1" Size="3">
                <TextCode X="10" Y="20">\4E2D</TextCode>
                <TextCode>AB</TextCode>
            </TextObject>"#,
        )
        .unwrap();
        let text = parse_text(xml.root_element());
        assert_eq!(text.runs[0].text, "中");
        assert_eq!((text.runs[1].origin_x, text.runs[1].origin_y), (10.0, 20.0));
    }

    #[test]
    fn parses_palette_and_complex_colors() {
        let xml = XmlDoc::parse(
            r##"<Root>
                  <FillColor ColorSpace="8" Index="1"/>
                  <FillColor>
                    <AxialShd StartPoint="0 0" EndPoint="10 0">
                      <Segment Position="0"><Color Value="0 0 0"/></Segment>
                      <Segment Position="1"><Color Value="255 255 255"/></Segment>
                    </AxialShd>
                  </FillColor>
                  <FillColor>
                    <LaGourandShd VerticesPerRow="2">
                      <Point X="0" Y="0"><Color Value="#ff 0 0"/></Point>
                      <Point X="1" Y="0"><Color Value="0 255 0"/></Point>
                      <Point X="0" Y="1"><Color Value="0 0 255"/></Point>
                      <Point X="1" Y="1"><Color Value="255 255 255"/></Point>
                    </LaGourandShd>
                  </FillColor>
                  <Palette><CV>255 0 0</CV><CV>0 255 0</CV></Palette>
            </Root>"##,
        )
        .unwrap();
        let root = xml.root_element();
        let fills: Vec<_> = root
            .children()
            .filter(|n| local(n) == "FillColor")
            .collect();
        let palette: Vec<_> = child(root, "Palette")
            .unwrap()
            .children()
            .filter(|n| local(n) == "CV")
            .filter_map(|cv| cv.text())
            .map(|text| {
                let basic = BasicColor {
                    components: Some(parse_color_components(text)),
                    index: None,
                    color_space: None,
                    alpha: 255,
                };
                resolve_basic_color_static(&basic, ColorSpaceKind::Rgb, 8)
            })
            .collect();
        assert_eq!(palette[1], Color::rgb(0, 255, 0));
        assert!(matches!(
            parse_color_node(fills[0]),
            Some(OfdColor::Basic(_))
        ));
        assert!(matches!(
            parse_color_node(fills[1]),
            Some(OfdColor::Axial(_))
        ));
        assert!(matches!(
            parse_color_node(fills[2]),
            Some(OfdColor::LatticeGouraud(_))
        ));
    }
}
