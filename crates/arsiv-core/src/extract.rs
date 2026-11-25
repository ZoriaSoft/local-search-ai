//! Per-format text extraction.
//!
//! PDF: each page's text layer is used when it carries real text; otherwise the
//! page is rendered at 300 DPI and OCR'd (handles scanned and mixed PDFs).
//! Images: OCR. DOCX: document/headers/footers XML. XLSX: every sheet's cells.

use std::io::{BufRead, Read, Seek};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use parking_lot::{Condvar, Mutex, RwLock};

use anyhow::{anyhow, bail, Context, Result};
use image::{GrayImage, RgbImage};
use pdfium_render::prelude::*;
use quick_xml::events::Event;
use quick_xml::Reader;

use crate::model::{ExtractedDocument, FileKind, PageText, TextSource};
use crate::ocr::{is_cancelled, Cancelled, OcrConfig};

/// A page needs OCR when its text layer has fewer word characters than this.
const MIN_TEXT_LAYER_CHARS: usize = 20;
/// Render DPI for scanned PDF pages, matching the lab corpus (300 DPI).
const RENDER_DPI: f32 = 300.0;
/// Upper bound on rendered page edge (pixels) to cap memory on huge pages.
const MAX_RENDER_EDGE: i32 = 7000;
/// Plain-text files above this are reported as failures rather than loaded
/// whole into one preview page (logs and data dumps, not documents).
pub const MAX_TEXT_FILE_BYTES: u64 = 16 * 1024 * 1024;
/// Bounds apply to decoded pixels, not the compressed image size. Rotation
/// and OCR conversions can each need another image-sized allocation.
pub(crate) const MAX_IMAGE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_IMAGE_DECODE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_IMAGE_EDGE: u32 = 20_000;
const MAX_IMAGE_PIXELS: u64 = 50_000_000;
pub(crate) const MAX_OFFICE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OFFICE_ENTRIES: usize = 4096;
const MAX_SHARED_STRINGS: u64 = 100_000;
const MAX_SHEET_CELLS: u64 = 250_000;
const MAX_WORKBOOK_CELLS: u64 = 1_000_000;
const MAX_WORKBOOK_SHEETS: usize = 256;

#[derive(Clone, Copy)]
struct OfficeLimits {
    member_bytes: usize,
    expanded_bytes: usize,
    text_bytes: usize,
}

const OFFICE_LIMITS: OfficeLimits = OfficeLimits {
    member_bytes: 8 * 1024 * 1024,
    expanded_bytes: 32 * 1024 * 1024,
    text_bytes: 16 * 1024 * 1024,
};
/// Cancellation flag for callers that never cancel.
static NEVER: AtomicBool = AtomicBool::new(false);

/// Counting semaphore for Tesseract processes. One budget is shared by every
/// file and every page being extracted, so the machine never runs more than
/// `capacity` OCR processes at once (a 4-core office PC keeps one core free).
pub struct OcrBudget {
    capacity: usize,
    free: Mutex<usize>,
    released: Condvar,
}

pub struct OcrSlot<'a>(&'a OcrBudget);

impl OcrBudget {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self { capacity, free: Mutex::new(capacity), released: Condvar::new() }
    }

    /// `max(1, cores - 1)`.
    pub fn default_capacity() -> usize {
        std::thread::available_parallelism().map_or(1, |n| n.get().saturating_sub(1)).max(1)
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn acquire(&self) -> OcrSlot<'_> {
        let mut free = self.free.lock();
        while *free == 0 {
            self.released.wait(&mut free);
        }
        *free -= 1;
        OcrSlot(self)
    }
}

impl Drop for OcrSlot<'_> {
    fn drop(&mut self) {
        *self.0.free.lock() += 1;
        self.0.released.notify_one();
    }
}

pub struct Extractor {
    pdfium: Option<Pdfium>,
    pdfium_error: Option<String>,
    /// Current OCR setup. Replaced whole when the document language
    /// changes; a running extraction keeps the snapshot it started with, so
    /// one document is never read with two languages.
    ocr: RwLock<Arc<OcrSetup>>,
    budget: OcrBudget,
    /// Level skewed scans before OCR (`deskew.rs`); off unless measured useful.
    deskew: bool,
    scratch: PathBuf,
}

/// An OCR configuration plus its end-to-end probe verdict.
pub struct OcrSetup {
    pub config: OcrConfig,
    /// Result of the end-to-end OCR probe, run once on first need (status
    /// query or first scanned page) so start-up never waits on Tesseract.
    state: OnceLock<Result<String, String>>,
}

impl OcrSetup {
    fn new(config: OcrConfig) -> Arc<Self> {
        Arc::new(Self { config, state: OnceLock::new() })
    }

    fn status(&self, scratch: &Path) -> Result<String, String> {
        self.state.get_or_init(|| self.config.check(scratch).map_err(|e| format!("{e:#}"))).clone()
    }
}

impl Extractor {
    /// `ocr_jobs`: concurrent Tesseract processes; 0 = `max(1, cores - 1)`.
    pub fn new(pdfium_dir: &Path, ocr: OcrConfig, scratch: PathBuf, ocr_jobs: usize) -> Self {
        let (pdfium, pdfium_error) = match Pdfium::bind_to_library(
            Pdfium::pdfium_platform_library_name_at_path(pdfium_dir),
        ) {
            Ok(bindings) => (Some(Pdfium::new(bindings)), None),
            // Bindings are process-global: a second extractor (engine
            // re-opened, tests) shares them. `default()` takes exactly that
            // branch when they already exist.
            Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => (Some(Pdfium::default()), None),
            Err(error) => (None, Some(format!("pdfium could not be loaded: {error}"))),
        };
        let jobs = if ocr_jobs == 0 { OcrBudget::default_capacity() } else { ocr_jobs };
        Self { pdfium, pdfium_error, ocr: RwLock::new(OcrSetup::new(ocr)), budget: OcrBudget::new(jobs), deskew: false, scratch }
    }

    /// Enables skew correction of OCR input (PDF renders and image files).
    pub fn with_deskew(mut self, enabled: bool) -> Self {
        self.deskew = enabled;
        self
    }

    /// Size of the shared OCR budget (also the useful number of file workers).
    pub fn ocr_jobs(&self) -> usize {
        self.budget.capacity()
    }

    pub fn pdf_status(&self) -> Result<(), String> {
        self.pdfium_error.clone().map_or(Ok(()), Err)
    }

    /// Cached OCR readiness (see `OcrConfig::check`).
    pub fn ocr_status(&self) -> Result<String, String> {
        self.ocr_setup().status(&self.scratch)
    }

    fn ocr_setup(&self) -> Arc<OcrSetup> {
        self.ocr.read().clone()
    }

    /// Tesseract language code currently used for scanned pages.
    pub fn ocr_language(&self) -> String {
        self.ocr_setup().config.language.clone()
    }

    /// Model files the given language would need but `tessdata` lacks.
    pub fn missing_models(&self, language: &str) -> Vec<PathBuf> {
        OcrConfig { language: language.to_string(), ..self.ocr_setup().config.clone() }.missing_models()
    }

    /// Switches the document language for every extraction started from
    /// now on (the probe runs again on first need).
    pub fn set_ocr_language(&self, language: &str) {
        let mut slot = self.ocr.write();
        if slot.config.language != language {
            *slot = OcrSetup::new(OcrConfig { language: language.to_string(), ..slot.config.clone() });
        }
    }

    /// Extracts every page it can. A page that fails to render or OCR is kept
    /// as an empty `failed` page; only a document with no readable page fails.
    pub fn extract(&self, path: &Path) -> Result<ExtractedDocument> {
        self.extract_cancellable(path, &NEVER)
    }

    /// `extract` that stops when `cancel` is set: running Tesseract processes
    /// are killed and the call fails with `Cancelled` (see `is_cancelled`)
    /// instead of returning a document with pages missing. A document whose
    /// pages all finished is returned as usual.
    pub fn extract_cancellable(&self, path: &Path, cancel: &AtomicBool) -> Result<ExtractedDocument> {
        let kind = FileKind::from_path(path).ok_or_else(|| anyhow!("Unsupported file type"))?;
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let ocr = self.ocr_setup();
        // An empty file (a "Yeni Microsoft Word Belgesi.docx" never saved,
        // a 0-byte download) is a document with no text — findable by its
        // name — not an unreadable one.
        if std::fs::metadata(path).is_ok_and(|m| m.len() == 0) {
            return Ok(ExtractedDocument::new(kind, vec![PageText::new(0, String::new(), TextSource::Native, None)]));
        }
        let pages = match kind {
            FileKind::Pdf => self.pdf(path, &ocr, cancel)?,
            FileKind::Image => {
                let _slot = self.budget.acquire();
                vec![self.ocr_image_file(path, &ocr, cancel)?]
            }
            FileKind::Word => docx_cancellable(path, cancel)?,
            FileKind::Excel => xlsx_cancellable(path, cancel)?,
            FileKind::Text | FileKind::Log => txt(path)?,
        };
        if let Some(first) = pages.iter().find_map(|p| p.error.as_deref()) {
            if pages.iter().all(PageText::is_failed) {
                bail!("No page could be read: {first}");
            }
        }
        let mut document = ExtractedDocument::new(kind, pages);
        if document.ocr_pages() > 0 {
            document.ocr_language = Some(ocr.config.language.clone());
        }
        Ok(document)
    }

