//! Font resolution. OFD fonts may embed a `FontFile` (TTF/OTF) or merely name a
//! family (e.g. 宋体 / SimSun), in which case the host may inject substitute
//! font bytes.
//!
//! [`FontResolver`] owns only host-injected fallback fonts. It performs no file
//! system discovery, preserving the core's bytes-in/bitmap-out boundary.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::model::Font;

/// Family names tried, in order, when an OFD font has no embedded file and its
/// declared family cannot be matched directly. Aimed at CJK coverage across
/// macOS / Windows / Linux.
const CJK_FALLBACKS: &[&str] = &[
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
];

/// Cross-platform aliases for the common Chinese system fonts. An OFD names a
/// font like 宋体 / SimSun; the real installed family differs by platform
/// (Songti SC on macOS, SimSun on Windows, Source Han on Linux). Each group
/// lists equivalent family names; resolving any member tries all the others.
const FONT_ALIAS_GROUPS: &[&[&str]] = &[
    // 宋体 (Song / serif)
    &[
        "宋体",
        "SimSun",
        "NSimSun",
        "STSong",
        "Songti SC",
        "Song",
        "Noto Serif CJK SC",
        "Source Han Serif SC",
    ],
    // 黑体 (Hei / sans)
    &[
        "黑体",
        "SimHei",
        "STHeiti",
        "Heiti SC",
        "Hei",
        "Noto Sans CJK SC",
        "Source Han Sans SC",
    ],
    // 楷体 (Kai)
    &[
        "楷体",
        "楷体_GB2312",
        "KaiTi",
        "KaiTi_GB2312",
        "STKaiti",
        "Kaiti SC",
        "Kai",
    ],
    // 仿宋 (FangSong)
    &[
        "仿宋",
        "仿宋_GB2312",
        "FangSong",
        "FangSong_GB2312",
        "STFangsong",
        "Fangsong SC",
    ],
    // 微软雅黑 / 苹方 (modern UI sans)
    &[
        "微软雅黑",
        "Microsoft YaHei",
        "PingFang SC",
        "苹方",
        "Heiti SC",
    ],
    // 等线 / 中易宋体 misc
    &["等线", "DengXian"],
];

/// Return candidate family names to try for `family`: the name itself, then any
/// aliases from its group.
fn family_candidates(family: &str) -> Vec<String> {
    let mut out = vec![family.to_string()];
    let key = family.trim();
    for group in FONT_ALIAS_GROUPS {
        if group.iter().any(|g| g.eq_ignore_ascii_case(key)) {
            for &g in *group {
                if !g.eq_ignore_ascii_case(key) {
                    out.push(g.to_string());
                }
            }
        }
    }
    out
}

fn resolution_candidates(family: &str) -> Vec<(String, bool)> {
    family_candidates(family)
        .into_iter()
        .enumerate()
        .map(|(index, name)| (name, index == 0))
        .collect()
}

/// A resolved face: owned font bytes plus the face index within a collection,
/// and (for CID-keyed CFFs) a CID → GID map for `CGTransform` glyph ids.
#[derive(Clone)]
pub struct ResolvedFont {
    pub data: Arc<Vec<u8>>,
    pub index: u32,
    pub cid_to_gid: Option<Arc<HashMap<u16, u16>>>,
    /// Whether `CGTransform` glyph **indices** can be trusted against this face.
    /// True only when the face is the document's embedded font or the exact
    /// declared family was found. Aliases and generic substitutes may have a
    /// different glyph order, so their producer indices are not trusted and the
    /// renderer maps by the real character instead.
    pub trusted_glyph_ids: bool,
    /// The requested text is italic but the selected face is upright. The
    /// renderer synthesizes an oblique outline only in this case, avoiding a
    /// second shear when an italic/oblique face was selected.
    pub synthetic_italic: bool,
}

#[derive(Clone, Debug)]
struct FontDescriptor {
    family: String,
    charset: Option<String>,
    italic: bool,
    bold: bool,
    serif: bool,
    fixed_width: bool,
}

impl FontDescriptor {
    fn from_font(font: &Font) -> Self {
        Self {
            family: font.family().to_string(),
            charset: font.charset.clone(),
            italic: font.italic,
            bold: font.bold,
            serif: font.serif,
            fixed_width: font.fixed_width,
        }
    }

