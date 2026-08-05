//! XML → model parser. Reads the container entries (`OFD.xml`,
//! `Document.xml`, per-page `Content.xml`, resource XML) using a read-only DOM
//! ([`roxmltree`]) and produces the [`crate::model`] types.
//!
//! Element names are matched by their *local* name (the `ofd:` namespace prefix
//! is resolved away by the XML parser), so producers that use different prefixes
//! still parse.

use std::collections::{BTreeSet, HashMap, HashSet};

use roxmltree::{Document as XmlDoc, Node};

use crate::container::{Container, ContainerLimits};
use crate::error::{OfdError, Result};
use crate::geom::{Matrix, Point, Rect};
use crate::model::*;

const OFD_NAMESPACE: &str = "http://www.ofdspec.org/2016";
const LEGACY_OFD_NAMESPACE: &str = "http://www.ofdspec.org";

/// Parse a whole OFD package from raw file bytes.
pub fn parse(bytes: Vec<u8>) -> Result<OfdPackage> {
    parse_with_limits(bytes, ContainerLimits::default())
}

/// Maximum positioned glyph/delta slots accepted in one text object. This is
/// high enough for normal page content while preventing attacker-controlled
/// `g N V` and `GlyphCount` values from causing unbounded work or allocation.
pub(crate) const MAX_TEXT_SLOTS: usize = 1_000_000;
const MAX_PATH_COMMANDS: usize = 1_000_000;
const MAX_XML_BYTES: usize = 16 * 1024 * 1024;
const MAX_XML_NODES: usize = 1_000_000;
const MAX_XML_DEPTH: usize = 256;
const MAX_ICC_PROFILE_BYTES: usize = 16 * 1024 * 1024;
const MAX_TEMPLATE_EXPANSION_BYTES: u64 = 64 * 1024 * 1024;
const MAX_TOTAL_XML_NODES: u64 = 2_000_000;
const MAX_GRAPHIC_OBJECTS: u64 = 1_000_000;
const MAX_MODEL_ITEMS: u64 = 4_000_000;

#[derive(Clone, Copy, Debug, Default)]
struct GraphicStats {
    graphic_objects: u64,
    /// Approximate owned model entries: source XML nodes plus expanded path,
    /// text, delta, and glyph-list items.
    model_items: u64,
}

struct GraphicValidation {
    malformed: Option<String>,
}

#[derive(Debug, Default)]
struct ParseBudget {
    xml_nodes: u64,
    graphic_objects: u64,
    model_items: u64,
}

#[derive(Debug, Default)]
struct IdRegistry {
    seen: HashMap<u64, String>,
    reported_duplicates: HashSet<u64>,
    max_id: u64,
}

impl IdRegistry {
    fn register(&mut self, id: u64, location: String, warnings: &mut Vec<String>) {
        self.max_id = self.max_id.max(id);
        if let Some(first) = self.seen.get(&id) {
            if self.reported_duplicates.insert(id) {
                warnings.push(format!(
                    "duplicate ST_ID {id} at {location}; first declared at {first}"
                ));
            }
        } else {
            self.seen.insert(id, location);
        }
    }
}

impl ParseBudget {
    fn charge(counter: &mut u64, amount: u64, limit: u64, resource: &str) -> Result<()> {
        let next = counter
            .checked_add(amount)
            .ok_or_else(|| OfdError::ResourceLimit(format!("{resource} budget overflow")))?;
        if next > limit {
            return Err(OfdError::ResourceLimit(format!(
                "{resource} requires {next}; limit is {limit}"
            )));
        }
        *counter = next;
        Ok(())
    }

    fn charge_xml_nodes(&mut self, nodes: u64) -> Result<()> {
        Self::charge(
            &mut self.xml_nodes,
            nodes,
            MAX_TOTAL_XML_NODES,
            "package XML nodes",
        )
    }

    fn charge_model_items(&mut self, items: u64) -> Result<()> {
        Self::charge(
            &mut self.model_items,
            items,
            MAX_MODEL_ITEMS,
            "package model items",
        )
    }

    fn charge_graphic_stats(&mut self, stats: GraphicStats) -> Result<()> {
        let next_objects = self
            .graphic_objects
            .checked_add(stats.graphic_objects)
            .ok_or_else(|| {
                OfdError::ResourceLimit("package graphic-object budget overflow".into())
            })?;
        let next_items = self
            .model_items
            .checked_add(stats.model_items)
            .ok_or_else(|| OfdError::ResourceLimit("package model-item budget overflow".into()))?;
        if next_objects > MAX_GRAPHIC_OBJECTS {
            return Err(OfdError::ResourceLimit(format!(
                "package graphic objects require {next_objects}; limit is {MAX_GRAPHIC_OBJECTS}"
            )));
        }
        if next_items > MAX_MODEL_ITEMS {
            return Err(OfdError::ResourceLimit(format!(
                "package model items require {next_items}; limit is {MAX_MODEL_ITEMS}"
            )));
        }
        self.graphic_objects = next_objects;
        self.model_items = next_items;
        Ok(())
    }
}

struct ParsedTemplate {
    area: Option<PageArea>,
    default_z_order: LayerKind,
    layers: Vec<Layer>,
    source_bytes: u64,
    stats: GraphicStats,
}

/// Parse a whole OFD package using caller-selected container limits.
pub fn parse_with_limits(bytes: Vec<u8>, limits: ContainerLimits) -> Result<OfdPackage> {
    let mut c = Container::open_with_limits(bytes, limits)?;
    let mut budget = ParseBudget::default();
    let root_xml = read_str(&mut c, "OFD.xml")?;
    let xml = XmlDoc::parse(&root_xml)?;
    validate_xml_structure_with_budget(xml.root_element(), &mut budget)?;
    let ofd = xml.root_element();
    validate_ofd_root(ofd, "OFD", "OFD.xml")?;
    let mut ofd_warnings = Vec::new();
    warn_nonstandard_namespaces(ofd, "OFD.xml", &mut ofd_warnings);
    ofd_warnings.extend(c.take_compatibility_path_warnings());

    let mut documents = Vec::new();
    for doc_body in children(ofd, "DocBody") {
        let metadata = doc_body
            .children()
            .find(|n| local(n) == "DocInfo")
            .map(parse_doc_info)
            .unwrap_or_default();
        let doc_root = child_text(doc_body, "DocRoot")
            .filter(|path| !path.is_empty())
            .ok_or_else(|| OfdError::Malformed("DocBody missing required DocRoot".into()))?;
        let signatures = child_text(doc_body, "Signatures");
        let mut document = parse_document(&mut c, &doc_root, signatures, metadata, &mut budget)?;
        let mut path_warnings = c.take_compatibility_path_warnings();
        if !ofd_warnings.is_empty() || !path_warnings.is_empty() {
            let prefix = ofd_warnings.iter().cloned().chain(path_warnings.drain(..));
            document.warnings.splice(0..0, prefix);
        }
        documents.push(document);
    }
    if documents.is_empty() {
        return Err(OfdError::Malformed(
            "OFD.xml contains no DocBody/DocRoot document".into(),
        ));
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
    budget: &mut ParseBudget,
) -> Result<Document> {
    let dir = parent_dir(doc_root); // e.g. "Doc_0"
    let doc_xml = read_str(c, doc_root)?;
    let xml = XmlDoc::parse(&doc_xml)?;
    validate_xml_structure_with_budget(xml.root_element(), budget)?;
    let root = xml.root_element();
    validate_ofd_root(root, "Document", doc_root)?;

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
    let mut ids = IdRegistry::default();
    warn_nonstandard_namespaces(root, doc_root, &mut warnings);
    register_st_ids(root, doc_root, &mut ids, &mut warnings);
    match common {
        None => warnings.push(format!("Document {doc_root}: missing required CommonData")),
        Some(common) => {
            if child_text(common, "MaxUnitID")
                .and_then(|value| value.parse::<u64>().ok())
                .is_none()
            {
                warnings.push(format!(
                    "Document {doc_root}: missing valid required CommonData/MaxUnitID"
                ));
            }
            if child(common, "PageArea")
                .map(parse_page_area)
                .and_then(|area| area.physical_box)
                .is_none()
            {
                warnings.push(format!(
                    "Document {doc_root}: missing valid required CommonData/PageArea/PhysicalBox"
                ));
            }
        }
    }
    if child(root, "Pages").is_none() {
        warnings.push(format!("Document {doc_root}: missing required Pages"));
    }
    // Declared-but-absent media (id → file): common in invoice templates that
    // list every province's seal but ship only the used one. We only warn for
    // those actually referenced by an object (checked after pages parse).
    let mut missing_media: HashMap<u64, String> = HashMap::new();

    // Resources (PublicRes + DocumentRes), each path relative to the doc dir.
    // A referenced resource file that is missing or malformed is recorded as a
    // warning rather than silently dropped: the render may then be missing
    // fonts/images/styles, which we want to be observable.
    let mut resources = Resources::default();
    let mut loaded_resource_files = HashSet::new();
    if let Some(cd) = common {
        for tag in ["PublicRes", "DocumentRes"] {
            for res_path in cd.children().filter(|n| local(n) == tag) {
                let Some(path) = res_path
                    .text()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                else {
                    warnings.push(format!("{tag}: missing required resource location"));
                    continue;
                };
                let full = join(&dir, path);
                load_resource_file(
                    c,
                    &full,
                    tag,
                    &mut resources,
                    &mut warnings,
                    &mut missing_media,
                    &mut loaded_resource_files,
                    &mut ids,
                    budget,
                );
            }
        }
    }

    // Template pages, keyed by their declared ID, parsed into reusable layers.
    let mut templates: HashMap<String, ParsedTemplate> = HashMap::new();
    if let Some(cd) = common {
        for tpl in cd.children().filter(|n| local(n) == "TemplatePage") {
            let Some(id) = tpl.attribute("ID").filter(|value| !value.is_empty()) else {
                warnings.push("TemplatePage declaration missing required ID".into());
                continue;
            };
            let Some(base) = tpl.attribute("BaseLoc").filter(|value| !value.is_empty()) else {
                warnings.push(format!(
                    "TemplatePage {id} declaration missing required BaseLoc"
                ));
                continue;
            };
            let full = join(&dir, base);
            let s = match read_str(c, &full) {
                Ok(s) => s,
                Err(e) => {
                    warnings.push(format!("TemplatePage {full}: {e}"));
                    continue;
                }
            };
            let px = match XmlDoc::parse(&s) {
                Ok(px) => px,
                Err(e) => {
                    warnings.push(format!("TemplatePage {full}: {e}"));
                    continue;
                }
            };
            if let Err(error) = validate_ofd_root(px.root_element(), "Page", &full) {
                warnings.push(format!("TemplatePage {full}: {error}"));
                continue;
            }
            warn_nonstandard_namespaces(px.root_element(), &full, &mut warnings);
            register_st_ids(px.root_element(), &full, &mut ids, &mut warnings);
            let validation = match validate_graphic_limits_with_budget(px.root_element(), budget) {
                Ok(validation) => validation,
                Err(e) => {
                    warnings.push(format!("TemplatePage {full}: {e}"));
                    continue;
                }
            };
            if let Some(message) = validation.malformed {
                // Structural non-conformance is observable and causes CLI
                // --strict to fail, but bounded best-effort model parsing is
                // still useful to viewer hosts.
                warnings.push(format!(
                    "TemplatePage {full}: malformed document: {message}"
                ));
            }
            let template_dir = parent_dir(&full);
            for res_path in px
                .root_element()
                .children()
                .filter(|node| local(node) == "PageRes")
            {
                let Some(path) = res_path
                    .text()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                else {
                    warnings.push(format!(
                        "TemplatePage {full}: PageRes missing required location"
                    ));
                    continue;
                };
                let resource_path = join(&template_dir, path);
                load_resource_file(
                    c,
                    &resource_path,
                    "Template PageRes",
                    &mut resources,
                    &mut warnings,
                    &mut missing_media,
                    &mut loaded_resource_files,
                    &mut ids,
                    budget,
                );
            }
            let default_z_order = parse_layer_kind(tpl.attribute("ZOrder"), LayerKind::Background);
            let layers = parse_page_layers_with_default(px.root_element(), default_z_order);
            let stats = graphic_stats_for_layers(&layers)?;
            templates
                .entry(id.to_string())
                .or_insert_with(|| ParsedTemplate {
                    area: child(px.root_element(), "Area").map(parse_page_area),
                    default_z_order,
                    layers,
                    source_bytes: s.len() as u64,
                    stats,
                });
        }
    }

    // Pages.
    let mut pages = Vec::new();
    let mut template_expansion_bytes = 0u64;
    if let Some(pages_node) = child(root, "Pages") {
        for page_node in pages_node.children().filter(|n| local(n) == "Page") {
            let id = page_node
                .attribute("ID")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            let Some(base) = page_node
                .attribute("BaseLoc")
                .filter(|value| !value.is_empty())
            else {
                warnings.push(format!("Page {id} declaration missing required BaseLoc"));
                continue;
            };
            let full = join(&dir, base);
            let s = read_str(c, &full)?;
            let px = XmlDoc::parse(&s)?;
            validate_ofd_root(px.root_element(), "Page", &full)?;
            warn_nonstandard_namespaces(px.root_element(), &full, &mut warnings);
            register_st_ids(px.root_element(), &full, &mut ids, &mut warnings);
            let validation = validate_graphic_limits_with_budget(px.root_element(), budget)?;
            if let Some(message) = validation.malformed {
                warnings.push(format!("Page {full}: malformed document: {message}"));
            }
            let page_dir = parent_dir(&full);
            for res_path in px
                .root_element()
                .children()
                .filter(|node| local(node) == "PageRes")
            {
                let Some(path) = res_path
                    .text()
                    .map(str::trim)
                    .filter(|path| !path.is_empty())
                else {
                    warnings.push(format!("Page {full}: PageRes missing required location"));
                    continue;
                };
                let resource_path = join(&page_dir, path);
                load_resource_file(
                    c,
                    &resource_path,
                    "PageRes",
                    &mut resources,
                    &mut warnings,
                    &mut missing_media,
                    &mut loaded_resource_files,
                    &mut ids,
                    budget,
                );
            }
            let mut page = parse_page(
                px.root_element(),
                &templates,
                &mut template_expansion_bytes,
                budget,
                &mut warnings,
            )?;
            page.id = id;
            pages.push(page);
        }
    }

    // Signatures / electronic seals (referenced from OFD.xml's DocBody).
    let mut seals = Vec::new();
    let mut sig_models = Vec::new();
    if let Some(sig_path) = signatures {
        parse_signatures(
            c,
            &sig_path,
            &mut seals,
            &mut sig_models,
            &mut warnings,
            budget,
        );
    }

    // Page annotations (referenced from Document.xml).
    let mut annotations = Vec::new();
    if let Some(annots_ref) = child_text(root, "Annotations") {
        parse_annotations(
            c,
            &join(&dir, &annots_ref),
            &mut annotations,
            &mut warnings,
            &mut ids,
            budget,
        );
    }

    validate_resource_references(
        &pages,
        &annotations,
        &resources,
        &missing_media,
        &mut warnings,
    );
    if ids.max_id > max_unit_id {
        warnings.push(format!(
            "CommonData/MaxUnitID {max_unit_id} is smaller than declared ST_ID {}",
            ids.max_id
        ));
    }

    // Document navigation: bookmarks, actions (event DO), and the outline tree.
    // Outline nodes target a page by object id, resolved here to a page index.
    let page_index = index_pages_first(&pages);
    let bookmarks = parse_bookmarks(root);
    let doc_actions = parse_actions(root);
    let outline = parse_outlines(root, &page_index);

    Ok(Document {
        max_unit_id,
        page_area,
        default_color_space,
        pages,
        resources,
        outline,
        bookmarks,
        actions: doc_actions,
        metadata,
        seals,
        annotations,
        signatures: sig_models,
        warnings,
    })
}

fn index_pages_first(pages: &[Page]) -> HashMap<u64, usize> {
    let mut page_index = HashMap::new();
    for (index, page) in pages.iter().enumerate() {
        page_index.entry(page.id).or_insert(index);
    }
    page_index
}

/// Load one resource description exactly once, sharing the package parse budget
/// and the global resource namespace across document, page, and template scopes.
#[allow(clippy::too_many_arguments)]
fn load_resource_file(
    c: &mut Container,
    full: &str,
    label: &str,
    resources: &mut Resources,
    warnings: &mut Vec<String>,
    missing_media: &mut HashMap<u64, String>,
    loaded: &mut HashSet<String>,
    ids: &mut IdRegistry,
    budget: &mut ParseBudget,
) {
    if !loaded.insert(full.to_string()) {
        return;
    }
    let source = match read_str(c, full) {
        Ok(source) => source,
        Err(error) => {
            warnings.push(format!("{label} {full}: unreadable: {error}"));
            return;
        }
    };
    let xml = match XmlDoc::parse(&source) {
        Ok(xml) => xml,
        Err(error) => {
            warnings.push(format!("{label} {full}: malformed xml: {error}"));
            return;
        }
    };
    if let Err(error) = validate_ofd_root(xml.root_element(), "Res", full) {
        warnings.push(format!("{label} {full}: {error}"));
        return;
    }
    warn_nonstandard_namespaces(xml.root_element(), full, warnings);
    register_st_ids(xml.root_element(), full, ids, warnings);
    let validation = match validate_graphic_limits_with_budget(xml.root_element(), budget) {
        Ok(validation) => validation,
        Err(error) => {
            warnings.push(format!("{label} {full}: {error}"));
            return;
        }
    };
    if let Some(message) = validation.malformed {
        warnings.push(format!("{label} {full}: malformed document: {message}"));
    }
    let resource_dir = parent_dir(full);
    parse_resources(
        c,
        xml.root_element(),
        &resource_dir,
        resources,
        warnings,
        missing_media,
    );
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
    ids: &mut IdRegistry,
    budget: &mut ParseBudget,
) {
    let Ok(list_xml) = read_str(c, annotations_path) else {
        warnings.push(format!("Annotations {annotations_path}: unreadable"));
        return;
    };
    let Ok(list) = XmlDoc::parse(&list_xml) else {
        warnings.push(format!("Annotations {annotations_path}: malformed xml"));
        return;
    };
    if let Err(error) = validate_ofd_root(list.root_element(), "Annotations", annotations_path) {
        warnings.push(format!("Annotations {annotations_path}: {error}"));
        return;
    }
    warn_nonstandard_namespaces(list.root_element(), annotations_path, warnings);
    if let Err(e) = validate_xml_structure_with_budget(list.root_element(), budget) {
        warnings.push(format!("Annotations {annotations_path}: {e}"));
        return;
    }
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
        if let Err(error) = validate_ofd_root(adoc.root_element(), "PageAnnot", &path) {
            warnings.push(format!("Annotation {path}: {error}"));
            continue;
        }
        warn_nonstandard_namespaces(adoc.root_element(), &path, warnings);
        register_st_ids(adoc.root_element(), &path, ids, warnings);
        let validation = match validate_graphic_limits_with_budget(adoc.root_element(), budget) {
            Ok(validation) => validation,
            Err(e) => {
                warnings.push(format!("Annotation {path}: {e}"));
                continue;
            }
        };
        if let Some(message) = validation.malformed {
            warnings.push(format!("Annotation {path}: malformed document: {message}"));
        }

        for annot in adoc
            .root_element()
            .descendants()
            .filter(|n| local(n) == "Annot")
        {
            let id = match annot.attribute("ID").and_then(|value| value.parse().ok()) {
                Some(id) => id,
                None => {
                    warnings.push(format!(
                        "Annotation {path}: Annot missing valid required ID"
                    ));
                    0
                }
            };
            let annot_type = required_annotation_attribute(annot, id, "Type", &path, warnings);
            let creator = required_annotation_attribute(annot, id, "Creator", &path, warnings);
            let last_mod_date =
                required_annotation_attribute(annot, id, "LastModDate", &path, warnings);
            let app = child(annot, "Appearance");
            if app.is_none() {
                warnings.push(format!(
                    "Annotation {path}: Annot {id} missing required Appearance"
                ));
            }
            let appearance_boundary = app
                .and_then(|appearance| appearance.attribute("Boundary"))
                .and_then(parse_rect);
            if app.is_some() && appearance_boundary.is_none() {
                warnings.push(format!(
                    "Annotation {path}: Annot {id} Appearance missing valid required Boundary"
                ));
            }
            let (ox, oy) = appearance_boundary
                .map(|r| (r.x, r.y))
                .unwrap_or((0.0, 0.0));
            let mut objects = Vec::new();
            if let Some(app) = app {
                for obj in app.children().filter(|n| n.is_element()) {
                    if let Some(mut object) = parse_object(obj) {
                        offset_object(&mut object, ox, oy);
                        objects.push(object);
                    }
                }
            }
            let mut parameters = Vec::new();
            if let Some(parameter_list) = child(annot, "Parameters") {
                for parameter in parameter_list
                    .children()
                    .filter(|node| local(node) == "Parameter")
                {
                    let name = match parameter.attribute("Name").filter(|name| !name.is_empty()) {
                        Some(name) => name.to_string(),
                        None => {
                            warnings.push(format!(
                                "Annotation {path}: Annot {id} Parameter missing required Name"
                            ));
                            String::new()
                        }
                    };
                    parameters.push(AnnotationParameter {
                        name,
                        value: parameter.text().unwrap_or("").to_string(),
                    });
                }
            }
            out.push(Annotation {
                page_id,
                id,
                annot_type,
                creator,
                last_mod_date,
                subtype: annot.attribute("Subtype").map(str::to_string),
                visible: attr_xs_boolean(annot, "Visible", true),
                print: attr_xs_boolean(annot, "Print", true),
                no_zoom: attr_xs_boolean(annot, "NoZoom", false),
                no_rotate: attr_xs_boolean(annot, "NoRotate", false),
                read_only: attr_xs_boolean(annot, "ReadOnly", true),
                remark: child_text(annot, "Remark"),
                parameters,
                appearance_boundary,
                objects,
            });
        }
    }
}