    fn ocr_config<'a>(&self, ocr: &'a OcrSetup) -> Result<&'a OcrConfig> {
        ocr.status(&self.scratch).map_err(|error| anyhow!("OCR unavailable: {error}"))?;
        Ok(&ocr.config)
    }

    /// Loose image: OCR'd in place, or via a levelled scratch copy when
    /// deskew is on and the scan is tilted (the user's file is never written).
    /// Phone photos usually store the picture sideways plus an EXIF
    /// "rotate" tag; Tesseract ignores the tag and read such pages as
    /// garbage, so a rotated image is OCR'd from an upright scratch copy.
    fn ocr_image_file(&self, path: &Path, ocr: &OcrSetup, cancel: &AtomicBool) -> Result<PageText> {
        self.ocr_config(ocr)?;
        let (image, rotated) = open_upright(path)?;
        if self.deskew || rotated {
            let (level, psm) = prepare_for_ocr(image.into_luma8(), self.deskew);
            if psm.is_some() || rotated {
                let file = self.save_scratch(&level)?;
                let result = self.ocr_file(&file, ocr, 0, psm, cancel);
                let _ = std::fs::remove_file(&file);
                return result;
            }
        }
        self.ocr_file(path, ocr, 0, None, cancel)
    }

    fn ocr_file(&self, path: &Path, ocr: &OcrSetup, index: usize, psm: Option<u8>, cancel: &AtomicBool) -> Result<PageText> {
        let output = self.ocr_config(ocr)?.recognize_cancellable(path, psm, Some(cancel))?;
        Ok(PageText::new(index, output.text, TextSource::Ocr, output.confidence))
    }

    /// Pages are walked and rendered in order on this thread (pdfium calls
    /// are serialized anyway); each scanned page's OCR then runs on its own
    /// thread once it holds a slot of the shared budget. The slot is taken
    /// before rendering, which also bounds how many rendered pages wait.
    /// On cancellation no further page starts, running OCR is killed and the
    /// whole document fails with `Cancelled` — never a half-read document.
    fn pdf(&self, path: &Path, ocr: &OcrSetup, cancel: &AtomicBool) -> Result<Vec<PageText>> {
        let pdfium = self.pdfium.as_ref().ok_or_else(|| {
            anyhow!("PDF support unavailable: {}", self.pdfium_error.as_deref().unwrap_or("?"))
        })?;
        let document = pdfium
            .load_pdf_from_file(path, None)
            .map_err(|e| anyhow!("Cannot open PDF: {e}"))?;
        std::thread::scope(|scope| {
            let mut pages: Vec<Option<PageText>> = Vec::new();
            let mut running = Vec::new();
            let mut cancelled = false;
            for (index, page) in document.pages().iter().enumerate() {
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
                let text = page.text().map(|t| t.all()).unwrap_or_default();
                if has_text_layer(&text) {
                    pages.push(Some(PageText::new(index, text, TextSource::TextLayer, None)));
                    continue;
                }
                if let Err(error) = self.ocr_config(ocr) {
                    pages.push(Some(PageText::failed(index, format!("{error:#}"))));
                    continue;
                }
                let slot = self.budget.acquire();
                // Waiting for a slot can outlast a cancel request.
                if cancel.load(Ordering::Relaxed) {
                    cancelled = true;
                    break;
                }
                match self.render_to_scratch(&page, index) {
                    Ok((file, psm)) => {
                        pages.push(None);
                        running.push((
                            index,
                            scope.spawn(move || {
                                let _slot = slot;
                                let result = self.ocr_file(&file, ocr, index, psm, cancel);
                                let _ = std::fs::remove_file(&file);
                                result
                            }),
                        ));
                    }
                    Err(error) => pages.push(Some(PageText::failed(index, format!("{error:#}")))),
                }
            }
            for (index, handle) in running {
                let page = match handle.join() {
                    Ok(Ok(page)) => page,
                    Ok(Err(error)) if is_cancelled(&error) => {
                        cancelled = true;
                        continue;
                    }
                    Ok(Err(error)) => PageText::failed(index, format!("{error:#}")),
                    Err(_) => PageText::failed(index, "OCR worker panicked".into()),
                };
                pages[index] = Some(page);
            }
            if cancelled {
                return Err(Cancelled.into());
            }
            Ok(pages.into_iter().flatten().collect())
        })
    }

    /// Renders a page for OCR; with deskew on, also returns the PSM chosen
    /// from the page as rendered (see `prepare_for_ocr`).
    fn render_to_scratch(&self, page: &PdfPage, index: usize) -> Result<(PathBuf, Option<u8>)> {
        let (image, psm) = prepare_for_ocr(render_page(page, index)?, self.deskew);
        Ok((self.save_scratch(&image)?, psm))
    }

    fn save_scratch(&self, image: &GrayImage) -> Result<PathBuf> {
        std::fs::create_dir_all(&self.scratch)?;
        let file = self
            .scratch
            .join(format!("page-{}-{}.png", std::process::id(), unique()));
        image.save(&file).context("Cannot write OCR scratch image")?;
        Ok(file)
    }
}

impl Extractor {
    /// A page as the user sees it (colour, `width` px wide) for the preview:
    /// pdfium render for PDFs, the image itself (scaled down) for images.
    pub fn page_image(&self, path: &Path, kind: FileKind, page: usize, width: u32) -> Result<RgbImage> {
        match kind {
            FileKind::Pdf => {
                let pdfium = self.pdfium.as_ref().ok_or_else(|| {
                    anyhow!("PDF support unavailable: {}", self.pdfium_error.as_deref().unwrap_or("?"))
                })?;
                let document = pdfium
                    .load_pdf_from_file(path, None)
                    .map_err(|e| anyhow!("Cannot open PDF: {e}"))?;
                let index = i32::try_from(page).map_err(|_| anyhow!("Page {page} out of range"))?;
                let page = document.pages().get(index).map_err(|e| anyhow!("Page {} unavailable: {e}", page + 1))?;
                let bitmap = page
                    .render_with_config(
                        &PdfRenderConfig::new()
                            .set_target_width(width as i32)
                            .set_maximum_height(MAX_RENDER_EDGE)
                            .render_form_data(true),
                    )
                    .map_err(|e| anyhow!("Cannot render page: {e}"))?;
                Ok(bitmap.as_image().map_err(|e| anyhow!("Cannot convert page: {e}"))?.into_rgb8())
            }
            FileKind::Image => {
                if page != 0 {
                    bail!("Images have a single page");
                }
                let (image, _) = open_upright(path)?;
                let image = if image.width() > width {
                    image.resize(width, u32::MAX, image::imageops::FilterType::Triangle)
                } else {
                    image
                };
                Ok(image.into_rgb8())
            }
            FileKind::Word | FileKind::Excel | FileKind::Text | FileKind::Log => bail!("No page image for {} files", kind.as_str()),
        }
    }
}

impl Extractor {
    /// Diagnostics: the skew `deskew` would correct on each page that goes
    /// to OCR (`None` for text-layer pages). Images count as one page. With
    /// `dump`, each OCR page is written there as rendered and as levelled.
    pub fn skew_estimates(&self, path: &Path, dump: Option<&Path>) -> Result<Vec<Option<f32>>> {
        let estimate = |gray: &GrayImage, page: usize| -> Result<f32> {
            let angle = crate::deskew::estimate_skew(gray);
            if let Some(dir) = dump {
                std::fs::create_dir_all(dir)?;
                gray.save(dir.join(format!("page-{}-rendered.png", page + 1)))?;
                if let Some((level, _)) = crate::deskew::deskew(gray) {
                    level.save(dir.join(format!("page-{}-deskewed.png", page + 1)))?;
                }
            }
            Ok(angle)
        };
        match FileKind::from_path(path) {
            Some(FileKind::Image) => {
                let gray = open_upright(path)?.0.into_luma8();
                Ok(vec![Some(estimate(&gray, 0)?)])
            }
            Some(FileKind::Pdf) => {
                let pdfium = self.pdfium.as_ref().ok_or_else(|| anyhow!("PDF support unavailable"))?;
                let document = pdfium.load_pdf_from_file(path, None).map_err(|e| anyhow!("Cannot open PDF: {e}"))?;
                let mut out = Vec::new();
                for (index, page) in document.pages().iter().enumerate() {
                    let text = page.text().map(|t| t.all()).unwrap_or_default();
                    out.push(if has_text_layer(&text) {
                        None
                    } else {
                        Some(estimate(&render_page(&page, index)?, index)?)
                    });
                }
                Ok(out)
            }
            _ => bail!("Only PDFs and images are OCR'd"),
        }
    }
}

