use std::{
    collections::{HashMap, HashSet},
    fs::{File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, RwLock,
    },
};

use image::ImageEncoder;
use ofd_core::{
    model::{OutlineItem, SignatureType},
    render::{RenderOptions, RenderSession},
    OfdPackage,
};
use printpdf::{
    ImageCompression, ImageOptimizationOptions, Mm, Op, PdfDocument, PdfPage, PdfSaveOptions,
    RawImage, RawImageData, RawImageFormat, XObjectTransform,
};
use serde::Serialize;
use tauri::{ipc::Response, Emitter, Manager, State};

const MAX_FONT_FAMILY_QUERIES: usize = 4096;
const MAX_FALLBACK_FONT_FACES: usize = 256;
const MAX_FALLBACK_FONT_BYTES: u64 = 256 * 1024 * 1024;
const MAX_EXPORT_PIXELS: f64 = 50_000_000.0;
const MIN_EXPORT_DPI: f32 = 96.0;
const MAX_EXPORT_DPI: f32 = 300.0;
const MAX_RENDER_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MM_PER_INCH: f32 = ofd_core::geom::MM_PER_INCH;

static TEMP_FILE_COUNTER: AtomicU64 = AtomicU64::new(0);
static BUNDLED_FONTS: OnceLock<Vec<Arc<Vec<u8>>>> = OnceLock::new();

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct RenderCacheKey {
    page_index: usize,
    dpi_bits: u32,
}

#[derive(Default)]
struct RenderCache {
    pages: HashMap<RenderCacheKey, Arc<Vec<u8>>>,
    bytes: usize,
}

struct LoadedDocument {
    file_path: PathBuf,
    file_size: u64,
    bytes: Vec<u8>,
    package: OfdPackage,
    fallback_fonts: Vec<Arc<Vec<u8>>>,
    render_cache: Mutex<RenderCache>,
}

impl LoadedDocument {
    fn document(&self) -> Result<&ofd_core::Document, String> {
        self.package
            .documents
            .first()
            .ok_or_else(|| "OFD package contains no documents".to_string())
    }

    fn page_area(&self, page_index: usize) -> Result<ofd_core::geom::Rect, String> {
        let document = self.document()?;
        let page = document
            .pages
            .get(page_index)
            .ok_or_else(|| format!("no page {page_index}"))?;
        page.area
            .unwrap_or(document.page_area)
            .render_box()
            .ok_or_else(|| format!("page {} has no physical area", page_index + 1))
    }
}

struct AppState {
    current: RwLock<Option<Arc<LoadedDocument>>>,
    launch_path: Mutex<Option<PathBuf>>,
    load_generation: AtomicU64,
}

