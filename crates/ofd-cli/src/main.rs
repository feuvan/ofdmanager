//! `ofd-cli` — render and inspect OFD files.
//!
//! Usage:
//!   `ofd-cli render <input.ofd> <output.png> [--dpi N] [--page I] [--region x,y,w,h] [--stem F] [--strict]`
//!   `ofd-cli verify <input.ofd>`   — check signature file-digest integrity

use std::io::Read;
use std::process::ExitCode;
use std::sync::Arc;

const MAX_FONT_FAMILY_QUERIES: usize = 4096;
const MAX_FALLBACK_FONT_FACES: usize = 256;
const MAX_FALLBACK_FONT_BYTES: u64 = 256 * 1024 * 1024;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let result = match args.get(1).map(|s| s.as_str()) {
        Some("verify") => verify(&args),
        Some("render") => run(&args),
        _ => Err("usage: ofd-cli <render|verify> <input.ofd> ...".into()),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// `verify` — report each signature's file-digest integrity (GB/T 33190 §18.2).
fn verify(args: &[String]) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let input = args.get(2).ok_or("usage: ofd-cli verify <input.ofd>")?;
    let bytes = read_ofd(input)?;
    let pkg = ofd_core::open(bytes.clone())?;
    if pkg.documents.is_empty() {
        return Err("OFD package contains no documents".into());
    }
    let signature_parse_warnings: Vec<&str> = pkg
        .documents
        .iter()
        .flat_map(|document| document.warnings.iter().map(String::as_str))
        .filter(|warning| warning_blocks_verification(warning))
        .collect();
    for warning in &signature_parse_warnings {
        eprintln!("warning: {warning}");
    }
    let verification_incomplete = !signature_parse_warnings.is_empty();
    let signatures: Vec<_> = pkg
        .documents
        .iter()
        .flat_map(|document| document.signatures.iter().cloned())
        .collect();
    let reports = ofd_core::sign::verify(bytes, &signatures)?;

    let mut all_ok = true;
    let mut any = false;
    for r in &reports {
        any = true;
        let kind = match r.sig_type {
            ofd_core::model::SignatureType::Seal => "Seal",
            ofd_core::model::SignatureType::Sign => "Sign",
        };
        let ok = r.integrity_ok();
        all_ok &= ok;
        println!(
            "Signature {} [{kind}] integrity: {}",
            r.id,
            if ok { "OK" } else { "FAILED" }
        );
        if let Some(p) = &r.provider {
            println!("  provider: {p}");
        }
        if let Some(m) = &r.signature_method {
            println!("  method:   {m}");
        }
        if let Some(t) = &r.signature_date_time {
            println!("  signed:   {t}");
        }
        println!("  protected files ({}):", r.references.len());
        for rf in &r.references {
            println!("    [{:?}] {} ({})", rf.status, rf.file_ref, rf.method);
        }
    }
    if !any {
        if verification_incomplete {
            println!("no signatures could be verified because signature metadata is incomplete");
            return Ok(ExitCode::FAILURE);
        }
        println!("no signatures in document");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "\nNote: this checks file-digest integrity only; cryptographic \
         authenticity (SM2 signature + certificate) is not verified."
    );
    Ok(if all_ok && !verification_incomplete {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn warning_blocks_verification(warning: &str) -> bool {
    warning.starts_with("Signatures ") || warning.starts_with("Signature ")
}

fn run(args: &[String]) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if args.len() < 4 || args[1] != "render" {
        return Err("usage: ofd-cli render <input.ofd> <output.png> [--dpi N] [--page I]".into());
    }
    let input = &args[2];
    let output = &args[3];
    let dpi = flag_value(args, "--dpi")?.unwrap_or(144.0);
    if dpi <= 0.0 {
        return Err("--dpi must be greater than zero".into());
    }
    let page_index = flag_usize(args, "--page")?.unwrap_or(0);
    let stem = flag_value(args, "--stem")?.unwrap_or(ofd_core::render::DEFAULT_STEM_DARKENING);
    if stem < 0.0 {
        return Err("--stem must be non-negative".into());
    }
    let region = flag_str(args, "--region")?
        .map(|value| {
            parse_region(value).ok_or_else(|| {
                "--region must be x,y,width,height with positive dimensions".to_string()
            })
        })
        .transpose()?;

    let bytes = read_ofd(input)?;
    let pkg = ofd_core::open(bytes)?;
    let doc = pkg
        .documents
        .first()
        .ok_or("OFD package contains no documents")?;
    let strict = args.iter().any(|a| a == "--strict");

    // Surface non-fatal parse problems (missing/malformed referenced resources).
    // `--strict` turns them into a hard failure so corrupted OFDs don't render
    // as silently-incomplete "successes".
    if !doc.warnings.is_empty() {
        for w in &doc.warnings {
            eprintln!("warning: {w}");
        }
        if strict {
            return Err(format!("{} parse warning(s) with --strict", doc.warnings.len()).into());
        }
    }

    let opts = ofd_core::render::RenderOptions {
        fallback_fonts: load_fallback_fonts(doc),
        text_stem_darkening: stem,
        strict,
        ..Default::default()
    };
    let bmp = ofd_core::render::render_page_with(doc, page_index, dpi, &opts)?;

    // Optional crop, for inspecting a sub-region: --region x,y,w,h (pixels).
    let (rgba, w, h) = match region {
        Some((x, y, rw, rh)) if x < bmp.width && y < bmp.height => {
            crop(&bmp.rgba, bmp.width, bmp.height, x, y, rw, rh)
        }
        Some(_) => return Err("--region starts outside the rendered page".into()),
        None => (bmp.rgba, bmp.width, bmp.height),
    };

    image::save_buffer(output, &rgba, w, h, image::ColorType::Rgba8)?;
    println!("wrote {output} ({w}x{h} @ {dpi}dpi)");
    Ok(ExitCode::SUCCESS)
}

fn flag_str<'a>(args: &'a [String], flag: &str) -> Result<Option<&'a str>, String> {
    let mut positions = args
        .iter()
        .enumerate()
        .filter_map(|(index, arg)| (arg == flag).then_some(index));
    let Some(index) = positions.next() else {
        return Ok(None);
    };
    if positions.next().is_some() {
        return Err(format!("{flag} may only be specified once"));
    }
    args.get(index + 1)
        .map(|value| Some(value.as_str()))
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn parse_region(s: &str) -> Option<(u32, u32, u32, u32)> {
    let values: Vec<u32> = s
        .split(',')
        .map(|token| token.trim().parse())
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    match values.as_slice() {
        &[x, y, width, height] if width > 0 && height > 0 => Some((x, y, width, height)),
        _ => None,
    }
}

fn crop(rgba: &[u8], w: u32, h: u32, x: u32, y: u32, cw: u32, ch: u32) -> (Vec<u8>, u32, u32) {
    let cw = cw.min(w.saturating_sub(x));
    let ch = ch.min(h.saturating_sub(y));
    let mut out = Vec::with_capacity((cw * ch * 4) as usize);
    for row in y..y + ch {
        let start = ((row * w + x) * 4) as usize;
        out.extend_from_slice(&rgba[start..start + (cw * 4) as usize]);
    }
    (out, cw, ch)
}

fn read_ofd(path: &str) -> std::io::Result<Vec<u8>> {
    let file = std::fs::File::open(path)?;
    let declared = file.metadata()?.len();
    read_limited(
        file,
        declared,
        ofd_core::container::ContainerLimits::default().max_archive_bytes,
    )
}

fn read_limited<R: Read>(reader: R, declared: u64, limit: u64) -> std::io::Result<Vec<u8>> {
    if declared > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("input declares {declared} bytes; limit is {limit}"),
        ));
    }
    let initial_capacity = usize::try_from(declared.min(limit).min(1024 * 1024)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(initial_capacity);
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("input exceeds the {limit} byte limit"),
        ));
    }
    Ok(bytes)
}