/// The pixels Tesseract gets and, when they were levelled, the PSM to use.
/// The layout policy (`select_psm`) was calibrated by the lab on pages as
/// scanned; levelling turns broken skewed text lines into long horizontal
/// runs that it reads as form rules, flipping PSM 3 to 6 and costing
/// accuracy. So the mode is decided on the original, the deskewed copy is
/// only what gets recognised.
/// Decodes an image (format sniffed from its bytes, not its extension) and
/// applies its EXIF orientation; `true` when that turned or flipped it.
pub fn open_upright(path: &Path) -> Result<(image::DynamicImage, bool)> {
    use image::ImageDecoder;
    let decode = || -> Result<(image::DynamicImage, bool)> {
        let file = std::fs::File::open(path)?;
        if file.metadata()?.len() > MAX_IMAGE_FILE_BYTES {
            bail!("Image file exceeds the {} MiB input limit", MAX_IMAGE_FILE_BYTES / (1024 * 1024));
        }
        let reader = image::ImageReader::new(std::io::BufReader::new(file)).with_guessed_format()?;
        let mut decoder = checked_image_decoder(reader)?;
        let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut image = image::DynamicImage::from_decoder(decoder)?;
        let rotated = orientation != image::metadata::Orientation::NoTransforms;
        image.apply_orientation(orientation);
        Ok((image, rotated))
    };
    decode().with_context(|| format!("Cannot decode image {}", path.display()))
}

/// `into_decoder` does not reserve the output buffer against `max_alloc` as
/// `ImageReader::decode` does. Check it explicitly before `from_decoder`,
/// including when the decoder backend only implements dimensional limits.
pub(crate) fn checked_image_decoder<'a, R: BufRead + Seek + 'a>(
    mut reader: image::ImageReader<R>,
) -> Result<impl image::ImageDecoder + 'a> {
    use image::ImageDecoder;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_EDGE);
    limits.max_image_height = Some(MAX_IMAGE_EDGE);
    limits.max_alloc = Some(MAX_IMAGE_DECODE_BYTES);
    reader.limits(limits.clone());
    let mut decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    limits.check_dimensions(width, height)?;
    if u64::from(width) * u64::from(height) > MAX_IMAGE_PIXELS
        || decoder.total_bytes() > MAX_IMAGE_DECODE_BYTES
    {
        bail!("Image decode exceeds the pixel or memory budget");
    }
    limits.reserve(decoder.total_bytes())?;
    decoder.set_limits(limits)?;
    Ok(decoder)
}

fn prepare_for_ocr(page: GrayImage, deskew: bool) -> (GrayImage, Option<u8>) {
    if deskew {
        if let Some((level, _)) = crate::deskew::deskew(&page) {
            let psm = crate::ocr::select_psm(&page);
            return (level, Some(psm));
        }
    }
    (page, None)
}

fn has_text_layer(text: &str) -> bool {
    text.chars().filter(|c| c.is_alphanumeric()).count() >= MIN_TEXT_LAYER_CHARS
}

/// Renders one PDF page at OCR resolution as 8-bit grayscale.
fn render_page(page: &PdfPage, index: usize) -> Result<GrayImage> {
    let scale = RENDER_DPI / 72.0;
    let width = ((page.width().value * scale) as i32).clamp(1, MAX_RENDER_EDGE);
    let height = ((page.height().value * scale) as i32).clamp(1, MAX_RENDER_EDGE);
    let bitmap = page
        .render_with_config(
            &PdfRenderConfig::new()
                .set_target_width(width)
                .set_maximum_height(height)
                .render_form_data(true),
        )
        .map_err(|e| anyhow!("Cannot render PDF page {}: {e}", index + 1))?;
    Ok(bitmap
        .as_image()
        .map_err(|e| anyhow!("Cannot convert PDF page {}: {e}", index + 1))?
        .into_luma8())
}

fn unique() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

fn check_cancel(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        return Err(Cancelled.into());
    }
    Ok(())
}

/// Read limits count actual expanded bytes, never a ZIP's claimed size. A
/// cancellation during a read is preserved as `Cancelled`, not an IO failure.
pub(crate) fn read_bounded(mut reader: impl Read, limit: usize, cancel: Option<&AtomicBool>) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut buffer = [0u8; 16 * 1024];
    loop {
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(Cancelled.into());
        }
        let count = reader.read(&mut buffer)?;
        if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
            return Err(Cancelled.into());
        }
        if count == 0 {
            return Ok(bytes);
        }
        if count > limit.saturating_sub(bytes.len()) {
            bail!("Expanded input exceeds the {limit}-byte read budget");
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
}

fn open_office_zip(path: &Path, cancel: &AtomicBool) -> Result<zip::ZipArchive<std::fs::File>> {
    check_cancel(cancel)?;
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() > MAX_OFFICE_FILE_BYTES {
        bail!("Office file exceeds the {} MiB input limit", MAX_OFFICE_FILE_BYTES / (1024 * 1024));
    }
    let archive = zip::ZipArchive::new(file).context("Not a valid Office ZIP file")?;
    if archive.len() > MAX_OFFICE_ENTRIES {
        bail!("Office ZIP exceeds the {MAX_OFFICE_ENTRIES}-member limit");
    }
    check_cancel(cancel)?;
    Ok(archive)
}

fn office_member(
    mut member: impl Read,
    declared_size: u64,
    remaining: &mut usize,
    limits: OfficeLimits,
    cancel: &AtomicBool,
) -> Result<Vec<u8>> {
    check_cancel(cancel)?;
    if declared_size > limits.member_bytes as u64 {
        bail!("Office ZIP member exceeds the {}-byte expansion budget", limits.member_bytes);
    }
    if declared_size > *remaining as u64 {
        bail!("Office ZIP exceeds the aggregate expansion budget");
    }
    let bytes = read_bounded(&mut member, limits.member_bytes.min(*remaining), Some(cancel))
        .context("Cannot read bounded Office ZIP member")?;
    *remaining -= bytes.len();
    Ok(bytes)
}

/// Shared across all sheets/parts so many individually small parts cannot
/// produce an unbounded document. Formatting writes directly into the result.
struct TextOutput {
    text: String,
    remaining: usize,
}

impl TextOutput {
    fn new(limit: usize) -> Self {
        Self { text: String::new(), remaining: limit }
    }

    fn push(&mut self, text: &str) -> Result<()> {
        std::fmt::Write::write_str(self, text).map_err(|_| anyhow!("Office extracted text exceeds the text budget"))
    }
}

impl std::fmt::Write for TextOutput {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        if text.len() > self.remaining {
            return Err(std::fmt::Error);
        }
        self.text.push_str(text);
        self.remaining -= text.len();
        Ok(())
    }
}

/// DOCX: body plus headers/footers/footnotes, one logical page.
pub fn docx(path: &Path) -> Result<Vec<PageText>> {
    docx_cancellable(path, &NEVER)
}

fn docx_cancellable(path: &Path, cancel: &AtomicBool) -> Result<Vec<PageText>> {
    docx_with_limits(path, cancel, OFFICE_LIMITS)
}

fn docx_with_limits(path: &Path, cancel: &AtomicBool, limits: OfficeLimits) -> Result<Vec<PageText>> {
    let mut archive = open_office_zip(path, cancel)?;
    let wanted = |n: &str| {
        n == "word/document.xml"
            || (n.starts_with("word/header") || n.starts_with("word/footer")) && n.ends_with(".xml")
            || n == "word/footnotes.xml"
            || n == "word/endnotes.xml"
    };
    let mut names = Vec::new();
    for name in archive.file_names() {
        check_cancel(cancel)?;
        let name = name?;
        if wanted(&name) {
            names.push(name.into_owned());
        }
    }
    if !names.iter().any(|n| n == "word/document.xml") {
        bail!("DOCX has no word/document.xml");
    }
    names.sort_by(|a, b| (a != "word/document.xml").cmp(&(b != "word/document.xml")).then(a.cmp(b)));
    let mut out = TextOutput::new(limits.text_bytes);
    let mut remaining = limits.expanded_bytes;
    for name in names {
        let member = archive.by_name(&name)?;
        let size = member.size();
        let bytes = office_member(member, size, &mut remaining, limits, cancel)
            .with_context(|| format!("Cannot read DOCX part {name}"))?;
        let xml = std::str::from_utf8(&bytes).context("DOCX XML is not UTF-8")?;
        docx_xml_text(xml, &mut out, cancel)?;
    }
    check_cancel(cancel)?;
    Ok(vec![PageText::new(0, out.text, TextSource::Native, None)])
}