    fn effective_style(&self, text_style: FontStyleRequest) -> FontStyleRequest {
        FontStyleRequest {
            weight: if self.bold {
                text_style.weight.max(fontdb::Weight::BOLD.0)
            } else {
                text_style.weight
            },
            italic: self.italic || text_style.italic,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct FontStyleRequest {
    weight: u16,
    italic: bool,
}

impl FontStyleRequest {
    fn new(weight: u16, italic: bool) -> Self {
        Self {
            weight: weight.clamp(fontdb::Weight::THIN.0, fontdb::Weight::BLACK.0),
            italic,
        }
    }
}

/// What a given OFD font id resolves from.
enum FontSource {
    /// The document's embedded font file, validated/prepared for `ttf-parser`
    /// (TTF/OTF as-is, or a bare CFF wrapped). Used directly so the document's
    /// own glyphs are always rendered. The optional map handles CID-keyed CFFs.
    Embedded(Arc<Vec<u8>>, Option<Arc<HashMap<u16, u16>>>),
    /// No usable embedded file — substitute using all standard CT_Font
    /// matching hints, not just the declared family name.
    Named(FontDescriptor),
}

/// Resolves OFD font ids to concrete font faces, caching results.
pub struct FontResolver {
    /// Host-injected fallback fonts, indexed in memory by their declared names.
    bundled: fontdb::Database,
    /// Every face id points back to the shared bytes loaded into `bundled`.
    /// Collections map multiple face ids to the same allocation.
    bundled_data: HashMap<fontdb::ID, Arc<Vec<u8>>>,
    /// OFD font id → embedded bytes or a family name to substitute.
    by_id: HashMap<u64, FontSource>,
    cache: HashMap<(u64, FontStyleRequest), Option<ResolvedFont>>,
    character_fallback_cache: HashMap<(u64, FontStyleRequest, char), Option<ResolvedFont>>,
}

impl FontResolver {
    /// Build a resolver from a document's font resources, with no bundled
    /// fallback fonts.
    pub fn new(fonts: &[Font]) -> Self {
        Self::with_bundled(fonts, &[])
    }

    /// Build a resolver, injecting deterministic fallback font files (raw
    /// TTF/OTF bytes) that take priority over system fonts when matching a
    /// declared family name.
    pub fn with_bundled(fonts: &[Font], bundled_fonts: &[Arc<Vec<u8>>]) -> Self {
        let mut bundled = fontdb::Database::new();
        let mut bundled_data = HashMap::new();
        for data in bundled_fonts {
            let source: Arc<dyn AsRef<[u8]> + Send + Sync> = data.clone();
            for id in bundled.load_font_source(fontdb::Source::Binary(source)) {
                bundled_data.insert(id, data.clone());
            }
        }

        let mut by_id = HashMap::new();
        for f in fonts {
            // Prefer the embedded file directly. `usable_font` returns the
            // bytes ttf-parser can read — the file as-is, or a synthesised OTTO
            // wrapper for bare-CFF fonts. Only substitute when there is no
            // usable embedded font. This guarantees the document's own font is
            // used and avoids the fragile shared-fontdb lookup.
            let source = match f.data.as_deref().and_then(crate::cff::usable_font) {
                Some(p) => FontSource::Embedded(Arc::new(p.data), p.cid_to_gid.map(Arc::new)),
                None => FontSource::Named(FontDescriptor::from_font(f)),
            };
            by_id.entry(f.id).or_insert(source);
        }

        Self {
            bundled,
            bundled_data,
            by_id,
            cache: HashMap::new(),
            character_fallback_cache: HashMap::new(),
        }
    }

    /// Resolve an OFD font id to face bytes, substituting a CJK font if needed.
    pub fn resolve(&mut self, font_id: u64) -> Option<ResolvedFont> {
        self.resolve_styled(font_id, 400, false)
    }

    /// Resolve a font for a text object's `Weight` and `Italic` attributes.
    pub(crate) fn resolve_styled(
        &mut self,
        font_id: u64,
        weight: u16,
        italic: bool,
    ) -> Option<ResolvedFont> {
        let text_style = FontStyleRequest::new(weight, italic);
        let key = (font_id, text_style);
        if let Some(hit) = self.cache.get(&key) {
            return hit.clone();
        }
        let resolved = self.resolve_uncached(font_id, text_style);
        self.cache.insert(key, resolved.clone());
        resolved
    }

    fn resolve_uncached(&self, font_id: u64, text_style: FontStyleRequest) -> Option<ResolvedFont> {
        let descriptor = match self.by_id.get(&font_id)? {
            // 1. Embedded face wins — exact, matches every implementation.
            FontSource::Embedded(bytes, cid_to_gid) => {
                let face_is_italic = ttf_parser::Face::parse(bytes, 0)
                    .ok()
                    .is_some_and(|face| face.is_italic());
                return Some(ResolvedFont {
                    data: bytes.clone(),
                    index: 0,
                    cid_to_gid: cid_to_gid.clone(),
                    trusted_glyph_ids: true,
                    synthetic_italic: text_style.italic && !face_is_italic,
                });
            }
            FontSource::Named(descriptor) => descriptor,
        };
        let style = descriptor.effective_style(text_style);

        // 2. The exact declared family can use explicit producer glyph ids. An
        // alias is still a substitute (often a different font implementation),
        // so its glyph ordering must not be trusted.
        if !descriptor.family.is_empty() {
            for (candidate, trusted) in resolution_candidates(&descriptor.family) {
                if let Some(f) = self.query(&candidate, style, trusted) {
                    return Some(f);
                }
            }
        }
        // 3. Generic CJK fallbacks, again bundled first. These are *not* the
        //    declared font, so CGTransform glyph indices must not be trusted.
        if charset_prefers_cjk(descriptor.charset.as_deref()) {
            for family in ordered_cjk_fallbacks(descriptor.serif) {
                if let Some(font) = self.query(family, style, false) {
                    if !descriptor.fixed_width || resolved_font_is_monospaced(&self.bundled, &font)
                    {
                        return Some(font);
                    }
                }
            }
        }
        // 4. Anything at all, ranked by the remaining CT_Font hints so text
        // still draws even when the declared family is unavailable.
        self.best_available_face(descriptor, style, false)
    }

    /// Find a different injected face that covers `ch` when a non-embedded
    /// font's selected substitute does not. Embedded fonts remain authoritative:
    /// their missing cmap entries and producer glyph ids are never mixed with a
    /// different face.
    pub(crate) fn fallback_for_char_styled(
        &mut self,
        font_id: u64,
        weight: u16,
        italic: bool,
        ch: char,
    ) -> Option<ResolvedFont> {
        let text_style = FontStyleRequest::new(weight, italic);
        let key = (font_id, text_style, ch);
        if let Some(hit) = self.character_fallback_cache.get(&key) {
            return hit.clone();
        }
        let descriptor = match self.by_id.get(&font_id) {
            Some(FontSource::Named(descriptor)) => descriptor.clone(),
            Some(FontSource::Embedded(..)) | None => {
                self.character_fallback_cache.insert(key, None);
                return None;
            }
        };
        let primary = self.resolve_styled(font_id, weight, italic);
        let fallback = self.find_character_fallback(&descriptor, text_style, ch, primary.as_ref());
        self.character_fallback_cache.insert(key, fallback.clone());
        fallback
    }

    fn find_character_fallback(
        &self,
        descriptor: &FontDescriptor,
        text_style: FontStyleRequest,
        ch: char,
        primary: Option<&ResolvedFont>,
    ) -> Option<ResolvedFont> {
        let style = descriptor.effective_style(text_style);
        let mut candidates = Vec::new();
        let mut seen = HashSet::new();
        let named = family_candidates(&descriptor.family).into_iter();
        let cjk = charset_prefers_cjk(descriptor.charset.as_deref())
            .then(|| ordered_cjk_fallbacks(descriptor.serif))
            .into_iter()
            .flatten()
            .map(str::to_string);
        for candidate in named.chain(cjk) {
            if let Some(id) = query_family(&self.bundled, &candidate, style) {
                if seen.insert(id) {
                    candidates.push(id);
                }
            }
        }
        let mut remaining: Vec<_> = self
            .bundled
            .faces()
            .filter(|face| !seen.contains(&face.id))
            .collect();
        remaining.sort_by_key(|face| font_match_score(face, descriptor, style));
        for face in remaining {
            if seen.insert(face.id) {
                candidates.push(face.id);
            }
        }

        candidates.into_iter().find_map(|id| {
            let candidate = face_bytes(&self.bundled, &self.bundled_data, id, false, style.italic)?;
            if primary.is_some_and(|primary| same_face(primary, &candidate))
                || !resolved_font_has_char(&candidate, ch)
            {
                return None;
            }
            Some(candidate)
        })
    }

    fn query(&self, family: &str, style: FontStyleRequest, trusted: bool) -> Option<ResolvedFont> {
        query_family(&self.bundled, family, style)
            .and_then(|id| face_bytes(&self.bundled, &self.bundled_data, id, trusted, style.italic))
    }

    fn best_available_face(
        &self,
        descriptor: &FontDescriptor,
        style: FontStyleRequest,
        trusted: bool,
    ) -> Option<ResolvedFont> {
        let id = self
            .bundled
            .faces()
            .min_by_key(|face| font_match_score(face, descriptor, style))?
            .id;
        face_bytes(&self.bundled, &self.bundled_data, id, trusted, style.italic)
    }
}

fn same_face(left: &ResolvedFont, right: &ResolvedFont) -> bool {
    left.index == right.index && Arc::ptr_eq(&left.data, &right.data)
}

fn resolved_font_has_char(font: &ResolvedFont, ch: char) -> bool {
    ttf_parser::Face::parse(&font.data, font.index)
        .ok()
        .and_then(|face| face.glyph_index(ch))
        .is_some_and(|glyph| glyph.0 != 0)
}

fn query_family(
    db: &fontdb::Database,
    family: &str,
    style: FontStyleRequest,
) -> Option<fontdb::ID> {
    db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: fontdb::Weight(style.weight),
        style: if style.italic {
            fontdb::Style::Italic
        } else {
            fontdb::Style::Normal
        },
        ..Default::default()
    })
}

fn charset_prefers_cjk(charset: Option<&str>) -> bool {
    let Some(charset) = charset else {
        return true;
    };
    let normalized = charset.trim().to_ascii_lowercase();
    normalized.is_empty()
        || normalized == "unicode"
        || normalized.contains("prc")
        || normalized.contains("gb")
        || normalized.contains("big5")
        || normalized.contains("jis")
        || normalized.contains("wansung")
        || normalized.contains("johab")
}

fn family_is_serif(family: &str) -> bool {
    let family = family.to_ascii_lowercase();
    ["serif", "song", "simsun", "fangsong", "ming"]
        .iter()
        .any(|marker| family.contains(marker))
}

fn family_is_cjk(family: &str) -> bool {
    CJK_FALLBACKS
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(family))
        || [
            "cjk", "simsun", "simhei", "songti", "heiti", "yahei", "pingfang",
        ]
        .iter()
        .any(|marker| family.to_ascii_lowercase().contains(marker))
}

