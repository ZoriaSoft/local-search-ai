//! Per-format text extraction.
//!
//! PDF: each page's text layer is used when it carries real text; otherwise the
//! page is rendered at 300 DPI and OCR'd (handles scanned and mixed PDFs).
//! Images: OCR. DOCX: document/headers/footers XML. XLSX: every sheet's cells.

use std::io::Read;
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
            FileKind::Word => docx(path)?,
            FileKind::Excel => xlsx(path)?,
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
                let gray = image::open(path)?.into_luma8();
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
        let mut decoder = image::ImageReader::open(path)?.with_guessed_format()?.into_decoder()?;
        let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
        let mut image = image::DynamicImage::from_decoder(decoder)?;
        let rotated = orientation != image::metadata::Orientation::NoTransforms;
        image.apply_orientation(orientation);
        Ok((image, rotated))
    };
    decode().with_context(|| format!("Cannot decode image {}", path.display()))
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

/// DOCX: body plus headers/footers/footnotes, one logical page.
pub fn docx(path: &Path) -> Result<Vec<PageText>> {
    let file = std::fs::File::open(path)?;
    let mut archive = zip::ZipArchive::new(file).context("Not a valid DOCX (zip) file")?;
    let wanted = |n: &str| {
        n == "word/document.xml"
            || (n.starts_with("word/header") || n.starts_with("word/footer")) && n.ends_with(".xml")
            || n == "word/footnotes.xml"
            || n == "word/endnotes.xml"
    };
    let mut names: Vec<String> = archive
        .file_names()
        .filter_map(|n| n.ok())
        .filter(|n| wanted(n))
        .map(|n| n.into_owned())
        .collect();
    if !names.iter().any(|n| n == "word/document.xml") {
        bail!("DOCX has no word/document.xml");
    }
    names.sort_by_key(|n| (n != "word/document.xml", n.clone()));
    let mut text = String::new();
    for name in names {
        let mut xml = String::new();
        archive.by_name(&name)?.read_to_string(&mut xml)?;
        docx_xml_text(&xml, &mut text)?;
    }
    Ok(vec![PageText::new(0, text, TextSource::Native, None)])
}

fn docx_xml_text(xml: &str, out: &mut String) -> Result<()> {
    let mut reader = Reader::from_str(xml);
    let mut in_text = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == "t" => in_text = true,
            Event::End(e) if e.local_name().as_ref() == "t" => in_text = false,
            Event::Text(t) if in_text => out.push_str(&t.xml10_content()),
            Event::CData(t) if in_text => out.push_str(&t.into_inner()),
            Event::GeneralRef(r) if in_text => {
                if let Some(ch) = r.resolve_char_ref()? {
                    out.push(ch);
                } else {
                    match r.as_ref() {
                        "amp" => out.push('&'),
                        "lt" => out.push('<'),
                        "gt" => out.push('>'),
                        "quot" => out.push('"'),
                        "apos" => out.push('\''),
                        _ => {}
                    }
                }
            }
            Event::Empty(e) => match e.local_name().as_ref() {
                "tab" => out.push('\t'),
                "br" | "cr" => out.push('\n'),
                _ => {}
            },
            Event::End(e) if matches!(e.local_name().as_ref(), "p" | "tc") => {
                out.push(if e.local_name().as_ref() == "tc" { '\t' } else { '\n' })
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(())
}

/// XLSX: one logical page per sheet, cells tab-separated, rows newline-separated.
pub fn xlsx(path: &Path) -> Result<Vec<PageText>> {
    use calamine::{open_workbook_auto, Data, Reader as _};
    let mut workbook = open_workbook_auto(path).map_err(|e| anyhow!("Cannot open workbook: {e}"))?;
    let mut pages = Vec::new();
    for (index, name) in workbook.sheet_names().to_vec().into_iter().enumerate() {
        let range = workbook
            .worksheet_range(&name)
            .map_err(|e| anyhow!("Cannot read sheet {name}: {e}"))?;
        let mut text = format!("{name}\n");
        for row in range.rows() {
            let cells: Vec<String> = row
                .iter()
                .filter(|c| !matches!(c, Data::Empty))
                .map(|c| c.to_string())
                .collect();
            if !cells.is_empty() {
                text.push_str(&cells.join("\t"));
                text.push('\n');
            }
        }
        pages.push(PageText::new(index, text, TextSource::Native, None));
    }
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