fn docx_xml_text(xml: &str, out: &mut TextOutput, cancel: &AtomicBool) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    let mut in_text = false;
    loop {
        check_cancel(cancel)?;
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == "t" => in_text = true,
            Event::End(e) if e.local_name().as_ref() == "t" => in_text = false,
            Event::Text(t) if in_text => out.push(&t.xml10_content())?,
            Event::CData(t) if in_text => out.push(&t.into_inner())?,
            Event::GeneralRef(r) if in_text => {
                if let Some(ch) = r.resolve_char_ref()? {
                    out.push(ch.encode_utf8(&mut [0; 4]))?;
                } else {
                    match r.as_ref() {
                        "amp" => out.push("&")?,
                        "lt" => out.push("<")?,
                        "gt" => out.push(">")?,
                        "quot" => out.push("\"")?,
                        "apos" => out.push("'")?,
                        _ => {}
                    }
                }
            }
            Event::Empty(e) => match e.local_name().as_ref() {
                "tab" => out.push("\t")?,
                "br" | "cr" => out.push("\n")?,
                _ => {}
            },
            Event::End(e) if matches!(e.local_name().as_ref(), "p" | "tc") => {
                out.push(if e.local_name().as_ref() == "tc" { "\t" } else { "\n" })?;
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

#[derive(Default)]
struct WorkbookCounts {
    strings: u64,
    cells: u64,
    sheets: usize,
}

#[derive(Default)]
struct SheetBounds {
    min: Option<(u32, u32)>,
    max: (u32, u32),
}

impl SheetBounds {
    fn include(&mut self, position: (u32, u32)) -> Result<()> {
        self.min = Some(match self.min {
            Some(min) => (min.0.min(position.0), min.1.min(position.1)),
            None => position,
        });
        self.max = (self.max.0.max(position.0), self.max.1.max(position.1));
        let min = self.min.unwrap();
        check_sheet_rectangle(min, self.max)
    }
}

fn check_sheet_rectangle(min: (u32, u32), max: (u32, u32)) -> Result<()> {
    if max.0 < min.0 || max.1 < min.1 {
        bail!("XLSX has a reversed cell rectangle");
    }
    let cells = (u64::from(max.0 - min.0) + 1) * (u64::from(max.1 - min.1) + 1);
    if cells > MAX_SHEET_CELLS {
        bail!("XLSX cell rectangle exceeds the {MAX_SHEET_CELLS}-cell memory budget");
    }
    Ok(())
}

/// Parse before Calamine: it builds a dense range from the actual sparse
/// cells, even when the worksheet's dimension is missing or understates it.
fn cell_position(value: &str) -> Result<(u32, u32)> {
    let split = value.find(|ch: char| !ch.is_ascii_alphabetic()).unwrap_or(value.len());
    let (letters, digits) = value.split_at(split);
    if letters.is_empty() || digits.is_empty() || !digits.bytes().all(|ch| ch.is_ascii_digit()) {
        bail!("Invalid XLSX cell reference: {value}");
    }
    let mut column = 0u32;
    for letter in letters.bytes() {
        column = column.checked_mul(26).and_then(|n| n.checked_add(u32::from(letter.to_ascii_uppercase() - b'A') + 1))
            .ok_or_else(|| anyhow!("XLSX column is out of range"))?;
        if column > 16_384 {
            bail!("XLSX column is out of range");
        }
    }
    let row = digits.parse::<u32>().context("XLSX row is out of range")?;
    if row == 0 || row > 1_048_576 {
        bail!("XLSX row is out of range");
    }
    Ok((row - 1, column - 1))
}

fn bounded_count(value: &str, maximum: u64, label: &str) -> Result<()> {
    let count = value.parse::<u64>().with_context(|| format!("Invalid XLSX {label}"))?;
    if count > maximum {
        bail!("XLSX {label} exceeds the {maximum}-entry memory budget");
    }
    Ok(())
}

fn preflight_xlsx_xml(xml: &str, counts: &mut WorkbookCounts, cancel: &AtomicBool) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut in_sheet = false;
    let mut in_strings = false;
    let mut row = 0u32;
    let mut column = 0u32;
    let mut bounds = SheetBounds::default();
    let mut sheet_cells = 0u64;
    loop {
        check_cancel(cancel)?;
        match reader.read_event()? {
            Event::Start(element) => match element.local_name().as_ref() {
                "sst" => {
                    in_strings = true;
                    for attribute in element.attributes() {
                        let attribute = attribute?;
                        match attribute.key.as_ref() {
                            "uniqueCount" => bounded_count(&attribute.value, MAX_SHARED_STRINGS, "shared-string uniqueCount")?,
                            "count" => bounded_count(&attribute.value, MAX_WORKBOOK_CELLS, "shared-string count")?,
                            _ => {}
                        }
                    }
                }
                "si" if in_strings => {
                    counts.strings += 1;
                    if counts.strings > MAX_SHARED_STRINGS {
                        bail!("XLSX shared strings exceed the memory budget");
                    }
                }
                "sheet" => {
                    counts.sheets += 1;
                    if counts.sheets > MAX_WORKBOOK_SHEETS {
                        bail!("XLSX exceeds the {MAX_WORKBOOK_SHEETS}-sheet memory budget");
                    }
                }
                "dimension" => {
                    if let Some(reference) = element.try_get_attribute("ref")? {
                        let mut positions = reference.value.split(':');
                        let min = cell_position(positions.next().unwrap_or_default())?;
                        let max = positions.next().map(cell_position).transpose()?.unwrap_or(min);
                        if positions.next().is_some() {
                            bail!("Invalid XLSX dimension");
                        }
                        check_sheet_rectangle(min, max)?;
                    }
                }
                "sheetData" => in_sheet = true,
                "row" if in_sheet => {
                    if let Some(reference) = element.try_get_attribute("r")? {
                        let value = reference.value.parse::<u32>().context("Invalid XLSX row index")?;
                        if value == 0 || value > 1_048_576 {
                            bail!("XLSX row is out of range");
                        }
                        row = value - 1;
                    }
                }
                "c" if in_sheet => {
                    let position = match element.try_get_attribute("r")? {
                        Some(reference) => cell_position(&reference.value)?,
                        None if row < 1_048_576 && column < 16_384 => (row, column),
                        None => bail!("XLSX implicit cell is out of range"),
                    };
                    column = position.1;
                    sheet_cells += 1;
                    if sheet_cells > MAX_SHEET_CELLS {
                        bail!("XLSX actual sheet cells exceed the memory budget");
                    }
                    counts.cells += 1;
                    if counts.cells > MAX_WORKBOOK_CELLS {
                        bail!("XLSX actual cells exceed the memory budget");
                    }
                    bounds.include(position)?;
                }
                _ => {}
            },
            Event::End(element) => match element.local_name().as_ref() {
                "sst" => in_strings = false,
                "sheetData" => in_sheet = false,
                "row" if in_sheet => { row += 1; column = 0; }
                "c" if in_sheet => column += 1,
                _ => {}
            },
            Event::Eof => return Ok(()),
            _ => {}
        }
    }
}

fn preflight_xlsx(archive: &mut zip::ZipArchive<std::fs::File>, cancel: &AtomicBool, limits: OfficeLimits) -> Result<()> {
    let mut remaining = limits.expanded_bytes;
    let mut counts = WorkbookCounts::default();
    // Relationships can route Calamine to nonstandard member paths. Inspect
    // every expanded member rather than trusting xl/worksheets or its suffix.
    for index in 0..archive.len() {
        let member = archive.by_index(index)?;
        let size = member.size();
        let name = member.name()?.into_owned();
        let bytes = office_member(member, size, &mut remaining, limits, cancel)
            .with_context(|| format!("Cannot preflight XLSX part {name}"))?;
        let content = bytes.strip_prefix(b"\xef\xbb\xbf").unwrap_or(&bytes);
        let is_xml = content.iter().find(|byte| !byte.is_ascii_whitespace()).is_some_and(|byte| *byte == b'<');
        let is_xml_name = name.rsplit('.').next().is_some_and(|extension| {
            extension.eq_ignore_ascii_case("xml") || extension.eq_ignore_ascii_case("rels")
        });
        if is_xml || is_xml_name {
            let xml = std::str::from_utf8(&bytes).context("XLSX XML is not UTF-8")?;
            preflight_xlsx_xml(xml, &mut counts, cancel)
                .with_context(|| format!("Unsafe XLSX metadata in {name}"))?;
        }
    }
    Ok(())
}

/// Only third-party workbook parsing is isolated: malformed metadata is a
/// file failure and cannot unwind out of the indexing/startup worker.
fn calamine_read<T>(read: impl FnOnce() -> std::result::Result<T, calamine::XlsxError>) -> Result<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(read)) {
        Ok(result) => result.map_err(|error| anyhow!("Cannot read workbook: {error}")),
        Err(_) => bail!("Workbook parser panicked on malformed input"),
    }
}

