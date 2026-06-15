//! `ofd-cli` — render and inspect OFD files.
//!
//! Usage:
//!   `ofd-cli render <input.ofd> <output.png> [--dpi N] [--page I] [--region x,y,w,h] [--stem F] [--strict]`
//!   `ofd-cli verify <input.ofd>`   — check signature file-digest integrity

use std::process::ExitCode;

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
    let bytes = std::fs::read(input)?;
    let pkg = ofd_core::open(bytes.clone())?;

    let mut all_ok = true;
    let mut any = false;
    for doc in &pkg.documents {
        let reports = ofd_core::sign::verify(bytes.clone(), &doc.signatures);
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
    }
    if !any {
        println!("no signatures in document");
        return Ok(ExitCode::SUCCESS);
    }
    println!(
        "\nNote: this checks file-digest integrity only; cryptographic \
         authenticity (SM2 signature + certificate) is not verified."
    );
    Ok(if all_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn run(args: &[String]) -> Result<ExitCode, Box<dyn std::error::Error>> {
    if args.len() < 4 || args[1] != "render" {
        return Err("usage: ofd-cli render <input.ofd> <output.png> [--dpi N] [--page I]".into());
    }
    let input = &args[2];
    let output = &args[3];
    let dpi = flag_value(args, "--dpi").unwrap_or(144.0);
    let page_index = flag_value(args, "--page").unwrap_or(0.0) as usize;

    let bytes = std::fs::read(input)?;
    let pkg = ofd_core::open(bytes)?;
    let doc = pkg
        .documents
        .first()
        .ok_or("OFD package contains no documents")?;

    // Surface non-fatal parse problems (missing/malformed referenced resources).
    // `--strict` turns them into a hard failure so corrupted OFDs don't render
    // as silently-incomplete "successes".
    if !doc.warnings.is_empty() {
        for w in &doc.warnings {
            eprintln!("warning: {w}");
        }
        if args.iter().any(|a| a == "--strict") {
            return Err(format!("{} parse warning(s) with --strict", doc.warnings.len()).into());
        }
    }

    let opts = ofd_core::render::RenderOptions {
        fallback_fonts: load_bundled_fonts(),
        text_stem_darkening: flag_value(args, "--stem")
            .unwrap_or(ofd_core::render::DEFAULT_STEM_DARKENING),
        ..Default::default()
    };
    let bmp = ofd_core::render::render_page_with(doc, page_index, dpi, &opts)?;

    // Optional crop, for inspecting a sub-region: --region x,y,w,h (pixels).
    let (rgba, w, h) = match flag_str(args, "--region").and_then(parse_region) {
        Some((x, y, rw, rh)) => crop(&bmp.rgba, bmp.width, bmp.height, x, y, rw, rh),
        None => (bmp.rgba, bmp.width, bmp.height),
    };

    image::save_buffer(output, &rgba, w, h, image::ColorType::Rgba8)?;
    println!("wrote {output} ({w}x{h} @ {dpi}dpi)");
    Ok(ExitCode::SUCCESS)
}

fn flag_str<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(|s| s.as_str())
}

fn parse_region(s: &str) -> Option<(u32, u32, u32, u32)> {
    let v: Vec<u32> = s.split(',').filter_map(|t| t.trim().parse().ok()).collect();
    (v.len() == 4).then(|| (v[0], v[1], v[2], v[3]))
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

/// Load the deterministic fallback fonts from the in-repo assets dir, if present
/// (run `scripts/fetch-fonts.sh` to populate it). Missing fonts are not fatal —
/// rendering falls back to system fonts.
fn load_bundled_fonts() -> Vec<Vec<u8>> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ofd-core/assets/fonts");
    [
        "simsun.ttf",
        "simhei.ttf",
        "simkai.ttf",
        "SIMFANG.TTF",
        "xbst.ttf",
    ]
    .iter()
    .filter_map(|name| std::fs::read(dir.join(name)).ok())
    .collect()
}

fn flag_value(args: &[String], flag: &str) -> Option<f32> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
}