/// Load deterministic bundled fonts plus the small subset of system fonts the
/// current document can use. File-system discovery belongs to this native host,
/// while `ofd-core` receives only font bytes.
fn load_fallback_fonts(doc: &ofd_core::Document) -> Vec<Arc<Vec<u8>>> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ofd-core/assets/fonts");
    let mut fonts = Vec::new();
    let mut font_bytes = 0u64;
    for data in [
        "simsun.ttf",
        "simhei.ttf",
        "simkai.ttf",
        "SIMFANG.TTF",
        "xbst.ttf",
    ]
    .iter()
    .filter_map(|name| std::fs::read(dir.join(name)).ok())
    {
        push_fallback_font(&mut fonts, &mut font_bytes, data);
    }

    let mut database = fontdb::Database::new();
    database.load_system_fonts();
    let common_families = [
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
        "STKaiti",
        "Kaiti SC",
        "STFangsong",
        "Fangsong SC",
    ];

    let mut queried = std::collections::HashSet::new();
    let mut seen_faces = std::collections::HashSet::new();
    for family in common_families
        .into_iter()
        .chain(doc.resources.fonts.iter().map(|font| font.family()))
    {
        let key = family.trim().to_lowercase();
        if !queried.insert(key) {
            continue;
        }
        if queried.len() > MAX_FONT_FAMILY_QUERIES {
            break;
        }
        let Some(id) = database.query(&fontdb::Query {
            families: &[fontdb::Family::Name(family)],
            ..Default::default()
        }) else {
            continue;
        };
        if seen_faces.insert(id) {
            let data = database
                .with_face_data(id, |data, _| {
                    fallback_font_fits(fonts.len(), font_bytes, data.len()).then(|| data.to_vec())
                })
                .flatten();
            if let Some(data) = data {
                push_fallback_font(&mut fonts, &mut font_bytes, data);
            }
        }
    }
    fonts
}

