//! Font resolution. OFD fonts may embed a `FontFile` (TTF/OTF) or merely name a
//! family (e.g. 宋体 / SimSun), in which case native builds may substitute a
//! system CJK font.
//!
//! [`FontResolver`] owns host-injected fallback fonts and, behind the `native`
//! feature, system fonts. It resolves an OFD font id to concrete face bytes that
//! the renderer outlines with `ttf-parser`.

use std::collections::HashMap;
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

/// A resolved face: owned font bytes plus the face index within a collection.
#[derive(Clone)]
pub struct ResolvedFont {
    pub data: Arc<Vec<u8>>,
    pub index: u32,
}

/// What a given OFD font id resolves from.
enum FontSource {
    /// The document's embedded font file (TTF/OTF), validated to parse. Used
    /// directly so the document's own glyphs are always rendered.
    Embedded(Arc<Vec<u8>>),
    /// No usable embedded file — substitute by this declared family name.
    Named(String),
}

/// Resolves OFD font ids to concrete font faces, caching results.
pub struct FontResolver {
    /// Deterministic, host-injected fallback fonts (the Windows core CJK fonts).
    /// Preferred over system fonts so rendering matches major implementations.
    bundled: fontdb::Database,
    /// System fonts, used to substitute non-embedded fonts on native builds.
    db: fontdb::Database,
    /// OFD font id → embedded bytes or a family name to substitute.
    by_id: HashMap<u64, FontSource>,
    cache: HashMap<u64, Option<ResolvedFont>>,
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
    pub fn with_bundled(fonts: &[Font], bundled_fonts: &[Vec<u8>]) -> Self {
        #[cfg(feature = "native")]
        let mut db = fontdb::Database::new();
        #[cfg(not(feature = "native"))]
        let db = fontdb::Database::new();
        #[cfg(feature = "native")]
        db.load_system_fonts();

        let mut bundled = fontdb::Database::new();
        for data in bundled_fonts {
            bundled.load_font_data(data.clone());
        }

        let mut by_id = HashMap::new();
        for f in fonts {
            // Prefer the embedded file directly. `usable_font` returns the
            // bytes ttf-parser can read — the file as-is, or a synthesised OTTO
            // wrapper for bare-CFF fonts. Only substitute when there is no
            // usable embedded font. This guarantees the document's own font is
            // used and avoids the fragile shared-fontdb lookup.
            let source = match f.data.as_deref().and_then(crate::cff::usable_font) {
                Some(bytes) => FontSource::Embedded(Arc::new(bytes)),
                None => FontSource::Named(f.family().to_string()),
            };
            by_id.insert(f.id, source);
        }

        Self {
            bundled,
            db,
            by_id,
            cache: HashMap::new(),
        }
    }

    /// Resolve an OFD font id to face bytes, substituting a CJK font if needed.
    pub fn resolve(&mut self, font_id: u64) -> Option<ResolvedFont> {
        if let Some(hit) = self.cache.get(&font_id) {
            return hit.clone();
        }
        let resolved = self.resolve_uncached(font_id);
        self.cache.insert(font_id, resolved.clone());
        resolved
    }

    fn resolve_uncached(&self, font_id: u64) -> Option<ResolvedFont> {
        let family = match self.by_id.get(&font_id)? {
            // 1. Embedded face wins — exact, matches every implementation.
            FontSource::Embedded(bytes) => {
                return Some(ResolvedFont {
                    data: bytes.clone(),
                    index: 0,
                });
            }
            FontSource::Named(family) => family,
        };

        // 2. Declared family + aliases, bundled fonts first (deterministic),
        //    then system fonts.
        if !family.is_empty() {
            for cand in family_candidates(family) {
                if let Some(f) = self.query_both(&cand) {
                    return Some(f);
                }
            }
        }
        // 3. Generic CJK fallbacks, again bundled first.
        for fam in CJK_FALLBACKS {
            if let Some(f) = self.query_both(fam) {
                return Some(f);
            }
        }
        // 4. Anything at all, so text still draws.
        self.bundled
            .faces()
            .next()
            .and_then(|info| face_bytes(&self.bundled, info.id))
            .or_else(|| {
                self.db
                    .faces()
                    .next()
                    .and_then(|info| face_bytes(&self.db, info.id))
            })
    }

    /// Query a family name in the bundled db first, then the system db.
    fn query_both(&self, family: &str) -> Option<ResolvedFont> {
        query_family(&self.bundled, family)
            .and_then(|id| face_bytes(&self.bundled, id))
            .or_else(|| query_family(&self.db, family).and_then(|id| face_bytes(&self.db, id)))
    }
}

fn query_family(db: &fontdb::Database, family: &str) -> Option<fontdb::ID> {
    db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        ..Default::default()
    })
}

fn face_bytes(db: &fontdb::Database, id: fontdb::ID) -> Option<ResolvedFont> {
    db.with_face_data(id, |data, index| ResolvedFont {
        data: Arc::new(data.to_vec()),
        index,
    })
}