fn ordered_cjk_fallbacks(serif: bool) -> Vec<&'static str> {
    let mut families = CJK_FALLBACKS.to_vec();
    families.sort_by_key(|family| family_is_serif(family) != serif);
    families
}

fn font_match_score(
    face: &fontdb::FaceInfo,
    descriptor: &FontDescriptor,
    style: FontStyleRequest,
) -> (bool, bool, bool, u8, u16) {
    let face_is_serif = face
        .families
        .iter()
        .any(|(family, _)| family_is_serif(family));
    let face_is_cjk = face
        .families
        .iter()
        .any(|(family, _)| family_is_cjk(family));
    let style_distance = match (style.italic, face.style) {
        (false, fontdb::Style::Normal) => 0,
        (true, fontdb::Style::Italic | fontdb::Style::Oblique) => 0,
        (true, fontdb::Style::Normal) => 1,
        (false, fontdb::Style::Oblique) => 2,
        (false, fontdb::Style::Italic) => 3,
    };
    (
        descriptor.fixed_width && !face.monospaced,
        face_is_serif != descriptor.serif,
        charset_prefers_cjk(descriptor.charset.as_deref()) && !face_is_cjk,
        style_distance,
        face.weight.0.abs_diff(style.weight),
    )
}

fn resolved_font_is_monospaced(db: &fontdb::Database, font: &ResolvedFont) -> bool {
    db.faces().any(|face| {
        face.index == font.index
            && face.monospaced
            && matches!(&face.source, fontdb::Source::Binary(data) if {
                let source = data.as_ref().as_ref();
                source.as_ptr() == font.data.as_ptr() && source.len() == font.data.len()
            })
    })
}