fn required_annotation_attribute(
    annot: Node,
    id: u64,
    name: &str,
    path: &str,
    warnings: &mut Vec<String>,
) -> String {
    match annot.attribute(name).filter(|value| !value.is_empty()) {
        Some(value) => value.to_string(),
        None => {
            warnings.push(format!(
                "Annotation {path}: Annot {id} missing required {name}"
            ));
            String::new()
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
        GraphicObject::Composite(co) => {
            co.common.boundary.x += dx;
            co.common.boundary.y += dy;
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
    budget: &mut ParseBudget,
) {
    let Ok(list_xml) = read_str(c, signatures_path) else {
        warnings.push(format!("Signatures {signatures_path}: unreadable"));
        return;
    };
    let Ok(list) = XmlDoc::parse(&list_xml) else {
        warnings.push(format!("Signatures {signatures_path}: malformed xml"));
        return;
    };
    if let Err(error) = validate_ofd_root(list.root_element(), "Signatures", signatures_path) {
        warnings.push(format!("Signatures {signatures_path}: {error}"));
        return;
    }
    warn_nonstandard_namespaces(list.root_element(), signatures_path, warnings);
    if let Err(e) = validate_xml_structure_with_budget(list.root_element(), budget) {
        warnings.push(format!("Signatures {signatures_path}: {e}"));
        return;
    }
    let sigs_dir = parent_dir(signatures_path);
    // Multiple signature descriptions can reference the same Seal.esl (most
    // commonly for several stamps or cross-page seals). Cache both successful
    // and failed extraction by the normalized package path so the container
    // entry is decompressed and ASN.1-decoded at most once.
    let mut appearance_cache: HashMap<String, Option<std::sync::Arc<SealAppearance>>> =
        HashMap::new();

    for sig in list
        .root_element()
        .descendants()
        .filter(|n| local(n) == "Signature")
    {
        let sig_type = match sig.attribute("Type") {
            Some(t) if t.eq_ignore_ascii_case("Sign") => SignatureType::Sign,
            _ => SignatureType::Seal,
        };
        let id = match sig.attribute("ID").filter(|id| !id.is_empty()) {
            Some(id) => id.to_string(),
            None => {
                warnings.push("Signature entry: missing required ID".into());
                String::new()
            }
        };
        let Some(base) = sig.attribute("BaseLoc") else {
            warnings.push(format!("Signature {id:?}: missing required BaseLoc"));
            continue;
        };
        // BaseLoc may be absolute (/Doc_0/...) or relative to Signatures.xml.
        let base = join(&sigs_dir, base);
        let sig_dir = parent_dir(&base);
        let Ok(sig_xml) = read_str(c, &base) else {
            warnings.push(format!("Signature {base}: unreadable"));
            continue;
        };
        let Ok(doc) = XmlDoc::parse(&sig_xml) else {
            warnings.push(format!("Signature {base}: malformed xml"));
            continue;
        };
        if let Err(error) = validate_ofd_root(doc.root_element(), "Signature", &base) {
            warnings.push(format!("Signature {base}: {error}"));
            continue;
        }
        warn_nonstandard_namespaces(doc.root_element(), &base, warnings);
        if let Err(e) = validate_xml_structure_with_budget(doc.root_element(), budget) {
            warnings.push(format!("Signature {base}: {e}"));
            continue;
        }
        let root = doc.root_element();
        let signed_info = child(root, "SignedInfo");

        // References (the protected files and their digests).
        let mut references = Vec::new();
        if let Some(refs) = signed_info.and_then(|si| child(si, "References")) {
            let check_method = refs.attribute("CheckMethod").unwrap_or("MD5").to_string();
            for r in refs.children().filter(|n| local(n) == "Reference") {
                let Some(file_ref) = r.attribute("FileRef").filter(|value| !value.is_empty())
                else {
                    warnings.push(format!(
                        "Signature {base}: Reference missing required FileRef"
                    ));
                    continue;
                };
                references.push(SignReference {
                    file_ref: file_ref.to_string(),
                    check_method: check_method.clone(),
                    check_value: child_text(r, "CheckValue").unwrap_or_default(),
                });
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
            .find_map(|loc| {
                let path = join(&sig_dir, &loc);
                if let Some(cached) = appearance_cache.get(&path) {
                    return cached.clone();
                }
                let appearance = read_bytes(c, &path)
                    .ok()
                    .and_then(|bytes| extract_seal_appearance(&bytes))
                    .map(std::sync::Arc::new);
                appearance_cache.insert(path, appearance.clone());
                appearance
            });
        let Some(appearance) = appearance else {
            warnings.push(format!(
                "Seal appearance for Signature {base}: no renderable seal picture"
            ));
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
                    clip: stamp.attribute("Clip").and_then(parse_rect),
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
        "tif" | "tiff" => SealAppearance::Raster {
            format: ImageFormat::Tiff,
            data: pic.data,
        },
        _ => SealAppearance::Raster {
            format: ImageFormat::Unknown,
            data: pic.data,
        },
    })
}

/// Parse a single page, prepending any referenced template layers underneath.
fn parse_page(
    page: Node,
    templates: &HashMap<String, ParsedTemplate>,
    template_expansion_bytes: &mut u64,
    budget: &mut ParseBudget,
    warnings: &mut Vec<String>,
) -> Result<Page> {
    let mut area = child(page, "Area").map(parse_page_area);

    let mut layers = Vec::new();
    // A template reference contributes content to its selected z-order band.
    // An explicit reference ZOrder overrides the template declaration; when it
    // is absent, the declaration supplies the template's default layer type.
    for tpl_ref in page.children().filter(|n| local(n) == "Template") {
        let Some(id) = tpl_ref
            .attribute("TemplateID")
            .filter(|value| !value.is_empty())
        else {
            warnings.push("Template reference missing required TemplateID".into());
            continue;
        };
        let Some(template) = templates.get(id) else {
            warnings.push(format!("unresolved TemplatePage id {id}"));
            continue;
        };
        charge_template_expansion(
            template_expansion_bytes,
            template.source_bytes,
            MAX_TEMPLATE_EXPANSION_BYTES,
        )?;
        budget.charge_graphic_stats(template.stats)?;
        if area.is_none() {
            area = template.area;
        }
        let override_kind = tpl_ref
            .attribute("ZOrder")
            .map(|value| parse_layer_kind(Some(value), template.default_z_order));
        layers.extend(template.layers.iter().cloned().map(|mut layer| {
            if let Some(kind) = override_kind {
                layer.kind = kind;
            }
            layer
        }));
    }
    layers.extend(parse_page_layers(page));
    Ok(Page {
        id: 0,
        area,
        layers,
        actions: parse_actions(page),
    })
}

fn charge_template_expansion(total: &mut u64, bytes: u64, limit: u64) -> Result<()> {
    let next = total
        .checked_add(bytes)
        .ok_or_else(|| OfdError::ResourceLimit("template expansion size overflow".into()))?;
    if next > limit {
        return Err(OfdError::ResourceLimit(format!(
            "expanded template sources require {next} bytes; limit is {limit}"
        )));
    }
    *total = next;
    Ok(())
}

/// Count the actual owned model cloned when a template is applied. The source
/// byte budget remains a separate bound; this count covers many-small-object and
/// expanded-vector/text attacks that compress well in XML.
fn graphic_stats_for_layers(layers: &[Layer]) -> Result<GraphicStats> {
    let mut stats = GraphicStats::default();
    add_model_items(&mut stats, layers.len() as u64, "template layers")?;
    for layer in layers {
        add_object_stats(&layer.objects, &mut stats)?;
    }
    Ok(stats)
}

fn add_model_items(stats: &mut GraphicStats, amount: u64, resource: &str) -> Result<()> {
    stats.model_items = stats
        .model_items
        .checked_add(amount)
        .ok_or_else(|| OfdError::ResourceLimit(format!("{resource} model-item count overflow")))?;
    Ok(())
}

fn add_graphic_object(stats: &mut GraphicStats) -> Result<()> {
    stats.graphic_objects = stats
        .graphic_objects
        .checked_add(1)
        .ok_or_else(|| OfdError::ResourceLimit("template graphic-object count overflow".into()))?;
    add_model_items(stats, 1, "graphic object")
}

fn add_object_stats(objects: &[GraphicObject], stats: &mut GraphicStats) -> Result<()> {
    for object in objects {
        add_graphic_object(stats)?;
        match object {
            GraphicObject::Text(text) => add_text_stats(text, stats)?,
            GraphicObject::Path(path) => add_path_stats(path, stats)?,
            GraphicObject::Image(image) => {
                add_common_stats(&image.common, stats)?;
                if image.border.is_some() {
                    add_model_items(stats, 1, "image border")?;
                }
                if let Some(color) = image
                    .border
                    .as_ref()
                    .and_then(|border| border.color.as_ref())
                {
                    add_color_stats(color, stats)?;
                }
            }
            GraphicObject::Composite(composite) => {
                add_common_stats(&composite.common, stats)?;
            }
            GraphicObject::Group(group) => add_object_stats(group, stats)?,
        }
    }
    Ok(())
}

fn add_text_stats(text: &TextObject, stats: &mut GraphicStats) -> Result<()> {
    add_common_stats(&text.common, stats)?;
    add_model_items(
        stats,
        (text.runs.len() + text.cg_transforms.len()) as u64,
        "text records",
    )?;
    for run in &text.runs {
        add_model_items(
            stats,
            run.text.chars().count() as u64 + run.delta_x.len() as u64 + run.delta_y.len() as u64,
            "text slots",
        )?;
    }
    for transform in &text.cg_transforms {
        add_model_items(stats, transform.glyphs.len() as u64, "explicit glyph ids")?;
    }
    for color in text.fill_color.iter().chain(text.stroke_color.iter()) {
        add_color_stats(color, stats)?;
    }
    Ok(())
}

fn add_path_stats(path: &PathObject, stats: &mut GraphicStats) -> Result<()> {
    add_common_stats(&path.common, stats)?;
    add_model_items(stats, path.commands.len() as u64, "path commands")?;
    for color in path.fill_color.iter().chain(path.stroke_color.iter()) {
        add_color_stats(color, stats)?;
    }
    Ok(())
}

fn add_common_stats(common: &GraphicCommon, stats: &mut GraphicStats) -> Result<()> {
    add_model_items(
        stats,
        (common.clips.len() + common.actions.len()) as u64,
        "graphic metadata",
    )?;
    for clip in &common.clips {
        add_model_items(stats, clip.areas.len() as u64, "clip areas")?;
        for area in &clip.areas {
            add_graphic_object(stats)?;
            match &area.shape {
                ClipShape::Path(path) => add_path_stats(path, stats)?,
                ClipShape::Text(text) => add_text_stats(text, stats)?,
            }
        }
    }
    for action in &common.actions {
        if let Some(region) = &action.region {
            add_model_items(stats, region.areas.len() as u64, "action regions")?;
            for area in &region.areas {
                add_model_items(stats, area.segments.len() as u64, "action segments")?;
            }
        }
    }
    Ok(())
}

fn add_color_stats(color: &OfdColor, stats: &mut GraphicStats) -> Result<()> {
    match color {
        OfdColor::Basic(color) => add_basic_color_stats(color, stats),
        OfdColor::Pattern(pattern) => {
            add_model_items(stats, 1, "pattern")?;
            add_object_stats(&pattern.cell_content, stats)
        }
        OfdColor::Axial(gradient) => {
            add_model_items(stats, gradient.segments.len() as u64 + 1, "axial gradient")?;
            for segment in &gradient.segments {
                add_basic_color_stats(&segment.color, stats)?;
            }
            Ok(())
        }
        OfdColor::Radial(gradient) => {
            add_model_items(stats, gradient.segments.len() as u64 + 1, "radial gradient")?;
            for segment in &gradient.segments {
                add_basic_color_stats(&segment.color, stats)?;
            }
            Ok(())
        }
        OfdColor::Gouraud(gradient) => {
            add_model_items(stats, gradient.points.len() as u64 + 1, "Gouraud gradient")?;
            for point in &gradient.points {
                add_basic_color_stats(&point.color, stats)?;
            }
            if let Some(color) = &gradient.back_color {
                add_basic_color_stats(color, stats)?;
            }
            Ok(())
        }
        OfdColor::LatticeGouraud(gradient) => {
            add_model_items(
                stats,
                gradient.points.len() as u64 + 1,
                "lattice Gouraud gradient",
            )?;
            for point in &gradient.points {
                add_basic_color_stats(&point.color, stats)?;
            }
            if let Some(color) = &gradient.back_color {
                add_basic_color_stats(color, stats)?;
            }
            Ok(())
        }
    }
}

fn add_basic_color_stats(color: &BasicColor, stats: &mut GraphicStats) -> Result<()> {
    add_model_items(
        stats,
        color
            .components
            .as_ref()
            .map_or(1, |components| components.len() + 1) as u64,
        "color components",
    )
}

/// Extract the drawing layers from a page or template page node.
fn parse_page_layers(page: Node) -> Vec<Layer> {
    parse_page_layers_with_default(page, LayerKind::Body)
}

fn parse_page_layers_with_default(page: Node, default_kind: LayerKind) -> Vec<Layer> {
    let mut layers = Vec::new();
    if let Some(content) = child(page, "Content") {
        for layer_node in content.children().filter(|n| local(n) == "Layer") {
            let kind = parse_layer_kind(layer_node.attribute("Type"), default_kind);
            let mut objects = Vec::new();
            for obj in layer_node.children().filter(|n| n.is_element()) {
                if let Some(o) = parse_object(obj) {
                    objects.push(o);
                }
            }
            // Keep the layer default separate from each object's own DrawParam.
            // The renderer resolves object -> layer -> standard defaults, so an
            // object-local style remains distinguishable and authoritative.
            let draw_param = layer_node
                .attribute("DrawParam")
                .and_then(|s| s.parse().ok());
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

fn parse_layer_kind(value: Option<&str>, default: LayerKind) -> LayerKind {
    match value {
        Some("Background") => LayerKind::Background,
        Some("Body") => LayerKind::Body,
        Some("Foreground") => LayerKind::Foreground,
        Some("Custom") => LayerKind::Custom,
        _ => default,
    }
}

fn parse_object(node: Node) -> Option<GraphicObject> {
    match local(&node) {
        "TextObject" => Some(GraphicObject::Text(parse_text(node))),
        "PathObject" => Some(GraphicObject::Path(parse_path(node))),
        "ImageObject" => Some(GraphicObject::Image(parse_image(node))),
        "PageBlock" => Some(GraphicObject::Group(parse_page_block_children(node))),
        "CompositeObject" => Some(GraphicObject::Composite(CompositeObject {
            common: parse_common(node),
            resource_id: attr_u64(node, "ResourceID"),
        })),
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
        visible: attr_xs_boolean(node, "Visible", true),
        ctm: node
            .attribute("CTM")
            .and_then(parse_matrix)
            .unwrap_or(Matrix::IDENTITY),
        draw_param: node.attribute("DrawParam").and_then(|s| s.parse().ok()),
        line_width: node.attribute("LineWidth").and_then(parse_f32),
        cap: node.attribute("Cap").map(|s| parse_cap(Some(s))),
        join: node.attribute("Join").map(|s| parse_join(Some(s))),
        miter_limit: node.attribute("MiterLimit").and_then(parse_f32),
        dash_offset: node.attribute("DashOffset").and_then(parse_f32),
        dash_pattern: node
            .attribute("DashPattern")
            .map(parse_floats)
            .filter(|v| !v.is_empty()),
        alpha: node
            .attribute("Alpha")
            .and_then(parse_f32)
            .map(|a| a.clamp(0.0, 255.0) as u8)
            .unwrap_or(255),
        clips: parse_clips(node),
        actions: parse_actions(node),
    }
}

/// Parse an `Actions/Action` list (§14) directly under `node` (a graphic object,
/// page, document, or outline node). Returns an empty vec when absent.
fn parse_actions(node: Node) -> Vec<Action> {
    let Some(list) = child(node, "Actions") else {
        return Vec::new();
    };
    list.children()
        .filter(|n| local(n) == "Action")
        .filter_map(parse_action)
        .collect()
}

/// Parse one `CT_Action` (§14): its `@Event`, optional `Region`, and the single
/// behavior element (Goto/URI/GotoA/Sound/Movie).
fn parse_action(node: Node) -> Option<Action> {
    let event = match node.attribute("Event") {
        Some("DO") => ActionEvent::DocumentOpen,
        Some("PO") => ActionEvent::PageOpen,
        _ => ActionEvent::Click,
    };
    let region = child(node, "Region").map(parse_region);
    // The behavior is the first element child that is not the Region.
    let kind = node
        .children()
        .filter(|n| n.is_element() && local(n) != "Region")
        .map(|c| match local(&c) {
            "Goto" => ActionKind::Goto(parse_goto(c)),
            "URI" => ActionKind::Uri(UriAction {
                uri: c.attribute("URI").unwrap_or("").to_string(),
                base: c.attribute("Base").map(|s| s.to_string()),
                target: c.attribute("Target").map(|s| s.to_string()),
            }),
            "GotoA" => ActionKind::GotoAttachment(GotoAttachment {
                attach_id: c.attribute("AttachID").unwrap_or("").to_string(),
                new_window: attr_xs_boolean(c, "NewWindow", true),
            }),
            "Sound" => ActionKind::Sound(SoundAction {
                resource_id: attr_u64(c, "ResourceID"),
                volume: c.attribute("Volume").and_then(|s| s.parse().ok()),
                repeat: c.attribute("Repeat").and_then(parse_xs_boolean),
                synchronous: c.attribute("Synchronous").and_then(parse_xs_boolean),
            }),
            "Movie" => ActionKind::Movie(MovieAction {
                resource_id: attr_u64(c, "ResourceID"),
                operator: match c.attribute("Operator") {
                    Some("Stop") => MovieOperator::Stop,
                    Some("Pause") => MovieOperator::Pause,
                    Some("Resume") => MovieOperator::Resume,
                    _ => MovieOperator::Play,
                },
            }),
            other => ActionKind::Other(other.to_string()),
        })
        .next()?;
    Some(Action {
        event,
        region,
        kind,
    })
}

/// Parse a `Goto` target (§14.2): an explicit `Dest` or a named `Bookmark`.
fn parse_goto(node: Node) -> GotoTarget {
    if let Some(dest) = child(node, "Dest") {
        GotoTarget::Dest(parse_dest(dest))
    } else {
        GotoTarget::Bookmark(
            child(node, "Bookmark")
                .and_then(|b| b.attribute("Name"))
                .unwrap_or("")
                .to_string(),
        )
    }
}

/// Parse a `CT_Dest` jump destination (§14.2, 表54).
fn parse_dest(node: Node) -> Dest {
    let f = |name: &str| node.attribute(name).and_then(parse_f32);
    Dest {
        kind: match node.attribute("Type") {
            Some("Fit") => DestKind::Fit,
            Some("FitH") => DestKind::FitH,
            Some("FitV") => DestKind::FitV,
            Some("FitR") => DestKind::FitR,
            _ => DestKind::Xyz,
        },
        page_id: attr_u64(node, "PageID"),
        left: f("Left"),
        top: f("Top"),
        right: f("Right"),
        bottom: f("Bottom"),
        zoom: f("Zoom"),
    }
}

/// Parse a `CT_Region` (§14.1): a set of `Area` outlines of explicit segments.
fn parse_region(node: Node) -> Region {
    Region {
        areas: node
            .children()
            .filter(|n| local(n) == "Area")
            .map(parse_region_area)
            .collect(),
    }
}

fn parse_region_area(node: Node) -> RegionArea {
    let start = node
        .attribute("Start")
        .and_then(parse_point)
        .unwrap_or(Point { x: 0.0, y: 0.0 });
    let mut segments = Vec::new();
    for c in node.children().filter(|n| n.is_element()) {
        let seg =
            match local(&c) {
                "Move" => c
                    .attribute("Point1")
                    .and_then(parse_point)
                    .map(RegionSegment::Move),
                "Line" => c
                    .attribute("Point1")
                    .and_then(parse_point)
                    .map(RegionSegment::Line),
                "QuadraticBezier" => match (
                    c.attribute("Point1").and_then(parse_point),
                    c.attribute("Point2").and_then(parse_point),
                ) {
                    (Some(p1), Some(p2)) => Some(RegionSegment::QuadraticBezier { p1, p2 }),
                    _ => None,
                },
                "CubicBezier" => c.attribute("Point3").and_then(parse_point).map(|p3| {
                    RegionSegment::CubicBezier {
                        p1: c.attribute("Point1").and_then(parse_point),
                        p2: c.attribute("Point2").and_then(parse_point),
                        p3,
                    }
                }),
                "Arc" => {
                    let es = c
                        .attribute("EllipseSize")
                        .map(parse_floats)
                        .unwrap_or_default();
                    match (
                        es.first(),
                        es.get(1),
                        c.attribute("EndPoint").and_then(parse_point),
                    ) {
                        (Some(&rx), Some(&ry), Some(end)) => Some(RegionSegment::Arc {
                            ellipse_size: (rx, ry),
                            rotation_angle: c
                                .attribute("RotationAngle")
                                .and_then(parse_f32)
                                .unwrap_or(0.0),
                            large_arc: attr_xs_boolean(c, "LargeArc", false),
                            sweep_clockwise: attr_xs_boolean(c, "SweepDirection", false),
                            end,
                        }),
                        _ => None,
                    }
                }
                "Close" => Some(RegionSegment::Close),
                _ => None,
            };
        if let Some(s) = seg {
            segments.push(s);
        }
    }
    RegionArea { start, segments }
}

/// Parse a document's `Bookmarks` (§7): named destinations for `Goto/Bookmark`.
fn parse_bookmarks(root: Node) -> Vec<Bookmark> {
    let Some(list) = child(root, "Bookmarks") else {
        return Vec::new();
    };
    list.children()
        .filter(|n| local(n) == "Bookmark")
        .map(|b| Bookmark {
            name: b.attribute("Name").unwrap_or("").to_string(),
            dest: child(b, "Dest").map(parse_dest),
        })
        .collect()
}

/// Parse a document's `Outlines` (§7) into [`OutlineItem`]s, resolving each
/// node's target page (via its first `Goto/Dest`) to a page index.
fn parse_outlines(root: Node, page_index: &HashMap<u64, usize>) -> Vec<OutlineItem> {
    let Some(outlines) = child(root, "Outlines") else {
        return Vec::new();
    };
    outlines
        .children()
        .filter(|n| local(n) == "OutlineElem")
        .map(|e| parse_outline_elem(e, page_index))
        .collect()
}

fn parse_outline_elem(node: Node, page_index: &HashMap<u64, usize>) -> OutlineItem {
    let actions = parse_actions(node);
    let target_page = actions.iter().find_map(|a| match &a.kind {
        ActionKind::Goto(GotoTarget::Dest(d)) => page_index.get(&d.page_id).copied(),
        _ => None,
    });
    OutlineItem {
        title: node.attribute("Title").unwrap_or("").to_string(),
        page_index: target_page,
        children: node
            .children()
            .filter(|n| local(n) == "OutlineElem")
            .map(|c| parse_outline_elem(c, page_index))
            .collect(),
        actions,
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

/// Parse an object's `Clips/Clip/Area` hierarchy (§8.4). Areas within one Clip
/// form a union; multiple Clip values form an intersection.
fn parse_clips(node: Node) -> Vec<Clip> {
    let mut out = Vec::new();
    let Some(clips) = child(node, "Clips") else {
        return out;
    };
    for clip in clips.children().filter(|n| local(n) == "Clip") {
        let mut areas = Vec::new();
        for area in clip.children().filter(|n| local(n) == "Area") {
            let ctm = area
                .attribute("CTM")
                .and_then(parse_matrix)
                .unwrap_or(Matrix::IDENTITY);
            let draw_param = area.attribute("DrawParam").and_then(|s| s.parse().ok());
            let shape = if let Some(path) = child(area, "Path") {
                Some(ClipShape::Path(Box::new(parse_path(path))))
            } else {
                child(area, "Text").map(|t| ClipShape::Text(Box::new(parse_text(t))))
            };
            if let Some(shape) = shape {
                areas.push(ClipArea {
                    ctm,
                    draw_param,
                    shape,
                });
            }
        }
        if !areas.is_empty() {
            out.push(Clip { areas });
        }
    }
    out
}

fn parse_text(node: Node) -> TextObject {
    let mut runs = Vec::new();
    let mut last_x = 0.0;
    let mut last_y = 0.0;
    for tc in node.children().filter(|n| local(n) == "TextCode") {
        let origin_x = tc.attribute("X").and_then(parse_f32).unwrap_or(last_x);
        let origin_y = tc.attribute("Y").and_then(parse_f32).unwrap_or(last_y);
        last_x = origin_x;
        last_y = origin_y;
        runs.push(TextRun {
            text: decode_text_code(&clean_text_code(tc.text().unwrap_or(""))),
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
                s.split(|c: char| c == ',' || c.is_ascii_whitespace())
                    .filter(|t| !t.is_empty())
                    // Package parsing validates every token first. Retaining a
                    // `.notdef` slot here also keeps direct/internal callers
                    // aligned instead of shifting later glyph ids left.
                    .map(|t| t.parse::<u16>().unwrap_or(0))
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
                .unwrap_or_else(|| glyphs.len().max(1))
                .min(MAX_TEXT_SLOTS),
            glyphs,
        });
    }

    TextObject {
        common: parse_common(node),
        font_id: attr_u64(node, "Font"),
        font_size: node.attribute("Size").and_then(parse_f32).unwrap_or(0.0),
        stroke: attr_xs_boolean(node, "Stroke", false),
        fill: attr_xs_boolean(node, "Fill", true),
        h_scale: node.attribute("HScale").and_then(parse_f32).unwrap_or(1.0),
        read_direction: Direction(attr_u16(node, "ReadDirection", 0)),
        char_direction: Direction(attr_u16(node, "CharDirection", 0)),
        weight: attr_u16(node, "Weight", 400),
        italic: attr_xs_boolean(node, "Italic", false),
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
        stroke: attr_xs_boolean(node, "Stroke", true),
        fill: attr_xs_boolean(node, "Fill", false),
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
                .and_then(parse_f32)
                .unwrap_or(0.353),
            horizontal_corner_radius: b
                .attribute("HorizonalCornerRadius")
                .and_then(parse_f32)
                .unwrap_or(0.0),
            vertical_corner_radius: b
                .attribute("VerticalCornerRadius")
                .and_then(parse_f32)
                .unwrap_or(0.0),
            dash_offset: b.attribute("DashOffset").and_then(parse_f32).unwrap_or(0.0),
            dash_pattern: b
                .attribute("DashPattern")
                .map(parse_floats)
                .filter(|pattern| !pattern.is_empty()),
            color: inline_color(b, "BorderColor"),
        }),
    }
}

// ---- Resources -------------------------------------------------------------

#[derive(Default)]
struct ResourceRefs {
    fonts: BTreeSet<u64>,
    required_images: BTreeSet<u64>,
    image_alternatives: BTreeSet<(u64, Option<u64>)>,
    composites: BTreeSet<u64>,
    draw_params: BTreeSet<u64>,
    color_spaces: BTreeSet<u64>,
}

fn validate_resource_references(
    pages: &[Page],
    annotations: &[Annotation],
    resources: &Resources,
    missing_media: &HashMap<u64, String>,
    warnings: &mut Vec<String>,
) {
    let mut refs = ResourceRefs::default();
    for page in pages {
        for layer in &page.layers {
            if let Some(id) = layer.draw_param {
                refs.draw_params.insert(id);
            }
            collect_resource_refs(&layer.objects, &mut refs);
        }
    }
    for annotation in annotations {
        collect_resource_refs(&annotation.objects, &mut refs);
    }
    for unit in &resources.composite_graphic_units {
        collect_resource_refs(&unit.objects, &mut refs);
        if unit.width <= 0.0 || unit.height <= 0.0 {
            warnings.push(format!(
                "CompositeGraphicUnit {} has invalid size {}x{}",
                unit.id, unit.width, unit.height
            ));
        }
    }

    let font_ids: HashSet<u64> = resources.fonts.iter().map(|f| f.id).collect();
    let image_ids: HashSet<u64> = resources.images.iter().map(|i| i.id).collect();
    let composite_ids: HashSet<u64> = resources
        .composite_graphic_units
        .iter()
        .map(|u| u.id)
        .collect();
    let draw_param_ids: HashSet<u64> = resources.draw_params.iter().map(|d| d.id).collect();
    let color_space_ids: HashSet<u64> = resources.color_spaces.iter().map(|c| c.id).collect();

    warn_missing(&refs.fonts, &font_ids, "font", warnings);
    for id in refs
        .required_images
        .iter()
        .filter(|id| !image_ids.contains(id))
    {
        warnings.push(missing_image_message(*id, missing_media));
    }
    for &(primary, substitution) in &refs.image_alternatives {
        if image_ids.contains(&primary) || substitution.is_some_and(|id| image_ids.contains(&id)) {
            continue;
        }
        match substitution {
            Some(fallback) => warnings.push(format!(
                "{}; substitution also unavailable: {}",
                missing_image_message(primary, missing_media),
                missing_image_message(fallback, missing_media)
            )),
            None => warnings.push(missing_image_message(primary, missing_media)),
        }
    }
    warn_missing(&refs.composites, &composite_ids, "composite", warnings);
    warn_missing(&refs.draw_params, &draw_param_ids, "DrawParam", warnings);
    warn_missing(&refs.color_spaces, &color_space_ids, "ColorSpace", warnings);

    validate_draw_param_graph(resources, warnings);
    validate_composite_graph(resources, warnings);
}

fn missing_image_message(id: u64, missing_media: &HashMap<u64, String>) -> String {
    match missing_media.get(&id) {
        Some(file) => format!("image {file} (id {id}) referenced but missing from container"),
        None => format!("unresolved image resource id {id}"),
    }
}

fn warn_missing(
    referenced: &BTreeSet<u64>,
    available: &HashSet<u64>,
    kind: &str,
    warnings: &mut Vec<String>,
) {
    for id in referenced.iter().filter(|id| !available.contains(id)) {
        warnings.push(format!("unresolved {kind} resource id {id}"));
    }
}

fn collect_resource_refs(objects: &[GraphicObject], refs: &mut ResourceRefs) {
    for object in objects {
        match object {
            GraphicObject::Text(text) => collect_text_refs(text, refs),
            GraphicObject::Path(path) => collect_path_refs(path, refs),
            GraphicObject::Image(image) => {
                collect_common_refs(&image.common, refs);
                refs.image_alternatives
                    .insert((image.resource_id, image.substitution));
                refs.required_images.extend(image.image_mask);
                if let Some(color) = image.border.as_ref().and_then(|b| b.color.as_ref()) {
                    collect_color_refs(color, refs);
                }
            }
            GraphicObject::Group(group) => collect_resource_refs(group, refs),
            GraphicObject::Composite(composite) => {
                collect_common_refs(&composite.common, refs);
                refs.composites.insert(composite.resource_id);
            }
        }
    }
}

fn collect_text_refs(text: &TextObject, refs: &mut ResourceRefs) {
    refs.fonts.insert(text.font_id);
    collect_common_refs(&text.common, refs);
    text.fill_color
        .as_ref()
        .into_iter()
        .chain(text.stroke_color.as_ref())
        .for_each(|color| collect_color_refs(color, refs));
}

fn collect_path_refs(path: &PathObject, refs: &mut ResourceRefs) {
    collect_common_refs(&path.common, refs);
    path.fill_color
        .as_ref()
        .into_iter()
        .chain(path.stroke_color.as_ref())
        .for_each(|color| collect_color_refs(color, refs));
}

fn collect_common_refs(common: &GraphicCommon, refs: &mut ResourceRefs) {
    refs.draw_params.extend(common.draw_param);
    for clip in &common.clips {
        for area in &clip.areas {
            refs.draw_params.extend(area.draw_param);
            match &area.shape {
                ClipShape::Path(path) => collect_path_refs(path, refs),
                ClipShape::Text(text) => collect_text_refs(text, refs),
            }
        }
    }
}

fn collect_color_refs(color: &OfdColor, refs: &mut ResourceRefs) {
    let mut basic = |color: &BasicColor| {
        refs.color_spaces.extend(color.color_space);
    };
    match color {
        OfdColor::Basic(color) => basic(color),
        OfdColor::Pattern(pattern) => collect_resource_refs(&pattern.cell_content, refs),
        OfdColor::Axial(gradient) => {
            gradient.segments.iter().for_each(|s| basic(&s.color));
        }
        OfdColor::Radial(gradient) => {
            gradient.segments.iter().for_each(|s| basic(&s.color));
        }
        OfdColor::Gouraud(gradient) => {
            gradient.points.iter().for_each(|p| basic(&p.color));
            if let Some(color) = &gradient.back_color {
                basic(color);
            }
        }
        OfdColor::LatticeGouraud(gradient) => {
            gradient.points.iter().for_each(|p| basic(&p.color));
            if let Some(color) = &gradient.back_color {
                basic(color);
            }
        }
    }
}

fn validate_draw_param_graph(resources: &Resources, warnings: &mut Vec<String>) {
    let mut graph = HashMap::new();
    for draw_param in &resources.draw_params {
        graph.entry(draw_param.id).or_insert(draw_param.relative);
    }
    let mut states = HashMap::new();
    let mut reported_cycles = BTreeSet::new();
    let mut reported_missing = BTreeSet::new();
    let mut starts: Vec<u64> = graph.keys().copied().collect();
    starts.sort_unstable();
    for start in starts {
        if states.get(&start) == Some(&GraphVisit::Complete) {
            continue;
        }
        let mut path = Vec::new();
        let mut current = Some(start);
        while let Some(id) = current {
            match states.get(&id) {
                Some(GraphVisit::Visiting) => {
                    if reported_cycles.insert(id) {
                        warnings.push(format!("DrawParam Relative cycle contains id {id}"));
                    }
                    break;
                }
                Some(GraphVisit::Complete) => break,
                None => {}
            }
            states.insert(id, GraphVisit::Visiting);
            path.push(id);
            current = match graph.get(&id) {
                Some(Some(next)) if graph.contains_key(next) => Some(*next),
                Some(Some(next)) => {
                    if reported_missing.insert((id, *next)) {
                        warnings.push(format!("DrawParam {id} has unresolved Relative id {next}"));
                    }
                    None
                }
                Some(None) => None,
                None => {
                    if reported_missing.insert((start, id)) {
                        warnings.push(format!("DrawParam {start} has unresolved Relative id {id}"));
                    }
                    None
                }
            };
        }
        for id in path {
            states.insert(id, GraphVisit::Complete);
        }
    }
}

fn validate_composite_graph(resources: &Resources, warnings: &mut Vec<String>) {
    let mut graph: HashMap<u64, Vec<u64>> = HashMap::new();
    for unit in &resources.composite_graphic_units {
        let mut refs = ResourceRefs::default();
        collect_resource_refs(&unit.objects, &mut refs);
        graph
            .entry(unit.id)
            .or_insert_with(|| refs.composites.into_iter().collect());
    }
    let mut states = HashMap::new();
    let mut reported = BTreeSet::new();
    let mut starts: Vec<u64> = graph.keys().copied().collect();
    starts.sort_unstable();
    for start in starts {
        if states.get(&start) == Some(&GraphVisit::Complete) {
            continue;
        }
        states.insert(start, GraphVisit::Visiting);
        let mut stack = vec![(start, 0usize)];
        while let Some((id, next_child)) = stack.last_mut() {
            let Some(children) = graph.get(id) else {
                states.insert(*id, GraphVisit::Complete);
                stack.pop();
                continue;
            };
            if let Some(child) = children.get(*next_child).copied() {
                *next_child += 1;
                if !graph.contains_key(&child) {
                    continue;
                }
                match states.get(&child) {
                    Some(GraphVisit::Visiting) => {
                        if reported.insert(child) {
                            warnings.push(format!(
                                "CompositeGraphicUnit reference cycle contains id {child}"
                            ));
                        }
                    }
                    Some(GraphVisit::Complete) => {}
                    None => {
                        states.insert(child, GraphVisit::Visiting);
                        stack.push((child, 0));
                    }
                }
            } else {
                states.insert(*id, GraphVisit::Complete);
                stack.pop();
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GraphVisit {
    Visiting,
    Complete,
}

fn parse_resources(
    c: &mut Container,
    res: Node,
    dir: &str,
    out: &mut Resources,
    warnings: &mut Vec<String>,
    missing_media: &mut HashMap<u64, String>,
) {
    let res_base = join(dir, res.attribute("BaseLoc").unwrap_or("Res"));
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
                        let path = join(&res_base, &file);
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
                        italic: attr_xs_boolean(f, "Italic", false),
                        bold: attr_xs_boolean(f, "Bold", false),
                        serif: attr_xs_boolean(f, "Serif", false),
                        fixed_width: attr_xs_boolean(f, "FixedWidth", false),
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
                        let path = join(&res_base, &file);
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
                                missing_media.entry(id).or_insert(file);
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
                        line_width: d.attribute("LineWidth").and_then(parse_f32),
                        cap: d.attribute("Cap").map(|s| parse_cap(Some(s))),
                        join: d.attribute("Join").map(|s| parse_join(Some(s))),
                        miter_limit: d.attribute("MiterLimit").and_then(parse_f32),
                        dash_offset: d.attribute("DashOffset").and_then(parse_f32),
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
                    let id = attr_u64(cs, "ID");
                    let kind = match cs.attribute("Type") {
                        Some("GRAY") => ColorSpaceKind::Gray,
                        Some("CMYK") => ColorSpaceKind::Cmyk,
                        _ => ColorSpaceKind::Rgb,
                    };
                    let bits_per_component = cs
                        .attribute("BitsPerComponent")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(8);
                    let bits_per_component = if matches!(bits_per_component, 1 | 2 | 4 | 8 | 16) {
                        bits_per_component
                    } else {
                        warnings.push(format!(
                            "ColorSpace {id} has invalid BitsPerComponent {bits_per_component}; using 8"
                        ));
                        8
                    };
                    let profile = cs.attribute("Profile").and_then(|location| {
                        if location.is_empty() {
                            warnings.push(format!(
                                "ColorSpace {id} has an empty Profile location"
                            ));
                            return None;
                        }
                        let path = join(&res_base, location);
                        let data = match read_bytes(c, &path) {
                            Ok(data) => data,
                            Err(error) => {
                                warnings.push(format!(
                                    "ColorSpace profile {path} (id {id}): {error}"
                                ));
                                return None;
                            }
                        };
                        if data.len() > MAX_ICC_PROFILE_BYTES {
                            warnings.push(format!(
                                "ColorSpace profile {path} (id {id}) is {} bytes; limit is {MAX_ICC_PROFILE_BYTES}",
                                data.len()
                            ));
                            return None;
                        }
                        match moxcms::ColorProfile::new_from_slice(&data) {
                            Ok(parsed) => {
                                let expected = match kind {
                                    ColorSpaceKind::Gray => moxcms::DataColorSpace::Gray,
                                    ColorSpaceKind::Rgb => moxcms::DataColorSpace::Rgb,
                                    ColorSpaceKind::Cmyk => moxcms::DataColorSpace::Cmyk,
                                };
                                if parsed.color_space != expected {
                                    warnings.push(format!(
                                        "ColorSpace profile {path} (id {id}) declares {:?}, expected {:?}",
                                        parsed.color_space, expected
                                    ));
                                }
                            }
                            Err(error) => warnings.push(format!(
                                "ColorSpace profile {path} (id {id}) is invalid: {error}"
                            )),
                        }
                        Some(IccProfile {
                            location: path,
                            data: std::sync::Arc::new(data),
                        })
                    });
                    out.color_spaces.push(ColorSpace {
                        id,
                        kind,
                        bits_per_component,
                        palette: child(cs, "Palette")
                            .map(|p| {
                                p.children()
                                    .filter(|n| local(n) == "CV")
                                    .filter_map(|cv| cv.text())
                                    .map(parse_color_components)
                                    .collect()
                            })
                            .unwrap_or_default(),
                        profile,
                    });
                }
            }
            "CompositeGraphicUnits" => {
                // CT_VectorG (§13): each unit holds a `Content` of graphic
                // objects in its own coordinate space, drawn via CompositeObject.
                for u in group
                    .children()
                    .filter(|n| local(n) == "CompositeGraphicUnit")
                {
                    let objects = u
                        .children()
                        .filter(|n| local(n) == "Content")
                        .flat_map(|content| content.children().filter(|n| n.is_element()))
                        .filter_map(parse_object)
                        .collect();
                    out.composite_graphic_units.push(CompositeGraphicUnit {
                        id: attr_u64(u, "ID"),
                        width: u.attribute("Width").and_then(parse_f32).unwrap_or(0.0),
                        height: u.attribute("Height").and_then(parse_f32).unwrap_or(0.0),
                        objects,
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
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
        "TIF" | "TIFF" => ImageFormat::Tiff,
        "JB2" | "JBIG2" | "GBIG2" => ImageFormat::Jbig2,
        "CCITT" | "FAX" => ImageFormat::Ccitt,
        _ => ImageFormat::Unknown,
    }
}

// ---- AbbreviatedData path parsing ------------------------------------------

/// Parse OFD `AbbreviatedData` (§9.3). Operators: `S`/`M` (start/move),
/// `L` (line), `Q` (quadratic), `B` (cubic), `A` (elliptical arc), `C` (close).
/// Arcs are converted to cubic Béziers since the renderer draws Béziers.
pub fn parse_abbreviated_data(s: &str) -> Vec<PathCommand> {
    let mut out = Vec::new();
    let mut nums: Vec<f32> = Vec::new();
    let mut op: Option<char> = None;
    let mut cur = (0.0f32, 0.0f32); // current point
    let mut sub_start = (0.0f32, 0.0f32); // start of current subpath

    for tok in s
        .split([' ', ',', '\n', '\r', '\t'])
        .filter(|t| !t.is_empty())
    {
        if let Ok(n) = tok.parse::<f32>() {
            if !n.is_finite() {
                // A non-finite operand invalidates the current command. OFD
                // coordinates use XML Schema finite numeric values, and
                // forwarding NaN/inf into the rasterizer produces undefined
                // geometry.
                nums.clear();
                op = None;
            } else if nums.len() < 7 {
                // Seven is the largest operand count of any abbreviated path
                // operator (`A`). Extra values are ignored by the grammar and
                // must not grow an attacker-controlled temporary vector.
                nums.push(n);
            }
        } else if let Some(ch) = tok.chars().next() {
            if let Some(prev) = op.take() {
                emit_path_op(prev, &nums, &mut out, &mut cur, &mut sub_start);
                if out.len() >= MAX_PATH_COMMANDS {
                    out.truncate(MAX_PATH_COMMANDS);
                    return out;
                }
            }
            nums.clear();
            op = Some(ch);
            if ch == 'C' {
                // Close takes no operands; emit immediately.
                emit_path_op('C', &nums, &mut out, &mut cur, &mut sub_start);
                if out.len() >= MAX_PATH_COMMANDS {
                    out.truncate(MAX_PATH_COMMANDS);
                    return out;
                }
                op = None;
            }
        }
    }
    if let Some(prev) = op {
        emit_path_op(prev, &nums, &mut out, &mut cur, &mut sub_start);
    }
    out.truncate(MAX_PATH_COMMANDS);
    out
}

/// Emit the path command(s) for one operator, tracking the current point.
fn emit_path_op(
    op: char,
    nums: &[f32],
    out: &mut Vec<PathCommand>,
    cur: &mut (f32, f32),
    sub_start: &mut (f32, f32),
) {
    match op {
        // 'S' starts a subpath edge, 'M' moves the current point; both begin a
        // new subpath at the point.
        'S' | 'M' if nums.len() >= 2 => {
            out.push(PathCommand::MoveTo {
                x: nums[0],
                y: nums[1],
            });
            *cur = (nums[0], nums[1]);
            *sub_start = *cur;
        }
        'L' if nums.len() >= 2 => {
            out.push(PathCommand::LineTo {
                x: nums[0],
                y: nums[1],
            });
            *cur = (nums[0], nums[1]);
        }
        'Q' if nums.len() >= 4 => {
            out.push(PathCommand::QuadTo {
                x1: nums[0],
                y1: nums[1],
                x: nums[2],
                y: nums[3],
            });
            *cur = (nums[2], nums[3]);
        }
        'B' if nums.len() >= 6 => {
            out.push(PathCommand::CubicTo {
                x1: nums[0],
                y1: nums[1],
                x2: nums[2],
                y2: nums[3],
                x: nums[4],
                y: nums[5],
            });
            *cur = (nums[4], nums[5]);
        }
        // A rx ry angle large sweep x y — elliptical arc (§9.3.5).
        'A' if nums.len() >= 7 => {
            arc_to_cubics(
                *cur,
                (nums[0], nums[1]),
                nums[2],
                (nums[3] != 0.0, nums[4] != 0.0),
                (nums[5], nums[6]),
                out,
            );
            *cur = (nums[5], nums[6]);
        }
        'C' => {
            out.push(PathCommand::Close);
            *cur = *sub_start;
        }
        _ => {}
    }
}

/// Convert an SVG/OFD elliptical arc (endpoint parameterization) to cubic
/// Béziers, appending them to `out`.
fn arc_to_cubics(
    (x1, y1): (f32, f32),
    (mut rx, mut ry): (f32, f32),
    phi_deg: f32,
    (large, sweep): (bool, bool),
    (x2, y2): (f32, f32),
    out: &mut Vec<PathCommand>,
) {
    use std::f32::consts::PI;
    let arc_start = out.len();
    let fallback_line = |out: &mut Vec<PathCommand>| {
        out.truncate(arc_start);
        out.push(PathCommand::LineTo { x: x2, y: y2 });
    };
    if (x1 - x2).abs() < 1e-6 && (y1 - y2).abs() < 1e-6 {
        return; // zero-length arc
    }
    rx = rx.abs();
    ry = ry.abs();
    if rx < 1e-6 || ry < 1e-6 {
        out.push(PathCommand::LineTo { x: x2, y: y2 });
        return;
    }
    let phi = phi_deg.to_radians();
    let (cos_p, sin_p) = (phi.cos(), phi.sin());

    // Step 1: (x1', y1') in the rotated frame.
    let dx = (x1 - x2) / 2.0;
    let dy = (y1 - y2) / 2.0;
    let x1p = cos_p * dx + sin_p * dy;
    let y1p = -sin_p * dx + cos_p * dy;

    // Correct out-of-range radii.
    let lambda = x1p * x1p / (rx * rx) + y1p * y1p / (ry * ry);
    if !lambda.is_finite() {
        fallback_line(out);
        return;
    }
    if lambda > 1.0 {
        let s = lambda.sqrt();
        rx *= s;
        ry *= s;
    }

    // Step 2: center (cx', cy') in the rotated frame.
    let sign = if large != sweep { 1.0 } else { -1.0 };
    let num = (rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p).max(0.0);
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let coef = sign * (num / den).sqrt();
    let cxp = coef * rx * y1p / ry;
    let cyp = coef * -ry * x1p / rx;

    // Step 3: center in the original frame.
    let cx = cos_p * cxp - sin_p * cyp + (x1 + x2) / 2.0;
    let cy = sin_p * cxp + cos_p * cyp + (y1 + y2) / 2.0;

    // Step 4: start angle and sweep angle.
    let ang = |ux: f32, uy: f32, vx: f32, vy: f32| {
        let dot = ux * vx + uy * vy;
        let len = ((ux * ux + uy * uy) * (vx * vx + vy * vy)).sqrt();
        let mut a = (dot / len).clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 {
            a = -a;
        }
        a
    };
    let ux = (x1p - cxp) / rx;
    let uy = (y1p - cyp) / ry;
    let theta1 = ang(1.0, 0.0, ux, uy);
    let mut dtheta = ang(ux, uy, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if ![rx, ry, cx, cy, theta1, dtheta]
        .into_iter()
        .all(f32::is_finite)
    {
        fallback_line(out);
        return;
    }
    if !sweep && dtheta > 0.0 {
        dtheta -= 2.0 * PI;
    } else if sweep && dtheta < 0.0 {
        dtheta += 2.0 * PI;
    }

    // Step 5: split into <=90° segments, one cubic each.
    let segments = (dtheta.abs() / (PI / 2.0)).ceil().max(1.0) as usize;
    let delta = dtheta / segments as f32;
    let t = 4.0 / 3.0 * (delta / 4.0).tan();

    let point = |theta: f32| {
        let (ct, st) = (theta.cos(), theta.sin());
        (
            cx + rx * ct * cos_p - ry * st * sin_p,
            cy + rx * ct * sin_p + ry * st * cos_p,
        )
    };
    let deriv = |theta: f32| {
        let (ct, st) = (theta.cos(), theta.sin());
        (
            -rx * st * cos_p - ry * ct * sin_p,
            -rx * st * sin_p + ry * ct * cos_p,
        )
    };

    let mut theta = theta1;
    let (mut px, mut py) = (x1, y1);
    for _ in 0..segments {
        let theta2 = theta + delta;
        let (ex, ey) = point(theta2);
        let (d1x, d1y) = deriv(theta);
        let (d2x, d2y) = deriv(theta2);
        let controls = [
            px + t * d1x,
            py + t * d1y,
            ex - t * d2x,
            ey - t * d2y,
            ex,
            ey,
        ];
        if !controls.into_iter().all(f32::is_finite) {
            fallback_line(out);
            return;
        }
        out.push(PathCommand::CubicTo {
            x1: controls[0],
            y1: controls[1],
            x2: controls[2],
            y2: controls[3],
            x: controls[4],
            y: controls[5],
        });
        px = ex;
        py = ey;
        theta = theta2;
    }
}

// ---- Small helpers ---------------------------------------------------------

fn read_str(c: &mut Container, path: &str) -> Result<String> {
    let bytes = c.read_normalized(path)?;
    if bytes.len() > MAX_XML_BYTES {
        return Err(OfdError::ResourceLimit(format!(
            "XML entry {path:?} is {} bytes; limit is {MAX_XML_BYTES}",
            bytes.len()
        )));
    }
    let decoded = decode_xml(&bytes, path)?;
    if decoded.len() > MAX_XML_BYTES {
        return Err(OfdError::ResourceLimit(format!(
            "decoded XML entry {path:?} is {} bytes; limit is {MAX_XML_BYTES}",
            decoded.len()
        )));
    }
    Ok(decoded)
}

fn decode_xml(bytes: &[u8], path: &str) -> Result<String> {
    if let Some(body) = bytes.strip_prefix(b"\xEF\xBB\xBF") {
        return String::from_utf8(body.to_vec())
            .map_err(|error| OfdError::Xml(format!("invalid UTF-8 XML in {path}: {error}")));
    }
    if let Some(body) = bytes.strip_prefix(b"\xFE\xFF") {
        return decode_xml_with(encoding_rs::UTF_16BE, body, "UTF-16BE", path);
    }
    if let Some(body) = bytes.strip_prefix(b"\xFF\xFE") {
        return decode_xml_with(encoding_rs::UTF_16LE, body, "UTF-16LE", path);
    }

    // XML's byte-order sniffing for UTF-16 without a BOM (§4.3.3).
    if bytes.starts_with(&[0x00, b'<', 0x00, b'?']) {
        return decode_xml_with(encoding_rs::UTF_16BE, bytes, "UTF-16BE", path);
    }
    if bytes.starts_with(&[b'<', 0x00, b'?', 0x00]) {
        return decode_xml_with(encoding_rs::UTF_16LE, bytes, "UTF-16LE", path);
    }

    match xml_declared_encoding(bytes).as_deref() {
        None | Some("utf-8") | Some("utf8") => String::from_utf8(bytes.to_vec())
            .map_err(|error| OfdError::Xml(format!("invalid UTF-8 XML in {path}: {error}"))),
        Some("gb18030") => decode_xml_with(encoding_rs::GB18030, bytes, "GB18030", path),
        Some(encoding) => Err(OfdError::Xml(format!(
            "unsupported XML encoding {encoding:?} in {path}"
        ))),
    }
}

fn decode_xml_with(
    encoding: &'static encoding_rs::Encoding,
    bytes: &[u8],
    label: &str,
    path: &str,
) -> Result<String> {
    encoding
        .decode_without_bom_handling_and_without_replacement(bytes)
        .map(|decoded| decoded.into_owned())
        .ok_or_else(|| OfdError::Xml(format!("invalid {label} XML in {path}")))
}

/// Read the ASCII XML declaration before decoding the document body. XML's
/// declaration grammar is deliberately small, so a bounded attribute lexer is
/// sufficient and avoids guessing from arbitrary body bytes.
fn xml_declared_encoding(bytes: &[u8]) -> Option<String> {
    let limit = bytes.len().min(1024);
    let prefix = bytes.get(..limit)?;
    let declaration_start = prefix.strip_prefix(b"<?xml")?;
    let declaration_end = declaration_start
        .windows(2)
        .position(|window| window == b"?>")?;
    let declaration = &declaration_start[..declaration_end];
    let mut offset = 0usize;

    while offset < declaration.len() {
        while declaration.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        let name_start = offset;
        while declaration.get(offset).is_some_and(|byte| {
            byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b':' | b'-' | b'.')
        }) {
            offset += 1;
        }
        if offset == name_start {
            return None;
        }
        let name = &declaration[name_start..offset];
        while declaration.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        if declaration.get(offset) != Some(&b'=') {
            return None;
        }
        offset += 1;
        while declaration.get(offset).is_some_and(u8::is_ascii_whitespace) {
            offset += 1;
        }
        let quote = *declaration.get(offset)?;
        if quote != b'\'' && quote != b'"' {
            return None;
        }
        offset += 1;
        let value_start = offset;
        while declaration.get(offset).is_some_and(|byte| *byte != quote) {
            offset += 1;
        }
        let value = declaration.get(value_start..offset)?;
        offset += 1;
        if name.eq_ignore_ascii_case(b"encoding") {
            return Some(String::from_utf8_lossy(value).trim().to_ascii_lowercase());
        }
    }
    None
}

fn read_bytes(c: &mut Container, path: &str) -> Result<Vec<u8>> {
    c.read_normalized(path)
}

fn validate_ofd_root(root: Node, expected: &str, path: &str) -> Result<()> {
    let actual = root.tag_name().name();
    if actual != expected {
        return Err(OfdError::Malformed(format!(
            "XML entry {path:?} has root {actual:?}; expected {expected:?}"
        )));
    }
    if root
        .tag_name()
        .namespace()
        .is_some_and(|namespace| namespace != OFD_NAMESPACE && namespace != LEGACY_OFD_NAMESPACE)
    {
        return Err(OfdError::Malformed(format!(
            "XML entry {path:?} root {expected} is not in a supported OFD namespace; expected {OFD_NAMESPACE:?}"
        )));
    }
    Ok(())
}

fn warn_nonstandard_namespaces(root: Node, path: &str, warnings: &mut Vec<String>) {
    let mut unqualified_count = 0usize;
    let mut first_unqualified = None;
    let mut legacy_count = 0usize;
    let mut first_legacy = None;
    for node in root.descendants().filter(|node| node.is_element()) {
        match node.tag_name().namespace() {
            None => {
                unqualified_count += 1;
                first_unqualified.get_or_insert_with(|| node.tag_name().name().to_string());
            }
            Some(LEGACY_OFD_NAMESPACE) => {
                legacy_count += 1;
                first_legacy.get_or_insert_with(|| node.tag_name().name().to_string());
            }
            _ => {}
        }
    }
    if let Some(first) = first_unqualified {
        warnings.push(format!(
            "XML entry {path:?} has {unqualified_count} unqualified element(s); first is <{first}>"
        ));
    }
    if let Some(first) = first_legacy {
        warnings.push(format!(
            "XML entry {path:?} has {legacy_count} element(s) in legacy OFD namespace {LEGACY_OFD_NAMESPACE:?}; first is <{first}>"
        ));
    }
}

fn register_st_ids(root: Node, path: &str, registry: &mut IdRegistry, warnings: &mut Vec<String>) {
    for node in root.descendants().filter(|node| node.is_element()) {
        if !requires_st_id(node) {
            continue;
        }
        let element = local(&node);
        let location = format!("{path} <{element}>");
        match node
            .attribute("ID")
            .and_then(|value| value.parse::<u64>().ok())
        {
            Some(0) | None => warnings.push(format!(
                "{location} is missing a valid nonzero required ST_ID"
            )),
            Some(id) => registry.register(id, location, warnings),
        }
    }
}

fn requires_st_id(node: Node) -> bool {
    match local(&node) {
        "Page" => node
            .parent()
            .is_some_and(|parent| local(&parent) == "Pages"),
        "TemplatePage" => node
            .parent()
            .is_some_and(|parent| local(&parent) == "CommonData"),
        "Layer"
        | "TextObject"
        | "PathObject"
        | "ImageObject"
        | "CompositeObject"
        | "PageBlock"
        | "ColorSpace"
        | "DrawParam"
        | "Font"
        | "MultiMedia"
        | "CompositeGraphicUnit" => true,
        "Annot" => local(&node.document().root_element()) == "PageAnnot",
        _ => false,
    }
}

fn local<'a>(n: &Node<'a, 'a>) -> &'a str {
    if n.is_element() {
        if let Some(namespace) = n.tag_name().namespace() {
            if namespace != OFD_NAMESPACE && namespace != LEGACY_OFD_NAMESPACE {
                return "";
            }
        }
    }
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
    let alpha = parse_color_alpha(node);
    for child_node in node.children().filter(|n| n.is_element()) {
        match local(&child_node) {
            "Pattern" => return parse_pattern(child_node, alpha).map(OfdColor::Pattern),
            "AxialShd" => return parse_axial(child_node, alpha).map(OfdColor::Axial),
            "RadialShd" => return parse_radial(child_node, alpha).map(OfdColor::Radial),
            "GouraudShd" => return Some(OfdColor::Gouraud(parse_gouraud(child_node, alpha))),
            "LaGouraudShd" | "LaGourandShd" => {
                return Some(OfdColor::LatticeGouraud(parse_lattice_gouraud(
                    child_node, alpha,
                )))
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
        alpha: parse_color_alpha(node),
    }
}

fn parse_color_alpha(node: Node) -> u8 {
    node.attribute("Alpha")
        .and_then(parse_f32)
        .map(|a| a.clamp(0.0, 255.0) as u8)
        .unwrap_or(255)
}

fn parse_pattern(node: Node, alpha: u8) -> Option<PatternColor> {
    let width = node.attribute("Width").and_then(parse_f32)?;
    let height = node.attribute("Height").and_then(parse_f32)?;
    let x_step = node
        .attribute("XStep")
        .and_then(parse_f32)
        .filter(|v| *v >= width)
        .unwrap_or(width);
    let y_step = node
        .attribute("YStep")
        .and_then(parse_f32)
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
        alpha,
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

fn parse_axial(node: Node, alpha: u8) -> Option<AxialGradient> {
    Some(AxialGradient {
        alpha,
        map_type: parse_map_type(node.attribute("MapType")),
        map_unit: node.attribute("MapUnit").and_then(parse_f32),
        extend: attr_u16(node, "Extend", 0).min(3) as u8,
        start: node.attribute("StartPoint").and_then(parse_point)?,
        end: node.attribute("EndPoint").and_then(parse_point)?,
        segments: parse_segments(node),
    })
}

fn parse_radial(node: Node, alpha: u8) -> Option<RadialGradient> {
    Some(RadialGradient {
        alpha,
        map_type: parse_map_type(node.attribute("MapType")),
        map_unit: node.attribute("MapUnit").and_then(parse_f32),
        eccentricity: node
            .attribute("Eccentricity")
            .and_then(parse_f32)
            .unwrap_or(0.0),
        angle: node.attribute("Angle").and_then(parse_f32).unwrap_or(0.0),
        start: node.attribute("StartPoint").and_then(parse_point)?,
        end: node.attribute("EndPoint").and_then(parse_point)?,
        start_radius: node
            .attribute("StartRadius")
            .and_then(parse_f32)
            .unwrap_or(0.0),
        end_radius: node.attribute("EndRadius").and_then(parse_f32)?,
        extend: attr_u16(node, "Extend", 0).min(3) as u8,
        segments: parse_segments(node),
    })
}

fn parse_gouraud(node: Node, alpha: u8) -> GouraudGradient {
    GouraudGradient {
        alpha,
        extend: attr_xs_boolean(node, "Extend", false),
        points: parse_gouraud_points(node),
        back_color: child(node, "BackColor").map(parse_basic_color),
    }
}

fn parse_lattice_gouraud(node: Node, alpha: u8) -> LatticeGouraudGradient {
    LatticeGouraudGradient {
        alpha,
        vertices_per_row: node
            .attribute("VerticesPerRow")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
        extend: attr_xs_boolean(node, "Extend", false),
        points: parse_gouraud_points(node),
        back_color: child(node, "BackColor").map(parse_basic_color),
    }
}

fn parse_gouraud_points(node: Node) -> Vec<GouraudPoint> {
    node.children()
        .filter(|n| local(n) == "Point")
        .map(|p| GouraudPoint {
            x: p.attribute("X").and_then(parse_f32).unwrap_or(0.0),
            y: p.attribute("Y").and_then(parse_f32).unwrap_or(0.0),
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
            position: s.attribute("Position").and_then(parse_f32),
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
    let mut components = Vec::new();
    for token in s
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
    {
        let Some(component) = parse_color_component(token) else {
            // Deleting one malformed channel would shift every later channel
            // into the wrong color-space position.
            return Vec::new();
        };
        components.push(component);
    }
    components
}

fn parse_color_component(s: &str) -> Option<f32> {
    if let Some(hex) = s.strip_prefix('#') {
        u32::from_str_radix(hex, 16).ok().map(|v| v as f32)
    } else {
        parse_f32(s)
    }
}

/// Strip XML pretty-print whitespace from a `TextCode`'s content.
///
/// GB/T 33190 §11.3 (表46, `TextCode`) requires that a **significant space in the
/// text content be escaped** (`\` + four hex digits, e.g. `\0020`) — alongside any
/// out-of-range code. So a literal, unescaped whitespace run is *not* glyph
/// content; here it is XML formatting (some producers, e.g. Suwell, place the text
/// on its own line, so the content arrives as `"\r\nHeaffixed…"`). Left in, a
/// leading newline would consume a `DeltaX` advance and shift every
/// `CGTransform/@CodePosition` by one.
///
/// This runs *before* [`decode_text_code`], so an escaped space (`\0020`) is still
/// the literal characters `\0020` at this point and is preserved. Unescaped XML
/// whitespace is formatting, including indentation inserted inside a wrapped
/// text node, and must not consume a glyph/delta slot.
fn clean_text_code(raw: &str) -> String {
    // XML's formatting whitespace is exactly #x20, #x9, #xD, and #xA.
    // Do not use `char::is_whitespace`: U+3000 and similar characters are
    // printable glyphs and are not XML formatting whitespace.
    raw.chars()
        .filter(|ch| !matches!(ch, ' ' | '\t' | '\r' | '\n'))
        .collect()
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

/// Parse the XML Schema `boolean` lexical space used by OFD (`true`, `false`,
/// `1`, and `0`). Invalid values are left to the caller's default/error policy.
fn parse_xs_boolean(value: &str) -> Option<bool> {
    match value.trim() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn attr_xs_boolean(node: Node, name: &str, default: bool) -> bool {
    node.attribute(name)
        .and_then(parse_xs_boolean)
        .unwrap_or(default)
}

/// Validate text cardinalities without expanding any run-length encoded arrays.
/// This runs on every XML file that can contain graphic objects, before model
/// construction, so oversized counts are rejected instead of silently truncated.
#[cfg(test)]
fn validate_graphic_limits(root: Node) -> Result<()> {
    let mut budget = ParseBudget::default();
    let validation = validate_graphic_limits_with_budget(root, &mut budget)?;
    match validation.malformed {
        Some(message) => Err(OfdError::Malformed(message)),
        None => Ok(()),
    }
}

fn validate_graphic_limits_with_budget(
    root: Node,
    budget: &mut ParseBudget,
) -> Result<GraphicValidation> {
    let xml_nodes = validate_xml_structure_with_budget(root, budget)?;
    let mut stats = GraphicStats {
        graphic_objects: 0,
        model_items: xml_nodes,
    };
    let mut expanded_model_items = 0u64;
    let mut malformed = None;

    for node in root.descendants() {
        let is_graphic = match local(&node) {
            "TextObject" | "PathObject" | "ImageObject" | "CompositeObject" | "PageBlock" => true,
            "Text" | "Path" => node.parent().is_some_and(|parent| local(&parent) == "Area"),
            _ => false,
        };
        if is_graphic {
            stats.graphic_objects = stats
                .graphic_objects
                .checked_add(1)
                .ok_or_else(|| OfdError::ResourceLimit("graphic-object count overflow".into()))?;
        }
    }

    for abbreviated in root
        .descendants()
        .filter(|node| local(node) == "AbbreviatedData")
    {
        if let Some(data) = abbreviated.text() {
            let count = validate_path_command_count(data, MAX_PATH_COMMANDS)? as u64;
            expanded_model_items = expanded_model_items.checked_add(count).ok_or_else(|| {
                OfdError::ResourceLimit("expanded path model count overflow".into())
            })?;
        }
    }

    for text in root.descendants().filter(|n| {
        matches!(local(n), "Text" | "TextObject") && n.children().any(|c| local(&c) == "TextCode")
    }) {
        let mut source_codes = 0usize;
        for code in text.children().filter(|n| local(n) == "TextCode") {
            let decoded_codes = decode_text_code(&clean_text_code(code.text().unwrap_or("")))
                .chars()
                .count();
            source_codes = source_codes
                .checked_add(decoded_codes)
                .ok_or_else(|| OfdError::ResourceLimit("text code count overflow".into()))?;
            if source_codes > MAX_TEXT_SLOTS {
                return Err(OfdError::ResourceLimit(format!(
                    "text object exceeds {MAX_TEXT_SLOTS} source-code slots"
                )));
            }
            for attr in ["DeltaX", "DeltaY"] {
                if let Some(value) = code.attribute(attr) {
                    let (count, delta_error) = validate_delta_count_best_effort(value)?;
                    if let Some(message) = delta_error {
                        record_malformed(&mut malformed, message);
                    }
                    let count = count as u64;
                    expanded_model_items =
                        expanded_model_items.checked_add(count).ok_or_else(|| {
                            OfdError::ResourceLimit("expanded delta model count overflow".into())
                        })?;
                }
            }
        }

        let mut explicit_glyphs = 0usize;
        let mut listed_glyphs = 0usize;
        let mut spans = Vec::new();
        for cg in text.children().filter(|n| local(n) == "CGTransform") {
            let mut glyph_list_len = 0usize;
            if let Some(glyphs) = child(cg, "Glyphs").and_then(|g| g.text()) {
                for token in glyphs
                    .split(|c: char| c == ',' || c.is_ascii_whitespace())
                    .filter(|value| !value.is_empty())
                {
                    if token.parse::<u16>().is_err() {
                        record_malformed(
                            &mut malformed,
                            format!("invalid CGTransform glyph id {token:?}"),
                        );
                    }
                    glyph_list_len = glyph_list_len.checked_add(1).ok_or_else(|| {
                        OfdError::ResourceLimit("CGTransform glyph-list count overflow".into())
                    })?;
                }
            }
            let glyph_count = match cg.attribute("GlyphCount") {
                Some(raw) => match raw.parse::<usize>() {
                    Ok(value) => value,
                    Err(_) => {
                        record_malformed(
                            &mut malformed,
                            format!("invalid CGTransform GlyphCount {raw:?}"),
                        );
                        glyph_list_len.max(1)
                    }
                },
                None => glyph_list_len.max(1),
            };
            if glyph_count == 0 {
                record_malformed(
                    &mut malformed,
                    "CGTransform GlyphCount must be at least 1".into(),
                );
            }
            if glyph_count > MAX_TEXT_SLOTS || glyph_list_len > MAX_TEXT_SLOTS {
                return Err(OfdError::ResourceLimit(format!(
                    "CGTransform exceeds {MAX_TEXT_SLOTS} glyph slots"
                )));
            }
            let code_count = match cg.attribute("CodeCount") {
                Some(raw) => match raw.parse::<usize>() {
                    Ok(value) => value,
                    Err(_) => {
                        record_malformed(
                            &mut malformed,
                            format!("invalid CGTransform CodeCount {raw:?}"),
                        );
                        1
                    }
                },
                None => 1,
            };
            if code_count == 0 {
                record_malformed(
                    &mut malformed,
                    "CGTransform CodeCount must be at least 1".into(),
                );
            }
            if code_count > MAX_TEXT_SLOTS {
                return Err(OfdError::ResourceLimit(format!(
                    "CGTransform CodeCount exceeds {MAX_TEXT_SLOTS}"
                )));
            }
            let code_position = match cg.attribute("CodePosition") {
                Some(raw) => match raw.parse::<usize>() {
                    Ok(value) => value,
                    Err(_) => {
                        record_malformed(
                            &mut malformed,
                            format!("invalid CGTransform CodePosition {raw:?}"),
                        );
                        0
                    }
                },
                None => 0,
            };
            let effective_code_count = code_count.max(1);
            let span_end = code_position
                .checked_add(effective_code_count)
                .ok_or_else(|| OfdError::ResourceLimit("CGTransform code span overflow".into()))?;
            if span_end > source_codes {
                record_malformed(
                    &mut malformed,
                    format!(
                        "CGTransform code span {code_position}..{span_end} exceeds {source_codes} source codes"
                    ),
                );
            } else {
                spans.push((code_position, span_end));
            }
            explicit_glyphs = explicit_glyphs.checked_add(glyph_count).ok_or_else(|| {
                OfdError::ResourceLimit("CGTransform glyph count overflow".into())
            })?;
            if explicit_glyphs > MAX_TEXT_SLOTS {
                return Err(OfdError::ResourceLimit(format!(
                    "text object exceeds {MAX_TEXT_SLOTS} explicit glyph slots"
                )));
            }
            listed_glyphs = listed_glyphs.checked_add(glyph_list_len).ok_or_else(|| {
                OfdError::ResourceLimit("CGTransform glyph-list count overflow".into())
            })?;
            if listed_glyphs > MAX_TEXT_SLOTS {
                return Err(OfdError::ResourceLimit(format!(
                    "text object exceeds {MAX_TEXT_SLOTS} listed glyph ids"
                )));
            }
        }

        spans.sort_unstable_by_key(|span| span.0);
        let mut covered_codes = 0usize;
        let mut covered_end = 0usize;
        for (start, end) in spans {
            if start < covered_end {
                record_malformed(&mut malformed, "CGTransform code spans overlap".into());
                if end > covered_end {
                    covered_codes =
                        covered_codes
                            .checked_add(end - covered_end)
                            .ok_or_else(|| {
                                OfdError::ResourceLimit("CGTransform covered-code overflow".into())
                            })?;
                    covered_end = end;
                }
            } else {
                covered_codes = covered_codes.checked_add(end - start).ok_or_else(|| {
                    OfdError::ResourceLimit("CGTransform covered-code overflow".into())
                })?;
                covered_end = end;
            }
        }
        let displayed_slots = source_codes
            .checked_sub(covered_codes)
            .and_then(|count| count.checked_add(explicit_glyphs))
            .ok_or_else(|| OfdError::ResourceLimit("text glyph-slot count overflow".into()))?;
        if displayed_slots > MAX_TEXT_SLOTS {
            return Err(OfdError::ResourceLimit(format!(
                "text object expands to {displayed_slots} glyph slots; limit is {MAX_TEXT_SLOTS}"
            )));
        }
        expanded_model_items = expanded_model_items
            .checked_add(source_codes as u64)
            .and_then(|items| items.checked_add(listed_glyphs as u64))
            .ok_or_else(|| OfdError::ResourceLimit("expanded text model count overflow".into()))?;
    }
    stats.model_items = stats
        .model_items
        .checked_add(expanded_model_items)
        .ok_or_else(|| OfdError::ResourceLimit("graphic model count overflow".into()))?;
    budget.charge_graphic_stats(GraphicStats {
        graphic_objects: stats.graphic_objects,
        model_items: expanded_model_items,
    })?;
    Ok(GraphicValidation { malformed })
}

fn record_malformed(target: &mut Option<String>, message: String) {
    if target.is_none() {
        *target = Some(message);
    }
}

/// Bound path model/raster work before constructing `PathCommand` values.
/// Elliptical arcs can expand to at most four cubic curves, so counting them as
/// four is a conservative allocation upper bound even for malformed operands.
fn validate_path_command_count(data: &str, limit: usize) -> Result<usize> {
    let mut count = 0usize;
    for token in data
        .split([' ', ',', '\n', '\r', '\t'])
        .filter(|token| !token.is_empty())
    {
        if token
            .parse::<f32>()
            .ok()
            .is_some_and(|value| value.is_finite())
        {
            continue;
        }
        let add = match token.chars().next() {
            Some('A') => 4,
            Some('S' | 'M' | 'L' | 'Q' | 'B' | 'C') => 1,
            _ => 0,
        };
        count = count
            .checked_add(add)
            .ok_or_else(|| OfdError::ResourceLimit("path command count overflow".into()))?;
        if count > limit {
            return Err(OfdError::ResourceLimit(format!(
                "path expands beyond {limit} drawing commands"
            )));
        }
    }
    Ok(count)
}

fn validate_xml_structure_with_budget(root: Node, budget: &mut ParseBudget) -> Result<u64> {
    let mut stack = vec![(root, 1usize)];
    let mut node_count = 0u64;
    while let Some((node, depth)) = stack.pop() {
        node_count = node_count
            .checked_add(1)
            .ok_or_else(|| OfdError::ResourceLimit("XML node count overflow".into()))?;
        budget.charge_xml_nodes(1)?;
        if node_count > MAX_XML_NODES as u64 {
            return Err(OfdError::ResourceLimit(format!(
                "XML tree exceeds {MAX_XML_NODES} nodes"
            )));
        }
        if depth > MAX_XML_DEPTH {
            return Err(OfdError::ResourceLimit(format!(
                "XML tree exceeds nesting depth {MAX_XML_DEPTH}"
            )));
        }
        stack.extend(node.children().map(|child| (child, depth + 1)));
    }
    // Every valid XML node is conservatively charged as one potential owned
    // model entry. Graphic validators add expanded arrays separately.
    budget.charge_model_items(node_count)?;
    Ok(node_count)
}

#[cfg(test)]
fn validate_delta_count(value: &str) -> Result<usize> {
    let (count, malformed) = validate_delta_count_best_effort(value)?;
    match malformed {
        Some(message) => Err(OfdError::Malformed(message)),
        None => Ok(count),
    }
}

fn validate_delta_count_best_effort(value: &str) -> Result<(usize, Option<String>)> {
    let mut count = 0usize;
    let mut malformed = None;
    let mut tokens = value
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|v| !v.is_empty());
    while let Some(token) = tokens.next() {
        let add = if token.eq_ignore_ascii_case("g") {
            let repeats = match tokens.next() {
                Some(raw_count) => match raw_count.parse::<usize>() {
                    Ok(value) => value,
                    Err(_) => {
                        record_malformed(
                            &mut malformed,
                            format!("invalid delta repeat count {raw_count:?}"),
                        );
                        0
                    }
                },
                None => {
                    record_malformed(
                        &mut malformed,
                        "delta run is missing its repeat count".into(),
                    );
                    0
                }
            };
            match tokens.next() {
                Some(raw_value) if parse_f32(raw_value).is_some() => {}
                Some(raw_value) => record_malformed(
                    &mut malformed,
                    format!("invalid or non-finite delta value {raw_value:?}"),
                ),
                None => record_malformed(
                    &mut malformed,
                    "delta run is missing its repeated value".into(),
                ),
            }
            repeats
        } else {
            if parse_f32(token).is_some() {
                1
            } else {
                record_malformed(
                    &mut malformed,
                    format!("invalid or non-finite delta value {token:?}"),
                );
                0
            }
        };
        count = count
            .checked_add(add)
            .ok_or_else(|| OfdError::ResourceLimit("expanded delta count overflow".into()))?;
        if count > MAX_TEXT_SLOTS {
            return Err(OfdError::ResourceLimit(format!(
                "expanded delta list exceeds {MAX_TEXT_SLOTS} entries"
            )));
        }
    }
    Ok((count, malformed))
}

/// Parse an OFD delta list (`DeltaX`/`DeltaY`), expanding the `g` run-length
/// operator: `g N V` yields `N` copies of `V`. Example: `"g 8 3.175 1.6 g 4 3"`
/// expands to eight `3.175`s, then `1.6`, then four `3`s.
fn parse_deltas(s: &str) -> Vec<f32> {
    let mut out = Vec::new();
    let mut toks = s
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty());
    while let Some(tok) = toks.next() {
        if tok == "g" || tok == "G" {
            let count = toks
                .next()
                .and_then(|t| t.parse::<usize>().ok())
                .unwrap_or(0);
            let value = toks
                .next()
                .and_then(|t| t.parse::<f32>().ok())
                .filter(|v| v.is_finite())
                .unwrap_or(0.0);
            let remaining = MAX_TEXT_SLOTS.saturating_sub(out.len());
            out.extend(std::iter::repeat_n(value, count.min(remaining)));
        } else if let Some(v) = tok.parse::<f32>().ok().filter(|v| v.is_finite()) {
            if out.len() == MAX_TEXT_SLOTS {
                break;
            }
            out.push(v);
        }
    }
    out
}

fn parse_f32(value: &str) -> Option<f32> {
    value.parse::<f32>().ok().filter(|value| value.is_finite())
}

fn parse_floats(s: &str) -> Vec<f32> {
    let mut values = Vec::new();
    for token in s
        .split(|c: char| c == ',' || c.is_ascii_whitespace())
        .filter(|t| !t.is_empty())
    {
        let Some(value) = parse_f32(token) else {
            // Do not silently delete a malformed component and shift every
            // subsequent coordinate/channel into the wrong position.
            return Vec::new();
        };
        values.push(value);
    }
    values
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
    fn abbreviated_data_subpath_start() {
        // 'S' starts a subpath like 'M'.
        let cmds = parse_abbreviated_data("S 1 2 L 3 4");
        assert_eq!(cmds[0], PathCommand::MoveTo { x: 1.0, y: 2.0 });
        assert_eq!(cmds[1], PathCommand::LineTo { x: 3.0, y: 4.0 });
    }

    #[test]
    fn abbreviated_data_arc_to_cubics() {
        // A 90° arc of a unit circle from (1,0) to (0,1) → cubic Béziers that
        // end at the arc endpoint.
        let cmds = parse_abbreviated_data("M 1 0 A 1 1 0 0 1 0 1");
        assert_eq!(cmds[0], PathCommand::MoveTo { x: 1.0, y: 0.0 });
        assert!(cmds.len() >= 2, "arc should emit at least one cubic");
        // The last command's endpoint must be the arc endpoint (0,1).
        match cmds.last().unwrap() {
            PathCommand::CubicTo { x, y, .. } => {
                assert!(
                    (x - 0.0).abs() < 1e-3 && (y - 1.0).abs() < 1e-3,
                    "endpoint {x},{y}"
                );
            }
            other => panic!("expected cubic, got {other:?}"),
        }
    }

    #[test]
    fn extreme_finite_arc_operands_do_not_create_non_finite_model_geometry() {
        let commands = parse_abbreviated_data("M 3e38 0 A 3e38 3e38 0 0 1 -3e38 0");
        for command in commands {
            let finite = match command {
                PathCommand::MoveTo { x, y } | PathCommand::LineTo { x, y } => {
                    x.is_finite() && y.is_finite()
                }
                PathCommand::CubicTo {
                    x1,
                    y1,
                    x2,
                    y2,
                    x,
                    y,
                } => [x1, y1, x2, y2, x, y].into_iter().all(f32::is_finite),
                PathCommand::QuadTo { x1, y1, x, y } => {
                    [x1, y1, x, y].into_iter().all(f32::is_finite)
                }
                PathCommand::Close => true,
            };
            assert!(finite, "{command:?}");
        }
    }

    /// Parse the single root element of an XML fragment for helper tests.
    fn frag(xml: &str) -> XmlDoc<'_> {
        XmlDoc::parse(xml).unwrap()
    }

    #[test]
    fn action_goto_dest_with_region() {
        let doc = frag(
            r#"<PathObject xmlns="http://www.ofdspec.org/2016" ID="1">
                 <Actions>
                   <Action Event="CLICK">
                     <Region><Area Start="0 0"><Line Point1="10 0"/><Line Point1="10 5"/><Close/></Area></Region>
                     <Goto><Dest Type="XYZ" PageID="42" Left="80.43" Top="26.07" Zoom="1"/></Goto>
                   </Action>
                 </Actions>
               </PathObject>"#,
        );
        let actions = parse_actions(doc.root_element());
        assert_eq!(actions.len(), 1);
        let a = &actions[0];
        assert_eq!(a.event, ActionEvent::Click);
        let region = a.region.as_ref().expect("region");
        assert_eq!(region.areas.len(), 1);
        assert_eq!(region.areas[0].segments.len(), 3); // Line, Line, Close
        match &a.kind {
            ActionKind::Goto(GotoTarget::Dest(d)) => {
                assert_eq!(d.kind, DestKind::Xyz);
                assert_eq!(d.page_id, 42);
                assert_eq!(d.left, Some(80.43));
                assert_eq!(d.zoom, Some(1.0));
            }
            other => panic!("expected goto dest, got {other:?}"),
        }
    }

    #[test]
    fn action_uri_sound_movie_gotoa() {
        let doc = frag(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016" ID="1">
                 <Actions>
                   <Action Event="CLICK"><URI URI="https://example.org" Base="https://b/"/></Action>
                   <Action Event="DO"><GotoA AttachID="att1" NewWindow="false"/></Action>
                   <Action Event="PO"><Sound ResourceID="7" Volume="80" Repeat="true"/></Action>
                   <Action Event="CLICK"><Movie ResourceID="9" Operator="Pause"/></Action>
                 </Actions>
               </TextObject>"#,
        );
        let actions = parse_actions(doc.root_element());
        assert_eq!(actions.len(), 4);
        match &actions[0].kind {
            ActionKind::Uri(u) => {
                assert_eq!(u.uri, "https://example.org");
                assert_eq!(u.base.as_deref(), Some("https://b/"));
            }
            other => panic!("expected uri, got {other:?}"),
        }
        assert_eq!(actions[1].event, ActionEvent::DocumentOpen);
        match &actions[1].kind {
            ActionKind::GotoAttachment(g) => {
                assert_eq!(g.attach_id, "att1");
                assert!(!g.new_window);
            }
            other => panic!("expected gotoA, got {other:?}"),
        }
        match &actions[2].kind {
            ActionKind::Sound(s) => {
                assert_eq!(s.resource_id, 7);
                assert_eq!(s.volume, Some(80));
                assert_eq!(s.repeat, Some(true));
            }
            other => panic!("expected sound, got {other:?}"),
        }
        match &actions[3].kind {
            ActionKind::Movie(m) => {
                assert_eq!(m.resource_id, 9);
                assert_eq!(m.operator, MovieOperator::Pause);
            }
            other => panic!("expected movie, got {other:?}"),
        }
    }

    #[test]
    fn region_arc_and_curves() {
        let doc = frag(
            r#"<Region xmlns="http://www.ofdspec.org/2016">
                 <Area Start="0 0">
                   <QuadraticBezier Point1="1 1" Point2="2 0"/>
                   <CubicBezier Point1="3 1" Point2="4 1" Point3="5 0"/>
                   <Arc EllipseSize="2 1" RotationAngle="30" LargeArc="true" SweepDirection="1" EndPoint="7 0"/>
                 </Area>
               </Region>"#,
        );
        let region = parse_region(doc.root_element());
        let segs = &region.areas[0].segments;
        assert!(matches!(segs[0], RegionSegment::QuadraticBezier { .. }));
        match &segs[1] {
            RegionSegment::CubicBezier { p1, p2, p3 } => {
                assert!(p1.is_some() && p2.is_some());
                assert_eq!(p3.x, 5.0);
            }
            other => panic!("expected cubic, got {other:?}"),
        }
        match &segs[2] {
            RegionSegment::Arc {
                ellipse_size,
                large_arc,
                sweep_clockwise,
                end,
                ..
            } => {
                assert_eq!(*ellipse_size, (2.0, 1.0));
                assert!(*large_arc && *sweep_clockwise);
                assert_eq!(end.x, 7.0);
            }
            other => panic!("expected arc, got {other:?}"),
        }
    }

    #[test]
    fn clip_text_shape() {
        let doc = frag(
            r#"<PathObject xmlns="http://www.ofdspec.org/2016" ID="1">
                 <Clips><Clip><Area CTM="1 0 0 1 0 0">
                   <Text Font="3" Size="4"><TextCode X="0" Y="3">秘</TextCode></Text>
                 </Area></Clip></Clips>
               </PathObject>"#,
        );
        let clips = parse_clips(doc.root_element());
        assert_eq!(clips.len(), 1);
        assert_eq!(clips[0].areas.len(), 1);
        match &clips[0].areas[0].shape {
            ClipShape::Text(t) => {
                assert_eq!(t.font_id, 3);
                assert_eq!(t.runs[0].text, "秘");
            }
            other => panic!("expected text clip, got {other:?}"),
        }
    }

    #[test]
    fn clip_hierarchy_preserves_area_unions_and_clip_intersections() {
        let doc = frag(
            r#"<PathObject xmlns="http://www.ofdspec.org/2016" ID="1">
                 <Clips>
                   <Clip>
                     <Area><Path Fill="true"><AbbreviatedData>M 0 0 L 1 0 L 1 1 C</AbbreviatedData></Path></Area>
                     <Area><Path Fill="true"><AbbreviatedData>M 2 0 L 3 0 L 3 1 C</AbbreviatedData></Path></Area>
                   </Clip>
                   <Clip>
                     <Area><Path Fill="true"><AbbreviatedData>M 0 0 L 3 0 L 3 1 C</AbbreviatedData></Path></Area>
                   </Clip>
                 </Clips>
               </PathObject>"#,
        );
        let clips = parse_clips(doc.root_element());
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0].areas.len(), 2);
        assert_eq!(clips[1].areas.len(), 1);
        assert!(matches!(clips[0].areas[0].shape, ClipShape::Path(_)));
    }

    #[test]
    fn xml_schema_boolean_numeric_forms_are_honored() {
        let doc = frag(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016" ID="1"
                 Visible="0" Stroke="1" Fill="0" Italic="1" Font="2" Size="3"/>"#,
        );
        let text = parse_text(doc.root_element());
        assert!(!text.common.visible);
        assert!(text.stroke);
        assert!(!text.fill);
        assert!(text.italic);

        let path = frag(
            r#"<PathObject xmlns="http://www.ofdspec.org/2016" ID="2"
                 Visible="1" Stroke="0" Fill="1"/>"#,
        );
        let path = parse_path(path.root_element());
        assert!(path.common.visible);
        assert!(!path.stroke);
        assert!(path.fill);
    }

    #[test]
    fn text_code_strips_formatting_newline() {
        // A TextCode pretty-printed onto its own line: the CRLF must not become a
        // glyph (which would shift DeltaX and CGTransform code positions).
        let doc = frag(
            "<TextObject xmlns=\"http://www.ofdspec.org/2016\" Font=\"3\" Size=\"4\">\
               <TextCode X=\"0\" Y=\"3\" DeltaX=\"4 4\">\r\nabc</TextCode>\
             </TextObject>",
        );
        let t = parse_text(doc.root_element());
        assert_eq!(t.runs[0].text, "abc"); // not "\r\nabc"
                                           // Significant spaces must use the standard's hex escape. Literal
                                           // whitespace, including wrapped indentation, is formatting.
        let doc2 = frag(
            "<TextObject xmlns=\"http://www.ofdspec.org/2016\" Font=\"3\" Size=\"4\">\
               <TextCode X=\"0\" Y=\"3\">a b\t\n c\\0020d</TextCode>\
             </TextObject>",
        );
        assert_eq!(parse_text(doc2.root_element()).runs[0].text, "abc d");

        let doc3 = frag(
            "<TextObject xmlns=\"http://www.ofdspec.org/2016\" Font=\"3\" Size=\"4\">\
               <TextCode X=\"0\" Y=\"3\">甲　乙</TextCode>\
             </TextObject>",
        );
        assert_eq!(parse_text(doc3.root_element()).runs[0].text, "甲　乙");
    }

    #[test]
    fn rejects_invalid_cg_transform_spans() {
        for cg in [
            r#"<CGTransform CodePosition="0" CodeCount="0" GlyphCount="1"><Glyphs>1</Glyphs></CGTransform>"#,
            r#"<CGTransform CodePosition="0" CodeCount="1" GlyphCount="0"><Glyphs>1</Glyphs></CGTransform>"#,
            r#"<CGTransform CodePosition="2" CodeCount="2" GlyphCount="1"><Glyphs>1</Glyphs></CGTransform>"#,
        ] {
            let xml = format!(
                r#"<TextObject xmlns="http://www.ofdspec.org/2016" Font="1" Size="3">
                     <TextCode>abc</TextCode>{cg}
                   </TextObject>"#
            );
            let doc = frag(&xml);
            assert!(matches!(
                validate_graphic_limits(doc.root_element()),
                Err(OfdError::Malformed(_))
            ));
        }

        let overlap = frag(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016" Font="1" Size="3">
                 <TextCode>abcd</TextCode>
                 <CGTransform CodePosition="0" CodeCount="3" GlyphCount="1"><Glyphs>1</Glyphs></CGTransform>
                 <CGTransform CodePosition="2" CodeCount="2" GlyphCount="1"><Glyphs>2</Glyphs></CGTransform>
               </TextObject>"#,
        );
        assert!(matches!(
            validate_graphic_limits(overlap.root_element()),
            Err(OfdError::Malformed(_))
        ));
    }

    #[test]
    fn cg_transform_glyphs_accept_all_xml_formatting_whitespace() {
        let doc = frag(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016" Font="1" Size="3">
                 <TextCode>ab</TextCode>
                 <CGTransform CodePosition="0" CodeCount="2" GlyphCount="2">
                   <Glyphs>12&#xA;  34&#xD;&#x9;</Glyphs>
                 </CGTransform>
               </TextObject>"#,
        );
        validate_graphic_limits(doc.root_element()).unwrap();
        let text = parse_text(doc.root_element());
        assert_eq!(text.cg_transforms[0].glyphs, vec![12, 34]);
    }

    #[test]
    fn rejects_invalid_cg_transform_glyph_tokens_without_shifting_slots() {
        let doc = frag(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016" Font="1" Size="3">
                 <TextCode>abc</TextCode>
                 <CGTransform CodePosition="0" CodeCount="3" GlyphCount="3">
                   <Glyphs>12 bad 34</Glyphs>
                 </CGTransform>
               </TextObject>"#,
        );
        assert!(matches!(
            validate_graphic_limits(doc.root_element()),
            Err(OfdError::Malformed(message)) if message.contains("glyph id")
        ));
        assert_eq!(
            parse_text(doc.root_element()).cg_transforms[0].glyphs,
            vec![12, 0, 34]
        );
    }

    #[test]
    fn path_command_validation_rejects_expansion_before_model_allocation() {
        assert!(validate_path_command_count("M 0 0 A 1 1 0 0 1 2 2", 5).is_ok());
        assert!(matches!(
            validate_path_command_count("A A", 7),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn malformed_float_lists_do_not_shift_later_components() {
        assert!(parse_rect("0 0 NaN 10").is_none());
        assert!(parse_matrix("1 0 invalid 1 0 0").is_none());
    }

    #[test]
    fn non_finite_scalar_and_color_values_use_safe_defaults() {
        let doc = frag(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016"
                 Font="3" Size="NaN" HScale="inf" LineWidth="-inf" Alpha="NaN">
                 <TextCode X="NaN" Y="inf">a</TextCode>
               </TextObject>"#,
        );
        let text = parse_text(doc.root_element());
        assert_eq!(text.font_size, 0.0);
        assert_eq!(text.h_scale, 1.0);
        assert_eq!(text.common.line_width, None);
        assert_eq!(text.common.alpha, 255);
        assert_eq!((text.runs[0].origin_x, text.runs[0].origin_y), (0.0, 0.0));

        assert_eq!(parse_color_components("1 bad 2"), Vec::<f32>::new());
        assert_eq!(parse_color_components("1 NaN 2"), Vec::<f32>::new());
        assert_eq!(parse_color_components("#ff\n1\r\t2"), vec![255.0, 1.0, 2.0]);

        let pattern =
            frag(r#"<Pattern xmlns="http://www.ofdspec.org/2016" Width="NaN" Height="2"/>"#);
        assert!(parse_pattern(pattern.root_element(), 255).is_none());
    }

    #[test]
    fn image_substitution_satisfies_resource_validation_but_mask_does_not() {
        let image = GraphicObject::Image(ImageObject {
            common: GraphicCommon::default(),
            resource_id: 1,
            substitution: Some(2),
            image_mask: Some(3),
            border: None,
        });
        let pages = vec![Page {
            id: 1,
            area: None,
            layers: vec![Layer {
                id: 1,
                kind: LayerKind::Body,
                draw_param: None,
                objects: vec![image],
            }],
            actions: Vec::new(),
        }];
        let resources = Resources {
            images: vec![MultiMedia {
                id: 2,
                kind: MediaKind::Image,
                format: ImageFormat::Unknown,
                data: Vec::new(),
            }],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        validate_resource_references(&pages, &[], &resources, &HashMap::new(), &mut warnings);
        assert_eq!(warnings, vec!["unresolved image resource id 3"]);

        let mut warnings = Vec::new();
        validate_resource_references(
            &pages,
            &[],
            &Resources::default(),
            &HashMap::new(),
            &mut warnings,
        );
        assert!(warnings.iter().any(|warning| {
            warning.contains("resource id 1") && warning.contains("resource id 2")
        }));
    }

    #[test]
    fn image_border_parses_all_standard_attributes() {
        let xml = frag(
            r#"<ImageObject xmlns="http://www.ofdspec.org/2016" ResourceID="1">
                 <Border LineWidth="0.5" HorizonalCornerRadius="2" VerticalCornerRadius="3" DashOffset="1" DashPattern="4 2">
                   <BorderColor Value="255 0 0"/>
                 </Border>
               </ImageObject>"#,
        );
        let image = parse_image(xml.root_element());
        let border = image.border.unwrap();
        assert_eq!(border.line_width, 0.5);
        assert_eq!(border.horizontal_corner_radius, 2.0);
        assert_eq!(border.vertical_corner_radius, 3.0);
        assert_eq!(border.dash_offset, 1.0);
        assert_eq!(border.dash_pattern, Some(vec![4.0, 2.0]));
        assert!(border.color.is_some());
    }

    #[test]
    fn tiff_and_bare_ccitt_are_distinct_formats() {
        assert_eq!(guess_format(Some("TIFF"), "image.bin"), ImageFormat::Tiff);
        assert_eq!(guess_format(None, "image.tif"), ImageFormat::Tiff);
        assert_eq!(guess_format(Some("CCITT"), "image.bin"), ImageFormat::Ccitt);
    }

    #[test]
    fn bookmarks_parse() {
        let doc = frag(
            r#"<Document xmlns="http://www.ofdspec.org/2016">
                 <Bookmarks><Bookmark Name="新建书签">
                   <Dest Type="XYZ" PageID="1" Right="167.873" Bottom="131.0356"/>
                 </Bookmark></Bookmarks>
               </Document>"#,
        );
        let bms = parse_bookmarks(doc.root_element());
        assert_eq!(bms.len(), 1);
        assert_eq!(bms[0].name, "新建书签");
        assert_eq!(bms[0].dest.as_ref().unwrap().page_id, 1);
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
    fn package_root_rejects_foreign_namespaces() {
        use std::io::{Cursor, Write};

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("OFD.xml", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer
            .write_all(br#"<OFD xmlns="https://example.invalid/ofd"><DocBody/></OFD>"#)
            .unwrap();
        let error = parse(writer.finish().unwrap().into_inner()).unwrap_err();
        assert!(error.to_string().contains(OFD_NAMESPACE));
    }

    #[test]
    fn legacy_and_unqualified_namespaces_are_lax_with_file_level_diagnostics() {
        let legacy = frag(
            r#"<Page xmlns="http://www.ofdspec.org"><Content><Layer ID="1"/></Content></Page>"#,
        );
        validate_ofd_root(legacy.root_element(), "Page", "legacy.xml").unwrap();
        assert_eq!(
            child(legacy.root_element(), "Content").map(|node| local(&node)),
            Some("Content")
        );
        let mut warnings = Vec::new();
        warn_nonstandard_namespaces(legacy.root_element(), "legacy.xml", &mut warnings);
        assert_eq!(
            warnings,
            ["XML entry \"legacy.xml\" has 3 element(s) in legacy OFD namespace \"http://www.ofdspec.org\"; first is <Page>"]
        );

        let unqualified = frag(r#"<Page><Content><Layer ID="1"/></Content></Page>"#);
        validate_ofd_root(unqualified.root_element(), "Page", "plain.xml").unwrap();
        warnings.clear();
        warn_nonstandard_namespaces(unqualified.root_element(), "plain.xml", &mut warnings);
        assert_eq!(
            warnings,
            ["XML entry \"plain.xml\" has 3 unqualified element(s); first is <Page>"]
        );
    }

    #[test]
    fn case_insensitive_st_loc_fallback_is_observable() {
        use std::io::{Cursor, Write};

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            (
                "OFD.xml",
                br#"<OFD xmlns="http://www.ofdspec.org/2016"><DocBody><DocRoot>doc_0/document.XML</DocRoot></DocBody></OFD>"#.as_slice(),
            ),
            (
                "Doc_0/Document.xml",
                br#"<Document xmlns="http://www.ofdspec.org/2016"><CommonData><MaxUnitID>2</MaxUnitID><PageArea><PhysicalBox>0 0 10 10</PhysicalBox></PageArea></CommonData><Pages><Page ID="1" BaseLoc="Page.xml"/></Pages></Document>"#.as_slice(),
            ),
            (
                "doc_0/Page.xml",
                br#"<Page xmlns="http://www.ofdspec.org/2016"><Content><Layer ID="2"/></Content></Page>"#.as_slice(),
            ),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }

        let package = parse(writer.finish().unwrap().into_inner()).unwrap();
        assert_eq!(
            package.documents[0].warnings,
            ["non-standard ST_Loc \"doc_0/document.XML\" resolved as ZIP entry \"Doc_0/Document.xml\"; paths are case-sensitive and use '/' separators"]
        );
    }

    #[test]
    fn gb18030_xml_is_decoded_without_lossy_replacement() {
        use std::io::{Cursor, Write};

        let ofd = r#"<?xml version="1.0" encoding="GB18030"?><OFD xmlns="http://www.ofdspec.org/2016"><DocBody><DocInfo><Title>中文标题</Title></DocInfo><DocRoot>Doc_0/Document.xml</DocRoot></DocBody></OFD>"#;
        let (ofd_bytes, _, had_errors) = encoding_rs::GB18030.encode(ofd);
        assert!(!had_errors);

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            ("OFD.xml", ofd_bytes.as_ref()),
            (
                "Doc_0/Document.xml",
                br#"<Document xmlns="http://www.ofdspec.org/2016"><CommonData><MaxUnitID>2</MaxUnitID><PageArea><PhysicalBox>0 0 10 10</PhysicalBox></PageArea></CommonData><Pages><Page ID="1" BaseLoc="Page.xml"/></Pages></Document>"#.as_slice(),
            ),
            (
                "Doc_0/Page.xml",
                br#"<Page xmlns="http://www.ofdspec.org/2016"><Content><Layer ID="2"/></Content></Page>"#.as_slice(),
            ),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }

        let package = parse(writer.finish().unwrap().into_inner()).unwrap();
        assert_eq!(
            package.documents[0].metadata.title.as_deref(),
            Some("中文标题")
        );
        assert!(package.documents[0].warnings.is_empty());

        assert!(decode_xml(
            b"<?xml version=\"1.0\" encoding=\"GB18030\"?><Root>\xFF</Root>",
            "bad.xml"
        )
        .is_err());
        assert!(decode_xml(
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><Root/>",
            "unsupported.xml"
        )
        .is_err());
    }

    #[test]
    fn utf16_xml_byte_order_is_detected() {
        let xml = "<?xml version=\"1.0\" encoding=\"UTF-16\"?><Root>ok</Root>";
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend(xml.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode_xml(&bytes, "utf16.xml").unwrap(), xml);
    }

    #[test]
    fn foreign_namespace_elements_cannot_shadow_ofd_elements() {
        let xml = frag(
            r#"<Page xmlns="http://www.ofdspec.org/2016" xmlns:foreign="https://example.invalid">
                 <Content><Layer ID="1">
                   <foreign:PathObject ID="2" Boundary="0 0 1 1" Fill="true"/>
                   <PathObject ID="3" Boundary="0 0 1 1" Fill="true"><AbbreviatedData>M 0 0 L 1 1</AbbreviatedData></PathObject>
                 </Layer></Content>
               </Page>"#,
        );
        let layers = parse_page_layers(xml.root_element());
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].objects.len(), 1);
        assert!(matches!(layers[0].objects[0], GraphicObject::Path(_)));
    }

    #[test]
    fn st_id_registry_reports_invalid_and_duplicate_ids_once() {
        let xml = frag(
            r#"<Page xmlns="http://www.ofdspec.org/2016" xmlns:foreign="https://example.invalid">
                 <Content><Layer ID="1">
                   <PathObject ID="2"/><ImageObject ID="2"/><PathObject ID="2"/>
                   <TextObject ID="0"/><CompositeObject/>
                   <foreign:PathObject ID="3"/>
                 </Layer></Content>
               </Page>"#,
        );
        let mut registry = IdRegistry::default();
        let mut warnings = Vec::new();
        register_st_ids(xml.root_element(), "Page.xml", &mut registry, &mut warnings);

        assert_eq!(registry.max_id, 2);
        assert_eq!(
            warnings,
            [
                "duplicate ST_ID 2 at Page.xml <ImageObject>; first declared at Page.xml <PathObject>",
                "Page.xml <TextObject> is missing a valid nonzero required ST_ID",
                "Page.xml <CompositeObject> is missing a valid nonzero required ST_ID",
            ]
        );
    }

    #[test]
    fn resource_base_location_is_relative_to_the_resource_description() {
        use std::io::{Cursor, Write};

        let entries: [(&str, &[u8]); 5] = [
            (
                "OFD.xml",
                br#"<OFD xmlns="http://www.ofdspec.org/2016"><DocBody><DocRoot>Doc_0/Document.xml</DocRoot></DocBody></OFD>"#,
            ),
            (
                "Doc_0/Document.xml",
                br#"<Document xmlns="http://www.ofdspec.org/2016">
                      <CommonData>
                        <MaxUnitID>10</MaxUnitID>
                        <PageArea><PhysicalBox>0 0 10 10</PhysicalBox></PageArea>
                        <PublicRes>Defs/PublicRes.xml</PublicRes>
                      </CommonData>
                      <Pages><Page ID="1" BaseLoc="Pages/Page.xml"/></Pages>
                    </Document>"#,
            ),
            (
                "Doc_0/Defs/PublicRes.xml",
                br#"<Res xmlns="http://www.ofdspec.org/2016" BaseLoc="../Media">
                      <MultiMedias><MultiMedia ID="7" Type="Image" Format="PNG"><MediaFile>pixel.png</MediaFile></MultiMedia></MultiMedias>
                    </Res>"#,
            ),
            (
                "Doc_0/Pages/Page.xml",
                br#"<Page xmlns="http://www.ofdspec.org/2016"><Content><Layer ID="2"><ImageObject ID="3" Boundary="0 0 1 1" ResourceID="7"/></Layer></Content></Page>"#,
            ),
            ("Doc_0/Media/pixel.png", b"not-a-real-png"),
        ];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }

        let package = parse(writer.finish().unwrap().into_inner()).unwrap();
        let document = &package.documents[0];
        assert!(document.warnings.is_empty(), "{:?}", document.warnings);
        assert_eq!(document.resources.images.len(), 1);
        assert_eq!(document.resources.images[0].id, 7);
        assert_eq!(document.resources.images[0].data, b"not-a-real-png");
    }

    #[test]
    fn color_space_profiles_load_relative_to_base_loc_and_are_validated() {
        use std::io::{Cursor, Write};

        let valid_profile = moxcms::ColorProfile::new_adobe_rgb().encode().unwrap();
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            ("Doc_0/Profiles/adobe.icc", valid_profile.as_slice()),
            ("Doc_0/Profiles/bad.icc", b"not-an-icc-profile".as_slice()),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        let mut container = Container::open(writer.finish().unwrap().into_inner()).unwrap();
        let xml = frag(
            r#"<Res xmlns="http://www.ofdspec.org/2016" BaseLoc="Profiles">
                 <ColorSpaces>
                   <ColorSpace ID="7" Type="RGB" Profile="adobe.icc"><Palette><CV>64 128 192</CV></Palette></ColorSpace>
                   <ColorSpace ID="8" Type="RGB" Profile="bad.icc" BitsPerComponent="3"/>
                 </ColorSpaces>
               </Res>"#,
        );
        let mut resources = Resources::default();
        let mut warnings = Vec::new();
        parse_resources(
            &mut container,
            xml.root_element(),
            "Doc_0",
            &mut resources,
            &mut warnings,
            &mut HashMap::new(),
        );

        let valid = &resources.color_spaces[0];
        assert_eq!(valid.palette, [vec![64.0, 128.0, 192.0]]);
        assert_eq!(
            valid
                .profile
                .as_ref()
                .map(|profile| profile.location.as_str()),
            Some("Doc_0/Profiles/adobe.icc")
        );
        assert_eq!(
            valid.profile.as_ref().unwrap().data.as_slice(),
            valid_profile
        );
        assert_eq!(resources.color_spaces[1].bits_per_component, 8);
        assert!(warnings
            .iter()
            .any(|warning| warning == "ColorSpace 8 has invalid BitsPerComponent 3; using 8"));
        assert!(warnings.iter().any(|warning| {
            warning.starts_with("ColorSpace profile Doc_0/Profiles/bad.icc (id 8) is invalid:")
        }));
    }

    #[test]
    fn page_resources_are_relative_to_the_page_and_loaded_once() {
        use std::io::{Cursor, Write};

        let entries: [(&str, &[u8]); 4] = [
            (
                "OFD.xml",
                br#"<OFD xmlns="http://www.ofdspec.org/2016"><DocBody><DocRoot>Doc_0/Document.xml</DocRoot></DocBody></OFD>"#,
            ),
            (
                "Doc_0/Document.xml",
                br#"<Document xmlns="http://www.ofdspec.org/2016">
                      <CommonData><MaxUnitID>10</MaxUnitID><PageArea><PhysicalBox>0 0 10 10</PhysicalBox></PageArea></CommonData>
                      <Pages><Page ID="1" BaseLoc="Pages/Page_0/Content.xml"/></Pages>
                    </Document>"#,
            ),
            (
                "Doc_0/Pages/Page_0/Content.xml",
                br#"<Page xmlns="http://www.ofdspec.org/2016">
                      <PageRes>Res/PageRes.xml</PageRes><PageRes>Res/PageRes.xml</PageRes>
                      <Content><Layer ID="2"><PathObject ID="3" Boundary="0 0 1 1" DrawParam="7" Fill="true"><AbbreviatedData>M 0 0 L 1 0 L 1 1 C</AbbreviatedData></PathObject></Layer></Content>
                    </Page>"#,
            ),
            (
                "Doc_0/Pages/Page_0/Res/PageRes.xml",
                br#"<Res xmlns="http://www.ofdspec.org/2016" BaseLoc="."><DrawParams><DrawParam ID="7"><FillColor Value="255 0 0"/></DrawParam></DrawParams></Res>"#,
            ),
        ];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }

        let package = parse(writer.finish().unwrap().into_inner()).unwrap();
        let document = &package.documents[0];
        assert!(document.warnings.is_empty(), "{:?}", document.warnings);
        assert_eq!(document.resources.draw_params.len(), 1);
        assert_eq!(document.resources.draw_params[0].id, 7);
    }

    #[test]
    fn template_z_order_and_area_supply_page_defaults() {
        let template_xml = frag(
            r#"<Page xmlns="http://www.ofdspec.org/2016">
                 <Area><PhysicalBox>1 2 30 40</PhysicalBox></Area>
                 <Content><Layer ID="10"/></Content>
               </Page>"#,
        );
        let layers =
            parse_page_layers_with_default(template_xml.root_element(), LayerKind::Foreground);
        let template = ParsedTemplate {
            area: child(template_xml.root_element(), "Area").map(parse_page_area),
            default_z_order: LayerKind::Foreground,
            stats: graphic_stats_for_layers(&layers).unwrap(),
            layers,
            source_bytes: 1,
        };
        let templates = HashMap::from([("9".to_string(), template)]);

        let page_xml = frag(
            r#"<Page xmlns="http://www.ofdspec.org/2016">
                 <Template TemplateID="9"/>
                 <Content><Layer ID="20"/></Content>
               </Page>"#,
        );
        let mut warnings = Vec::new();
        let page = parse_page(
            page_xml.root_element(),
            &templates,
            &mut 0,
            &mut ParseBudget::default(),
            &mut warnings,
        )
        .unwrap();
        assert!(warnings.is_empty());
        assert_eq!(
            page.area.and_then(|area| area.physical_box),
            Some(Rect::new(1.0, 2.0, 30.0, 40.0))
        );
        assert_eq!(page.layers[0].kind, LayerKind::Foreground);
        assert_eq!(page.layers[1].kind, LayerKind::Body);

        let overridden = frag(
            r#"<Page xmlns="http://www.ofdspec.org/2016"><Template TemplateID="9" ZOrder="Background"/></Page>"#,
        );
        let page = parse_page(
            overridden.root_element(),
            &templates,
            &mut 0,
            &mut ParseBudget::default(),
            &mut warnings,
        )
        .unwrap();
        assert_eq!(page.layers[0].kind, LayerKind::Background);
    }

    #[test]
    fn duplicate_template_and_page_ids_resolve_to_the_first_declaration() {
        use std::io::{Cursor, Write};

        let entries: [(&str, &[u8]); 5] = [
            (
                "OFD.xml",
                br#"<OFD xmlns="http://www.ofdspec.org/2016"><DocBody><DocRoot>Doc_0/Document.xml</DocRoot></DocBody></OFD>"#,
            ),
            (
                "Doc_0/Document.xml",
                br#"<Document xmlns="http://www.ofdspec.org/2016">
                      <CommonData>
                        <MaxUnitID>100</MaxUnitID>
                        <PageArea><PhysicalBox>0 0 10 10</PhysicalBox></PageArea>
                        <TemplatePage ID="9" BaseLoc="TplFirst.xml" ZOrder="Background"/>
                        <TemplatePage ID="9" BaseLoc="TplSecond.xml" ZOrder="Foreground"/>
                      </CommonData>
                      <Pages><Page ID="1" BaseLoc="Page.xml"/></Pages>
                    </Document>"#,
            ),
            (
                "Doc_0/TplFirst.xml",
                br#"<Page xmlns="http://www.ofdspec.org/2016"><Content><Layer ID="10"><PathObject ID="11" Boundary="0 0 1 1"/></Layer></Content></Page>"#,
            ),
            (
                "Doc_0/TplSecond.xml",
                br#"<Page xmlns="http://www.ofdspec.org/2016"><Content><Layer ID="20"><PathObject ID="21" Boundary="0 0 1 1"/></Layer></Content></Page>"#,
            ),
            (
                "Doc_0/Page.xml",
                br#"<Page xmlns="http://www.ofdspec.org/2016"><Template TemplateID="9"/><Content><Layer ID="30"/></Content></Page>"#,
            ),
        ];
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in entries {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }

        let package = parse(writer.finish().unwrap().into_inner()).unwrap();
        let document = &package.documents[0];
        assert_eq!(
            document.warnings,
            ["duplicate ST_ID 9 at Doc_0/Document.xml <TemplatePage>; first declared at Doc_0/Document.xml <TemplatePage>"]
        );
        assert_eq!(document.pages[0].layers[0].kind, LayerKind::Background);
        assert_eq!(document.pages[0].layers[0].id, 10);
        assert!(matches!(
            &document.pages[0].layers[0].objects[0],
            GraphicObject::Path(path) if path.common.id == 11
        ));

        let duplicate_pages = [
            Page {
                id: 7,
                ..Default::default()
            },
            Page {
                id: 7,
                ..Default::default()
            },
        ];
        assert_eq!(index_pages_first(&duplicate_pages).get(&7), Some(&0));
    }

    #[test]
    fn annotation_flags_metadata_and_appearance_are_retained() {
        use std::io::{Cursor, Write};

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            (
                "Doc_0/Annots/Annotations.xml",
                br#"<Annotations xmlns="http://www.ofdspec.org/2016"><Page PageID="1"><FileLoc>Page_0.xml</FileLoc></Page></Annotations>"#.as_slice(),
            ),
            (
                "Doc_0/Annots/Page_0.xml",
                br#"<PageAnnot xmlns="http://www.ofdspec.org/2016">
                      <Annot ID="9" Type="Watermark" Creator="alice" LastModDate="2026-07-29" Subtype="review" Visible="0" Print="0" NoZoom="1" NoRotate="true" ReadOnly="false">
                        <Remark>classified</Remark><Parameters><Parameter Name="scope">page</Parameter></Parameters>
                        <Appearance Boundary="1 2 3 4"><PathObject ID="10" Boundary="0 0 1 1" Fill="true"><AbbreviatedData>M 0 0 L 1 0 L 1 1 C</AbbreviatedData></PathObject></Appearance>
                      </Annot>
                    </PageAnnot>"#.as_slice(),
            ),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        let mut container = Container::open(writer.finish().unwrap().into_inner()).unwrap();
        let mut annotations = Vec::new();
        let mut warnings = Vec::new();
        parse_annotations(
            &mut container,
            "Doc_0/Annots/Annotations.xml",
            &mut annotations,
            &mut warnings,
            &mut IdRegistry::default(),
            &mut ParseBudget::default(),
        );

        assert!(warnings.is_empty(), "{warnings:?}");
        let annotation = &annotations[0];
        assert_eq!(annotation.page_id, 1);
        assert_eq!(annotation.id, 9);
        assert_eq!(annotation.creator, "alice");
        assert_eq!(annotation.last_mod_date, "2026-07-29");
        assert_eq!(annotation.subtype.as_deref(), Some("review"));
        assert!(!annotation.visible);
        assert!(!annotation.print);
        assert!(annotation.no_zoom);
        assert!(annotation.no_rotate);
        assert!(!annotation.read_only);
        assert_eq!(annotation.remark.as_deref(), Some("classified"));
        assert_eq!(
            annotation.parameters,
            [AnnotationParameter {
                name: "scope".into(),
                value: "page".into(),
            }]
        );
        assert_eq!(
            annotation.appearance_boundary,
            Some(Rect::new(1.0, 2.0, 3.0, 4.0))
        );
        match &annotation.objects[0] {
            GraphicObject::Path(path) => {
                assert_eq!(path.common.boundary, Rect::new(1.0, 2.0, 1.0, 1.0));
            }
            object => panic!("expected annotation path, got {object:?}"),
        }
    }

    #[test]
    fn signatures_share_an_appearance_loaded_from_the_same_path() {
        use std::io::{Cursor, Write};

        fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
            assert!(content.len() < 128);
            let mut out = vec![tag, content.len() as u8];
            out.extend_from_slice(content);
            out
        }
        fn sequence(parts: &[Vec<u8>]) -> Vec<u8> {
            tlv(0x30, &parts.concat())
        }
        fn ia5(value: &str) -> Vec<u8> {
            tlv(0x16, value.as_bytes())
        }
        fn integer(value: u8) -> Vec<u8> {
            tlv(0x02, &[value])
        }

        let header = sequence(&[ia5("ES"), integer(4), ia5("VID")]);
        let picture = sequence(&[ia5("png"), tlv(0x04, b"PNGDATA"), integer(30), integer(20)]);
        let seal = sequence(&[header, ia5("seal-id"), sequence(&[integer(1)]), picture]);
        let signatures = br#"<Signatures xmlns="http://www.ofdspec.org/2016">
            <Signature ID="1" Type="Seal" BaseLoc="S1/Signature.xml"/>
            <Signature ID="2" Type="Seal" BaseLoc="S2/Signature.xml"/>
        </Signatures>"#;
        let signature = br#"<Signature xmlns="http://www.ofdspec.org/2016">
            <SignedInfo/>
            <Seal><BaseLoc>../Shared/Seal.esl</BaseLoc></Seal>
            <StampAnnot PageRef="1" Boundary="0 0 5 5"/>
        </Signature>"#;

        let cursor = Cursor::new(Vec::new());
        let mut writer = zip::ZipWriter::new(cursor);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, data) in [
            ("Doc_0/Signs/Signatures.xml", signatures.as_slice()),
            ("Doc_0/Signs/S1/Signature.xml", signature.as_slice()),
            ("Doc_0/Signs/S2/Signature.xml", signature.as_slice()),
            ("Doc_0/Signs/Shared/Seal.esl", seal.as_slice()),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        let bytes = writer.finish().unwrap().into_inner();
        let mut container = Container::open(bytes).unwrap();
        let mut seals = Vec::new();
        let mut models = Vec::new();
        let mut warnings = Vec::new();
        let mut budget = ParseBudget::default();
        parse_signatures(
            &mut container,
            "Doc_0/Signs/Signatures.xml",
            &mut seals,
            &mut models,
            &mut warnings,
            &mut budget,
        );

        assert_eq!(seals.len(), 2, "warnings: {warnings:?}");
        assert!(std::sync::Arc::ptr_eq(
            &seals[0].appearance,
            &seals[1].appearance
        ));
    }

    #[test]
    fn signature_entries_missing_base_location_are_reported() {
        use std::io::{Cursor, Write};

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file(
                "Doc_0/Signs/Signatures.xml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        writer
            .write_all(
                br#"<Signatures xmlns="http://www.ofdspec.org/2016"><Signature ID="broken" Type="Sign"/></Signatures>"#,
            )
            .unwrap();
        let mut container = Container::open(writer.finish().unwrap().into_inner()).unwrap();
        let mut seals = Vec::new();
        let mut signatures = Vec::new();
        let mut warnings = Vec::new();
        parse_signatures(
            &mut container,
            "Doc_0/Signs/Signatures.xml",
            &mut seals,
            &mut signatures,
            &mut warnings,
            &mut ParseBudget::default(),
        );

        assert!(seals.is_empty());
        assert!(signatures.is_empty());
        assert_eq!(warnings, ["Signature \"broken\": missing required BaseLoc"]);
    }

    #[test]
    fn signature_references_missing_file_location_are_reported() {
        use std::io::{Cursor, Write};

        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        let options = zip::write::SimpleFileOptions::default();
        for (name, data) in [
            (
                "Doc_0/Signs/Signatures.xml",
                br#"<Signatures xmlns="http://www.ofdspec.org/2016"><Signature ID="sig" Type="Sign" BaseLoc="Sign.xml"/></Signatures>"#.as_slice(),
            ),
            (
                "Doc_0/Signs/Sign.xml",
                br#"<Signature xmlns="http://www.ofdspec.org/2016"><SignedInfo><References><Reference><CheckValue>AA==</CheckValue></Reference></References></SignedInfo></Signature>"#.as_slice(),
            ),
        ] {
            writer.start_file(name, options).unwrap();
            writer.write_all(data).unwrap();
        }
        let mut container = Container::open(writer.finish().unwrap().into_inner()).unwrap();
        let mut signatures = Vec::new();
        let mut warnings = Vec::new();
        parse_signatures(
            &mut container,
            "Doc_0/Signs/Signatures.xml",
            &mut Vec::new(),
            &mut signatures,
            &mut warnings,
            &mut ParseBudget::default(),
        );

        assert_eq!(signatures.len(), 1);
        assert!(signatures[0].references.is_empty());
        assert_eq!(
            warnings,
            ["Signature Doc_0/Signs/Sign.xml: Reference missing required FileRef"]
        );
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
    fn oversized_or_non_integer_delta_runs_are_rejected_without_expansion() {
        assert!(validate_delta_count(&format!("g {} 1", MAX_TEXT_SLOTS + 1)).is_err());
        assert!(validate_delta_count("g 1e30 1").is_err());
        assert!(parse_deltas("g 1e30 1").is_empty());

        let xml = format!(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016" Font="1" Size="3">
                 <TextCode DeltaX="g {} 1">x</TextCode>
               </TextObject>"#,
            MAX_TEXT_SLOTS + 1
        );
        let doc = frag(&xml);
        assert!(validate_graphic_limits(doc.root_element()).is_err());
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
    fn template_expansion_budget_is_cumulative_and_checked() {
        let mut total = 0;
        charge_template_expansion(&mut total, 4, 8).unwrap();
        charge_template_expansion(&mut total, 4, 8).unwrap();
        assert_eq!(total, 8);
        assert!(matches!(
            charge_template_expansion(&mut total, 1, 8),
            Err(OfdError::ResourceLimit(_))
        ));

        let mut overflow = u64::MAX;
        assert!(matches!(
            charge_template_expansion(&mut overflow, 1, u64::MAX),
            Err(OfdError::ResourceLimit(_))
        ));
    }

    #[test]
    fn package_parse_budgets_are_cumulative_and_template_charges_are_atomic() {
        let mut xml_budget = ParseBudget {
            xml_nodes: MAX_TOTAL_XML_NODES,
            ..Default::default()
        };
        assert!(matches!(
            xml_budget.charge_xml_nodes(1),
            Err(OfdError::ResourceLimit(_))
        ));

        let mut budget = ParseBudget {
            graphic_objects: MAX_GRAPHIC_OBJECTS - 1,
            model_items: MAX_MODEL_ITEMS,
            ..Default::default()
        };
        assert!(matches!(
            budget.charge_graphic_stats(GraphicStats {
                graphic_objects: 1,
                model_items: 1,
            }),
            Err(OfdError::ResourceLimit(_))
        ));
        assert_eq!(budget.graphic_objects, MAX_GRAPHIC_OBJECTS - 1);
        assert_eq!(budget.model_items, MAX_MODEL_ITEMS);
    }

    #[test]
    fn graphic_validation_charges_nodes_objects_and_expanded_arrays() {
        let doc = frag(
            r#"<PageBlock xmlns="http://www.ofdspec.org/2016">
                 <PathObject><AbbreviatedData>M 0 0 L 1 0 C</AbbreviatedData></PathObject>
                 <TextObject Font="1" Size="3">
                   <TextCode DeltaX="g 2 1">ab</TextCode>
                   <CGTransform CodePosition="0" CodeCount="2" GlyphCount="2">
                     <Glyphs>1 2</Glyphs>
                   </CGTransform>
                 </TextObject>
               </PageBlock>"#,
        );
        let mut budget = ParseBudget::default();
        let validation =
            validate_graphic_limits_with_budget(doc.root_element(), &mut budget).unwrap();

        assert!(validation.malformed.is_none());
        assert_eq!(budget.graphic_objects, 3);
        assert!(budget.model_items > budget.xml_nodes);
    }

    #[test]
    fn resource_graph_validation_handles_long_chains_without_quadratic_walks() {
        const COUNT: u64 = 4096;
        let mut resources = Resources {
            draw_params: (1..=COUNT)
                .map(|id| DrawParam {
                    id,
                    relative: (id < COUNT).then_some(id + 1),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        };
        let mut warnings = Vec::new();
        validate_draw_param_graph(&resources, &mut warnings);
        assert!(warnings.is_empty());

        resources.draw_params.last_mut().unwrap().relative = Some(COUNT / 2);
        validate_draw_param_graph(&resources, &mut warnings);
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("Relative cycle"))
                .count(),
            1
        );

        resources.composite_graphic_units = (1..=COUNT)
            .map(|id| CompositeGraphicUnit {
                id,
                width: 1.0,
                height: 1.0,
                objects: if id < COUNT {
                    vec![GraphicObject::Composite(CompositeObject {
                        common: GraphicCommon::default(),
                        resource_id: id + 1,
                    })]
                } else {
                    Vec::new()
                },
            })
            .collect();
        validate_composite_graph(&resources, &mut warnings);
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("CompositeGraphicUnit reference cycle"))
                .count(),
            0
        );
        resources
            .composite_graphic_units
            .last_mut()
            .unwrap()
            .objects = vec![GraphicObject::Composite(CompositeObject {
            common: GraphicCommon::default(),
            resource_id: COUNT / 2,
        })];
        validate_composite_graph(&resources, &mut warnings);
        assert_eq!(
            warnings
                .iter()
                .filter(|warning| warning.contains("CompositeGraphicUnit reference cycle"))
                .count(),
            1
        );
    }

    #[test]
    fn resource_graph_warnings_are_stably_ordered_by_resource_id() {
        let mut resources = Resources {
            draw_params: vec![
                DrawParam {
                    id: 20,
                    relative: Some(200),
                    ..Default::default()
                },
                DrawParam {
                    id: 10,
                    relative: Some(100),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        let mut warnings = Vec::new();
        validate_draw_param_graph(&resources, &mut warnings);
        assert_eq!(
            warnings,
            [
                "DrawParam 10 has unresolved Relative id 100",
                "DrawParam 20 has unresolved Relative id 200",
            ]
        );

        resources.composite_graphic_units = [20, 10]
            .into_iter()
            .map(|id| CompositeGraphicUnit {
                id,
                width: 1.0,
                height: 1.0,
                objects: vec![GraphicObject::Composite(CompositeObject {
                    common: GraphicCommon::default(),
                    resource_id: id,
                })],
            })
            .collect();
        warnings.clear();
        validate_composite_graph(&resources, &mut warnings);
        assert_eq!(
            warnings,
            [
                "CompositeGraphicUnit reference cycle contains id 10",
                "CompositeGraphicUnit reference cycle contains id 20",
            ]
        );
    }

    #[test]
    fn text_code_inherits_position_and_decodes_hex() {
        let xml = XmlDoc::parse(
            r#"<TextObject xmlns="http://www.ofdspec.org/2016" ID="1" Font="1" Size="3">
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
            r##"<Root xmlns="http://www.ofdspec.org/2016">
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

    #[test]
    fn complex_colors_preserve_the_containing_color_alpha() {
        let xml = XmlDoc::parse(
            r#"<Root xmlns="http://www.ofdspec.org/2016">
                  <Color Alpha="101"><Pattern Width="1" Height="1"/></Color>
                  <Color Alpha="102"><AxialShd StartPoint="0 0" EndPoint="1 0"/></Color>
                  <Color Alpha="103"><RadialShd StartPoint="0 0" EndPoint="1 0" EndRadius="1"/></Color>
                  <Color Alpha="104"><GouraudShd/></Color>
                  <Color Alpha="105"><LaGouraudShd VerticesPerRow="2"/></Color>
                </Root>"#,
        )
        .unwrap();
        let colors: Vec<_> = xml
            .root_element()
            .children()
            .filter(|node| local(node) == "Color")
            .map(|node| parse_color_node(node).unwrap())
            .collect();
        assert!(matches!(&colors[0], OfdColor::Pattern(color) if color.alpha == 101));
        assert!(matches!(&colors[1], OfdColor::Axial(color) if color.alpha == 102));
        assert!(matches!(&colors[2], OfdColor::Radial(color) if color.alpha == 103));
        assert!(matches!(&colors[3], OfdColor::Gouraud(color) if color.alpha == 104));
        assert!(matches!(&colors[4], OfdColor::LatticeGouraud(color) if color.alpha == 105));
    }
}