fn xlsx_cell_is_blank(cell: &calamine::DataRef<'_>) -> bool {
    use calamine::DataRef;
    match cell {
        DataRef::Empty => true,
        DataRef::String(value) | DataRef::DateTimeIso(value) | DataRef::DurationIso(value) => value.is_empty(),
        DataRef::SharedString(value) => value.is_empty(),
        _ => false,
    }
}

fn push_xlsx_cell(out: &mut TextOutput, cell: &calamine::DataRef<'_>) -> Result<()> {
    use std::fmt::Write as _;
    use calamine::DataRef;
    let result = match cell {
        DataRef::Empty => Ok(()),
        DataRef::String(value) | DataRef::DateTimeIso(value) | DataRef::DurationIso(value) => out.write_str(value),
        DataRef::SharedString(value) => out.write_str(value),
        DataRef::Int(value) => write!(out, "{value}"),
        DataRef::Float(value) => write!(out, "{value}"),
        DataRef::Bool(value) => write!(out, "{value}"),
        DataRef::DateTime(value) => write!(out, "{value}"),
        DataRef::Error(value) => write!(out, "{value}"),
    };
    result.map_err(|_| anyhow!("Office extracted text exceeds the text budget"))
}

/// XLSX: one logical page per sheet, cells tab-separated, rows newline-separated.
pub fn xlsx(path: &Path) -> Result<Vec<PageText>> {
    xlsx_cancellable(path, &NEVER)
}

fn xlsx_cancellable(path: &Path, cancel: &AtomicBool) -> Result<Vec<PageText>> {
    xlsx_with_limits(path, cancel, OFFICE_LIMITS)
}

fn xlsx_with_limits(path: &Path, cancel: &AtomicBool, limits: OfficeLimits) -> Result<Vec<PageText>> {
    use calamine::{Reader as _, ReaderRef as _};
    let mut archive = open_office_zip(path, cancel)?;
    preflight_xlsx(&mut archive, cancel, limits)?;
    check_cancel(cancel)?;
    let file = archive.into_inner();
    let mut workbook = calamine_read(|| calamine::Xlsx::new(file))?;
    let mut pages = Vec::new();
    let mut out = TextOutput::new(limits.text_bytes);
    for (index, name) in workbook.sheet_names().to_vec().into_iter().enumerate() {
        check_cancel(cancel)?;
        // Borrow shared strings: worksheet_range() would clone a large shared
        // string for every referencing cell before the text budget can act.
        let book = &mut workbook;
        let sheet_name = name.as_str();
        let range = calamine_read(move || book.worksheet_range_ref(sheet_name))?;
        check_cancel(cancel)?;
        out.push(&name)?;
        out.push("\n")?;
        let leading_columns = range.start().map_or(0, |(_, column)| column);
        for row in range.rows() {
            check_cancel(cancel)?;
            if let Some(last) = row.iter().rposition(|cell| !xlsx_cell_is_blank(cell)) {
                for _ in 0..leading_columns {
                    out.push("\t")?;
                }
                for (column, cell) in row[..=last].iter().enumerate() {
                    if column != 0 {
                        out.push("\t")?;
                    }
                    push_xlsx_cell(&mut out, cell)?;
                }
                out.push("\n")?;
            }
        }
        pages.push(PageText::new(index, std::mem::take(&mut out.text), TextSource::Native, None));
    }
    check_cancel(cancel)?;
    Ok(pages)
}

/// Plain text as one page. Encoding: the BOM when present (UTF-8/UTF-16),
/// else UTF-8 when the bytes are valid UTF-8, else Windows-1254 — the Turkish
/// ANSI code page older Notepad and office software saved in.
pub fn txt(path: &Path) -> Result<Vec<PageText>> {
    let size = std::fs::metadata(path).with_context(|| format!("Cannot read {}", path.display()))?.len();
    if size > MAX_TEXT_FILE_BYTES {
        bail!(
            "Text file too large: {} MB (limit {} MB)",
            size.div_ceil(1024 * 1024),
            MAX_TEXT_FILE_BYTES / (1024 * 1024)
        );
    }
    let bytes = std::fs::read(path).with_context(|| format!("Cannot read {}", path.display()))?;
    Ok(vec![PageText::new(0, decode_text(&bytes), TextSource::Native, None)])
}