fn face_bytes(
    db: &fontdb::Database,
    data: &HashMap<fontdb::ID, Arc<Vec<u8>>>,
    id: fontdb::ID,
    trusted: bool,
    requested_italic: bool,
) -> Option<ResolvedFont> {
    let face = db.face(id)?;
    Some(ResolvedFont {
        data: data.get(&id)?.clone(),
        index: face.index,
        // Substituted faces are sfnt fonts addressed by GID; CGTransform ids
        // (when used) are GIDs, so no CID remapping is needed.
        cid_to_gid: None,
        trusted_glyph_ids: trusted,
        synthetic_italic: requested_italic && face.style == fontdb::Style::Normal,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_declared_family_trusts_producer_glyph_ids() {
        let candidates = resolution_candidates("SimSun");
        assert_eq!(candidates.first(), Some(&("SimSun".to_string(), true)));
        assert!(candidates.len() > 1);
        assert!(candidates.iter().skip(1).all(|(_, trusted)| !trusted));
    }

    #[test]
    fn duplicate_font_ids_keep_the_first_declaration() {
        let fonts = [
            Font {
                id: 7,
                font_name: "First".into(),
                ..Default::default()
            },
            Font {
                id: 7,
                font_name: "Second".into(),
                ..Default::default()
            },
        ];
        let resolver = FontResolver::new(&fonts);
        assert!(matches!(
            resolver.by_id.get(&7),
            Some(FontSource::Named(descriptor)) if descriptor.family == "First"
        ));
    }

    #[test]
    fn non_embedded_fonts_fall_back_per_character() {
        let fixture =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/zsbk.ofd");
        let bytes = std::fs::read(fixture).unwrap();
        let mut container = crate::container::Container::open(bytes).unwrap();
        let names: Vec<String> = container
            .entry_names()
            .into_iter()
            .filter(|name| name.ends_with(".ttf"))
            .collect();
        let mut bundled: Vec<Arc<Vec<u8>>> = names
            .iter()
            .map(|name| Arc::new(container.read(name).unwrap()))
            .collect();
        bundled.sort_by_key(|font| font.len());
        let fonts = [Font {
            id: 42,
            font_name: "Missing Declared Family".into(),
            ..Default::default()
        }];
        let mut resolver = FontResolver::with_bundled(&fonts, &bundled);
        let primary = resolver.resolve(42).unwrap();
        let candidates: Vec<ResolvedFont> = resolver
            .bundled
            .faces()
            .filter_map(|face| {
                face_bytes(
                    &resolver.bundled,
                    &resolver.bundled_data,
                    face.id,
                    false,
                    false,
                )
            })
            .filter(|font| !same_face(font, &primary))
            .collect();
        let character = (0x3400..=0x9fff)
            .filter_map(char::from_u32)
            .find(|character| {
                !resolved_font_has_char(&primary, *character)
                    && candidates
                        .iter()
                        .any(|font| resolved_font_has_char(font, *character))
            })
            .expect("fixture fonts should have disjoint subset coverage");

        let fallback = resolver
            .fallback_for_char_styled(42, 400, false, character)
            .unwrap();
        assert!(!same_face(&primary, &fallback));
        assert!(resolved_font_has_char(&fallback, character));

        let embedded = [Font {
            id: 43,
            font_name: "Embedded".into(),
            data: Some(primary.data.as_ref().clone()),
            ..Default::default()
        }];
        let mut embedded_resolver = FontResolver::with_bundled(&embedded, &bundled);
        assert!(embedded_resolver
            .fallback_for_char_styled(43, 400, false, character)
            .is_none());
    }
}