fn fallback_font_fits(font_count: usize, total: u64, data_len: usize) -> bool {
    let Ok(bytes) = u64::try_from(data_len) else {
        return false;
    };
    font_count < MAX_FALLBACK_FONT_FACES
        && total
            .checked_add(bytes)
            .is_some_and(|next| next <= MAX_FALLBACK_FONT_BYTES)
}

fn push_fallback_font(fonts: &mut Vec<Arc<Vec<u8>>>, total: &mut u64, data: Vec<u8>) {
    if !fallback_font_fits(fonts.len(), *total, data.len()) {
        return;
    }
    let Ok(bytes) = u64::try_from(data.len()) else {
        return;
    };
    *total += bytes;
    fonts.push(Arc::new(data));
}

fn flag_value(args: &[String], flag: &str) -> Result<Option<f32>, String> {
    let Some(value) = flag_str(args, flag)? else {
        return Ok(None);
    };
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite())
        .map(Some)
        .ok_or_else(|| format!("{flag} requires a finite number"))
}

fn flag_usize(args: &[String], flag: &str) -> Result<Option<usize>, String> {
    let Some(value) = flag_str(args, flag)? else {
        return Ok(None);
    };
    value
        .parse::<usize>()
        .map(Some)
        .map_err(|_| format!("{flag} requires a non-negative integer"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn bounded_input_read_rejects_declared_and_streamed_oversize() {
        assert!(read_limited(Cursor::new(b"small"), 6, 5).is_err());
        assert!(read_limited(Cursor::new(b"123456"), 0, 5).is_err());
        assert_eq!(read_limited(Cursor::new(b"12345"), 5, 5).unwrap(), b"12345");
    }

    #[test]
    fn fallback_font_budget_is_cumulative() {
        let mut fonts = Vec::new();
        let mut total = MAX_FALLBACK_FONT_BYTES - 2;
        push_fallback_font(&mut fonts, &mut total, vec![0; 2]);
        push_fallback_font(&mut fonts, &mut total, vec![0; 1]);
        assert_eq!(fonts.len(), 1);
        assert_eq!(total, MAX_FALLBACK_FONT_BYTES);
    }

    #[test]
    fn region_and_float_flags_reject_invalid_or_non_finite_fields() {
        assert_eq!(parse_region("1,2,3,4"), Some((1, 2, 3, 4)));
        assert_eq!(parse_region("1,bad,2,3,4"), None);
        assert_eq!(parse_region("1,2,3"), None);
        assert_eq!(parse_region("1,2,0,4"), None);

        let args = vec!["ofd-cli".into(), "--dpi".into(), "NaN".into()];
        assert!(flag_value(&args, "--dpi").is_err());

        let args = vec!["ofd-cli".into(), "--page".into(), "1.9".into()];
        assert!(flag_usize(&args, "--page").is_err());

        let args = vec![
            "ofd-cli".into(),
            "--dpi".into(),
            "96".into(),
            "--dpi".into(),
            "144".into(),
        ];
        assert!(flag_value(&args, "--dpi").is_err());
    }

    #[test]
    fn signature_parse_failures_block_verification_but_appearance_warnings_do_not() {
        assert!(warning_blocks_verification(
            "Signatures Doc_0/Signs.xml: unreadable"
        ));
        assert!(warning_blocks_verification(
            "Signature Doc_0/Sign.xml: malformed xml"
        ));
        assert!(!warning_blocks_verification(
            "Seal appearance for Signature Doc_0/Sign.xml: no renderable seal picture"
        ));
        assert!(!warning_blocks_verification(
            "unresolved image resource id 7"
        ));
    }
}