pub fn decode_text(bytes: &[u8]) -> String {
    if let Some((encoding, bom)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding.decode_without_bom_handling(&bytes[bom..]).0.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => encoding_rs::WINDOWS_1254.decode_without_bom_handling(bytes).0.into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Extractor over the bundled pdfium with OCR deliberately unavailable,
    /// so scanned (text-less) pages fail while text-layer pages succeed.
    fn extractor(dir: &Path) -> Option<Extractor> {
        let pdfium = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/pdfium");
        let ocr = OcrConfig {
            binary: dir.join("missing-tesseract"),
            tessdata: dir.to_path_buf(),
            language: "tur".into(),
            timeout: Duration::from_secs(5),
            threads: 1,
        };
        let library = Pdfium::pdfium_platform_library_name_at_path(&pdfium);
        let extractor = Extractor::new(&pdfium, ocr, dir.join("scratch"), 2);
        // Only skip when the bundled library is genuinely absent.
        assert!(extractor.pdfium.is_some() || !Path::new(&library).exists(), "{:?}", extractor.pdf_status());
        extractor.pdfium.is_some().then_some(extractor)
    }

    /// `None` pages are blank (no text layer), i.e. they go to OCR.
    fn write_pdf(extractor: &Extractor, path: &Path, pages: &[Option<&str>]) {
        let pdfium = extractor.pdfium.as_ref().unwrap();
        let mut document = pdfium.create_new_pdf().unwrap();
        let font = document.fonts_mut().helvetica();
        for text in pages {
            let mut page = document.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
            if let Some(text) = text {
                page.objects_mut()
                    .create_text_object(PdfPoints::new(50.0), PdfPoints::new(700.0), *text, font, PdfPoints::new(14.0))
                    .unwrap();
            }
        }
        document.save_to_file(path).unwrap();
    }

    #[test]
    fn an_unreadable_page_does_not_drop_the_document() {
        let dir = tempfile::tempdir().unwrap();
        let Some(extractor) = extractor(dir.path()) else {
            eprintln!("skipped: bundled pdfium unavailable");
            return;
        };
        let pdf = dir.path().join("karma.pdf");
        write_pdf(
            &extractor,
            &pdf,
            &[Some("Kira sozlesmesi madde bir taraflar"), None, Some("Ikinci metin sayfasi kira bedeli")],
        );
        let document = extractor.extract(&pdf).unwrap();
        assert_eq!(document.pages.len(), 3);
        assert!(document.pages[0].text.contains("sozlesmesi"));
        assert!(document.pages[2].text.contains("bedeli"));
        assert_eq!(document.failed_pages(), 1);
        let failed = &document.pages[1];
        assert_eq!(failed.index, 1);
        assert!(failed.text.is_empty());
        assert!(failed.error.as_deref().unwrap().contains("OCR unavailable"), "{failed:?}");
    }

    #[test]
    fn image_page_images_are_scaled_down_never_up() {
        let dir = tempfile::tempdir().unwrap();
        let extractor = Extractor::new(&dir.path().join("no-pdfium"), OcrConfig {
            binary: dir.path().join("none"),
            tessdata: dir.path().to_path_buf(),
            language: "tur".into(),
            timeout: Duration::from_secs(5),
            threads: 1,
        }, dir.path().join("scratch"), 1);
        let path = dir.path().join("tarama.png");
        image::GrayImage::from_pixel(1200, 1600, image::Luma([200])).save(&path).unwrap();
        let small = extractor.page_image(&path, FileKind::Image, 0, 600).unwrap();
        assert_eq!(small.dimensions(), (600, 800));
        let same = extractor.page_image(&path, FileKind::Image, 0, 2000).unwrap();
        assert_eq!(same.dimensions(), (1200, 1600));
        assert!(extractor.page_image(&path, FileKind::Image, 1, 600).is_err());
        assert!(extractor.page_image(&path, FileKind::Word, 0, 600).is_err());
    }

    #[test]
    fn deskewed_pages_keep_the_layout_mode_of_the_scan() {
        let mut page = GrayImage::from_pixel(1000, 1300, image::Luma([255]));
        for line in 0..30 {
            for x in 120..880 {
                if (x / 7) % 6 != 0 {
                    for t in 0..6 {
                        page.put_pixel(x, 150 + line * 32 + t, image::Luma([20]));
                    }
                }
            }
        }
        let skewed = crate::deskew::rotate(&page, 2.5);
        let (level, psm) = prepare_for_ocr(skewed.clone(), true);
        assert_eq!(psm, Some(crate::ocr::select_psm(&skewed)), "mode decided on the scan as rendered");
        assert_ne!(level.dimensions(), skewed.dimensions(), "pixels were levelled");
        let (same, none) = prepare_for_ocr(skewed.clone(), false);
        assert_eq!((same.dimensions(), none), (skewed.dimensions(), None));
        let (_, level_psm) = prepare_for_ocr(page, true);
        assert_eq!(level_psm, None, "a level page is left alone");
    }

    #[test]
    fn text_files_are_decoded_by_bom_utf8_or_turkish_ansi() {
        let expected = "Şimşek Ağrı Iğdır çözüm";
        assert_eq!(decode_text(expected.as_bytes()), expected);
        assert_eq!(decode_text(&[b"\xef\xbb\xbf".as_slice(), expected.as_bytes()].concat()), expected);
        let utf16: Vec<u8> = [0xFF, 0xFE].into_iter().chain(expected.encode_utf16().flat_map(u16::to_le_bytes)).collect();
        assert_eq!(decode_text(&utf16), expected);
        // Windows-1254, as older Turkish Notepad saved it.
        let ansi = b"\xdeim\xfeek A\xf0r\xfd I\xf0d\xfdr \xe7\xf6z\xfcm";
        assert_eq!(decode_text(ansi), expected);

        let dir = tempfile::tempdir().unwrap();
        let note = dir.path().join("not.txt");
        std::fs::write(&note, ansi).unwrap();
        let pages = txt(&note).unwrap();
        assert_eq!((pages.len(), pages[0].text.as_str(), pages[0].source), (1, expected, TextSource::Native));
        let huge = dir.path().join("dump.txt");
        std::fs::File::create(&huge).unwrap().set_len(MAX_TEXT_FILE_BYTES + 1).unwrap();
        assert!(format!("{:#}", txt(&huge).unwrap_err()).contains("Text file too large"));
    }

    #[test]
    fn a_document_with_no_readable_page_still_fails() {
        let dir = tempfile::tempdir().unwrap();
        let Some(extractor) = extractor(dir.path()) else {
            eprintln!("skipped: bundled pdfium unavailable");
            return;
        };
        let pdf = dir.path().join("tarama.pdf");
        write_pdf(&extractor, &pdf, &[None, None]);
        let error = extractor.extract(&pdf).unwrap_err();
        assert!(format!("{error:#}").contains("No page could be read"), "{error:#}");
    }
}

#[cfg(all(test, unix))]
mod status_cache_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use super::*;

    /// `app_info` asks for OCR status on every call; the probe must run once.
    #[test]
    fn ocr_status_runs_the_probe_once_and_caches_it() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("calls.log");
        let binary = dir.path().join("tesseract");
        std::fs::write(&binary, format!("#!/bin/sh\necho \"$@\" >> '{}'\nexit 1\n", log.display())).unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("tur.traineddata"), b"model").unwrap();
        let ocr = OcrConfig {
            binary,
            tessdata: dir.path().to_path_buf(),
            language: "tur".into(),
            timeout: Duration::from_secs(5),
            threads: 1,
        };
        let extractor = Extractor::new(&dir.path().join("no-pdfium"), ocr, dir.path().join("scratch"), 1);
        assert!(!log.exists(), "construction must not run Tesseract");
        for _ in 0..3 {
            let error = extractor.ocr_status().unwrap_err();
            assert!(error.contains("--version exited"), "{error}");
        }
        let calls = std::fs::read_to_string(&log).unwrap();
        assert_eq!(calls.lines().count(), 1, "{calls}");
        // Recognition is gated on the cached verdict.
        let image = dir.path().join("x.png");
        std::fs::write(&image, b"png").unwrap();
        assert!(format!("{:#}", extractor.extract(&image).unwrap_err()).contains("OCR unavailable"));
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }
}

#[cfg(all(test, unix))]
mod budget_tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn the_budget_never_exceeds_its_capacity() {
        let budget = OcrBudget::new(3);
        let (now, peak) = (AtomicUsize::new(0), AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..12 {
                scope.spawn(|| {
                    let _slot = budget.acquire();
                    let running = now.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(running, Ordering::SeqCst);
                    // Hold the slot until the budget is full once (bounded),
                    // so reaching the capacity does not hinge on thread
                    // start-up speed under a loaded test run.
                    let deadline = std::time::Instant::now() + Duration::from_secs(30);
                    while peak.load(Ordering::SeqCst) < 3 && std::time::Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                    now.fetch_sub(1, Ordering::SeqCst);
                });
            }
        });
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(*budget.free.lock(), 3);
        assert!(OcrBudget::default_capacity() >= 1);
        assert_eq!(OcrBudget::new(0).capacity(), 1);
    }

    /// Scanned pages of one PDF are OCR'd concurrently, never above the
    /// budget, and come back in page order.
    #[test]
    fn scanned_pages_of_one_pdf_run_in_parallel_within_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let pdfium = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/pdfium");
        let running = dir.path().join("running");
        let log = dir.path().join("concurrency.log");
        let binary = dir.path().join("tesseract");
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'tesseract 9.9.9'; exit 0; fi\n\
                 mkdir -p '{run}'; touch \"{run}/$$\"; ls '{run}' | wc -l >> '{log}'\n\
                 sleep 0.3; rm -f \"{run}/$$\"\n\
                 printf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n'\n\
                 printf '5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t9\\t9\\t90\\t2026\\n'\n",
                run = running.display(),
                log = log.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("tur.traineddata"), b"model").unwrap();
        let ocr = OcrConfig {
            binary,
            tessdata: dir.path().to_path_buf(),
            language: "tur".into(),
            timeout: Duration::from_secs(30),
            threads: 1,
        };
        let extractor = Extractor::new(&pdfium, ocr, dir.path().join("scratch"), 2);
        let Some(pdfium) = extractor.pdfium.as_ref() else {
            eprintln!("skipped: bundled pdfium unavailable");
            return;
        };
        let pdf = dir.path().join("tarama.pdf");
        let mut document = pdfium.create_new_pdf().unwrap();
        for _ in 0..6 {
            document.pages_mut().create_page_at_end(PdfPagePaperSize::new_custom(PdfPoints::new(144.0), PdfPoints::new(144.0))).unwrap();
        }
        document.save_to_file(&pdf).unwrap();
        drop(document);

        assert!(extractor.ocr_status().is_ok(), "{:?}", extractor.ocr_status());
        let extracted = extractor.extract(&pdf).unwrap();
        let indexes: Vec<usize> = extracted.pages.iter().map(|p| p.index).collect();
        assert_eq!(indexes, [0, 1, 2, 3, 4, 5]);
        assert!(extracted.pages.iter().all(|p| p.text.trim() == "2026" && !p.is_failed()));
        let peak = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .filter_map(|l| l.trim().parse::<usize>().ok())
            .max()
            .unwrap();
        assert_eq!(peak, 2, "OCR must run in parallel but within the budget");
    }

    /// Cancelling mid-document discards the document even though some of
    /// its pages were already read: no half-read PDF reaches the index.
    #[test]
    fn a_cancelled_pdf_is_not_returned_half_read() {
        let dir = tempfile::tempdir().unwrap();
        let pdfium = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/pdfium");
        let calls = dir.path().join("calls");
        let slow = dir.path().join("slow-started");
        let binary = dir.path().join("tesseract");
        // Probe and first page answer at once; every later page hangs.
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'tesseract 9.9.9'; exit 0; fi\n\
                 echo x >> '{calls}'\n\
                 if [ $(wc -l < '{calls}') -gt 2 ]; then touch '{slow}'; exec sleep 30; fi\n\
                 printf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n'\n\
                 printf '5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t9\\t9\\t90\\t2026\\n'\n",
                calls = calls.display(),
                slow = slow.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.path().join("tur.traineddata"), b"model").unwrap();
        let ocr = OcrConfig {
            binary,
            tessdata: dir.path().to_path_buf(),
            language: "tur".into(),
            timeout: Duration::from_secs(60),
            threads: 1,
        };
        let extractor = Extractor::new(&pdfium, ocr, dir.path().join("scratch"), 1);
        let Some(pdfium) = extractor.pdfium.as_ref() else {
            eprintln!("skipped: bundled pdfium unavailable");
            return;
        };
        let pdf = dir.path().join("tarama.pdf");
        let mut document = pdfium.create_new_pdf().unwrap();
        for _ in 0..3 {
            document.pages_mut().create_page_at_end(PdfPagePaperSize::new_custom(PdfPoints::new(144.0), PdfPoints::new(144.0))).unwrap();
        }
        document.save_to_file(&pdf).unwrap();
        drop(document);
        assert!(extractor.ocr_status().is_ok(), "{:?}", extractor.ocr_status());

        let cancel = AtomicBool::new(false);
        let started = std::time::Instant::now();
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !slow.exists() {
                    std::thread::sleep(Duration::from_millis(10));
                }
                cancel.store(true, Ordering::SeqCst);
            });
            extractor.extract_cancellable(&pdf, &cancel)
        });
        let error = result.expect_err("a cancelled PDF must not come back with pages missing");
        assert!(is_cancelled(&error), "{error:#}");
        assert!(started.elapsed() < Duration::from_secs(10), "OCR was not killed: {:?}", started.elapsed());
        let ocr_calls = std::fs::read_to_string(&calls).unwrap().lines().count();
        assert_eq!(ocr_calls, 3, "probe, page 1 (read), page 2 (killed); page 3 never started");
        let scratch: Vec<_> = std::fs::read_dir(dir.path().join("scratch")).unwrap().collect();
        assert!(scratch.is_empty(), "scratch renders left behind: {scratch:?}");
    }
}