impl AppState {
    fn new() -> Self {
        Self {
            current: RwLock::new(None),
            launch_path: Mutex::new(launch_document_path()),
            load_generation: AtomicU64::new(0),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PageSummary {
    index: usize,
    width_mm: f32,
    height_mm: f32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutlineNode {
    title: String,
    page_index: Option<usize>,
    children: Vec<OutlineNode>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentSummary {
    file_path: String,
    file_name: String,
    file_size: u64,
    version: Option<String>,
    doc_type: Option<String>,
    page_count: usize,
    title: Option<String>,
    author: Option<String>,
    subject: Option<String>,
    creator: Option<String>,
    creation_date: Option<String>,
    doc_id: Option<String>,
    warning_count: usize,
    signature_count: usize,
    seal_count: usize,
    annotation_count: usize,
    warnings: Vec<String>,
    pages: Vec<PageSummary>,
    outline: Vec<OutlineNode>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReferenceReportDto {
    file_ref: String,
    method: String,
    status: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SignatureReportDto {
    id: String,
    signature_type: &'static str,
    provider: Option<String>,
    signature_method: Option<String>,
    signature_date_time: Option<String>,
    integrity_ok: bool,
    references: Vec<ReferenceReportDto>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportProgress {
    current: usize,
    total: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExportResult {
    path: String,
    page_count: usize,
    file_size: u64,
}

fn launch_document_path() -> Option<PathBuf> {
    std::env::args_os().skip(1).map(PathBuf::from).find(|path| {
        path.extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| extension.eq_ignore_ascii_case("ofd"))
    })
}

fn current_document(state: &AppState) -> Result<Arc<LoadedDocument>, String> {
    state
        .current
        .read()
        .map_err(|_| "document state is unavailable".to_string())?
        .clone()
        .ok_or_else(|| "no OFD document is open".to_string())
}

async fn load_into_state(
    path: PathBuf,
    resource_dir: Option<PathBuf>,
    state: &AppState,
) -> Result<DocumentSummary, String> {
    let generation = state.load_generation.fetch_add(1, Ordering::Relaxed) + 1;
    let loaded = tauri::async_runtime::spawn_blocking(move || load_document(path, resource_dir))
        .await
        .map_err(|error| format!("document loading task failed: {error}"))??;
    if state.load_generation.load(Ordering::Relaxed) != generation {
        return Err("document load was superseded by another open request".to_string());
    }
    let summary = summarize_document(&loaded)?;
    *state
        .current
        .write()
        .map_err(|_| "document state is unavailable".to_string())? = Some(Arc::new(loaded));
    Ok(summary)
}

#[tauri::command]
async fn open_document(
    path: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<DocumentSummary, String> {
    load_into_state(
        PathBuf::from(path),
        app.path().resource_dir().ok(),
        state.inner(),
    )
    .await
}

#[tauri::command]
async fn open_launch_document(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<DocumentSummary>, String> {
    let path = state
        .launch_path
        .lock()
        .map_err(|_| "launch document state is unavailable".to_string())?
        .take();
    match path {
        Some(path) => load_into_state(path, app.path().resource_dir().ok(), state.inner())
            .await
            .map(Some),
        None => Ok(None),
    }
}

#[tauri::command]
fn close_document(state: State<'_, AppState>) -> Result<(), String> {
    state.load_generation.fetch_add(1, Ordering::Relaxed);
    *state
        .current
        .write()
        .map_err(|_| "document state is unavailable".to_string())? = None;
    Ok(())
}

#[tauri::command]
async fn render_page(
    page_index: usize,
    dpi: f32,
    state: State<'_, AppState>,
) -> Result<Response, String> {
    if !dpi.is_finite() || !(36.0..=300.0).contains(&dpi) {
        return Err("render DPI must be between 36 and 300".to_string());
    }

    let loaded = current_document(state.inner())?;
    let cache_key = RenderCacheKey {
        page_index,
        dpi_bits: dpi.to_bits(),
    };
    if let Some(png) = cached_page(&loaded, cache_key) {
        return Ok(Response::new(png));
    }
    let render_loaded = loaded.clone();
    let png = tauri::async_runtime::spawn_blocking(move || {
        let document = render_loaded.document()?;
        let options = RenderOptions {
            fallback_fonts: render_loaded.fallback_fonts.clone(),
            ..Default::default()
        };
        let bitmap = ofd_core::render::render_page_with(document, page_index, dpi, &options)
            .map_err(|error| error.to_string())?;
        encode_png(bitmap)
    })
    .await
    .map_err(|error| format!("page render task failed: {error}"))??;

    cache_page(&loaded, cache_key, &png);
    Ok(Response::new(png))
}

fn cached_page(loaded: &LoadedDocument, key: RenderCacheKey) -> Option<Vec<u8>> {
    loaded
        .render_cache
        .lock()
        .ok()
        .and_then(|cache| cache.pages.get(&key).map(|png| png.as_ref().clone()))
}

fn cache_page(loaded: &LoadedDocument, key: RenderCacheKey, png: &[u8]) {
    if png.len() > MAX_RENDER_CACHE_BYTES {
        return;
    }
    let Ok(mut cache) = loaded.render_cache.lock() else {
        return;
    };
    if cache.bytes.saturating_add(png.len()) > MAX_RENDER_CACHE_BYTES {
        cache.pages.clear();
        cache.bytes = 0;
    }
    if let Some(previous) = cache.pages.remove(&key) {
        cache.bytes = cache.bytes.saturating_sub(previous.len());
    }
    let bytes = Arc::new(png.to_vec());
    cache.bytes += bytes.len();
    cache.pages.insert(key, bytes);
}

#[tauri::command]
async fn verify_document(state: State<'_, AppState>) -> Result<Vec<SignatureReportDto>, String> {
    let loaded = current_document(state.inner())?;
    tauri::async_runtime::spawn_blocking(move || {
        let document = loaded.document()?;
        let reports = ofd_core::sign::verify(loaded.bytes.clone(), &document.signatures)
            .map_err(|error| error.to_string())?;

        Ok(reports
            .into_iter()
            .map(|report| {
                let integrity_ok = report.integrity_ok();
                SignatureReportDto {
                    id: report.id,
                    signature_type: match report.sig_type {
                        SignatureType::Seal => "seal",
                        SignatureType::Sign => "sign",
                    },
                    provider: report.provider,
                    signature_method: report.signature_method,
                    signature_date_time: report.signature_date_time,
                    integrity_ok,
                    references: report
                        .references
                        .into_iter()
                        .map(|reference| ReferenceReportDto {
                            file_ref: reference.file_ref,
                            method: reference.method,
                            status: digest_status_name(reference.status),
                        })
                        .collect(),
                }
            })
            .collect())
    })
    .await
    .map_err(|error| format!("signature verification task failed: {error}"))?
}

#[tauri::command]
async fn export_pdf(
    path: String,
    dpi: f32,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ExportResult, String> {
    if !dpi.is_finite() || !(MIN_EXPORT_DPI..=MAX_EXPORT_DPI).contains(&dpi) {
        return Err(format!(
            "export DPI must be between {MIN_EXPORT_DPI} and {MAX_EXPORT_DPI}"
        ));
    }

    let loaded = current_document(state.inner())?;
    let output = normalized_pdf_path(PathBuf::from(path));
    tauri::async_runtime::spawn_blocking(move || {
        export_loaded_pdf(loaded, output, dpi, |progress| {
            let _ = app.emit("export-progress", progress);
        })
    })
    .await
    .map_err(|error| format!("PDF export task failed: {error}"))?
}

fn load_document(path: PathBuf, resource_dir: Option<PathBuf>) -> Result<LoadedDocument, String> {
    let canonical_path = path.canonicalize().unwrap_or(path);
    let file_size = canonical_path
        .metadata()
        .map_err(|error| format!("cannot read document metadata: {error}"))?
        .len();
    let bytes = read_limited(&canonical_path)?;
    let package = ofd_core::open(bytes.clone()).map_err(|error| error.to_string())?;
    let document = package
        .documents
        .first()
        .ok_or_else(|| "OFD package contains no documents".to_string())?;
    let fallback_fonts = load_fallback_fonts(document, resource_dir.as_deref());

    Ok(LoadedDocument {
        file_path: canonical_path,
        file_size,
        bytes,
        package,
        fallback_fonts,
        render_cache: Mutex::new(RenderCache::default()),
    })
}

fn read_limited(path: &Path) -> Result<Vec<u8>, String> {
    let limit = ofd_core::container::ContainerLimits::default().max_archive_bytes;
    let file = File::open(path).map_err(|error| format!("cannot open document: {error}"))?;
    let declared = file
        .metadata()
        .map_err(|error| format!("cannot read document metadata: {error}"))?
        .len();
    if declared > limit {
        return Err(format!(
            "document declares {declared} bytes; limit is {limit}"
        ));
    }

    let mut bytes = Vec::with_capacity(
        usize::try_from(declared.min(limit).min(1024 * 1024)).unwrap_or_default(),
    );
    file.take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read document: {error}"))?;
    if bytes.len() as u64 > limit {
        return Err(format!("document exceeds the {limit} byte limit"));
    }
    Ok(bytes)
}

fn summarize_document(loaded: &LoadedDocument) -> Result<DocumentSummary, String> {
    let document = loaded.document()?;
    let pages = document
        .pages
        .iter()
        .enumerate()
        .map(|(index, _page)| {
            let area = loaded.page_area(index)?;
            Ok(PageSummary {
                index,
                width_mm: area.w,
                height_mm: area.h,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let metadata = &document.metadata;

    Ok(DocumentSummary {
        file_path: loaded.file_path.to_string_lossy().into_owned(),
        file_name: loaded
            .file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document.ofd".to_string()),
        file_size: loaded.file_size,
        version: loaded.package.version.clone(),
        doc_type: loaded.package.doc_type.clone(),
        page_count: pages.len(),
        title: metadata.title.clone(),
        author: metadata.author.clone(),
        subject: metadata.subject.clone(),
        creator: metadata.creator.clone(),
        creation_date: metadata.creation_date.clone(),
        doc_id: metadata.doc_id.clone(),
        warning_count: document.warnings.len(),
        signature_count: document.signatures.len(),
        seal_count: document.seals.len(),
        annotation_count: document.annotations.len(),
        warnings: document.warnings.clone(),
        pages,
        outline: document.outline.iter().map(outline_node).collect(),
    })
}

fn outline_node(item: &OutlineItem) -> OutlineNode {
    OutlineNode {
        title: item.title.clone(),
        page_index: item.page_index,
        children: item.children.iter().map(outline_node).collect(),
    }
}

fn encode_png(bitmap: ofd_core::Bitmap) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            &bitmap.rgba,
            bitmap.width,
            bitmap.height,
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| format!("cannot encode rendered page: {error}"))?;
    Ok(png)
}

fn export_loaded_pdf<F>(
    loaded: Arc<LoadedDocument>,
    output: PathBuf,
    requested_dpi: f32,
    mut on_progress: F,
) -> Result<ExportResult, String>
where
    F: FnMut(ExportProgress),
{
    let document = loaded.document()?;
    if document.pages.is_empty() {
        return Err("OFD document contains no pages".to_string());
    }

    let dpi = constrained_export_dpi(document, requested_dpi)?;
    let title = document
        .metadata
        .title
        .as_deref()
        .or_else(|| loaded.file_path.file_stem().and_then(|name| name.to_str()))
        .unwrap_or("OFD document");
    let mut pdf = PdfDocument::new(title);
    let options = RenderOptions {
        fallback_fonts: loaded.fallback_fonts.clone(),
        ..Default::default()
    };
    let mut renderer = RenderSession::new(document, options);
    let total = document.pages.len();
    let mut pages = Vec::with_capacity(total);

    for (index, _page) in document.pages.iter().enumerate() {
        let area = loaded.page_area(index)?;
        let bitmap = renderer
            .render_page(index, dpi)
            .map_err(|error| format!("cannot render page {}: {error}", index + 1))?;
        let scale_x = (area.w * dpi / MM_PER_INCH) / bitmap.width as f32;
        let scale_y = (area.h * dpi / MM_PER_INCH) / bitmap.height as f32;
        let image = RawImage {
            pixels: RawImageData::U8(rgba_to_rgb(&bitmap.rgba)),
            width: bitmap.width as usize,
            height: bitmap.height as usize,
            data_format: RawImageFormat::RGB8,
            tag: Vec::new(),
        };
        let image_id = pdf.add_image(&image);
        pages.push(PdfPage::new(
            Mm(area.w),
            Mm(area.h),
            vec![Op::UseXobject {
                id: image_id,
                transform: XObjectTransform {
                    scale_x: Some(scale_x),
                    scale_y: Some(scale_y),
                    dpi: Some(dpi),
                    ..Default::default()
                },
            }],
        ));
        on_progress(ExportProgress {
            current: index + 1,
            total,
        });
    }

    pdf.with_pages(pages);
    let save_options = PdfSaveOptions {
        image_optimization: Some(ImageOptimizationOptions {
            quality: Some(0.94),
            max_image_size: None,
            auto_optimize: Some(false),
            format: Some(ImageCompression::Jpeg),
            ..Default::default()
        }),
        ..Default::default()
    };
    let file_size = write_pdf_atomic(&output, &pdf, &save_options)?;

    Ok(ExportResult {
        path: output.to_string_lossy().into_owned(),
        page_count: total,
        file_size,
    })
}

fn write_pdf_atomic(
    path: &Path,
    pdf: &PdfDocument,
    options: &PdfSaveOptions,
) -> Result<u64, String> {
    atomic_write(path, |file| {
        pdf.save_writer(file, options, &mut Vec::new());
        Ok(())
    })
}

fn atomic_write<F>(path: &Path, writer: F) -> Result<u64, String>
where
    F: FnOnce(&mut File) -> Result<(), String>,
{
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let counter = TEMP_FILE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".ofd-manager-{}-{}.tmp",
        std::process::id(),
        counter
    ));
    let write_result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(|error| format!("cannot create temporary PDF file: {error}"))?;
        writer(&mut file)?;
        file.sync_all()
            .map_err(|error| format!("cannot finish PDF file: {error}"))?;
        let size = file
            .metadata()
            .map_err(|error| format!("cannot inspect temporary PDF file: {error}"))?
            .len();
        Ok::<_, String>(size)
    })();

    let size = match write_result {
        Ok(size) => size,
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }
    };

    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)
            .map_err(|error| format!("cannot replace existing PDF file: {error}"))?;
    }
    if let Err(error) = std::fs::rename(&temporary, path) {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("cannot finalize PDF file: {error}"));
    }
    Ok(size)
}

fn constrained_export_dpi(document: &ofd_core::Document, requested: f32) -> Result<f32, String> {
    let total_area = document.pages.iter().try_fold(0.0_f64, |sum, page| {
        let area = page
            .area
            .unwrap_or(document.page_area)
            .render_box()
            .ok_or_else(|| "a page has no physical area".to_string())?;
        Ok::<_, String>(sum + f64::from(area.w) * f64::from(area.h))
    })?;
    let pixels = total_area * f64::from(requested / MM_PER_INCH).powi(2);
    if pixels <= MAX_EXPORT_PIXELS {
        return Ok(requested);
    }

    let constrained = f64::from(requested) * (MAX_EXPORT_PIXELS / pixels).sqrt();
    if constrained < f64::from(MIN_EXPORT_DPI) {
        return Err(format!(
            "document is too large to export within the {} megapixel safety limit",
            (MAX_EXPORT_PIXELS / 1_000_000.0) as u64
        ));
    }
    Ok((constrained as f32).floor())
}

fn normalized_pdf_path(mut path: PathBuf) -> PathBuf {
    if !path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("pdf"))
    {
        path.set_extension("pdf");
    }
    path
}

fn rgba_to_rgb(rgba: &[u8]) -> Vec<u8> {
    let mut rgb = Vec::with_capacity(rgba.len() / 4 * 3);
    for pixel in rgba.chunks_exact(4) {
        rgb.extend_from_slice(&pixel[..3]);
    }
    rgb
}

fn digest_status_name(status: ofd_core::sign::DigestStatus) -> &'static str {
    use ofd_core::sign::DigestStatus;
    match status {
        DigestStatus::Ok => "ok",
        DigestStatus::Mismatch => "mismatch",
        DigestStatus::FileMissing => "fileMissing",
        DigestStatus::ResourceLimit => "resourceLimit",
        DigestStatus::ReadError => "readError",
        DigestStatus::UnsupportedMethod => "unsupportedMethod",
        DigestStatus::BadCheckValue => "badCheckValue",
    }
}

fn load_fallback_fonts(
    document: &ofd_core::Document,
    resource_dir: Option<&Path>,
) -> Vec<Arc<Vec<u8>>> {
    let bundled = BUNDLED_FONTS
        .get_or_init(|| load_bundled_fonts(resource_dir))
        .clone();
    let mut fonts: Vec<Arc<Vec<u8>>> = bundled;
    let mut font_bytes = fonts.iter().map(|font| font.len() as u64).sum();

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

    let mut queried = HashSet::new();
    let mut seen_faces = HashSet::new();
    for family in common_families
        .into_iter()
        .chain(document.resources.fonts.iter().map(|font| font.family()))
    {
        if !queried.insert(family.trim().to_lowercase()) {
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

fn load_bundled_fonts(resource_dir: Option<&Path>) -> Vec<Arc<Vec<u8>>> {
    let mut fonts = Vec::new();
    let mut font_bytes = 0u64;
    let manifest_dir =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../crates/ofd-core/assets/fonts");
    let directories = resource_dir
        .map(|path| path.join("fonts"))
        .into_iter()
        .chain(std::iter::once(manifest_dir));
    for directory in directories {
        for name in [
            "simsun.ttf",
            "simhei.ttf",
            "simkai.ttf",
            "SIMFANG.TTF",
            "xbst.ttf",
        ] {
            let Ok(data) = std::fs::read(directory.join(name)) else {
                continue;
            };
            if fonts
                .iter()
                .any(|font: &Arc<Vec<u8>>| font.as_ref().as_slice() == data.as_slice())
            {
                continue;
            }
            push_fallback_font(&mut fonts, &mut font_bytes, data);
        }
        if !fonts.is_empty() {
            break;
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            open_document,
            open_launch_document,
            close_document,
            render_page,
            verify_document,
            export_pdf
        ])
        .build(tauri::generate_context!())
        .expect("failed to build OFD Manager");

    app.run(|_app, _event| {
        #[cfg(target_os = "macos")]
        if let tauri::RunEvent::Opened { urls } = _event {
            if let Some(path) = urls
                .iter()
                .filter_map(|url| url.to_file_path().ok())
                .find(|path| {
                    path.extension()
                        .and_then(|extension| extension.to_str())
                        .is_some_and(|extension| extension.eq_ignore_ascii_case("ofd"))
                })
            {
                if let Ok(mut launch_path) = _app.state::<AppState>().launch_path.lock() {
                    *launch_path = Some(path.clone());
                }
                let _ = _app.emit("open-document-request", path.to_string_lossy().into_owned());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pdf_path_is_normalized_without_changing_existing_extension_case() {
        assert_eq!(
            normalized_pdf_path(PathBuf::from("/tmp/invoice")),
            PathBuf::from("/tmp/invoice.pdf")
        );
        assert_eq!(
            normalized_pdf_path(PathBuf::from("/tmp/invoice.PDF")),
            PathBuf::from("/tmp/invoice.PDF")
        );
    }

    #[test]
    fn opaque_rgba_is_compacted_for_pdf_images() {
        assert_eq!(
            rgba_to_rgb(&[1, 2, 3, 255, 4, 5, 6, 255]),
            vec![1, 2, 3, 4, 5, 6]
        );
    }

    #[test]
    fn fixture_exports_to_a_readable_image_pdf() {
        struct Cleanup(PathBuf);

        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }

        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/helloworld.ofd");
        let output =
            std::env::temp_dir().join(format!("ofd-manager-export-{}.pdf", std::process::id()));
        let _cleanup = Cleanup(output.clone());
        let loaded = Arc::new(load_document(fixture, None).unwrap());
        let mut progress = Vec::new();
        let result = export_loaded_pdf(loaded, output.clone(), 96.0, |event| {
            progress.push((event.current, event.total));
        })
        .unwrap();

        let bytes = std::fs::read(output).unwrap();
        let parsed = PdfDocument::parse(
            &bytes,
            &printpdf::PdfParseOptions::default(),
            &mut Vec::new(),
        )
        .unwrap();
        assert_eq!(result.page_count, 1);
        assert_eq!(parsed.pages.len(), 1);
        assert_eq!(progress, vec![(1, 1)]);
    }
}