#[cfg(test)]
pub(crate) mod parser_test_support {
    use std::io::Write;
    use std::path::Path;

    pub(crate) fn write_zip(path: &Path, members: &[(&str, &str)]) {
        let mut zip = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated)
            .last_modified_time(zip::DateTime::default());
        for (name, text) in members {
            zip.start_file(*name, options).unwrap();
            zip.write_all(text.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    /// A real OOXML package, not just the worksheet XML: shared-string
    /// metadata is consumed during Calamine construction, before any sheet.
    pub(crate) fn write_xlsx(path: &Path, sheet_xml: &str, shared_xml: Option<&str>) {
        let mut members = vec![
            ("[Content_Types].xml", r#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#),
            ("_rels/.rels", r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#),
            ("xl/workbook.xml", r#"<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets><sheet name="Sayfa" sheetId="1" r:id="rId1"/></sheets></workbook>"#),
            ("xl/_rels/workbook.xml.rels", r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/></Relationships>"#),
            ("xl/worksheets/sheet1.xml", sheet_xml),
        ];
        if let Some(shared) = shared_xml {
            members.push(("xl/sharedStrings.xml", shared));
        }
        write_zip(path, &members);
    }

    /// Deliberately contains no pixel data. A decoder may inspect the tiny
    /// header, but budget rejection must precede allocating/reading pixels.
    pub(crate) fn bmp_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0u8; 54];
        bytes[..2].copy_from_slice(b"BM");
        bytes[2..6].copy_from_slice(&54u32.to_le_bytes());
        bytes[10..14].copy_from_slice(&54u32.to_le_bytes());
        bytes[14..18].copy_from_slice(&40u32.to_le_bytes());
        bytes[18..22].copy_from_slice(&width.to_le_bytes());
        bytes[22..26].copy_from_slice(&height.to_le_bytes());
        bytes[26..28].copy_from_slice(&1u16.to_le_bytes());
        bytes[28..30].copy_from_slice(&24u16.to_le_bytes());
        bytes
    }
}

#[cfg(test)]
mod parser_budget_tests {
    use super::*;
    use super::parser_test_support::{bmp_header, write_xlsx, write_zip};

    const HEALTHY_SHEET: &str = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData><row r="1"><c r="A1" t="inlineStr"><is><t>Sağlıklı fatura</t></is></c></row></sheetData></worksheet>"#;

    #[test]
    fn malicious_shared_string_counts_are_file_errors_and_the_next_file_reads() {
        let dir = tempfile::tempdir().unwrap();
        let bad = dir.path().join("bad.xlsx");
        for (attribute, value) in [
            ("uniqueCount", "18446744073709551615"),
            ("uniqueCount", "18446744073709551616"),
            ("count", "18446744073709551615"),
        ] {
            let shared = format!("<sst {attribute}=\"{value}\"><si><t>x</t></si></sst>");
            write_xlsx(&bad, HEALTHY_SHEET, Some(&shared));
            let error = xlsx(&bad).unwrap_err();
            assert!(format!("{error:#}").contains("shared-string"), "{error:#}");
        }
        let good = dir.path().join("good.xlsx");
        write_xlsx(&good, HEALTHY_SHEET, None);
        assert_eq!(xlsx(&good).unwrap()[0].text, "Sayfa\nSağlıklı fatura\n");
    }

    #[test]
    fn actual_shared_strings_cannot_bypass_a_small_declared_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strings.xlsx");
        let shared = format!("<sst uniqueCount=\"1\">{}</sst>", "<si><t>x</t></si>".repeat(MAX_SHARED_STRINGS as usize + 1));
        write_xlsx(&path, HEALTHY_SHEET, Some(&shared));
        let error = xlsx(&path).unwrap_err();
        assert!(format!("{error:#}").contains("shared strings exceed"), "{error:#}");
    }

    #[test]
    fn calamine_panics_are_contained_to_one_file() {
        let error = calamine_read(|| -> std::result::Result<(), calamine::XlsxError> {
            panic!("capacity overflow");
        }).unwrap_err();
        assert!(format!("{error:#}").contains("Workbook parser panicked"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("healthy.xlsx");
        write_xlsx(&path, HEALTHY_SHEET, None);
        assert!(xlsx(&path).unwrap()[0].text.contains("Sağlıklı fatura"));
    }

    #[test]
    fn sparse_cells_are_bounded_even_when_dimensions_lie_or_are_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sparse.xlsx");
        for dimension in ["", "<dimension ref=\"A1\"/>", "<dimension ref=\"A1:XFD1048576\"/>"] {
            let sheet = format!("<worksheet>{dimension}<sheetData><row r=\"1\"><c r=\"A1\"><v>1</v></c></row><row r=\"1048576\"><c r=\"XFD1048576\"><v>2</v></c></row></sheetData></worksheet>");
            write_xlsx(&path, &sheet, None);
            let error = xlsx(&path).unwrap_err();
            assert!(format!("{error:#}").contains("cell rectangle"), "{error:#}");
        }
        // Calamine also accepts omitted cell references, inferring position
        // from row indices. Those inferred cells must enter the same bounds.
        let sheet = "<worksheet><dimension ref=\"A1\"/><sheetData><row r=\"1\"><c><v>1</v></c></row><row r=\"1048576\"><c><v>2</v></c></row></sheetData></worksheet>";
        write_xlsx(&path, sheet, None);
        assert!(format!("{:#}", xlsx(&path).unwrap_err()).contains("cell rectangle"));
    }

    #[test]
    fn overflowing_cell_references_are_rejected_before_calamine() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coordinates.xlsx");
        for reference in ["A0", "XFE1", "A1048577", "ZZZZZZZZZZ9999999999999999999999"] {
            let sheet = format!("<worksheet><sheetData><row><c r=\"{reference}\"><v>1</v></c></row></sheetData></worksheet>");
            write_xlsx(&path, &sheet, None);
            let error = xlsx(&path).unwrap_err();
            assert!(format!("{error:#}").contains("out of range"), "{error:#}");
        }
    }

    #[test]
    fn xlsx_retains_blank_columns_and_omits_only_trailing_cells_and_blank_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("columns.xlsx");
        write_xlsx(&path, r#"<worksheet><dimension ref="A1:F5"/><sheetData>
            <row r="1"><c r="A1" t="inlineStr"><is><t>Ad</t></is></c><c r="C1"><v>12</v></c><c r="F1"/></row>
            <row r="2"><c r="B2"><v>5</v></c><c r="D2" t="inlineStr"><is><t>Şimşek</t></is></c><c r="F2" t="inlineStr"><is><t></t></is></c></row>
            <row r="3"><c r="A3"/><c r="F3"/></row>
            <row r="5"><c r="C5" t="inlineStr"><is><t>Son</t></is></c></row>
            </sheetData></worksheet>"#, None);
        assert_eq!(xlsx(&path).unwrap()[0].text, "Sayfa\nAd\t\t12\n\t5\t\tŞimşek\n\t\tSon\n");
        // All data may start after column A. Keep the same sheet coordinates.
        write_xlsx(&path, r#"<worksheet><sheetData><row r="5"><c r="C5"><v>3</v></c><c r="E5"><v>5</v></c></row></sheetData></worksheet>"#, None);
        assert_eq!(xlsx(&path).unwrap()[0].text, "Sayfa\n\t\t3\t\t5\n");
    }

    #[test]
    fn repeated_shared_values_obey_the_text_budget_without_owned_cell_copies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repeated.xlsx");
        let shared = format!("<sst uniqueCount=\"1\" count=\"3\"><si><t>{}</t></si></sst>", "ş".repeat(1024));
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1" t="s"><v>0</v></c></row><row r="2"><c r="A2" t="s"><v>0</v></c></row><row r="3"><c r="A3" t="s"><v>0</v></c></row></sheetData></worksheet>"#;
        write_xlsx(&path, sheet, Some(&shared));
        let limits = OfficeLimits { text_bytes: 4096, ..OFFICE_LIMITS };
        let error = xlsx_with_limits(&path, &NEVER, limits).unwrap_err();
        assert!(format!("{error:#}").contains("text budget"), "{error:#}");
        let pages = xlsx(&path).unwrap();
        assert_eq!(pages[0].text.matches('ş').count(), 3072);
    }

    #[test]
    fn docx_keeps_unicode_entities_cdata_headers_and_footnotes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unicode.docx");
        write_zip(&path, &[
            ("word/document.xml", r#"<w:document xmlns:w="word"><w:body><w:p><w:r><w:t>Şirket &amp; &lt;ödeme&gt; &#x130;&#287;dır &#351;</w:t></w:r><w:tab/><w:r><w:t><![CDATA[özel <satır>]]></w:t></w:r><w:br/></w:p></w:body></w:document>"#),
            ("word/header1.xml", r#"<w:hdr xmlns:w="word"><w:p><w:r><w:t>Üst &quot;Başlık&quot;</w:t></w:r></w:p></w:hdr>"#),
            ("word/footer1.xml", r#"<w:ftr xmlns:w="word"><w:p><w:r><w:t>Alt &apos;Not&apos;</w:t></w:r></w:p></w:ftr>"#),
            ("word/footnotes.xml", r#"<w:footnotes xmlns:w="word"><w:p><w:r><w:t>Dipnot İstanbul</w:t></w:r></w:p></w:footnotes>"#),
        ]);
        let pages = docx(&path).unwrap();
        assert!(pages[0].text.starts_with("Şirket & <ödeme> İğdır ş\tözel <satır>\n\n"), "{:?}", pages[0].text);
        for part in ["Üst \"Başlık\"", "Alt 'Not'", "Dipnot İstanbul"] {
            assert!(pages[0].text.contains(part), "{:?}", pages[0].text);
        }
    }

    #[test]
    fn docx_member_aggregate_and_text_limits_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("expanded.docx");
        let body = format!("<document><p><t>{}</t></p></document>", "x".repeat(1024));
        let header = format!("<hdr><p><t>{}</t></p></hdr>", "Ü".repeat(512));
        write_zip(&path, &[("word/document.xml", &body), ("word/header1.xml", &header)]);
        assert!(std::fs::metadata(&path).unwrap().len() < body.len() as u64, "fixture is actually compressed");
        for (limits, expected) in [
            (OfficeLimits { member_bytes: body.len() - 1, ..OFFICE_LIMITS }, "member exceeds"),
            (OfficeLimits { expanded_bytes: body.len() + header.len() - 1, ..OFFICE_LIMITS }, "aggregate expansion"),
            (OfficeLimits { text_bytes: 1024 + 1 + 1024, ..OFFICE_LIMITS }, "text budget"),
        ] {
            let error = docx_with_limits(&path, &NEVER, limits).unwrap_err();
            assert!(format!("{error:#}").contains(expected), "{expected}: {error:#}");
        }
        assert!(docx(&path).unwrap()[0].text.contains('Ü'));
    }

    #[test]
    fn actual_zip_expansion_is_bounded_when_size_metadata_lies() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lying-size.docx");
        let body = format!("<document><p><t>{}</t></p></document>", "x".repeat(8192));
        write_zip(&path, &[("word/document.xml", &body)]);
        assert_eq!(docx(&path).unwrap()[0].text.trim(), "x".repeat(8192));
        let mut bytes = std::fs::read(&path).unwrap();
        let central = bytes.windows(4).position(|window| window == b"PK\x01\x02").unwrap();
        bytes[central + 24..central + 28].copy_from_slice(&1u32.to_le_bytes());
        bytes[22..26].copy_from_slice(&1u32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();
        let limits = OfficeLimits { member_bytes: 64, ..OFFICE_LIMITS };
        assert!(docx_with_limits(&path, &NEVER, limits).is_err());
    }

    #[test]
    fn cancellation_during_zip_expansion_never_returns_partial_text() {
        struct CancelOnRead<'a, R> { inner: R, cancel: &'a AtomicBool }
        impl<R: Read> Read for CancelOnRead<'_, R> {
            fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
                let read = self.inner.read(buffer)?;
                self.cancel.store(true, Ordering::SeqCst);
                Ok(read)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cancel.docx");
        write_zip(&path, &[("word/document.xml", "<document><p><t>Belge</t></p></document>")]);
        let cancel = AtomicBool::new(false);
        let mut archive = open_office_zip(&path, &cancel).unwrap();
        let member = archive.by_name("word/document.xml").unwrap();
        let size = member.size();
        let mut remaining = OFFICE_LIMITS.expanded_bytes;
        let error = office_member(CancelOnRead { inner: member, cancel: &cancel }, size, &mut remaining, OFFICE_LIMITS, &cancel).unwrap_err();
        assert!(is_cancelled(&error), "{error:#}");
        assert!(is_cancelled(&docx_cancellable(&path, &cancel).unwrap_err()));
        let mut out = TextOutput::new(100);
        assert!(is_cancelled(&docx_xml_text("<p><t>partial</t></p>", &mut out, &cancel).unwrap_err()));
        assert!(out.text.is_empty());
        assert!(is_cancelled(&xlsx_cancellable(&path, &cancel).unwrap_err()));
    }

    #[test]
    fn image_decode_budget_is_checked_before_orientation_and_preview_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bmp");
        std::fs::write(&path, bmp_header(10_000, 5000)).unwrap();
        let error = open_upright(&path).unwrap_err();
        assert!(format!("{error:#}").contains("pixel or memory budget"), "{error:#}");
        let extractor = Extractor::new(dir.path(), OcrConfig {
            binary: dir.path().join("unused"), tessdata: dir.path().into(), language: "tur".into(),
            timeout: std::time::Duration::from_secs(1), threads: 1,
        }, dir.path().join("scratch"), 1);
        let error = extractor.page_image(&path, FileKind::Image, 0, 300).unwrap_err();
        assert!(format!("{error:#}").contains("pixel or memory budget"), "{error:#}");
        // Edge and pixel-count caps also apply to low-byte-depth images.
        for (width, height) in [(MAX_IMAGE_EDGE + 1, 1), (8000, 8000)] {
            std::fs::write(&path, bmp_header(width, height)).unwrap();
            assert!(open_upright(&path).is_err());
        }
        let encoded = dir.path().join("encoded.png");
        std::fs::File::create(&encoded).unwrap().set_len(MAX_IMAGE_FILE_BYTES + 1).unwrap();
        assert!(format!("{:#}", open_upright(&encoded).unwrap_err()).contains("input limit"));
    }

    #[test]
    fn bounded_orientation_decode_still_applies_exif_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("phone.jpg");
        image::RgbImage::from_pixel(2, 3, image::Rgb([40, 80, 120])).save(&path).unwrap();
        let original = std::fs::read(&path).unwrap();
        let exif = b"Exif\0\0II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00\x00";
        let mut jpeg = original[..2].to_vec();
        jpeg.extend_from_slice(b"\xff\xe1");
        jpeg.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        jpeg.extend_from_slice(exif);
        jpeg.extend_from_slice(&original[2..]);
        std::fs::write(&path, jpeg).unwrap();
        let (upright, rotated) = open_upright(&path).unwrap();
        assert!(rotated);
        assert_eq!((upright.width(), upright.height()), (3, 2));
    }
}
