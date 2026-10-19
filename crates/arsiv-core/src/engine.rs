//! The application engine: folder management, incremental indexing in a
//! background thread, live folder watching, search and document views.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

use parking_lot::{Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer_opt, DebounceEventResult, Debouncer, NoCache};
use rayon::prelude::*;
use serde::Serialize;

use crate::catalog::{is_within, normalize_folder, Catalog, FailureInfo, FileRecord, FolderInfo};
use crate::embed::{document_title, ChunkVector, EmbedConfig, Embedder};
use crate::extract::Extractor;
use crate::index::{
    lead_snippet, page_stats, query_terms, quoted_phrases, terms_present, snippets, utf16_ranges, Matcher, SearchFilter, SearchIndex, SearchOptions,
    SearchResults,
};
use crate::model::{is_low_confidence, ExtractedDocument, FileKind, TextSource};
use crate::ocr::{is_cancelled, Cancelled, OcrConfig};
use crate::vectors::{
    fuse, idf_coverage, is_code_query, short_text_adjusted, term_weights, FusedHit, HybridOptions, VectorIndex,
};

/// OCR pages whose mean Tesseract word confidence is below this are flagged.
/// The values were measured on a labelled 200-document Turkish corpus.
pub const LOW_CONFIDENCE_FLOOR: f32 = 0.60;
const SNIPPET_WIDTH: usize = 220;
const COMMIT_EVERY: usize = 25;
/// Rendered preview pages kept in memory (each a JPEG of ~100–400 KB).
const PAGE_IMAGE_CACHE: usize = 24;
pub const PAGE_IMAGE_MIN_WIDTH: u32 = 200;
pub const PAGE_IMAGE_MAX_WIDTH: u32 = 2400;

/// Page text read into memory for the search path: the first
/// `SNIPPET_READ_BYTES` bytes (incremental blob read) plus whether the
/// stored page continues beyond them.
struct PageRead {
    text: String,
    truncated: bool,
}
/// How much of a stored page the search path may read before a match is
/// decided from the head + anchor-located tail instead of a full decode.
const SNIPPET_READ_BYTES: usize = 512 * 1024;

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IndexPhase {
    Idle,
    Scanning,
    Indexing,
    /// Writing chunk embeddings for files whose extraction finished
    /// (upgrade backfill or an embed that was skipped mid-scan).
    Embedding,
    Committing,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexProgress {
    pub phase: IndexPhase,
    pub total: usize,
    pub done: usize,
    pub failed: usize,
    pub current: Option<String>,
    /// Chunks of the file being embedded (`current`), in the backfill and
    /// during a scan's embeds; done/total remain file counts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks_done: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunks_total: Option<usize>,
    /// Heartbeat while tokenizing or waiting inside a long ONNX run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_elapsed_secs: Option<u64>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub watching: bool,
    /// A cancel was requested and the run is winding down (running OCR is
    /// being stopped).
    pub cancelling: bool,
    /// The last run was cancelled before it finished; what it indexed is
    /// kept and a rescan resumes with the rest.
    pub cancelled: bool,
    /// Second stage, shown next to "text read" (`done/total`): files of this
    /// run whose semantic (embedding) index is finished, out of the files
    /// that need one (readable text, a working model). During a scan the
    /// total grows as text is read; in the backfill it is the pending list.
    /// Both stay 0 without a model.
    pub embed_done: usize,
    pub embed_total: usize,
    /// First stage: files whose text is read (or failed to read) in this
    /// run, counted before their embed starts — `done` waits for the embed.
    pub text_done: usize,
    /// Files a cancelled scan never read (0 otherwise).
    pub unread: usize,
    /// Set while idle: catalogued files whose text is read but whose
    /// semantic index is pending (embedded=0). `None` without a model.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_embed: Option<usize>,
}

impl IndexProgress {
    /// A fresh run starting in `phase`.
    fn start(phase: IndexPhase, watching: bool) -> Self {
        IndexProgress {
            phase,
            total: 0,
            done: 0,
            failed: 0,
            current: None,
            chunks_done: None,
            chunks_total: None,
            embedding_elapsed_secs: None,
            started_at: Some(now()),
            finished_at: None,
            watching,
            cancelling: false,
            cancelled: false,
            embed_done: 0,
            embed_total: 0,
            text_done: 0,
            unread: 0,
            pending_embed: None,
        }
    }

    fn clear_embedding(&mut self) {
        self.chunks_done = None;
        self.chunks_total = None;
        self.embedding_elapsed_secs = None;
    }

}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snippet {
    pub page: Option<usize>,
    pub text: String,
    pub highlights: Vec<(usize, usize)>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchHit {
    pub path: String,
    pub file_name: String,
    pub folder: String,
    pub kind: FileKind,
    pub size: i64,
    pub modified: i64,
    pub page_count: i64,
    pub low_confidence_pages: i64,
    pub ocr_pages: i64,
    /// Pages that could not be read (render/OCR failure); not searchable.
    pub failed_pages: i64,
    /// In the results only because the vector index ranked it (RRF fusion);
    /// the BM25 side found nothing — the "anlamca benzer" badge.
    pub semantic_only: bool,
    /// Found only by its file name (no page text matched).
    pub name_only: bool,
    pub score: f32,
    pub snippets: Vec<Snippet>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub total: usize,
    pub hits: Vec<SearchHit>,
    pub took_ms: u64,
}

/// Wall time of one search per stage, in ms (latency work).
/// `catalog_wait` is the time spent waiting for the catalog lock and is
/// also part of the stage it happened in.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchTiming {
    /// Query embedding (tokenizer + the query session).
    pub embed: f64,
    /// Tantivy (BM25 candidates and total).
    pub bm25: f64,
    /// Head coverage: per-term file counts and page-prefix reads.
    pub coverage: f64,
    /// Per-term unfiltered file counts, also broken down in `coverage_terms`.
    pub coverage_counts: f64,
    /// Catalog document-count and page-prefix reads (excluding lock waits).
    pub coverage_reads: f64,
    /// Matching the page prefixes against the query terms.
    pub coverage_match: f64,
    pub coverage_terms: Vec<(String, f64)>,
    /// Page-prefix read attempts and bytes returned by successful reads.
    pub coverage_page_reads: usize,
    pub coverage_bytes: usize,
    /// Vector scan (part of `fusion`).
    pub vectors: f64,
    /// Vector candidates fused with BM25 (includes `vectors`).
    pub fusion: f64,
    /// Snippet page reads and matching.
    pub snippets: f64,
    pub catalog_wait: f64,
    pub total: f64,
}

#[derive(Clone, Debug)]
pub struct SearchRequest {
    pub query: String,
    pub kinds: Vec<FileKind>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
    pub limit: usize,
    pub offset: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageView {
    pub index: usize,
    pub text: String,
    pub source: TextSource,
    pub confidence: Option<f32>,
    pub low_confidence: bool,
    pub highlights: Vec<(usize, usize)>,
    /// Why this page could not be read; `None` for readable pages.
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocumentView {
    pub path: String,
    pub file_name: String,
    pub kind: FileKind,
    pub size: i64,
    pub modified: i64,
    pub pages: Vec<PageView>,
}

/// A rendered page for the preview's "Sayfa" tab.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageImage {
    pub page: usize,
    pub width: u32,
    pub height: u32,
    /// `data:image/jpeg;base64,…` (allowed by the CSP's `img-src data:`).
    pub data_url: String,
}

type PageImageKey = (String, usize, u32, i64);

/// Typed failure codes matching docs/API.md `ErrorCode`.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{code}: {message}")]
    Coded { code: &'static str, message: String },
}

impl EngineError {
    pub fn code(&self) -> &'static str {
        match self {
            EngineError::Coded { code, .. } => code,
        }
    }
    fn new(code: &'static str, message: impl Into<String>) -> anyhow::Error {
        EngineError::Coded { code, message: message.into() }.into()
    }
}

/// Document languages a user can pick for scanned pages, as Tesseract
/// language codes. `tur+eng` reads mixed archives at about 1.6× the OCR time
/// of a single language. On noisy synthetic scans Turkish-only OCR misread
/// 44% of an English page and `tur+eng` 20%, with Turkish pages at 6-7%.
pub const OCR_LANGUAGES: &[&str] = &["tur", "eng", "tur+eng"];
/// Catalog setting holding the chosen document language.
const OCR_LANGUAGE_SETTING: &str = "ocr_language";

/// The document-language setting as the UI shows it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OcrLanguageInfo {
    /// Current Tesseract language code (`tur`, `eng`, `tur+eng`).
    pub language: String,
    /// The choices whose models are installed, in `OCR_LANGUAGES` order.
    pub available: Vec<String>,
    /// Indexed scans read with another language, still to be read again.
    pub pending_rereads: i64,
}

pub struct EnginePaths {
    pub data_dir: PathBuf,
    pub pdfium_dir: PathBuf,
    pub ocr: OcrConfig,
    /// Concurrent Tesseract processes across all files and pages;
    /// 0 = `max(1, cores - 1)`.
    pub ocr_jobs: usize,
    /// Level skewed scans before OCR (see `deskew.rs`);
    /// the app turns it on.
    pub deskew: bool,
    /// Semantic embedding pieces (ONNX runtime + model + tokenizer). `None`
    /// disables semantic ranking entirely (no model bundled, e.g. tests).
    pub embed: Option<EmbedConfig>,
}

enum Job {
    Folder(i64),
    All,
    Paths(Vec<PathBuf>),
    /// Drop a folder's records whose kind is no longer selected. Quick (no
    /// extraction) and never discarded by a cancel.
    Purge(i64),
    /// Run the embedding backfill now ("Şimdi tamamla" after a cancel).
    Embed,
    /// Scan every folder again after a removal stopped a run. Like `Purge`,
    /// never discarded by the cancel that removal caused.
    Resume,
    /// Tests: answered once every job queued before it has run (or dropped
    /// by a cancel, which drops the sender).
    #[cfg(test)]
    Barrier(Sender<()>),
}

pub type ProgressSink = Arc<dyn Fn(&IndexProgress) + Send + Sync>;
/// Optional extra observer used by the blocking evaluation path.
type Observer<'a> = Option<&'a (dyn Fn(&IndexProgress) + Sync)>;
pub type ChangedSink = Arc<dyn Fn(i64) + Send + Sync>;

pub struct Engine {
    catalog: Arc<Mutex<Catalog>>,
    index: Arc<SearchIndex>,
    extractor: Arc<Extractor>,
    embedder: Option<Arc<Embedder>>,
    vectors: Arc<VectorIndex>,
    hybrid: parking_lot::RwLock<HybridOptions>,
    progress: Arc<Mutex<IndexProgress>>,
    jobs: Sender<Job>,
    cancel: Arc<AtomicBool>,
    on_progress: ProgressSink,
    on_changed: ChangedSink,
    // No file-id cache: on Windows it opens every file under every folder each
    // time a folder is watched (seconds to minutes on a large tree), and the
    // worker classifies each changed path itself, renames included.
    watcher: Mutex<Option<Debouncer<notify::RecommendedWatcher, NoCache>>>,
    /// Serializes every catalog/index mutation a scan performs: the worker's
    /// queued jobs, `index_folder_blocking`, `remove_folder` and the
    /// embedding backfill. Without it a synchronous caller could scan a
    /// folder while the worker's own scan of it was in flight (same files
    /// extracted twice, torn stores) and a folder removal could resurrect
    /// its records through an in-flight store.
    scan: Arc<Mutex<()>>,
    index_dir: PathBuf,
    ready: Arc<Ready>,
    page_images: Mutex<lru::LruCache<PageImageKey, Arc<PageImage>>>,
}

/// Set once the start-up work (index rebuild, if any, and the initial rescan)
/// has finished.
#[derive(Default)]
struct Ready {
    done: Mutex<bool>,
    signal: Condvar,
}

impl Ready {
    fn set(&self) {
        *self.done.lock() = true;
        self.signal.notify_all();
    }
}

fn now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn file_times(path: &Path) -> Option<(i64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let modified = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs() as i64)?;
    Some((meta.len() as i64, modified))
}

fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

impl Engine {
    pub fn open(paths: EnginePaths, on_progress: ProgressSink, on_changed: ChangedSink) -> Result<Arc<Self>> {
        let index_dir = paths.data_dir.join("index");
        let catalog = Arc::new(Mutex::new(Catalog::open(&paths.data_dir.join("catalog.db"))?));
        let index = Arc::new(SearchIndex::open(&index_dir)?);
        // Rebuild from the catalog when the index was discarded (schema bump)
        // or lost while the catalog still knows files.
        let rebuild = index.discarded_stale() || (index.page_count() == 0 && catalog.lock().document_count()? > 0);
        // The user's document language, unless its models went missing.
        let mut ocr = paths.ocr;
        if let Some(language) = catalog.lock().setting(OCR_LANGUAGE_SETTING)? {
            let candidate = OcrConfig { language, ..ocr.clone() };
            if OCR_LANGUAGES.contains(&candidate.language.as_str()) && candidate.missing_models().is_empty() {
                ocr = candidate;
            }
        }
        let extractor = Arc::new(Extractor::new(
            &paths.pdfium_dir,
            ocr,
            paths.data_dir.join("scratch"),
            paths.ocr_jobs,
        )
        .with_deskew(paths.deskew));
        let progress = Arc::new(Mutex::new(IndexProgress {
            started_at: None,
            ..IndexProgress::start(IndexPhase::Idle, false)
        }));
        let (jobs, receiver) = channel();
        let embedder = paths.embed.map(|config| Arc::new(Embedder::new(config)));
        // The vector index mirrors the catalog's stored chunks; the worker
        // keeps it in step on every store/forget.
        let vectors = Arc::new(VectorIndex::empty());
        if embedder.is_some() {
            let catalog = catalog.lock();
            let mut by_path: HashMap<String, (FileKind, i64, Vec<(usize, Vec<f32>)>)> = HashMap::new();
            for (path, kind, modified, page, vector) in catalog.all_chunks()? {
                by_path.entry(path).or_insert((kind, modified, Vec::new())).2.push((page, vector));
            }
            for (path, (kind, modified, chunks)) in by_path {
                vectors.set(&path, kind, modified, &chunks);
            }
        }
        let engine = Arc::new(Self {
            catalog,
            index,
            extractor,
            embedder,
            vectors,
            hybrid: parking_lot::RwLock::new(HybridOptions::default()),
            progress,
            jobs,
            cancel: Arc::new(AtomicBool::new(false)),
            on_progress: on_progress.clone(),
            on_changed: on_changed.clone(),
            watcher: Mutex::new(None),
            scan: Arc::new(Mutex::new(())),
            index_dir,
            ready: Arc::new(Ready::default()),
            page_images: Mutex::new(lru::LruCache::new(
                std::num::NonZeroUsize::new(PAGE_IMAGE_CACHE).expect("non-zero cache"),
            )),
        });
        let worker = Worker {
            catalog: engine.catalog.clone(),
            index: engine.index.clone(),
            extractor: engine.extractor.clone(),
            embedder: engine.embedder.clone(),
            vectors: engine.vectors.clone(),
            progress: engine.progress.clone(),
            scan: engine.scan.clone(),
            cancel: engine.cancel.clone(),
            on_progress,
            on_changed,
        };
        let ready = engine.ready.clone();
        std::thread::Builder::new()
            .name("arsiv-indexer".into())
            .spawn(move || worker.run(receiver, rebuild, &ready))?;
        engine.restart_watcher()?;
        Ok(engine)
    }

    pub fn ocr_status(&self) -> Result<String, String> {
        self.extractor.ocr_status()
    }
    pub fn pdf_status(&self) -> Result<(), String> {
        self.extractor.pdf_status()
    }
    pub fn index_dir(&self) -> &Path {
        &self.index_dir
    }

    /// Blocks until start-up indexing (catalog rebuild + initial rescan) is
    /// done. Headless callers use it so results reflect the whole index.
    pub fn wait_ready(&self) {
        let mut done = self.ready.done.lock();
        while !*done {
            self.ready.signal.wait(&mut done);
        }
    }

    /// Enables/disables inflection expansion for the search index (eval A/B).
    pub fn set_search_expansion(&self, enabled: bool) {
        self.index.set_expansion(enabled);
    }

    pub fn search_options(&self) -> SearchOptions {
        self.index.options()
    }

    /// Query-time matching switches (eval A/B); defaults are production.
    pub fn set_search_options(&self, options: SearchOptions) {
        self.index.set_options(options);
    }

    pub fn document_count(&self) -> Result<i64> {
        self.catalog.lock().document_count()
    }

    pub fn ocr_language(&self) -> Result<OcrLanguageInfo> {
        let language = self.extractor.ocr_language();
        let available = OCR_LANGUAGES
            .iter()
            .filter(|l| self.extractor.missing_models(l).is_empty())
            .map(|l| l.to_string())
            .collect();
        let pending_rereads = self.catalog.lock().files_to_reread(&language)?;
        Ok(OcrLanguageInfo { language, available, pending_rereads })
    }

    /// Sets the document language for scanned pages and saves it. Scans
    /// already read with another language are read again in the background
    /// (their current text stays searchable until then).
    pub fn set_ocr_language(&self, language: &str) -> Result<OcrLanguageInfo> {
        if !OCR_LANGUAGES.contains(&language) || !self.extractor.missing_models(language).is_empty() {
            return Err(EngineError::new("ocr_language_unavailable", language));
        }
        self.catalog.lock().set_setting(OCR_LANGUAGE_SETTING, language)?;
        let changed = self.extractor.ocr_language() != language;
        self.extractor.set_ocr_language(language);
        let info = self.ocr_language()?;
        if changed && info.pending_rereads > 0 {
            self.jobs.send(Job::All).map_err(|_| anyhow!("indexer stopped"))?;
        }
        Ok(info)
    }

    pub fn folders(&self) -> Result<Vec<FolderInfo>> {
        let mut folders = self.catalog.lock().folders()?;
        let semantic = self.embedder.as_ref().is_some_and(|e| !e.failed());
        for folder in &mut folders {
            folder.missing = !Path::new(&folder.path).is_dir();
            if !semantic {
                // Nothing will ever embed them: not "pending".
                folder.pending_embed_count = 0;
            }
        }
        Ok(folders)
    }

    /// Adds a folder indexing only `kinds` (every kind when `None`).
    pub fn add_folder(&self, path: &str, kinds: Option<&[FileKind]>) -> Result<FolderInfo> {
        let folder = Path::new(path);
        if !folder.is_dir() {
            return Err(EngineError::new("folder_missing", format!("Not a folder: {path}")));
        }
        let id = {
            let catalog = self.catalog.lock();
            catalog
                .add_folder_with_kinds(path, kinds.unwrap_or(&FileKind::DEFAULT))
                .map_err(|e| match e.to_string().as_str() {
                    "folder_duplicate" => EngineError::new("folder_duplicate", path),
                    "folder_nested" => EngineError::new("folder_nested", path),
                    "kinds_empty" => EngineError::new("kinds_empty", "Select at least one file kind"),
                    _ => e,
                })?
        };
        self.watch_folder(path)?;
        self.jobs.send(Job::Folder(id)).map_err(|_| anyhow!("indexer stopped"))?;
        self.folders()?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| anyhow!("folder vanished"))
    }

    /// Changes which kinds a folder indexes. Records of kinds no longer
    /// selected are dropped (queued ahead of any later scan, kept even if a
    /// run is cancelled); newly selected kinds are picked up by a rescan of
    /// the folder. The watcher applies the new selection from its next event.
    pub fn set_folder_kinds(&self, id: i64, kinds: &[FileKind]) -> Result<FolderInfo> {
        let previous = {
            let catalog = self.catalog.lock();
            let previous = catalog
                .folder_kinds(id)?
                .ok_or_else(|| EngineError::new("folder_missing", format!("No folder with id {id}")))?;
            catalog.set_folder_kinds(id, kinds).map_err(|e| match e.to_string().as_str() {
                "kinds_empty" => EngineError::new("kinds_empty", "Select at least one file kind"),
                _ => e,
            })?;
            previous
        };
        let send = |job| self.jobs.send(job).map_err(|_| anyhow!("indexer stopped"));
        if previous.iter().any(|kind| !kinds.contains(kind)) {
            send(Job::Purge(id))?;
        }
        if kinds.iter().any(|kind| !previous.contains(kind)) {
            send(Job::Folder(id))?;
        }
        self.folders()?
            .into_iter()
            .find(|f| f.id == id)
            .ok_or_else(|| anyhow!("folder vanished"))
    }

    /// Asks the running indexing run to stop: no new file starts, running
    /// Tesseract processes are killed, files not finished are not written
    /// (neither as indexed nor as failed) and queued scans are dropped. What
    /// was indexed stays; `IndexProgress.cancelled` reports the outcome and a
    /// rescan continues with the rest. Returns false when nothing was running.
    pub fn cancel_indexing(&self) -> bool {
        let snapshot = {
            let mut progress = self.progress.lock();
            if progress.phase == IndexPhase::Idle {
                return false;
            }
            self.cancel.store(true, Ordering::SeqCst);
            progress.cancelling = true;
            progress.clone()
        };
        (self.on_progress)(&snapshot);
        true
    }

    pub fn remove_folder(&self, id: i64) -> Result<()> {
        // A scan in flight could still `store` this folder's files after
        // the deletion and resurrect them as failures. Cancel it first
        // (bounded: a scan stops within seconds) so the scan lock below is
        // not held for the rest of the run.
        // The run it stops may be scanning other folders too: those resume
        // afterwards (unless the user had already asked to stop).
        let resume = !self.progress.lock().cancelling && self.cancel_indexing();
        let (count, watcher_result) = {
            let _scan = self.scan.lock();
            let folder_path = self.catalog.lock().folder_path(id)?;
            let paths = self.catalog.lock().remove_folder(id)?;
            let mut writer = self.index.writer().map_err(|e| EngineError::new("index_busy", format!("{e:#}")))?;
            for path in &paths {
                self.index.delete_path(&writer, path);
                self.vectors.remove(path);
            }
            writer.commit()?;
            self.index.reload()?;
            let count = self.document_count()?;
            (count, folder_path.map_or(Ok(()), |path| self.unwatch_folder(&path)))
        };
        if resume && !self.catalog.lock().folder_ids()?.is_empty() {
            // Survives the cancel's queue drain (see `settle`).
            let _ = self.jobs.send(Job::Resume);
        }
        // The visible results must refresh after a removal too. Observers
        // may read or search the engine: release catalog, writer and scan
        // locks before calling them. Deletions still changed the index if
        // restarting the watcher failed, so notify before returning that error.
        (self.on_changed)(count);
        watcher_result
    }

    /// Starts the embedding backfill now: files whose text is read but whose
    /// semantic index is pending (a cancel stopped it). Without this they
    /// are finished by the next scan. Returns at once; progress events
    /// report the `embedding` phase.
    pub fn complete_embeddings(&self) -> Result<()> {
        self.jobs.send(Job::Embed).map_err(|_| anyhow!("indexer stopped"))
    }

    pub fn reindex(&self, folder_id: Option<i64>) -> Result<()> {
        // A folder that was missing may be back: watch it again.
        self.restart_watcher()?;
        let job = folder_id.map_or(Job::All, Job::Folder);
        self.jobs.send(job).map_err(|_| anyhow!("indexer stopped"))
    }

    pub fn progress(&self) -> IndexProgress {
        self.progress.lock().clone()
    }

    pub fn failures(&self, folder_id: Option<i64>) -> Result<Vec<FailureInfo>> {
        self.catalog.lock().failures(folder_id)
    }

    /// "Tekrar dene" on unreadable / partly read files: they are read again
    /// now (and by every later scan until they succeed or fail anew).
    pub fn retry_failures(&self, folder_id: Option<i64>) -> Result<usize> {
        let paths = self.catalog.lock().retry_failures(folder_id)?;
        let count = paths.len();
        if count > 0 {
            self.jobs
                .send(Job::Paths(paths.into_iter().map(PathBuf::from).collect()))
                .map_err(|_| anyhow!("indexer stopped"))?;
        }
        Ok(count)
    }

    fn restart_watcher(&self) -> Result<()> {
        let folders = self.catalog.lock().folder_ids()?;
        let jobs = self.jobs.clone();
        let mut slot = self.watcher.lock();
        *slot = None;
        if folders.is_empty() {
            self.progress.lock().watching = false;
            return Ok(());
        }
        // Directory events matter too (folder deleted/renamed/moved in), so
        // paths are not filtered here; `Worker::update_paths` classifies them.
        let handler = move |result: DebounceEventResult| {
            if let Ok(events) = result {
                let paths: HashSet<PathBuf> = events
                    .into_iter()
                    .filter(|e| may_change_content(&e.event.kind))
                    .flat_map(|e| e.event.paths.clone())
                    .collect();
                if !paths.is_empty() {
                    let _ = jobs.send(Job::Paths(paths.into_iter().collect()));
                }
            }
        };
        let mut debouncer = new_debouncer_opt::<_, notify::RecommendedWatcher, NoCache>(
            Duration::from_secs(2),
            None,
            handler,
            NoCache,
            notify::Config::default(),
        )?;
        let mut watching = false;
        for (_, folder) in &folders {
            watching |= debouncer.watch(Path::new(folder), RecursiveMode::Recursive).is_ok();
        }
        self.progress.lock().watching = watching;
        *slot = Some(debouncer);
        Ok(())
    }

    /// Starts watching one more folder without rebuilding the watcher.
    fn watch_folder(&self, path: &str) -> Result<()> {
        {
            let mut slot = self.watcher.lock();
            if let Some(debouncer) = slot.as_mut() {
                let ok = debouncer.watch(Path::new(path), RecursiveMode::Recursive).is_ok();
                let mut progress = self.progress.lock();
                progress.watching |= ok;
                return Ok(());
            }
        }
        self.restart_watcher()
    }

    /// Stops watching a removed folder; the others keep their watches.
    fn unwatch_folder(&self, path: &str) -> Result<()> {
        let remaining = self.catalog.lock().folder_ids()?.len();
        let mut slot = self.watcher.lock();
        if remaining == 0 {
            *slot = None;
            self.progress.lock().watching = false;
        } else if let Some(debouncer) = slot.as_mut() {
            // A folder that went missing was never watched: nothing to undo.
            let _ = debouncer.unwatch(Path::new(path));
        }
        Ok(())
    }

    /// Hybrid ranking switches (eval A/B); defaults are lab-measured.
    pub fn hybrid_options(&self) -> HybridOptions {
        *self.hybrid.read()
    }

    pub fn set_hybrid_options(&self, options: HybridOptions) {
        *self.hybrid.write() = options;
    }

    /// Embedder verdict, same contract as `ocr_status`.
    pub fn semantic_status(&self) -> Result<String, String> {
        match &self.embedder {
            Some(embedder) => embedder.check(),
            None => Err("No embedding model is bundled".into()),
        }
    }

    /// Chunks currently in the vector index (diagnostics/tests).
    pub fn vector_chunk_count(&self) -> usize {
        self.vectors.chunk_count()
    }

    pub fn search(&self, request: &SearchRequest) -> Result<SearchResponse> {
        Ok(self.search_timed(request)?.0)
    }

    /// `search` plus how long each stage took (`SearchTiming`); the results
    /// are the same. `ARSIV_SEARCH_TIMING=1` prints the stages to stderr.
    pub fn search_timed(&self, request: &SearchRequest) -> Result<(SearchResponse, SearchTiming)> {
        let started = Instant::now();
        let print = std::env::var_os("ARSIV_SEARCH_TIMING").is_some();
        let mut timing = SearchTiming::default();
        let mut stage = Instant::now();
        let mut lap = |slot: &mut f64| {
            let now = Instant::now();
            *slot = ms(now - stage);
            stage = now;
        };
        if query_terms(&request.query).is_empty() {
            return Err(EngineError::new("query_empty", "Query has no searchable words"));
        }
        let limit = request.limit.clamp(1, 200);
        let filter = SearchFilter {
            kinds: &request.kinds,
            modified_after: request.modified_after,
            modified_before: request.modified_before,
        };
        let hybrid = *self.hybrid.read();
        // The vector side only fuses when an embedder is live and the vector
        // index has something to compare against; otherwise pure BM25.
        // Number/code lookups (`1548`, `ÖR-CAHNFY-27454`) keep the vector side
        // too: vector-only look-alikes are dropped in `fused_hits`, but the
        // vector ranks still reorder the BM25 matches. OCR garbles codes
        // (`ÖR-BEĞUNF-66337` read as `ÖRMEĞUNE 66VT`), and skipping the vector
        // side dropped that lab query out of the top 10 (found in review).
        let vector_query = if hybrid.vec_weight > 0.0 && !self.vectors.is_empty() {
            self.embedder.as_ref().and_then(|embedder| {
                let text = request.query.replace(['"', '“', '”'], " ");
                embedder.embed_query(text.trim()).ok()
            })
        } else {
            None
        };
        lap(&mut timing.embed);
        // Hybrid needs a deeper BM25 candidate pool than the answer window:
        // a relevant file the vector side promotes may sit below it.
        let depth = if vector_query.is_some() {
            (request.offset + limit).max(hybrid.candidates)
        } else {
            request.offset + limit
        };
        let results = self.index.search_counted(&request.query, &filter, depth)?;
        lap(&mut timing.bm25);
        let matcher = Matcher::new(&query_terms(&request.query), &self.index.options());
        // A quoted phrase is exact-only: the BM25 side enforces it, so
        // vector-only candidates must not bypass it.
        let phrase_required = !quoted_phrases(&request.query).is_empty();
        // Keyword confidence of the BM25 head (see HybridOptions::head_coverage).
        // Page text read for coverage is a bounded blob-prefix on huge pages
        // and is reused for the snippet pass: a multi-MB log page is never
        // fully decoded on the search path.
        let mut page_texts: HashMap<(String, usize), PageRead> = HashMap::new();
        let coverage = if vector_query.is_some() && hybrid.head_coverage > 0.0 {
            self.head_coverage(&request.query, &results, &hybrid, &mut page_texts, &mut timing)
        } else {
            Vec::new()
        };
        lap(&mut timing.coverage);
        let catalog = self.catalog_timed(&mut timing.catalog_wait);
        let ranked = self.fused_hits(
            &catalog,
            &request.query,
            &page_texts,
            &results,
            &coverage,
            vector_query.as_deref(),
            &filter,
            &hybrid,
            phrase_required,
            request.offset + limit,
            &mut timing.vectors,
        );
        // Distinct matching files: BM25 matches plus vector candidates that
        // matched no BM25 term.
        let mut ranked = ranked;
        let mut total = results.total_files
            + if vector_query.is_some() {
                ranked.iter().filter(|h| !results.matched_paths.contains(&h.path)).count()
            } else {
                0
            };
        // File-name matches the content side did not find (a photo with no
        // text, "IMG_2023…", a scan named after its subject) come first,
        // newest first: every query word is in the name, which is the
        // strongest signal a user gives. A quoted phrase stays content-only.
        let mut name_only: HashSet<String> = HashSet::new();
        if !phrase_required {
            let words: Vec<String> = query_terms(&request.query)
                .iter()
                .map(|t| crate::turkish::ascii_fold(t).replace(['\'', '’'], ""))
                .filter(|w| !w.is_empty())
                .collect();
            let shown: HashSet<&str> = ranked.iter().map(|h| h.path.as_str()).collect();
            let mut named: Vec<(String, i64)> = catalog
                .files_named(&words)?
                .into_iter()
                .filter(|(path, kind, modified)| {
                    !results.matched_paths.contains(path)
                        && !shown.contains(path.as_str())
                        && (filter.kinds.is_empty() || filter.kinds.contains(kind))
                        && filter.modified_after.is_none_or(|after| *modified >= after)
                        && filter.modified_before.is_none_or(|before| *modified <= before)
                })
                .map(|(path, _, modified)| (path, modified))
                .collect();
            named.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            total += named.len();
            let mut first: Vec<FusedHit> = Vec::with_capacity(named.len() + ranked.len());
            for (path, _) in named {
                name_only.insert(path.clone());
                first.push(FusedHit { path, page: 0, score: 0.0, semantic_only: false });
            }
            first.append(&mut ranked);
            ranked = first;
        }
        lap(&mut timing.fusion);
        let mut hits = Vec::new();
        // Identical files (same content_hash, e.g. logcat dumps kept in two
        // folders) get one snippet computation each — the text, the scan and
        // the highlights are the same work done twice otherwise.
        let mut snippet_cache: HashMap<(String, usize), Vec<(String, Vec<(usize, usize)>)>> = HashMap::new();
        for hit in ranked.into_iter().skip(request.offset).take(limit) {
            let hit_started = Instant::now();
            let Some(record) = catalog.file(&hit.path)? else { continue };
            let mut found = Vec::new();
            let dedup = record.content_hash.clone().unwrap_or_else(|| hit.path.clone());
            if let Some(cached) = snippet_cache.get(&(dedup.clone(), hit.page)) {
                found = cached.iter().map(|(text, highlights)| Snippet { page: Some(hit.page), text: text.clone(), highlights: highlights.clone() }).collect();
            } else {
                let mut read = match page_texts.remove(&(hit.path.clone(), hit.page)) {
                    Some(read) => Some(read),
                    // The snippet pass only ever scans the first 256 KiB plus
                    // anchor-located far matches, so a multi-MB page is read
                    // incrementally (blob API) instead of one big decode.
                    None => {
                        let t = Instant::now();
                        let r = catalog.page_text_prefix(&hit.path, hit.page, SNIPPET_READ_BYTES)?.map(|(text, truncated)| PageRead { text, truncated });
                        if print {
                            eprintln!("    read {:?} {:.1} ms", hit.path, ms(t.elapsed()));
                        }
                        r
                    },
                };
                if let Some(read) = read.take() {
                    // Vector-only results need context, without spending time
                    // searching a page for terms BM25 did not find.
                    let excerpts = if hit.semantic_only || name_only.contains(&hit.path) {
                        lead_snippet(&read.text, SNIPPET_WIDTH)
                    } else {
                        snippets(&read.text, &matcher, 2, SNIPPET_WIDTH)
                    };
                    if print {
                        eprintln!("    scan {:?} {:.1} ms", hit.path, ms(hit_started.elapsed()));
                    }
                    for (text, highlights) in excerpts {
                        found.push(Snippet { page: Some(hit.page), text, highlights });
                    }
                }
                snippet_cache.insert((dedup, hit.page), found.iter().map(|s| (s.text.clone(), s.highlights.clone())).collect());
            }
            if print {
                eprintln!("  hit {:?} p{} {:.1} ms", hit.path, hit.page, ms(hit_started.elapsed()));
            }
            let path = Path::new(&hit.path);
            let is_name_only = name_only.contains(&hit.path);
            hits.push(SearchHit {
                file_name: path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                folder: path.parent().map(path_key).unwrap_or_default(),
                path: hit.path,
                kind: record.kind,
                size: record.size,
                modified: record.modified,
                page_count: record.page_count,
                low_confidence_pages: record.low_confidence_pages,
                ocr_pages: record.ocr_pages,
                failed_pages: record.failed_pages,
                semantic_only: hit.semantic_only,
                name_only: is_name_only,
                score: hit.score,
                snippets: found,
            });
        }
        drop(catalog);
        lap(&mut timing.snippets);
        timing.total = ms(started.elapsed());
        if print {
            eprintln!(
                "search-timing query={:?} total={:.1} embed={:.1} bm25={:.1} coverage={:.1} fusion={:.1} vectors={:.1} snippets={:.1} catalog_wait={:.1}",
                request.query, timing.total, timing.embed, timing.bm25, timing.coverage, timing.fusion, timing.vectors, timing.snippets, timing.catalog_wait
            );
        }
        Ok((SearchResponse { total, hits, took_ms: started.elapsed().as_millis() as u64 }, timing))
    }

    /// The catalog lock, adding the time spent waiting for it to `wait` (ms).
    fn catalog_timed(&self, wait: &mut f64) -> parking_lot::MutexGuard<'_, Catalog> {
        let asked = Instant::now();
        let guard = self.catalog.lock();
        *wait += ms(asked.elapsed());
        guard
    }

    /// IDF coverage of the first `bm25_head` BM25 hits: each query term is
    /// weighted by how few files contain it (unfiltered corpus statistic),
    /// and a hit scores the weight share of the terms its page contains.
    fn head_coverage(
        &self,
        query: &str,
        results: &SearchResults,
        hybrid: &HybridOptions,
        page_texts: &mut HashMap<(String, usize), PageRead>,
        timing: &mut SearchTiming,
    ) -> Vec<f32> {
        let terms = query_terms(query);
        let head = hybrid.bm25_head.min(results.hits.len());
        if terms.len() < 2 || head == 0 {
            // One known word: any page that matched it covers the query.
            return Vec::new();
        }
        let everything = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let counts_started = Instant::now();
        let term_files: Vec<usize> = terms
            .iter()
            .map(|t| {
                let started = Instant::now();
                let files = self.index.count_files(t, &everything).unwrap_or(0);
                timing.coverage_terms.push((t.clone(), ms(started.elapsed())));
                files
            })
            .collect();
        timing.coverage_counts += ms(counts_started.elapsed());
        let files = {
            let catalog = self.catalog_timed(&mut timing.catalog_wait);
            let started = Instant::now();
            let files = catalog.document_count().unwrap_or(0).max(0) as usize;
            timing.coverage_reads += ms(started.elapsed());
            files
        };
        let weights = term_weights(&term_files, files.max(1));
        let options = self.index.options();
        let matchers: Vec<Matcher> = terms.iter().map(|t| Matcher::new(std::slice::from_ref(t), &options)).collect();
        let catalog = self.catalog_timed(&mut timing.catalog_wait);
        results.hits[..head]
            .iter()
            .map(|hit| {
                let started = Instant::now();
                let read = catalog.page_text_prefix(&hit.path, hit.page, SNIPPET_READ_BYTES);
                timing.coverage_reads += ms(started.elapsed());
                timing.coverage_page_reads += 1;
                match read {
                    Ok(Some((text, truncated))) => {
                        timing.coverage_bytes += text.len();
                        let started = Instant::now();
                        let covered = idf_coverage(&weights, &terms_present(&text, &matchers));
                        timing.coverage_match += ms(started.elapsed());
                        page_texts.insert((hit.path.clone(), hit.page), PageRead { text, truncated });
                        covered
                    }
                    _ => 1.0,
                }
            })
            .collect()
    }

    /// BM25 hits fused with vector candidates (RRF). When the query carries
    /// a required (quoted) phrase, vector candidates that matched no BM25
    /// clause are dropped before fusion: an exact-match requirement must not
    /// be bypassed by semantic similarity alone.
    #[allow(clippy::too_many_arguments)]
    fn fused_hits(
        &self,
        catalog: &Catalog,
        query_text: &str,
        page_texts: &HashMap<(String, usize), PageRead>,
        results: &SearchResults,
        coverage: &[f32],
        vector_query: Option<&[f32]>,
        filter: &SearchFilter,
        hybrid: &HybridOptions,
        phrase_required: bool,
        needed: usize,
        vectors_ms: &mut f64,
    ) -> Vec<FusedHit> {
        let Some(query) = vector_query else {
            return results
                .hits
                .iter()
                .map(|hit| FusedHit { path: hit.path.clone(), page: hit.page, score: hit.score, semantic_only: false })
                .collect();
        };
        let scan = Instant::now();
        let mut vectors = self.vectors.search(query, filter, hybrid);
        *vectors_ms = ms(scan.elapsed());
        if phrase_required {
            vectors.retain(|hit| results.matched_paths.contains(hit.path.as_str()));
        }
        // A number/code lookup is exact: files that match none of its words
        // are look-alikes, not answers (HybridOptions::code_vec_only_cosine).
        if is_code_query(query_text) {
            vectors.retain(|hit| {
                results.matched_paths.contains(hit.path.as_str()) || hit.score >= hybrid.code_vec_only_cosine
            });
        }
        if hybrid.short_text_chars > 0 && hybrid.short_text_penalty > 0.0 {
            for hit in vectors.iter_mut() {
                // The stored `chars` column measures the page: a page already
                // loaded for coverage is only a prefix there, so the penalty
                // counts its read length plus a "truncated" tail flag.
                let chars = match page_texts.get(&(hit.path.clone(), hit.page)) {
                    Some(read) => read.text.chars().count() + read.truncated as usize,
                    None => catalog.page_chars(&hit.path, hit.page).ok().flatten().unwrap_or(0),
                };
                hit.score = short_text_adjusted(hit.score, chars, hybrid);
            }
            vectors.retain(|hit| hit.score >= hybrid.min_cosine);
            vectors.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        }
        fuse(&results.hits, coverage, &vectors, hybrid, needed)
    }

    pub fn document(&self, path: &str, query: &str) -> Result<DocumentView> {
        let catalog = self.catalog.lock();
        let record = catalog
            .file(path)?
            .ok_or_else(|| EngineError::new("not_indexed", path.to_string()))?;
        let matcher = Matcher::new(&query_terms(query), &self.index.options());
        let pages = catalog
            .pages(path)?
            .into_iter()
            .map(|page| {
                // Bounded on multi-MB pages (Matcher::page_ranges).
                let highlights = utf16_ranges(&page.text, &matcher.page_ranges(&page.text));
                PageView {
                    low_confidence: is_low_confidence(&page, LOW_CONFIDENCE_FLOOR),
                    index: page.index,
                    source: page.source,
                    confidence: page.confidence,
                    highlights,
                    error: page.error,
                    text: page.text,
                }
            })
            .collect();
        Ok(DocumentView {
            file_name: Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
            path: record.path,
            kind: record.kind,
            size: record.size,
            modified: record.modified,
            pages,
        })
    }

    /// Page `page` of an indexed PDF or image, rendered `width` px wide
    /// (clamped to 200–2400) as JPEG. Recently used pages come from an LRU
    /// cache keyed by path, page, width and the indexed mtime.
    pub fn page_image(&self, path: &str, page: usize, width: u32) -> Result<PageImage> {
        let record = self
            .catalog
            .lock()
            .file(path)?
            .ok_or_else(|| EngineError::new("not_indexed", path.to_string()))?;
        if !matches!(record.kind, FileKind::Pdf | FileKind::Image) {
            return Err(EngineError::new("preview_unavailable", format!("No page image for {} files", record.kind.as_str())));
        }
        if page as i64 >= record.page_count {
            return Err(EngineError::new("preview_unavailable", format!("Page {} of {}", page + 1, record.page_count)));
        }
        let width = width.clamp(PAGE_IMAGE_MIN_WIDTH, PAGE_IMAGE_MAX_WIDTH);
        let key = (path.to_string(), page, width, record.modified);
        if let Some(hit) = self.page_images.lock().get(&key) {
            return Ok(PageImage::clone(hit));
        }
        if !Path::new(path).is_file() {
            return Err(EngineError::new("file_missing", path.to_string()));
        }
        let image = self
            .extractor
            .page_image(Path::new(path), record.kind, page, width)
            .map_err(|e| EngineError::new("preview_unavailable", format!("{e:#}")))?;
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85).encode_image(&image)?;
        use base64::Engine as _;
        let rendered = PageImage {
            page,
            width: image.width(),
            height: image.height(),
            data_url: format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(&jpeg)),
        };
        self.page_images.lock().put(key, Arc::new(rendered.clone()));
        Ok(rendered)
    }

    /// Per-page skew estimates of a PDF/image (deskew diagnostics).
    pub fn skew_estimates(&self, path: &Path, dump: Option<&Path>) -> Result<Vec<Option<f32>>> {
        self.extractor.skew_estimates(path, dump)
    }

    /// Synchronous full index of one folder (used by the evaluation CLI).
    pub fn index_folder_blocking(&self, path: &str, on_progress: &(dyn Fn(&IndexProgress) + Sync)) -> Result<i64> {
        self.wait_ready();
        let worker = self.detached_worker();
        // The scan lock keeps this indivisible against the worker's own
        // queued scans — the same product guarantee the job channel gives
        // the app, which the blocking path used to bypass.
        let _scan = self.scan.lock();
        let id = self.ensure_folder(path)?;
        worker.scan_folders(&[(id, path.to_string())], Some(on_progress))?;
        Ok(id)
    }

    /// Registers `path` unless already known (the eval CLI reuses copied
    /// data dirs); the two-step catalog guard drop before the
    /// `folder_duplicate` arm is required by the non-reentrant mutex.
    fn ensure_folder(&self, path: &str) -> Result<i64> {
        let added = self.catalog.lock().add_folder(path);
        match added {
            Ok(id) => Ok(id),
            Err(e) if e.to_string() == "folder_duplicate" => self.catalog.lock().folder_id(path)?.ok_or(e),
            Err(e) => Err(e),
        }
    }

    /// Same, plus a synchronous embedding pass for whatever the scan left
    /// unembedded (cancelled mid-embed); no-op without an embedder. Scan and
    /// backfill run under one scan lock so a queued worker job can never
    /// interleave between them.
    pub fn index_folder_blocking_full(&self, path: &str, on_progress: &(dyn Fn(&IndexProgress) + Sync)) -> Result<i64> {
        self.wait_ready();
        let worker = self.detached_worker();
        let _scan = self.scan.lock();
        let id = self.ensure_folder(path)?;
        worker.scan_folders(&[(id, path.to_string())], Some(on_progress))?;
        let result = if self.cancel.load(Ordering::Relaxed) {
            Err(Cancelled.into())
        } else {
            worker.backfill_embeddings(Some(on_progress))
        };
        worker.settle(&result, None);
        if result.is_err() {
            on_progress(&self.progress());
        }
        result?;
        Ok(id)
    }

    /// A worker sharing this engine's state, for synchronous callers.
    fn detached_worker(&self) -> Worker {
        Worker {
            catalog: self.catalog.clone(),
            index: self.index.clone(),
            extractor: self.extractor.clone(),
            embedder: self.embedder.clone(),
            vectors: self.vectors.clone(),
            progress: self.progress.clone(),
            scan: self.scan.clone(),
            cancel: self.cancel.clone(),
            on_progress: Arc::new(|_| {}),
            on_changed: Arc::new(|_| {}),
        }
    }
}

struct Worker {
    catalog: Arc<Mutex<Catalog>>,
    index: Arc<SearchIndex>,
    extractor: Arc<Extractor>,
    embedder: Option<Arc<Embedder>>,
    vectors: Arc<VectorIndex>,
    progress: Arc<Mutex<IndexProgress>>,
    cancel: Arc<AtomicBool>,
    scan: Arc<Mutex<()>>,
    on_progress: ProgressSink,
    on_changed: ChangedSink,
}

impl Worker {
    fn run(self, receiver: Receiver<Job>, rebuild: bool, ready: &Ready) {
        if rebuild {
            // Not cancellable (a half-rebuilt index would not be rebuilt
            // again); a cancel request stops the start-up scan below instead.
            let _scan = self.scan.lock();
            let _ = self.rebuild_index();
        }
        // Index everything once at start-up (picks up changes made while closed).
        // The scan lock makes the start-up job indivisible against a
        // synchronous `index_folder_blocking` started by a caller that did
        // not wait for `ready`.
        let result = {
            let _scan = self.scan.lock();
            self.handle(Job::All)
        };
        let flagged = self.settle(&result, Some(&receiver));
        // A cancelled or failed start-up scan must not spawn an embedding
        // run: `handle` reports `Cancelled` and the pending files wait for
        // the next scan, exactly like the post-job rule below. `flagged`
        // catches a cancel that landed after the last file finished.
        if result.is_ok() && !flagged {
            // Files missing embeddings (upgrade migration, or an embed cancelled
            // earlier) finish before `ready`: headless callers then see the full
            // index AND vectors. The UI does not gate on `ready`; the phase is
            // shown in the panel meanwhile.
            let result = {
                let _scan = self.scan.lock();
                self.backfill_embeddings(None)
            };
            self.settle(&result, Some(&receiver));
        }
        ready.set();
        while let Ok(job) = receiver.recv() {
            // An explicit backfill needs no second pass right after it (a
            // file whose embed failed would be retried at once).
            let embed_job = matches!(job, Job::Embed);
            let result = {
                let _scan = self.scan.lock();
                self.handle(job)
            };
            let flagged = self.settle(&result, Some(&receiver));
            // Successful scans may still leave unembedded files (embed was
            // skipped mid-run); finish them in the background. A cancelled
            // job leaves the rest for the next run — no background churn
            // right after "İptal".
            if result.is_ok() && !flagged && !embed_job {
                let result = {
                    let _scan = self.scan.lock();
                    self.backfill_embeddings(None)
                };
                self.settle(&result, Some(&receiver));
            }
        }
    }

    /// Closes a run: one that stopped early (cancel, or an error) is moved
    /// to idle, and after a cancel the queued jobs are dropped — except
    /// purges, which run now. The cancel flag is cleared under the progress
    /// lock, the same lock `Engine::cancel_indexing` sets it under, so a
    /// request can never leak into the next run.
    fn settle(&self, result: &Result<()>, receiver: Option<&Receiver<Job>>) -> bool {
        // Counted before the progress lock (never catalog inside progress).
        let pending = self.pending_embed();
        let (flagged, snapshot) = {
            let mut progress = self.progress.lock();
            let flagged = self.cancel.swap(false, Ordering::SeqCst);
            let stopped_early = progress.phase != IndexPhase::Idle;
            if stopped_early {
                progress.phase = IndexPhase::Idle;
                progress.current = None;
                progress.clear_embedding();
                progress.finished_at = Some(now());
                progress.cancelled = flagged || result.as_ref().is_err_and(is_cancelled);
            }
            if progress.phase == IndexPhase::Idle {
                progress.pending_embed = pending;
            }
            let changed = stopped_early || progress.cancelling;
            progress.cancelling = false;
            (flagged, changed.then(|| progress.clone()))
        };
        if let Some(snapshot) = snapshot {
            (self.on_progress)(&snapshot);
        }
        if flagged {
            if let Some(receiver) = receiver {
                let mut resume = false;
                let purges: Vec<i64> = receiver
                    .try_iter()
                    .filter_map(|job| match job {
                        Job::Purge(id) => Some(id),
                        Job::Resume => {
                            resume = true;
                            None
                        }
                        _ => None,
                    })
                    .collect();
                for id in purges {
                    let _scan = self.scan.lock();
                    let _ = self.purge(id);
                }
                if resume {
                    // The removal that cancelled this run asked for the
                    // other folders to carry on: scan them now.
                    let result = {
                        let _scan = self.scan.lock();
                        self.handle(Job::All)
                    };
                    let flagged = self.settle(&result, Some(receiver));
                    if result.is_ok() && !flagged {
                        let result = {
                            let _scan = self.scan.lock();
                            self.backfill_embeddings(None)
                        };
                        return self.settle(&result, Some(receiver));
                    }
                    return flagged;
                }
            }
        }
        flagged
    }

    fn handle(&self, job: Job) -> Result<()> {
        match job {
            Job::All | Job::Resume => {
                let folders = self.catalog.lock().folder_ids()?;
                self.scan_folders(&folders, None)
            }
            Job::Folder(id) => {
                let Some(path) = self.catalog.lock().folder_path(id)? else { return Ok(()) };
                self.scan_folders(&[(id, path)], None)
            }
            Job::Paths(paths) => self.update_paths(paths),
            Job::Purge(id) => self.purge(id),
            Job::Embed => self.backfill_embeddings(None),
            #[cfg(test)]
            Job::Barrier(done) => {
                let _ = done.send(());
                Ok(())
            }
        }
    }

    /// Files whose text is read but whose semantic index is pending; `None`
    /// when no model is in use (configured but failed to load, or absent).
    fn pending_embed(&self) -> Option<usize> {
        self.embedder.as_ref().filter(|e| !e.failed())?;
        self.catalog.lock().unembedded_count().ok()
    }

    fn folder_kinds(&self, folder_id: i64) -> Result<Vec<FileKind>> {
        Ok(self.catalog.lock().folder_kinds(folder_id)?.unwrap_or_default())
    }

    /// Drops the records (indexed and failed) of kinds the folder no longer
    /// selects, from the catalog and the index, and commits.
    fn purge(&self, folder_id: i64) -> Result<()> {
        let dropped = {
            let catalog = self.catalog.lock();
            let Some(kinds) = catalog.folder_kinds(folder_id)? else { return Ok(()) };
            catalog.paths_outside_kinds(folder_id, &kinds)?
        };
        if dropped.is_empty() {
            return Ok(());
        }
        let mut writer = self.index.writer()?;
        for path in &dropped {
            self.index.delete_path(&writer, path);
            self.vectors.remove(path);
            self.catalog.lock().forget(path)?;
        }
        self.catalog.lock().adjust_file_count(folder_id, -(dropped.len() as i64))?;
        writer.commit()?;
        self.index.reload()?;
        let count = self.catalog.lock().document_count()?;
        (self.on_changed)(count);
        Ok(())
    }

    fn emit(&self, update: impl FnOnce(&mut IndexProgress), extra: Observer) {
        let mut snapshot = {
            let mut progress = self.progress.lock();
            update(&mut progress);
            if progress.phase != IndexPhase::Embedding {
                progress.clear_embedding();
            }
            if progress.phase != IndexPhase::Idle {
                progress.pending_embed = None;
            }
            progress.clone()
        };
        if snapshot.phase == IndexPhase::Idle {
            // A run just ended: say how many files still wait for their
            // semantic index (catalog read outside the progress lock).
            let pending = self.pending_embed();
            let mut progress = self.progress.lock();
            if progress.phase == IndexPhase::Idle {
                progress.pending_embed = pending;
            }
            snapshot.pending_embed = pending;
        }
        (self.on_progress)(&snapshot);
        if let Some(callback) = extra {
            callback(&snapshot);
        }
    }

    /// Re-adds every catalogued file to an empty index from stored page text
    /// (no extraction, no OCR). Used after a schema bump discarded the index.
    fn rebuild_index(&self) -> Result<()> {
        let paths = self.catalog.lock().indexed_paths()?;
        self.emit(
            |p| {
                *p = IndexProgress {
                    total: paths.len(),
                    // A cancel requested during the rebuild is honoured by
                    // the start-up scan that follows.
                    cancelling: p.cancelling,
                    ..IndexProgress::start(IndexPhase::Indexing, p.watching)
                }
            },
            None,
        );
        let mut writer = self.index.writer()?;
        writer.delete_all_documents()?;
        let mut last = Instant::now() - Duration::from_secs(1);
        for (done, path) in paths.iter().enumerate() {
            let (record, pages) = {
                let catalog = self.catalog.lock();
                (catalog.file(path)?, catalog.pages(path)?)
            };
            if let Some(record) = record {
                let document = ExtractedDocument::new(record.kind, pages);
                self.index.add(&writer, path, record.modified, &document)?;
            }
            if last.elapsed() >= Duration::from_millis(200) || done + 1 == paths.len() {
                last = Instant::now();
                self.emit(
                    |p| {
                        p.done = done + 1;
                        p.text_done = done + 1;
                        p.current = Some(path.clone());
                    },
                    None,
                );
            }
        }
        self.emit(|p| p.phase = IndexPhase::Committing, None);
        writer.commit()?;
        self.index.reload()?;
        let count = self.catalog.lock().document_count()?;
        (self.on_changed)(count);
        self.emit(
            |p| {
                p.phase = IndexPhase::Idle;
                p.current = None;
                p.finished_at = Some(now());
            },
            None,
        );
        Ok(())
    }
    fn scan_folders(&self, folders: &[(i64, String)], extra: Observer) -> Result<()> {
        if folders.is_empty() {
            return Ok(());
        }
        self.emit(
            |p| *p = IndexProgress { cancelling: p.cancelling, ..IndexProgress::start(IndexPhase::Scanning, p.watching) },
            extra,
        );
        let mut work = Vec::new();
        let mut vanished = Vec::new();
        for (folder_id, folder) in folders {
            // Records of kinds the folder no longer selects go first, even
            // when the root is missing or the run is cancelled later.
            self.purge(*folder_id)?;
            // A missing root (deleted, renamed, unplugged drive) keeps its
            // records: the UI shows it as not found, nothing is dropped.
            if !Path::new(folder).is_dir() {
                continue;
            }
            let kinds = self.folder_kinds(*folder_id)?;
            let present = self.collect_dir(*folder_id, Path::new(folder), &kinds, &mut work)?;
            let catalog = self.catalog.lock();
            for known in catalog.file_paths(*folder_id)? {
                if !present.contains(&known) {
                    vanished.push(known);
                }
            }
            catalog.finish_scan(*folder_id, present.len() as i64, now())?;
        }
        self.process(work, vanished, extra)
    }

    /// Walks `dir`, queues files of the folder's `kinds` that changed since
    /// they were catalogued, and returns every such path found. Stops with
    /// `Cancelled` when a cancel is requested (large trees take a while).
    fn collect_dir(
        &self,
        folder_id: i64,
        dir: &Path,
        kinds: &[FileKind],
        work: &mut Vec<WorkItem>,
    ) -> Result<HashSet<String>> {
        let mut present = HashSet::new();
        for entry in walkdir::WalkDir::new(dir).follow_links(false).into_iter().filter_map(|e| e.ok()) {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            let path = entry.path();
            if !entry.file_type().is_file() || !is_selected(path, kinds) || is_temporary(path) {
                continue;
            }
            let Some((size, modified)) = file_times(path) else { continue };
            let key = path_key(path);
            present.insert(key.clone());
            if !self.catalog.lock().is_current(&key, size, modified, &self.extractor.ocr_language())? {
                work.push((folder_id, path.to_path_buf(), size, modified));
            }
        }
        Ok(present)
    }

    /// Applies watcher events. Paths may be files or directories, existing or
    /// gone: a vanished directory (deleted, or the old name of a rename)
    /// drops every record under it; an appearing directory (new name of a
    /// rename, moved in) is walked and indexed. Events touching a watched
    /// root itself never delete records — a missing root is a UI state.
    fn update_paths(&self, paths: Vec<PathBuf>) -> Result<()> {
        let mut work = Vec::new();
        let mut vanished = Vec::new();
        let mut roots_touched = false;
        let roots = self.catalog.lock().folder_ids()?;
        for path in paths {
            let key = path_key(&path);
            let normalized = normalize_folder(&key);
            if roots.iter().any(|(_, root)| {
                let root = normalize_folder(root);
                root == normalized || is_within(&root, &normalized)
            }) {
                roots_touched = true;
                continue;
            }
            let Some((folder_id, root)) = self.catalog.lock().folder_for(&key)? else { continue };
            if !Path::new(&root).is_dir() {
                roots_touched = true;
                continue;
            }
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_dir() => {
                    let kinds = self.folder_kinds(folder_id)?;
                    let present = self.collect_dir(folder_id, &path, &kinds, &mut work)?;
                    for known in self.catalog.lock().paths_under(folder_id, &key)? {
                        if !present.contains(&known) {
                            vanished.push(known);
                        }
                    }
                }
                Ok(meta) if meta.is_file() => {
                    if !is_selected(&path, &self.folder_kinds(folder_id)?) || is_temporary(&path) {
                        continue;
                    }
                    if let Some((size, modified)) = file_times(&path) {
                        if !self.catalog.lock().is_current(&key, size, modified, &self.extractor.ocr_language())? {
                            work.push((folder_id, path, size, modified));
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => {
                    // Gone: one file, or a whole directory of them.
                    for known in self.catalog.lock().paths_under(folder_id, &key)? {
                        if !Path::new(&known).exists() {
                            vanished.push(known);
                        }
                    }
                }
            }
        }
        let mut seen = HashSet::new();
        work.retain(|item| seen.insert(item.1.clone()));
        vanished.sort();
        vanished.dedup();
        // Keep the folders' file counts in step with what appeared or went,
        // so the UI's "indexed/found" does not drift until the next scan.
        {
            let catalog = self.catalog.lock();
            let mut deltas: HashMap<i64, i64> = HashMap::new();
            for (folder_id, path, _, _) in &work {
                if !catalog.knows(&path_key(path))? {
                    *deltas.entry(*folder_id).or_default() += 1;
                }
            }
            for known in &vanished {
                if let Some((folder_id, _)) = catalog.folder_for(known)? {
                    *deltas.entry(folder_id).or_default() -= 1;
                }
            }
            for (folder_id, delta) in deltas {
                catalog.adjust_file_count(folder_id, delta)?;
            }
        }
        if work.is_empty() && vanished.is_empty() {
            if roots_touched {
                // Let the UI re-read folders (missing/back) without a rescan.
                let count = self.catalog.lock().document_count()?;
                (self.on_changed)(count);
            }
            return Ok(());
        }
        self.emit(|p| *p = IndexProgress::start(IndexPhase::Indexing, p.watching), None);
        self.process(work, vanished, None)
    }

    fn process(
        &self,
        work: Vec<WorkItem>,
        vanished: Vec<String>,
        extra: Observer,
    ) -> Result<()> {
        let total = work.len();
        self.emit(|p| {
            p.phase = IndexPhase::Indexing;
            p.total = total;
        }, extra);
        let mut writer = self.index.writer()?;
        let throttle = Mutex::new(Instant::now() - Duration::from_secs(1));
        let since_commit = Mutex::new(0usize);
        let writer_lock = Mutex::new(&mut writer);
        // Files a cancel kept from finishing (not started, OCR killed, or
        // their embed was stopped — those keep their text, embedded=0).
        let skipped = std::sync::atomic::AtomicUsize::new(0);
        // Second progress stage: only with a model that loads (a missing
        // model would show "0 / N" forever).
        let semantic_on = self.embedder.as_ref().is_some_and(|e| e.usable());
        // Files are processed by as many workers as there are OCR slots; the
        // slots themselves (shared with every page of every file) cap
        // Tesseract processes at max(1, cores - 1).
        let pool = rayon::ThreadPoolBuilder::new().num_threads(self.extractor.ocr_jobs()).build()?;
        pool.install(|| {
            work.par_iter().for_each(|(folder_id, path, size, modified)| {
                if self.cancel.load(Ordering::Relaxed) {
                    skipped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let key = path_key(path);
                let content_hash = content_hash(path).ok();
                let reuse = self.reusable(path, &key, content_hash.as_deref());
                // `copied` = a fully read identical file donated its pages;
                // its chunk vectors are copied inside `store` too.
                let (result, copied) = match reuse {
                    Some((document, source)) => (Ok(document), Some(source)),
                    None => (self.extractor.extract_cancellable(path, &self.cancel), None),
                };
                // A half-read file is neither indexed nor recorded as failed:
                // it is simply not current, so the next scan picks it up.
                if result.as_ref().is_err_and(is_cancelled) {
                    skipped.fetch_add(1, Ordering::Relaxed);
                    return;
                }
                let failed = result.is_err();
                // Stage 1 is over for this file before its (slow) embed
                // starts: report it now, so "Metin okundu" is not held back
                // by the embedding queue.
                let needs_semantic = semantic_on
                    && result
                        .as_ref()
                        .is_ok_and(|d| d.pages.iter().any(|p| !p.is_failed() && !p.text.trim().is_empty()));
                {
                    let snapshot = {
                        let mut progress = self.progress.lock();
                        progress.text_done += 1;
                        progress.embed_total += needs_semantic as usize;
                        progress.clone()
                    };
                    let mut last = throttle.lock();
                    if last.elapsed() >= Duration::from_millis(200) || snapshot.text_done == snapshot.total {
                        *last = Instant::now();
                        (self.on_progress)(&snapshot);
                        if let Some(callback) = extra {
                            callback(&snapshot);
                        }
                    }
                }
                // This file needs a semantic index / got one in this run.
                let mut semantic = false;
                let mut embed_finished = false;
                match result {
                    Ok(document) => {
                        // Embed readable pages unless the copy already brings
                        // chunks; a skipped/failed embed leaves embedded=0
                        // for the backfill pass — never a file failure.
                        let outcome = match (copied.as_ref(), &self.embedder) {
                            (Some((_, true)), _) => EmbedOutcome::Copied, // cloned inside `store`
                            (_, Some(embedder)) => {
                                let started = Instant::now();
                                let claimed = AtomicBool::new(false);
                                EmbedOutcome::from_result(embedder.embed_document_with_progress(
                                    &document_title(&key),
                                    document.kind,
                                    &document.pages,
                                    &self.cancel,
                                    &|chunks_done, chunks_total| {
                                        self.scan_chunk_progress(&key, &claimed, chunks_done, chunks_total, started, &throttle, extra)
                                    },
                                ))
                            }
                            _ => EmbedOutcome::NoEmbedder,
                        };
                        if matches!(outcome, EmbedOutcome::Cancelled) {
                            // The text is read and stored below (searchable
                            // by keyword), but the run is reported cancelled
                            // and the file waits at embedded=0 for backfill.
                            skipped.fetch_add(1, Ordering::Relaxed);
                        }
                        let has_text = document.pages.iter().any(|p| !p.is_failed() && !p.text.trim().is_empty());
                        let embedded = outcome.marks_embedded(has_text);
                        semantic = semantic_on && has_text;
                        embed_finished = semantic && matches!(outcome, EmbedOutcome::Copied | EmbedOutcome::Embedded(_));
                        let chunks = outcome.into_chunks();
                        let stats = page_stats(&document, LOW_CONFIDENCE_FLOOR);
                        let record = FileRecord {
                            path: key.clone(),
                            folder_id: *folder_id,
                            kind: document.kind,
                            size: *size,
                            modified: *modified,
                            page_count: stats.pages as i64,
                            ocr_pages: stats.ocr as i64,
                            low_confidence_pages: stats.low_confidence as i64,
                            failed_pages: stats.failed as i64,
                            content_hash,
                            ocr_language: document.ocr_language.clone(),
                        };
                        let chunk_refs: Vec<(usize, Vec<f32>)> =
                            chunks.iter().map(|c| (c.page, c.vector.clone())).collect();
                        // Only an embedded donor is asked to donate its
                        // chunks; with an unembedded one the fresh
                        // `chunk_refs` are the file's vectors.
                        let copy_from = copied
                            .as_ref()
                            .and_then(|(source, source_embedded)| source_embedded.then_some(source.as_str()));
                        // `store` re-checks the donor's content hash inside
                        // its transaction; on a refusal the file is stored
                        // unembedded and its pages are embedded fresh below.
                        // The catalog lock covers the catalog write only: the
                        // guard used to live through the whole expression, so
                        // a search waited for the Tantivy add too (163 ms on a
                        // 3 MB text file). The writer lock still
                        // keeps store + add together against other workers.
                        let stored = {
                            let writer = writer_lock.lock();
                            let cloned = self.catalog.lock().store(&record, &document.pages, &chunk_refs, copy_from, embedded, now());
                            cloned.and_then(|cloned| self.index.add(&writer, &key, *modified, &document).map(|()| cloned))
                        };
                        match stored {
                            Err(error) => {
                                // Recorded as a failure, not a pending embed.
                                semantic = false;
                                embed_finished = false;
                                self.vectors.remove(&key);
                                let _ = self.catalog.lock().record_failure(
                                    *folder_id, &key, *size, *modified, &format!("{error:#}"), now(),
                                );
                            }
                            Ok(cloned) => {
                                // A refused copy means the donor changed or
                                // vanished after `reusable` read its pages:
                                // nothing was cloned, so embed the pages
                                // fresh. Embedded=0 is already stored; an
                                // embed that still yields nothing is left
                                // for the backfill pass.
                                let file_chunks = if cloned {
                                    self.catalog.lock().chunks(&key).unwrap_or_default()
                                } else if copy_from.is_some() && chunk_refs.is_empty() {
                                    // Any failure or cancel here leaves the
                                    // stored embedded=0 for the backfill.
                                    let fresh = self
                                        .embedder
                                        .as_ref()
                                        .filter(|_| has_text)
                                        .map(|embedder| {
                                            embedder.embed_document(&document_title(&key), document.kind, &document.pages, &self.cancel)
                                        });
                                    match fresh {
                                        Some(Ok(vectors)) => vectors.into_iter().map(|c| (c.page, c.vector)).collect(),
                                        Some(Err(error)) => {
                                            if is_cancelled(&error) {
                                                skipped.fetch_add(1, Ordering::Relaxed);
                                            }
                                            Vec::new()
                                        }
                                        None => Vec::new(),
                                    }
                                } else {
                                    chunk_refs
                                };
                                if copy_from.is_some() {
                                    // A refused copy is done only if the
                                    // fresh embed above produced vectors.
                                    embed_finished = semantic && (cloned || !file_chunks.is_empty());
                                }
                                if copy_from.is_some() && !cloned && !file_chunks.is_empty() {
                                    // The refused copy wrote no chunks and
                                    // kept embedded=0; `store_chunks` writes
                                    // the vectors and flips the flag.
                                    let _ = self.catalog.lock().store_chunks(&key, &file_chunks);
                                }
                                self.vectors.set(&key, document.kind, *modified, &file_chunks);
                            }
                        }
                    }
                    Err(error) => {
                        let writer = writer_lock.lock();
                        self.index.delete_path(&writer, &key);
                        self.vectors.remove(&key);
                        let _ = self.catalog.lock().record_failure(
                            *folder_id, &key, *size, *modified, &format!("{error:#}"), now(),
                        );
                    }
                }
                let commit_now = {
                    let mut count = since_commit.lock();
                    *count += 1;
                    if *count >= COMMIT_EVERY {
                        *count = 0;
                        true
                    } else {
                        false
                    }
                };
                if commit_now {
                    let mut writer = writer_lock.lock();
                    if writer.commit().is_ok() && self.index.reload().is_ok() {
                        let count = self.catalog.lock().document_count();
                        if let Ok(count) = count {
                            (self.on_changed)(count);
                        }
                    }
                }
                let mut last = throttle.lock();
                let snapshot = {
                    let mut progress = self.progress.lock();
                    progress.done += 1;
                    progress.failed += failed as usize;
                    if needs_semantic && !semantic {
                        // Its store failed: a failure, not a pending embed.
                        progress.embed_total -= 1;
                    }
                    progress.embed_done += embed_finished as usize;
                    // This file's chunk line ends with it; while another
                    // file's chunks are shown, `current` stays that file.
                    if progress.chunks_total.is_some() && progress.current.as_deref() == Some(key.as_str()) {
                        progress.clear_embedding();
                    }
                    if progress.chunks_total.is_none() {
                        progress.current = Some(key);
                    }
                    progress.clone()
                };
                if last.elapsed() >= Duration::from_millis(200) || snapshot.done == snapshot.total {
                    *last = Instant::now();
                    (self.on_progress)(&snapshot);
                    if let Some(callback) = extra {
                        callback(&snapshot);
                    }
                }
            });
        });
        drop(writer_lock);
        let interrupted = skipped.into_inner() > 0;
        // Vanished paths go last: a moved/copied file above could still
        // reuse their stored pages instead of being OCR'd again. A cancelled
        // run leaves them for the rescan that finishes its work, so moved
        // files it did not reach can still reuse them.
        if !interrupted {
            for path in &vanished {
                self.index.delete_path(&writer, path);
                self.vectors.remove(path);
                self.catalog.lock().forget(path)?;
            }
        }
        self.emit(|p| p.phase = IndexPhase::Committing, extra);
        writer.commit()?;
        self.index.reload()?;
        let count = self.catalog.lock().document_count()?;
        (self.on_changed)(count);
        self.emit(
            |p| {
                p.phase = IndexPhase::Idle;
                p.current = None;
                p.finished_at = Some(now());
                p.cancelled = interrupted;
                p.unread = if interrupted { p.total.saturating_sub(p.done) } else { 0 };
            },
            extra,
        );
        if interrupted {
            // The run was cancelled: report it so callers (worker loop, eval
            // CLI, tests) skip follow-up work — most importantly the
            // embedding backfill, which must never start on a cancelled run.
            return Err(Cancelled.into());
        }
        Ok(())
    }
}

impl Worker {
    /// Chunk progress of a scan-time embed, reported like the
    /// backfill's: without it the semantic stage of a scan only moved when a
    /// whole file finished, so one big file (256 chunks, minutes on an
    /// office PC) held "Anlam dizini 0 / 1" and the bar still. Several files
    /// can be inside the embedder at once — the next waits for its turn while
    /// the previous one's last chunks run — so one file is shown at a time:
    /// the shown one keeps reporting, a free line is taken by any file, and a
    /// file whose first chunk just finished takes the line over (it is the
    /// one that got the sessions); `claimed` remembers that per file.
    #[allow(clippy::too_many_arguments)]
    fn scan_chunk_progress(
        &self,
        key: &str,
        claimed: &AtomicBool,
        chunks_done: usize,
        chunks_total: usize,
        started: Instant,
        throttle: &Mutex<Instant>,
        extra: Observer,
    ) {
        if chunks_total == 0 {
            return;
        }
        let first_chunk = chunks_done > 0 && !claimed.swap(true, Ordering::SeqCst);
        let snapshot = {
            let mut progress = self.progress.lock();
            let shown = progress.chunks_total.is_some() && progress.current.as_deref() == Some(key);
            if !(shown || progress.chunks_total.is_none() || first_chunk) {
                return;
            }
            progress.current = Some(key.to_string());
            progress.chunks_done = Some(chunks_done);
            progress.chunks_total = Some(chunks_total);
            progress.embedding_elapsed_secs = Some(started.elapsed().as_secs());
            progress.clone()
        };
        let mut last = throttle.lock();
        if last.elapsed() >= Duration::from_millis(200) || chunks_done == chunks_total {
            *last = Instant::now();
            (self.on_progress)(&snapshot);
            if let Some(callback) = extra {
                callback(&snapshot);
            }
        }
    }

    /// Stored pages of an identical, fully read file elsewhere in the
    /// catalog (copy, move, rename) — the expensive OCR is not repeated, and
    /// when that file is already embedded its chunk vectors are copied too.
    /// Donor lookup and the page read share one catalog lock, so a parallel
    /// worker re-storing the donor can never pair a new record with pages
    /// from another record. Returns the document plus `(source path, source
    /// was embedded)`; the chunk copy itself is re-verified inside `store`.
    fn reusable(&self, path: &Path, key: &str, content_hash: Option<&str>) -> Option<(ExtractedDocument, (String, bool))> {
        let kind = FileKind::from_path(path)?;
        let catalog = self.catalog.lock();
        let language = self.extractor.ocr_language();
        let (source, embedded) = catalog.reusable_source(content_hash?, kind, key, &language).ok()??;
        let pages = catalog.pages(&source).ok()?;
        let mut document = ExtractedDocument::new(kind, pages);
        if document.ocr_pages() > 0 {
            document.ocr_language = Some(language);
        }
        Some((document, (source, embedded)))
    }

    /// Fills chunk embeddings for catalogued files that lack them: files
    /// from before the model shipped (catalog migration keeps them at
    /// `embedded=0`), files whose embed was cancelled mid-run, and copies
    /// whose donor was not embedded yet. Runs after a job settles; a cancel
    /// stops between files — finished files keep their chunks.
    fn backfill_embeddings(&self, extra: Observer) -> Result<()> {
        if self.cancel.load(Ordering::Relaxed) {
            // A cancel already landed: after "İptal" no new embedding run
            // starts — the pending files wait for the next scan/backfill.
            return Err(Cancelled.into());
        }
        let Some(embedder) = &self.embedder else { return Ok(()) };
        if !embedder.usable() {
            return Ok(());
        }
        let catalog = self.catalog.lock();
        let pending = catalog.unembedded_paths()?;
        drop(catalog);
        if pending.is_empty() {
            return Ok(());
        }
        self.emit(
            |p| {
                *p = IndexProgress {
                    total: pending.len(),
                    embed_total: pending.len(),
                    cancelling: p.cancelling,
                    ..IndexProgress::start(IndexPhase::Embedding, p.watching)
                }
            },
            extra,
        );
        for (done, path) in pending.iter().enumerate() {
            if self.cancel.load(Ordering::Relaxed) {
                return Err(Cancelled.into());
            }
            self.emit(
                |p| {
                    p.current = Some(path.clone());
                    p.chunks_done = Some(0);
                    p.chunks_total = None;
                    p.embedding_elapsed_secs = Some(0);
                },
                extra,
            );
            let (record, pages) = {
                let catalog = self.catalog.lock();
                (catalog.file(path)?, catalog.pages(path)?)
            };
            let mut finished = false;
            if let Some(record) = record {
                let started = Instant::now();
                // An ONNX batch can take much longer than a second. The
                // scoped timer stops and joins on success, error or unwind;
                // no late event can overwrite the next file/phase.
                let vectors = with_heartbeat(
                    Duration::from_secs(1),
                    || {
                        embedder.embed_document_with_progress(
                            &document_title(path),
                            record.kind,
                            &pages,
                            &self.cancel,
                            &|chunks_done, chunks_total| {
                                self.emit(
                                    |p| {
                                        p.chunks_done = Some(chunks_done);
                                        p.chunks_total = Some(chunks_total);
                                        p.embedding_elapsed_secs = Some(started.elapsed().as_secs());
                                    },
                                    extra,
                                );
                            },
                        )
                    },
                    || self.emit(|p| p.embedding_elapsed_secs = Some(started.elapsed().as_secs()), extra),
                );
                // One failed embed stays pending for the next pass; it is
                // still a processed file in the existing done/total counter.
                match vectors {
                    Ok(vectors) => {
                        let chunk_refs: Vec<(usize, Vec<f32>)> =
                            vectors.iter().map(|c| (c.page, c.vector.clone())).collect();
                        self.catalog.lock().store_chunks(path, &chunk_refs)?;
                        self.vectors.set(path, record.kind, record.modified, &chunk_refs);
                        finished = true;
                    }
                    Err(error) if is_cancelled(&error) => return Err(error),
                    Err(_) => {}
                }
            }
            self.emit(
                |p| {
                    p.done = done + 1;
                    p.embed_done += finished as usize;
                    p.clear_embedding();
                },
                extra,
            );
        }
        self.emit(
            |p| {
                p.phase = IndexPhase::Idle;
                p.current = None;
                p.finished_at = Some(now());
            },
            extra,
        );
        Ok(())
    }
}

/// What the scan-time embed of one file produced; decides `files.embedded`
/// ("no backfill needed"). Only a finished embed (or a file with no
/// readable text at all) is done: a cancelled or failed embed must stay at
/// embedded=0, or the file never enters the semantic index
/// (`unwrap_or_default` used to turn a cancel into "nothing to embed").
#[derive(Debug)]
enum EmbedOutcome {
    /// An embedded donor's chunks are cloned inside `store`.
    Copied,
    /// The model ran to the end. Empty = the text cleans to nothing worth
    /// embedding (markup): done, re-embedding it is pure churn.
    Embedded(Vec<ChunkVector>),
    Cancelled,
    Failed,
    /// No model configured: text-bearing files wait for one.
    NoEmbedder,
}

impl EmbedOutcome {
    fn from_result(result: Result<Vec<ChunkVector>>) -> Self {
        match result {
            Ok(vectors) => Self::Embedded(vectors),
            Err(error) if is_cancelled(&error) => Self::Cancelled,
            Err(_) => Self::Failed,
        }
    }

    fn marks_embedded(&self, has_text: bool) -> bool {
        match self {
            Self::Copied | Self::Embedded(_) => true,
            Self::Cancelled | Self::Failed | Self::NoEmbedder => !has_text,
        }
    }

    fn into_chunks(self) -> Vec<ChunkVector> {
        match self {
            Self::Embedded(vectors) => vectors,
            _ => Vec::new(),
        }
    }
}

/// Keeps progress alive even while one blocking model call is running.
/// Dropping the sender wakes the timer immediately; a scope also joins it
/// when `operation` unwinds, so it cannot outlive its file or worker.
fn with_heartbeat<T>(interval: Duration, operation: impl FnOnce() -> T, heartbeat: impl Fn() + Sync) -> T {
    std::thread::scope(|scope| {
        let (stop, receiver) = channel::<()>();
        let heartbeat = &heartbeat;
        scope.spawn(move || {
            while matches!(receiver.recv_timeout(interval), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
                heartbeat();
            }
        });
        let result = operation();
        drop(stop);
        result
    })
}

fn content_hash(path: &Path) -> Result<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(std::fs::File::open(path)?)?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// (folder id, path, size, mtime) of a file to (re)index.
type WorkItem = (i64, PathBuf, i64, i64);

/// False for pure access events (open, read, close without writing): the
/// indexer's own reads produce them (inotify reports `IN_OPEN`), and after a
/// cancel they would re-queue exactly the files the user stopped.
fn may_change_content(kind: &notify::EventKind) -> bool {
    use notify::event::{AccessKind, AccessMode};
    match kind {
        notify::EventKind::Access(AccessKind::Close(AccessMode::Write)) => true,
        notify::EventKind::Access(_) => false,
        _ => true,
    }
}

/// A supported file whose kind is among the folder's selected `kinds`.
fn is_selected(path: &Path, kinds: &[FileKind]) -> bool {
    FileKind::from_path(path).is_some_and(|kind| kinds.contains(&kind))
}

/// Office lock/temp files (~$x.docx) and partial downloads are skipped.
fn is_temporary(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map_or(false, |n| n.starts_with("~$") || n.ends_with(".tmp") || n.ends_with(".partial"))
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write;

    use super::*;
    use crate::index::PageHit;
    use crate::model::PageText;

    /// Minimal DOCX (stored, uncompressed: same text length → same size).
    pub(crate) fn write_docx(path: &Path, text: &str) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .last_modified_time(zip::DateTime::default());
        zip.start_file("word/document.xml", options).unwrap();
        write!(
            zip,
            r#"<?xml version="1.0" encoding="UTF-8"?><w:document xmlns:w="http://schemas.openxmlformats.org/wordprocessingml/2006/main"><w:body><w:p><w:r><w:t>{text}</w:t></w:r></w:p></w:body></w:document>"#
        )
        .unwrap();
        zip.finish().unwrap();
    }

    /// Engine without pdfium or Tesseract: DOCX/XLSX still index.
    pub(crate) fn test_engine(data: &Path) -> Arc<Engine> {
        let engine = Engine::open(
            EnginePaths {
                data_dir: data.to_path_buf(),
                pdfium_dir: data.join("no-pdfium"),
                ocr: OcrConfig {
                    binary: data.join("no-tesseract"),
                    tessdata: data.join("no-tessdata"),
                    language: "tur".into(),
                    timeout: Duration::from_secs(5),
                    threads: 1,
                },
                ocr_jobs: 2,
                deskew: false,
                embed: None,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap();
        engine.wait_ready();
        engine
    }

    pub(crate) fn request(query: &str, limit: usize, offset: usize) -> SearchRequest {
        SearchRequest { query: query.into(), kinds: vec![], modified_after: None, modified_before: None, limit, offset }
    }

    #[test]
    fn embedding_progress_preserves_file_counts_and_clears_details_on_exit() {
        let dir = tempfile::tempdir().unwrap();
        let engine = test_engine(&dir.path().join("data"));
        let worker = engine.detached_worker();
        worker.emit(
            |p| {
                *p = IndexProgress::start(IndexPhase::Embedding, false);
                p.total = 913;
                p.done = 7;
                p.current = Some("big.txt".into());
                p.chunks_done = Some(8);
                p.chunks_total = Some(256);
                p.embedding_elapsed_secs = Some(2);
            },
            None,
        );
        let json = serde_json::to_value(engine.progress()).unwrap();
        assert_eq!(json["phase"], "embedding");
        assert_eq!((json["done"].as_u64(), json["total"].as_u64()), (Some(7), Some(913)));
        assert_eq!((json["chunksDone"].as_u64(), json["chunksTotal"].as_u64()), (Some(8), Some(256)));
        assert_eq!(json["embeddingElapsedSecs"], 2);
        worker.emit(|p| p.phase = IndexPhase::Committing, None);
        let json = serde_json::to_value(engine.progress()).unwrap();
        assert!(json.get("chunksDone").is_none());
        assert!(json.get("chunksTotal").is_none());
        assert!(json.get("embeddingElapsedSecs").is_none());
        assert_eq!(engine.progress().done, 7);

        worker.emit(
            |p| {
                p.phase = IndexPhase::Embedding;
                p.chunks_done = Some(0);
                p.chunks_total = Some(32);
                p.embedding_elapsed_secs = Some(4);
            },
            None,
        );
        worker.settle(&Err(Cancelled.into()), None);
        let progress = engine.progress();
        assert_eq!(progress.phase, IndexPhase::Idle);
        assert!(progress.cancelled);
        assert!(progress.chunks_done.is_none() && progress.chunks_total.is_none() && progress.embedding_elapsed_secs.is_none());
    }

    /// `with_heartbeat` has returned (or unwound): its timer thread is
    /// joined and the heartbeat closure — the only sender — is dropped. Ticks
    /// sent before that may still sit in the channel (a loaded machine runs
    /// the 10 ms timer more often than the operation reads it); once they are
    /// drained the channel must report `Disconnected` at once, never wait.
    fn assert_heartbeat_stopped(receiver: &Receiver<()>) {
        let leftover = receiver.try_iter().count();
        assert_eq!(
            receiver.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected),
            "heartbeat still alive after {leftover} queued ticks"
        );
    }

    #[test]
    fn blocking_embedding_operation_has_heartbeats_and_stops_on_error() {
        let (ticks, receiver) = channel();
        let result = with_heartbeat(
            Duration::from_millis(10),
            || {
                // Simulate a model run that blocks without reporting chunks.
                // Generous waits: only how long a tick takes under load.
                receiver.recv_timeout(Duration::from_secs(30)).unwrap();
                receiver.recv_timeout(Duration::from_secs(30)).unwrap();
                Err::<(), _>("model failure")
            },
            move || { let _ = ticks.send(()); },
        );
        assert_eq!(result, Err("model failure"));
        assert_heartbeat_stopped(&receiver);
    }

    #[test]
    fn embedding_heartbeat_joins_even_when_the_operation_panics() {
        let (ticks, receiver) = channel();
        let result = std::panic::catch_unwind(|| {
            with_heartbeat(
                Duration::from_millis(10),
                || {
                    receiver.recv_timeout(Duration::from_secs(30)).unwrap();
                    panic!("simulated embedding panic");
                },
                move || { let _ = ticks.send(()); },
            );
        });
        assert!(result.is_err());
        assert_heartbeat_stopped(&receiver);
    }

    #[test]
    fn removing_a_folder_notifies_observers_after_unlocking_the_updated_index() {
        let dir = tempfile::tempdir().unwrap();
        let removed = dir.path().join("kaldirilan");
        let retained = dir.path().join("kalan");
        std::fs::create_dir_all(&removed).unwrap();
        std::fs::create_dir_all(&retained).unwrap();
        std::fs::write(removed.join("bir.txt"), "kira sözleşmesi").unwrap();
        std::fs::write(retained.join("iki.txt"), "kira sözleşmesi").unwrap();
        let mut engine = test_engine(&dir.path().join("data"));
        let id = engine.index_folder_blocking(&key(&removed), &|_| {}).unwrap();
        engine.index_folder_blocking(&key(&retained), &|_| {}).unwrap();
        assert_eq!(engine.search(&request("kira", 20, 0)).unwrap().total, 2);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let probe = Arc::new(Mutex::new(None::<std::sync::Weak<Engine>>));
        let sink: ChangedSink = {
            let observed = observed.clone();
            let probe = probe.clone();
            Arc::new(move |count| {
                let engine = probe.lock().as_ref().unwrap().upgrade().unwrap();
                // Fail immediately instead of hanging if the callback is
                // accidentally moved back under either engine lock.
                assert!(engine.catalog.try_lock().is_some(), "observer must not hold the catalog lock");
                assert!(engine.scan.try_lock().is_some(), "observer must not hold the scan lock");
                let response = engine.search(&request("kira", 20, 0)).unwrap();
                observed.lock().push((count, response.total, response.hits.into_iter().map(|h| h.path).collect::<Vec<_>>()));
            })
        };
        Arc::get_mut(&mut engine).expect("only this test owns the engine").on_changed = sink;
        *probe.lock() = Some(Arc::downgrade(&engine));
        engine.remove_folder(id).unwrap();
        assert_eq!(*observed.lock(), vec![(1, 1, vec![key(&retained.join("iki.txt"))])]);
        assert_eq!(engine.folders().unwrap().len(), 1);
    }

    #[test]
    fn worker_change_observers_can_search_without_holding_the_catalog_lock() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("belgeler");
        std::fs::create_dir_all(&docs).unwrap();
        // Reach the intermediate commit as well as the final scan callback.
        for i in 0..COMMIT_EVERY {
            std::fs::write(docs.join(format!("kira-{i}.txt")), "kira sözleşmesi").unwrap();
        }
        let engine = test_engine(&dir.path().join("data"));
        let id = engine.ensure_folder(&key(&docs)).unwrap();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut worker = engine.detached_worker();
        worker.on_changed = {
            let probe = Arc::downgrade(&engine);
            let observed = observed.clone();
            Arc::new(move |count| {
                let Some(engine) = probe.upgrade() else { return };
                // Fail before searching instead of hanging if a temporary
                // catalog guard ever survives into this callback again.
                assert!(engine.catalog.try_lock().is_some(), "observer must not hold the catalog lock");
                assert_eq!(engine.search(&request("kira", 200, 0)).unwrap().total, count as usize);
                observed.lock().push(count);
            })
        };
        // Exercise the same synchronous scan path as index_folder_blocking,
        // with a change observer instead of the detached worker's no-op.
        // Worker callbacks may still run under the scan and writer locks.
        let _scan = engine.scan.lock();
        worker.scan_folders(&[(id, key(&docs))], None).unwrap();
        worker.rebuild_index().unwrap();
        worker.update_paths(vec![docs.clone()]).unwrap();
        engine.catalog.lock().set_folder_kinds(id, &[FileKind::Pdf]).unwrap();
        worker.purge(id).unwrap();
        assert_eq!(*observed.lock(), vec![COMMIT_EVERY as i64; 4].into_iter().chain([0]).collect::<Vec<_>>());
    }

    #[test]
    fn search_total_counts_every_matching_file() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("belgeler");
        std::fs::create_dir_all(&docs).unwrap();
        for i in 0..35 {
            write_docx(&docs.join(format!("fatura-{i:02}.docx")), &format!("Fatura numarası {i} tutar"));
        }
        write_docx(&docs.join("liste.docx"), "Tahsilat listesi");
        let engine = test_engine(&dir.path().join("data"));
        engine.index_folder_blocking(docs.to_str().unwrap(), &|_| {}).unwrap();

        let first = engine.search(&request("fatura", 10, 0)).unwrap();
        assert_eq!((first.hits.len(), first.total), (10, 35));
        let last = engine.search(&request("fatura", 10, 30)).unwrap();
        assert_eq!((last.hits.len(), last.total), (5, 35));
    }

    /// the timed search is the search — same hits, scores,
    /// snippets and total — plus a stage breakdown that adds up.
    #[test]
    fn timed_search_returns_the_same_results_and_its_stages() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("belgeler");
        std::fs::create_dir_all(&docs).unwrap();
        for i in 0..12 {
            write_docx(&docs.join(format!("kira-{i:02}.docx")), &format!("Kira sözleşmesi {i} depozito bedeli ödendi"));
        }
        let engine = test_engine(&dir.path().join("data"));
        engine.index_folder_blocking(docs.to_str().unwrap(), &|_| {}).unwrap();
        for query in ["kira sözleşmesi", "depozito", "\"kira sözleşmesi 3\""] {
            let plain = engine.search(&request(query, 5, 2)).unwrap();
            let (timed, timing) = engine.search_timed(&request(query, 5, 2)).unwrap();
            assert_eq!(serde_json::to_value(&plain.hits).unwrap(), serde_json::to_value(&timed.hits).unwrap(), "{query}");
            assert_eq!(plain.total, timed.total);
            let stages = timing.embed + timing.bm25 + timing.coverage + timing.fusion + timing.snippets;
            assert!(timing.total >= stages - 0.5 && stages >= 0.0 && timing.vectors <= timing.fusion + 0.5, "{timing:?}");
        }
        // This fixture has no embedder, so exercise coverage directly too.
        // Every page contains both terms: instrumentation must retain full
        // coverage while accounting for each count and each prefix read.
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let results = engine.index.search_counted("kira sözleşmesi", &filter, 12).unwrap();
        let hybrid = HybridOptions::default();
        let mut pages = HashMap::new();
        let mut timing = SearchTiming::default();
        let coverage = engine.head_coverage("kira sözleşmesi", &results, &hybrid, &mut pages, &mut timing);
        assert_eq!(coverage, vec![1.0; hybrid.bm25_head.min(results.hits.len())]);
        assert_eq!(timing.coverage_terms.iter().map(|(term, _)| term.clone()).collect::<Vec<_>>(), query_terms("kira sözleşmesi"));
        assert_eq!(timing.coverage_page_reads, coverage.len());
        assert_eq!(timing.coverage_bytes, pages.values().map(|page| page.text.len()).sum::<usize>());
        assert!(timing.coverage_counts >= timing.coverage_terms.iter().map(|(_, elapsed)| elapsed).sum::<f64>());
        assert_eq!(error_code(engine.search_timed(&request("  ", 5, 0)).unwrap_err()), Some("query_empty"));
    }

    #[test]
    fn a_required_phrase_drops_vector_only_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let engine = test_engine(&dir.path().join("data"));
        // A file that matched the BM25 side, and one only the vector side
        // could have produced.
        engine.vectors.set("/bm25.pdf", FileKind::Pdf, 1, &[(0usize, vec![1.0, 0.0])]);
        engine.vectors.set("/sem.pdf", FileKind::Pdf, 1, &[(0usize, vec![1.0, 0.0])]);
        let results = SearchResults {
            hits: vec![PageHit { path: "/bm25.pdf".into(), page: 0, score: 5.0 }],
            total_files: 1,
            matched_paths: HashSet::from(["/bm25.pdf".to_string()]),
        };
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let hybrid = HybridOptions::default();
        let query = [1.0f32, 0.0];

        // Open query: the vector-only file joins the fused list.
        let catalog = engine.catalog.lock();
        let open = engine.fused_hits(&catalog, "kira", &HashMap::new(), &results, &[], Some(&query), &filter, &hybrid, false, 10, &mut 0.0);
        assert!(open.iter().any(|h| h.path == "/sem.pdf" && h.semantic_only));

        // Same candidates, phrase required: only the BM25-matched file stays.
        let gated = engine.fused_hits(&catalog, "kira", &HashMap::new(), &results, &[], Some(&query), &filter, &hybrid, true, 10, &mut 0.0);
        assert_eq!(gated.len(), 1);
        assert_eq!(gated[0].path, "/bm25.pdf");
        assert!(!gated[0].semantic_only);

        // A number/code query drops vector-only look-alikes ...
        let code = engine.fused_hits(&catalog, "fatura 1548", &HashMap::new(), &results, &[], Some(&query), &filter, &hybrid, false, 10, &mut 0.0);
        assert_eq!(code.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["/bm25.pdf"]);
        // ... and with no keyword match at all the answer is empty.
        let none = SearchResults { hits: vec![], total_files: 0, matched_paths: HashSet::new() };
        assert!(engine.fused_hits(&catalog, "1548", &HashMap::new(), &none, &[], Some(&query), &filter, &hybrid, false, 10, &mut 0.0).is_empty());
        // A floor at or below the cosine lets them through again.
        let lenient = HybridOptions { code_vec_only_cosine: 0.9, ..hybrid };
        assert_eq!(engine.fused_hits(&catalog, "1548", &HashMap::new(), &none, &[], Some(&query), &filter, &lenient, false, 10, &mut 0.0).len(), 2);
    }

    #[test]
    fn files_without_an_embedder_stay_unembedded_for_backfill() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("belgeler");
        std::fs::create_dir_all(&docs).unwrap();
        write_docx(&docs.join("kira.docx"), "kira sozlesmesi");
        std::fs::write(docs.join("bos.txt"), "").unwrap();
        let engine = test_engine(&dir.path().join("data"));
        engine.index_folder_blocking(docs.to_str().unwrap(), &|_| {}).unwrap();

        // No model bundled: a file with readable text must keep embedded=0
        // so a later backfill embeds it; a file with nothing to embed stays
        // out of the pending list either way.
        let pending = engine.catalog.lock().unembedded_paths().unwrap();
        assert_eq!(pending, [key(&docs.join("kira.docx"))]);
        assert!(engine.failures(None).unwrap().is_empty());
    }

    #[test]
    fn reindexing_a_registered_folder_is_a_rescan_not_a_deadlock() {
        // The second run takes `folder_duplicate`, which re-locks the
        // catalog. A match-scrutinee guard once kept that lock across the
        // arm and deadlocked the call (non-reentrant mutex).
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("belgeler");
        std::fs::create_dir_all(&docs).unwrap();
        write_docx(&docs.join("tek.docx"), "Kira sözleşmesi");
        let engine = test_engine(&dir.path().join("data"));
        let path = docs.to_str().unwrap();
        let first = engine.index_folder_blocking(path, &|_| {}).unwrap();
        let second = engine.index_folder_blocking(path, &|_| {}).unwrap();
        assert_eq!(first, second);
    }

    fn paths(engine: &Engine, query: &str) -> Vec<String> {
        let mut out: Vec<String> = engine.search(&request(query, 50, 0)).unwrap().hits.into_iter().map(|h| h.path).collect();
        out.sort();
        out
    }

    fn key(path: &Path) -> String {
        path.to_str().unwrap().to_string()
    }

    #[test]
    fn deleting_a_subfolder_drops_its_records() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        let old = root.join("2025");
        std::fs::create_dir_all(&old).unwrap();
        write_docx(&old.join("a.docx"), "kira sozlesmesi");
        write_docx(&old.join("b.docx"), "kira bedeli");
        write_docx(&root.join("c.docx"), "kira makbuzu");
        let engine = test_engine(&dir.path().join("data"));
        engine.index_folder_blocking(&key(&root), &|_| {}).unwrap();
        assert_eq!(paths(&engine, "kira").len(), 3);

        std::fs::remove_dir_all(&old).unwrap();
        // The watcher reports only the directory path for a folder delete.
        engine.detached_worker().update_paths(vec![old.clone()]).unwrap();
        assert_eq!(paths(&engine, "kira"), [key(&root.join("c.docx"))]);
        assert_eq!(engine.folders().unwrap()[0].indexed_count, 1);
    }

    #[test]
    fn renaming_a_subfolder_moves_its_records() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        let old = root.join("eski");
        let new = root.join("yeni");
        std::fs::create_dir_all(&old).unwrap();
        write_docx(&old.join("a.docx"), "fatura sozlesme");
        let engine = test_engine(&dir.path().join("data"));
        engine.index_folder_blocking(&key(&root), &|_| {}).unwrap();

        std::fs::rename(&old, &new).unwrap();
        engine.detached_worker().update_paths(vec![old.clone(), new.clone()]).unwrap();
        assert_eq!(paths(&engine, "fatura"), [key(&new.join("a.docx"))]);
        assert!(engine.document(&key(&old.join("a.docx")), "").is_err(), "ghost record left behind");
        assert!(engine.failures(None).unwrap().is_empty());
    }

    #[test]
    fn a_missing_root_keeps_its_records_and_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        write_docx(&root.join("a.docx"), "dilekce basvuru");
        let engine = test_engine(&dir.path().join("data"));
        engine.index_folder_blocking(&key(&root), &|_| {}).unwrap();
        assert!(!engine.folders().unwrap()[0].missing);

        let parked = dir.path().join("park");
        std::fs::rename(&root, &parked).unwrap();
        let worker = engine.detached_worker();
        worker.update_paths(vec![root.clone()]).unwrap();
        worker.update_paths(vec![root.join("a.docx")]).unwrap();
        worker.handle(Job::All).unwrap();
        let folder = &engine.folders().unwrap()[0];
        assert!(folder.missing);
        assert_eq!(folder.indexed_count, 1, "records must survive a missing root");
        assert_eq!(paths(&engine, "dilekce").len(), 1);

        std::fs::rename(&parked, &root).unwrap();
        assert!(!engine.folders().unwrap()[0].missing);
        engine.reindex(None).unwrap();
    }

    /// OCR is unavailable in the test engine, so a scanned image can only
    /// become searchable by reusing the pages of an identical file.
    #[test]
    fn a_copied_scan_reuses_stored_pages_instead_of_ocr() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        let engine = test_engine(&dir.path().join("data"));
        engine.index_folder_blocking(&key(&root), &|_| {}).unwrap();

        let original = root.join("tarama.png");
        std::fs::write(&original, b"\x89PNG fake scan bytes").unwrap();
        let pages = [PageText::new(0, "Taranmış kira sözleşmesi".into(), TextSource::Ocr, Some(0.92))];
        let record = FileRecord {
            path: key(&original),
            folder_id: 1,
            kind: FileKind::Image,
            size: 1,
            modified: 1,
            page_count: 1,
            ocr_pages: 1,
            low_confidence_pages: 0,
            failed_pages: 0,
            content_hash: Some(content_hash(&original).unwrap()),
            ocr_language: Some("tur".into()),
        };
        engine.catalog.lock().store(&record, &pages, &[], None, true, 1).unwrap();

        let copy = root.join("alt").join("kopya.png");
        std::fs::create_dir_all(copy.parent().unwrap()).unwrap();
        std::fs::copy(&original, &copy).unwrap();
        engine.detached_worker().update_paths(vec![copy.clone()]).unwrap();
        assert_eq!(paths(&engine, "sözleşmesi"), [key(&copy)]);
        let view = engine.document(&key(&copy), "kira").unwrap();
        assert_eq!(view.pages[0].confidence, Some(0.92));
        assert!(engine.failures(None).unwrap().is_empty());

        // A moved file whose only source is the path vanishing in the same
        // batch: vanished records are dropped after reuse, not before.
        std::fs::remove_file(&original).unwrap();
        engine.detached_worker().update_paths(vec![original.clone()]).unwrap();
        assert_eq!(paths(&engine, "kira"), [key(&copy)]);
        let moved = root.join("tasindi.png");
        std::fs::rename(&copy, &moved).unwrap();
        engine.detached_worker().update_paths(vec![copy.clone(), moved.clone()]).unwrap();
        assert_eq!(paths(&engine, "kira"), [key(&moved)]);
    }

    #[test]
    fn page_images_render_pdf_pages_and_are_cached() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        let pdfium_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/pdfium");
        let library = pdfium_render::prelude::Pdfium::pdfium_platform_library_name_at_path(&pdfium_dir);
        if !Path::new(&library).exists() {
            eprintln!("skipped: bundled pdfium unavailable");
            return;
        }
        {
            use pdfium_render::prelude::*;
            let pdfium = match Pdfium::bind_to_library(&library) {
                Ok(bindings) => Pdfium::new(bindings),
                Err(PdfiumError::PdfiumLibraryBindingsAlreadyInitialized) => Pdfium::default(),
                Err(error) => panic!("{error}"),
            };
            let mut document = pdfium.create_new_pdf().unwrap();
            let font = document.fonts_mut().helvetica();
            for text in ["Kira sozlesmesi birinci sayfa metni", "Ikinci sayfa kira bedeli metni"] {
                let mut page = document.pages_mut().create_page_at_end(PdfPagePaperSize::a4()).unwrap();
                page.objects_mut()
                    .create_text_object(PdfPoints::new(50.0), PdfPoints::new(700.0), text, font, PdfPoints::new(14.0))
                    .unwrap();
            }
            document.save_to_file(&root.join("kira.pdf")).unwrap();
        }
        write_docx(&root.join("not.docx"), "kira notu");
        let data = dir.path().join("data");
        let engine = Engine::open(
            EnginePaths {
                data_dir: data.clone(),
                pdfium_dir,
                ocr: OcrConfig {
                    binary: data.join("no-tesseract"),
                    tessdata: data.join("no-tessdata"),
                    language: "tur".into(),
                    timeout: Duration::from_secs(5),
                    threads: 1,
                },
                ocr_jobs: 1,
                deskew: false,
                embed: None,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap();
        engine.index_folder_blocking(&key(&root), &|_| {}).unwrap();
        let pdf = key(&root.join("kira.pdf"));
        assert!(engine.failures(None).unwrap().is_empty(), "{:?}", engine.failures(None).unwrap());

        let image = engine.page_image(&pdf, 1, 400).unwrap();
        assert_eq!((image.page, image.width), (1, 400));
        assert!(image.height > 500, "A4 aspect: {}", image.height);
        assert!(image.data_url.starts_with("data:image/jpeg;base64,/9j/"), "JPEG data URL");
        assert_eq!(engine.page_image(&pdf, 0, 10).unwrap().width, PAGE_IMAGE_MIN_WIDTH, "width is clamped");

        // Cached: still served after the file disappears.
        std::fs::rename(root.join("kira.pdf"), dir.path().join("moved.pdf")).unwrap();
        assert_eq!(engine.page_image(&pdf, 1, 400).unwrap().data_url, image.data_url);
        let error = engine.page_image(&pdf, 0, 600).unwrap_err();
        assert_eq!(error.downcast_ref::<EngineError>().map(EngineError::code), Some("file_missing"));

        let code = |path: &str, page| engine.page_image(path, page, 400).unwrap_err().downcast_ref::<EngineError>().map(EngineError::code);
        assert_eq!(code(&pdf, 2), Some("preview_unavailable"), "page out of range");
        assert_eq!(code(&key(&root.join("not.docx")), 0), Some("preview_unavailable"));
        assert_eq!(code("nowhere.pdf", 0), Some("not_indexed"));
    }

    /// File names are searchable: an empty file (or a photo with no text)
    /// is found by its name, after the content matches, with `name_only`.
    /// An empty file is an empty document, never an unreadable one.
    #[test]
    fn empty_files_index_and_file_names_are_searchable() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arşiv عربي");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("Yeni Microsoft Word Belgesi.docx"), b"").unwrap();
        std::fs::write(root.join("Şirket Notları.txt"), b"").unwrap();
        std::fs::write(root.join("kira.txt"), "kira bedeli ödendi").unwrap();
        std::fs::write(root.join("not.txt"), "belge notları burada").unwrap();
        let engine = test_engine(&dir.path().join("data"));
        engine.add_folder(&key(&root), None).unwrap();
        flush(&engine);
        assert!(engine.failures(None).unwrap().is_empty(), "{:?}", engine.failures(None).unwrap());

        let response = engine.search(&request("microsoft word", 10, 0)).unwrap();
        assert_eq!(response.total, 1);
        assert_eq!(response.hits[0].path, key(&root.join("Yeni Microsoft Word Belgesi.docx")));
        assert!(response.hits[0].name_only);

        // Folded: "notlari" finds "Notları"; name matches come first.
        let response = engine.search(&request("notlari", 10, 0)).unwrap();
        let found: Vec<(String, bool)> = response.hits.iter().map(|h| (h.path.clone(), h.name_only)).collect();
        assert_eq!(
            found,
            [(key(&root.join("Şirket Notları.txt")), true), (key(&root.join("not.txt")), false)]
        );
        assert_eq!(response.total, 2);
        // Paging keeps the order.
        let second = engine.search(&request("notlari", 1, 1)).unwrap();
        assert_eq!(second.hits[0].path, key(&root.join("not.txt")));
        assert_eq!(second.total, 2);
        // A file that matches by content is never listed twice.
        let response = engine.search(&request("kira", 10, 0)).unwrap();
        assert_eq!(response.total, 1);
        assert!(!response.hits[0].name_only);
        // Kind filter applies to name matches too.
        let mut filtered = request("microsoft", 10, 0);
        filtered.kinds = vec![FileKind::Text];
        assert_eq!(engine.search(&filtered).unwrap().total, 0);
        // A quoted phrase is content-only.
        assert_eq!(engine.search(&request("\"microsoft word\"", 10, 0)).unwrap().total, 0);
    }

    /// A failure recorded by an older extractor is retried by the next scan,
    /// and "Tekrar dene" re-reads failures right away.
    #[test]
    fn old_failures_are_retried_and_retry_rereads_now() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("c.docx"), b"not a document").unwrap();
        let engine = test_engine(&dir.path().join("data"));
        let folder = engine.add_folder(&key(&root), None).unwrap();
        flush(&engine);
        let failures = engine.failures(None).unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].reason, "damaged");
        let first_at = failures[0].at;

        // Fixed on disk with the same size and time is still a failure for
        // this generation: a plain rescan does not touch it…
        engine.reindex(Some(folder.id)).unwrap();
        flush(&engine);
        assert_eq!(engine.failures(None).unwrap()[0].at, first_at);
        // …an older generation's failure is retried by the scan.
        engine.catalog.lock().db_for_tests().execute("UPDATE failures SET version=0, at=0", []).unwrap();
        engine.reindex(Some(folder.id)).unwrap();
        flush(&engine);
        assert_ne!(engine.failures(None).unwrap()[0].at, 0, "re-read by the scan");

        // Retry: re-read now; a file that now reads leaves the list.
        write_docx(&root.join("c.docx"), "artık okunuyor");
        assert_eq!(engine.retry_failures(None).unwrap(), 1);
        flush(&engine);
        assert!(engine.failures(None).unwrap().is_empty(), "{:?}", engine.failures(None).unwrap());
    }

    #[test]
    fn reading_a_file_is_not_a_change() {
        use notify::event::{AccessKind, AccessMode, CreateKind, ModifyKind, RemoveKind, RenameMode};
        use notify::EventKind;
        assert!(!may_change_content(&EventKind::Access(AccessKind::Open(AccessMode::Any))));
        assert!(!may_change_content(&EventKind::Access(AccessKind::Read)));
        assert!(!may_change_content(&EventKind::Access(AccessKind::Close(AccessMode::Read))));
        assert!(may_change_content(&EventKind::Access(AccessKind::Close(AccessMode::Write))));
        assert!(may_change_content(&EventKind::Create(CreateKind::File)));
        assert!(may_change_content(&EventKind::Modify(ModifyKind::Name(RenameMode::Both))));
        assert!(may_change_content(&EventKind::Remove(RemoveKind::Folder)));
        assert!(may_change_content(&EventKind::Any));
    }

    /// Waits until every job queued so far has run (or was dropped by a cancel).
    fn flush(engine: &Engine) {
        let (done, wait) = channel();
        engine.jobs.send(Job::Barrier(done)).unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(60));
    }

    fn error_code(error: anyhow::Error) -> Option<&'static str> {
        error.downcast_ref::<EngineError>().map(EngineError::code)
    }

    #[test]
    fn folder_kind_selection_drives_scans_the_watcher_and_removals() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(root.join("alt")).unwrap();
        write_docx(&root.join("a.docx"), "kira sozlesmesi");
        std::fs::write(root.join("alt").join("b.txt"), "kira notu").unwrap();
        std::fs::write(root.join("c.xlsx"), b"not a workbook").unwrap();
        let engine = test_engine(&dir.path().join("data"));

        let folder = engine.add_folder(&key(&root), Some(&[FileKind::Word])).unwrap();
        assert_eq!(folder.kinds, [FileKind::Word]);
        flush(&engine);
        assert_eq!(paths(&engine, "kira"), [key(&root.join("a.docx"))]);
        assert!(engine.failures(None).unwrap().is_empty(), "unselected kinds are not even tried");
        assert_eq!(engine.folders().unwrap()[0].file_count, 1);

        // Swap the selection: Word records go, Excel and text come in.
        let folder = engine.set_folder_kinds(folder.id, &[FileKind::Text, FileKind::Excel]).unwrap();
        assert_eq!(folder.kinds, [FileKind::Excel, FileKind::Text]);
        flush(&engine);
        assert_eq!(paths(&engine, "kira"), [key(&root.join("alt").join("b.txt"))]);
        assert_eq!(error_code(engine.document(&key(&root.join("a.docx")), "").unwrap_err()), Some("not_indexed"));
        let failures = engine.failures(None).unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].path, key(&root.join("c.xlsx")));

        // The watcher follows the selection.
        write_docx(&root.join("alt").join("d.docx"), "kira makbuzu");
        std::fs::write(root.join("e.txt"), "kira makbuzu").unwrap();
        engine.jobs.send(Job::Paths(vec![root.join("alt").join("d.docx"), root.join("e.txt")])).unwrap();
        engine.jobs.send(Job::Paths(vec![root.join("alt")])).unwrap();
        flush(&engine);
        assert_eq!(paths(&engine, "makbuzu"), [key(&root.join("e.txt"))]);
        let info = &engine.folders().unwrap()[0];
        assert_eq!((info.indexed_count, info.failed_count, info.file_count), (2, 1, 3), "watcher adds count as found");
        std::fs::remove_file(root.join("e.txt")).unwrap();
        engine.jobs.send(Job::Paths(vec![root.join("e.txt")])).unwrap();
        flush(&engine);
        assert_eq!(engine.folders().unwrap()[0].file_count, 2, "watcher removals leave the file count");
        std::fs::write(root.join("e.txt"), "kira makbuzu").unwrap();
        engine.jobs.send(Job::Paths(vec![root.join("e.txt")])).unwrap();
        flush(&engine);

        // Dropping a kind also clears its failures.
        engine.set_folder_kinds(folder.id, &[FileKind::Text]).unwrap();
        flush(&engine);
        assert!(engine.failures(None).unwrap().is_empty());
        assert_eq!(engine.folders().unwrap()[0].indexed_count, 2);
        assert_eq!(engine.folders().unwrap()[0].file_count, 2, "purged kinds leave the file count");

        // Records go even while the folder itself is missing.
        let parked = dir.path().join("park");
        std::fs::rename(&root, &parked).unwrap();
        engine.set_folder_kinds(folder.id, &[FileKind::Pdf]).unwrap();
        flush(&engine);
        let info = &engine.folders().unwrap()[0];
        assert!(info.missing);
        assert_eq!((info.indexed_count, info.failed_count), (0, 0));
        assert!(paths(&engine, "kira").is_empty());

        // The selection is kept across restarts.
        std::fs::rename(&parked, &root).unwrap();
        drop(engine);
        let engine = test_engine(&dir.path().join("data"));
        assert_eq!(engine.folders().unwrap()[0].kinds, [FileKind::Pdf]);
        assert!(paths(&engine, "kira").is_empty(), "start-up scan honours the selection");

        assert_eq!(error_code(engine.set_folder_kinds(folder.id, &[]).unwrap_err()), Some("kinds_empty"));
        assert_eq!(error_code(engine.set_folder_kinds(999, &[FileKind::Pdf]).unwrap_err()), Some("folder_missing"));
        let other = dir.path().join("diger");
        std::fs::create_dir_all(&other).unwrap();
        assert_eq!(error_code(engine.add_folder(&key(&other), Some(&[])).unwrap_err()), Some("kinds_empty"));
        assert_eq!(engine.folders().unwrap().len(), 1, "rejected folder was not added");
    }

    #[test]
    fn log_files_are_opt_in_and_toggle_in_and_out_of_the_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("app.log"), "2026-10-15 INFO fatura 4471923 kesildi").unwrap();
        std::fs::write(root.join("notlar.md"), "# fatura notu").unwrap();
        std::fs::write(root.join("liste.csv"), "no;fatura\n1;kesildi").unwrap();
        let engine = test_engine(&dir.path().join("data"));

        // Default selection (what a new folder gets): plain text in, logs out.
        let folder = engine.add_folder(&key(&root), None).unwrap();
        assert_eq!(folder.kinds, FileKind::DEFAULT);
        flush(&engine);
        assert_eq!(paths(&engine, "fatura"), [key(&root.join("liste.csv")), key(&root.join("notlar.md"))]);
        assert!(paths(&engine, "4471923").is_empty(), "logs are not indexed by default");

        // Turning logs on indexes them ...
        let folder = engine.set_folder_kinds(folder.id, &FileKind::ALL).unwrap();
        assert_eq!(folder.kinds, FileKind::ALL);
        flush(&engine);
        assert_eq!(paths(&engine, "4471923"), [key(&root.join("app.log"))]);
        let hit = engine.search(&request("4471923", 5, 0)).unwrap().hits.remove(0);
        assert_eq!(hit.kind, FileKind::Log);

        // ... and turning them off drops them again.
        engine.set_folder_kinds(folder.id, &FileKind::DEFAULT).unwrap();
        flush(&engine);
        assert!(paths(&engine, "4471923").is_empty());
        assert_eq!(paths(&engine, "fatura").len(), 2);
    }

    /// Fake Tesseract: answers the start-up probe at once; page OCR answers
    /// "taranmis" unless `<dir>/slow` exists, then it hangs (pid logged).
    #[cfg(unix)]
    fn fake_tesseract(dir: &Path) -> OcrConfig {
        use std::os::unix::fs::PermissionsExt;
        let binary = dir.join("tesseract");
        let tsv = "printf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n'";
        std::fs::create_dir_all(dir.join("pids")).unwrap();
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'tesseract 9.9.9'; exit 0; fi\n\
                 cat > \"$0.in.$$\"\n\
                 if cmp -s \"$0.in.$$\" '{probe}'; then rm -f \"$0.in.$$\"; {tsv}; printf '5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t9\\t9\\t90\\t2026\\n'; exit 0; fi\n\
                 rm -f \"$0.in.$$\"\n\
                 if [ -e '{dir}/slow' ]; then touch '{dir}/pids/'$$; exec sleep 30; fi\n\
                 {tsv}; printf '5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t9\\t9\\t90\\ttaranmis\\n'\n",
                dir = dir.display(),
                probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/ocr-probe.png").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(dir.join("tur.traineddata"), b"model").unwrap();
        OcrConfig { binary, tessdata: dir.to_path_buf(), language: "tur".into(), timeout: Duration::from_secs(60), threads: 1 }
    }

    #[cfg(unix)]
    fn wait_for(what: &str, condition: impl Fn() -> bool) {
        let started = Instant::now();
        while !condition() {
            assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Engine with a fake Tesseract that "recognises" `dil<language>`
    /// (`+` written as `x`) and logs every page it reads, so a test sees
    /// which language read a scan and how often OCR ran.
    #[cfg(unix)]
    fn language_echo_engine(data: &Path, tools: &Path, models: &[&str]) -> Arc<Engine> {
        use std::os::unix::fs::PermissionsExt;
        let binary = tools.join("tesseract");
        let tsv = "printf 'level\\tpage_num\\tblock_num\\tpar_num\\tline_num\\tword_num\\tleft\\ttop\\twidth\\theight\\tconf\\ttext\\n'";
        std::fs::write(
            &binary,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = \"--version\" ]; then echo 'tesseract 9.9.9'; exit 0; fi\n\
                 lang=; prev=; for a in \"$@\"; do if [ \"$prev\" = \"-l\" ]; then lang=$a; fi; prev=$a; done\n\
                 cat > \"$0.in.$$\"\n\
                 if cmp -s \"$0.in.$$\" '{probe}'; then rm -f \"$0.in.$$\"; {tsv}; printf '5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t9\\t9\\t90\\t2026\\n'; exit 0; fi\n\
                 rm -f \"$0.in.$$\"\n\
                 echo \"$lang\" >> '{log}'\n\
                 {tsv}; printf '5\\t1\\t1\\t1\\t1\\t1\\t0\\t0\\t9\\t9\\t90\\tdil%s\\n' \"$(echo \"$lang\" | tr + x)\"\n",
                log = tools.join("calls.log").display(),
                probe = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets/ocr-probe.png").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        for model in models {
            std::fs::write(tools.join(format!("{model}.traineddata")), b"model").unwrap();
        }
        let engine = Engine::open(
            EnginePaths {
                data_dir: data.to_path_buf(),
                pdfium_dir: data.join("no-pdfium"),
                ocr: OcrConfig {
                    binary,
                    tessdata: tools.to_path_buf(),
                    language: "tur".into(),
                    timeout: Duration::from_secs(60),
                    threads: 1,
                },
                ocr_jobs: 1,
                deskew: false,
                embed: None,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap();
        engine.wait_ready();
        engine
    }

    /// The document language decides how scans are read: switching it reads
    /// every scan again with the new language (typed files are not touched),
    /// the choice survives a restart, and a language without its model is
    /// refused.
    #[cfg(unix)]
    #[test]
    fn changing_the_document_language_reads_scans_again_with_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        let tools = dir.path().join("ocr");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        let scan = root.join("tarama.png");
        image::GrayImage::from_pixel(32, 32, image::Luma([200])).save(&scan).unwrap();
        write_docx(&root.join("not.docx"), "yazili belge");
        let calls = || std::fs::read_to_string(tools.join("calls.log")).unwrap_or_default().lines().count();
        let text = |engine: &Engine| engine.catalog.lock().pages(&key(&scan)).unwrap()[0].text.trim().to_string();

        let engine = language_echo_engine(&dir.path().join("data"), &tools, &["tur", "eng"]);
        engine.add_folder(&key(&root), None).unwrap();
        flush(&engine);
        assert_eq!(text(&engine), "diltur");
        assert_eq!(calls(), 1);
        let info = engine.ocr_language().unwrap();
        assert_eq!(info.language, "tur");
        assert_eq!(info.available, ["tur", "eng", "tur+eng"]);
        assert_eq!(info.pending_rereads, 0);

        let info = engine.set_ocr_language("tur+eng").unwrap();
        assert_eq!((info.language.as_str(), info.pending_rereads), ("tur+eng", 1));
        flush(&engine);
        assert_eq!(text(&engine), "dilturxeng");
        assert_eq!(paths(&engine, "dilturxeng"), [key(&scan)]);
        assert_eq!(engine.ocr_language().unwrap().pending_rereads, 0);
        assert_eq!(calls(), 2, "only the scan is read again");
        assert_eq!(paths(&engine, "yazili"), [key(&root.join("not.docx"))]);

        // Picking the current language again reads nothing.
        engine.set_ocr_language("tur+eng").unwrap();
        flush(&engine);
        assert_eq!(calls(), 2);

        let refused = engine.set_ocr_language("deu").unwrap_err();
        assert_eq!(error_code(refused), Some("ocr_language_unavailable"));

        drop(engine);
        let engine = language_echo_engine(&dir.path().join("data"), &tools, &["tur", "eng"]);
        flush(&engine);
        assert_eq!(engine.ocr_language().unwrap().language, "tur+eng", "kept across restarts");
        assert_eq!(calls(), 2, "a restart reads nothing again");

        // A saved language whose model went missing falls back to the
        // configured one (and its scans are read again with it).
        drop(engine);
        std::fs::remove_file(tools.join("eng.traineddata")).unwrap();
        let engine = language_echo_engine(&dir.path().join("data"), &tools, &["tur"]);
        flush(&engine);
        let info = engine.ocr_language().unwrap();
        assert_eq!((info.language.as_str(), info.available.as_slice()), ("tur", ["tur".to_string()].as_slice()));
        assert_eq!(text(&engine), "diltur");
        assert_eq!(engine.set_ocr_language("eng").map_err(error_code).unwrap_err(), Some("ocr_language_unavailable"));
    }

    #[cfg(unix)]
    #[test]
    fn cancelling_a_scan_keeps_finished_files_and_a_rescan_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        let tools = dir.path().join("ocr");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        write_docx(&root.join("kira.docx"), "kira sozlesmesi");
        let progress_log: Arc<Mutex<Vec<IndexProgress>>> = Arc::default();
        let log = progress_log.clone();
        let engine = Engine::open(
            EnginePaths {
                data_dir: dir.path().join("data"),
                pdfium_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/pdfium"),
                ocr: fake_tesseract(&tools),
                ocr_jobs: 2,
                deskew: false,
                embed: None,
            },
            Arc::new(move |p: &IndexProgress| log.lock().push(p.clone())),
            Arc::new(|_| {}),
        )
        .unwrap();
        engine.wait_ready();
        assert!(!engine.cancel_indexing(), "nothing to cancel while idle");
        let folder = engine.add_folder(&key(&root), None).unwrap();
        flush(&engine);
        assert_eq!(paths(&engine, "kira").len(), 1);
        // This test queues every scan itself; a watcher batch for the files
        // written below would land between its steps.
        *engine.watcher.lock() = None;

        // Four scans whose OCR hangs, plus a queued scan of another folder.
        std::fs::write(tools.join("slow"), b"").unwrap();
        for i in 0..4u8 {
            image::GrayImage::from_pixel(32, 32, image::Luma([200 + i])).save(root.join(format!("tarama-{i}.png"))).unwrap();
        }
        engine.reindex(Some(folder.id)).unwrap();
        wait_for("OCR to start", || std::fs::read_dir(tools.join("pids")).unwrap().count() == 2);
        engine.jobs.send(Job::All).unwrap();
        assert_eq!(engine.progress().phase, IndexPhase::Indexing);

        let started = Instant::now();
        assert!(engine.cancel_indexing());
        assert!(engine.progress().cancelling);
        flush(&engine);
        wait_for("the run to stop", || engine.progress().phase == IndexPhase::Idle && !engine.progress().cancelling);
        assert!(started.elapsed() < Duration::from_secs(10), "OCR was waited for, not killed: {:?}", started.elapsed());
        let progress = engine.progress();
        assert!(progress.cancelled, "{progress:?}");
        assert_eq!((progress.done, progress.failed), (0, 0), "no scan counted as processed");
        assert!(progress_log.lock().iter().any(|p| p.cancelling), "the UI saw the cancel in progress");
        for pid in std::fs::read_dir(tools.join("pids")).unwrap() {
            let pid = pid.unwrap().file_name();
            assert!(!Path::new("/proc").join(&pid).exists(), "tesseract {pid:?} still running");
        }
        // Nothing half-done was written: not indexed, not recorded as failed.
        assert!(engine.failures(None).unwrap().is_empty(), "{:?}", engine.failures(None).unwrap());
        for i in 0..4 {
            let scan = key(&root.join(format!("tarama-{i}.png")));
            assert_eq!(error_code(engine.document(&scan, "").unwrap_err()), Some("not_indexed"));
        }
        // What was indexed before stays searchable; the queued scan was dropped.
        assert_eq!(paths(&engine, "kira"), [key(&root.join("kira.docx"))]);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(engine.progress().phase, IndexPhase::Idle, "queued scan must not start after a cancel");

        // Resume: only the unfinished files are queued.
        std::fs::remove_file(tools.join("slow")).unwrap();
        progress_log.lock().clear();
        engine.reindex(Some(folder.id)).unwrap();
        flush(&engine);
        let progress = engine.progress();
        assert!(!progress.cancelled);
        assert_eq!((progress.total, progress.done, progress.failed), (4, 4, 0));
        assert_eq!(paths(&engine, "taranmis").len(), 4);
        assert_eq!(paths(&engine, "kira").len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn removing_a_folder_mid_scan_lets_the_other_folders_finish() {
        // Removing one folder cancels the running scan (it may be reading
        // that folder); the other folders' scan must carry on by itself
        // instead of being left "Durduruldu".
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("kalan");
        let gone = dir.path().join("silinen");
        let tools = dir.path().join("ocr");
        for d in [&keep, &gone, &tools] {
            std::fs::create_dir_all(d).unwrap();
        }
        write_docx(&gone.join("eski.docx"), "eski kayit");
        let engine = Engine::open(
            EnginePaths {
                data_dir: dir.path().join("data"),
                pdfium_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/pdfium"),
                ocr: fake_tesseract(&tools),
                ocr_jobs: 2,
                deskew: false,
                embed: None,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap();
        engine.wait_ready();
        let removed = engine.add_folder(&key(&gone), None).unwrap();
        flush(&engine);
        assert_eq!(paths(&engine, "eski").len(), 1);

        // A slow scan of the other folder is running when the removal comes.
        std::fs::write(tools.join("slow"), b"").unwrap();
        for i in 0..3u8 {
            image::GrayImage::from_pixel(32, 32, image::Luma([200 + i])).save(keep.join(format!("tarama-{i}.png"))).unwrap();
        }
        let kept = engine.add_folder(&key(&keep), None).unwrap();
        wait_for("OCR to start", || std::fs::read_dir(tools.join("pids")).unwrap().count() > 0);
        std::fs::remove_file(tools.join("slow")).unwrap();

        let started = Instant::now();
        engine.remove_folder(removed.id).unwrap();
        assert!(started.elapsed() < Duration::from_secs(10), "removal waited {:?}", started.elapsed());
        assert!(paths(&engine, "eski").is_empty());

        wait_for("the kept folder to finish", || {
            let p = engine.progress();
            p.phase == IndexPhase::Idle && !p.cancelling && paths(&engine, "taranmis").len() == 3
        });
        let progress = engine.progress();
        assert!(!progress.cancelled, "{progress:?}");
        let folders = engine.folders().unwrap();
        assert_eq!(folders.iter().map(|f| f.id).collect::<Vec<_>>(), [kept.id]);
        assert_eq!(folders[0].indexed_count, 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_cancelled_scan_reports_cancelled_and_starts_no_embedding_run() {
        // Regression: after "İptal" the worker loop saw
        // `Ok(())`, so a fresh Embedding run (the unembedded-file backfill)
        // started on its own. The scan must report `Cancelled` and the
        // backfill must refuse to run while the flag is set.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        let tools = dir.path().join("ocr");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&tools).unwrap();
        std::fs::write(tools.join("slow"), b"").unwrap();
        for i in 0..4u8 {
            image::GrayImage::from_pixel(32, 32, image::Luma([200 + i])).save(root.join(format!("tarama-{i}.png"))).unwrap();
        }
        let engine = Engine::open(
            EnginePaths {
                data_dir: dir.path().join("data"),
                pdfium_dir: Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/pdfium"),
                ocr: fake_tesseract(&tools),
                ocr_jobs: 2,
                deskew: false,
                embed: None,
            },
            Arc::new(|_| {}),
            Arc::new(|_| {}),
        )
        .unwrap();
        engine.wait_ready();
        let folder = engine.add_folder(&key(&root), None).unwrap();
        engine.reindex(Some(folder.id)).unwrap();
        wait_for("OCR to start", || std::fs::read_dir(tools.join("pids")).unwrap().count() >= 1);
        assert!(engine.cancel_indexing());
        wait_for("the run to stop", || engine.progress().phase == IndexPhase::Idle && !engine.progress().cancelling);
        assert!(engine.progress().cancelled);
        // The scan reported `Cancelled` to the worker: no Embedding phase
        // started afterwards and the run stays Idle.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(engine.progress().phase, IndexPhase::Idle, "no embedding run after a cancel");
        // The gate is inside `backfill_embeddings` too: a flag still set
        // (cancel landing between jobs) refuses the pass outright.
        let worker = engine.detached_worker();
        engine.cancel.store(true, Ordering::SeqCst);
        let result = worker.backfill_embeddings(None);
        engine.cancel.store(false, Ordering::SeqCst);
        assert!(result.as_ref().is_err_and(crate::ocr::is_cancelled), "{result:?}");
    }

    fn unit_vectors(batch: usize) -> Vec<Vec<f32>> {
        let mut vector = vec![0.0f32; 768];
        vector[0] = 1.0;
        vec![vector; batch]
    }

    #[test]
    fn only_a_finished_embed_or_a_textless_file_counts_as_embedded() {
        assert!(EmbedOutcome::Copied.marks_embedded(true));
        assert!(EmbedOutcome::Embedded(Vec::new()).marks_embedded(true), "markup that cleans to nothing is done");
        for pending in [EmbedOutcome::Cancelled, EmbedOutcome::Failed, EmbedOutcome::NoEmbedder] {
            assert!(!pending.marks_embedded(true), "{pending:?} must wait for the backfill");
            assert!(pending.marks_embedded(false), "{pending:?}: a file without text has nothing to embed");
        }
    }

    /// Regression (2026-10-15): "İptal" during the scan
    /// turned the cancelled embed into an empty chunk list
    /// (`unwrap_or_default`) and stored every file with embedded=1 and no
    /// chunks — the backfill never saw them again and the run reported
    /// `cancelled:false`. A cancelled embed must keep the file's text
    /// (keyword search) at embedded=0, stop the scan at the next file, report
    /// the run cancelled, and the next scan + backfill must embed the rest.
    #[test]
    fn a_cancelled_embed_keeps_files_unembedded_and_reports_the_run_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        let files: Vec<PathBuf> = (0..12).map(|i| root.join(format!("not-{i:02}.txt"))).collect();
        for (i, file) in files.iter().enumerate() {
            std::fs::write(file, format!("kira notu {i} benzersiz satir {i} odeme bilgisi")).unwrap();
        }
        let engine = test_engine(&dir.path().join("data"));
        *engine.watcher.lock() = None;
        let cancel = engine.cancel.clone();
        // The first model run lands the cancel ("İptal" while a batch runs).
        let cancelling = Arc::new(Embedder::fake({
            let cancel = cancel.clone();
            move |batch| {
                cancel.store(true, Ordering::SeqCst);
                Ok(unit_vectors(batch))
            }
        }));
        let worker = Worker { embedder: Some(cancelling), ..engine.detached_worker() };
        let _scan = engine.scan.lock();
        let id = engine.ensure_folder(&key(&root)).unwrap();
        let result = worker.scan_folders(&[(id, key(&root))], None);
        assert!(result.as_ref().is_err_and(is_cancelled), "{result:?}");
        let progress = engine.progress();
        assert!(progress.cancelled, "the run must report cancelled: {progress:?}");
        assert!(progress.done >= 1 && progress.done < files.len(), "the scan stops at a file boundary: {progress:?}");
        // the two stages are told apart — text read, semantic
        // index not done — and the files never read are counted.
        assert_eq!(progress.text_done, progress.done, "{progress:?}");
        assert_eq!(progress.embed_total, progress.done, "every read file has text: {progress:?}");
        assert_eq!(progress.embed_done, 0, "no embed finished: {progress:?}");
        assert_eq!(progress.unread, files.len() - progress.done, "{progress:?}");
        assert_eq!(progress.pending_embed, Some(progress.done), "idle snapshot counts the pending embeds: {progress:?}");
        {
            let catalog = engine.catalog.lock();
            let pending = catalog.unembedded_paths().unwrap();
            let mut stored = 0;
            for file in &files {
                let path = key(file);
                if catalog.file(&path).unwrap().is_some() {
                    stored += 1;
                    assert!(pending.contains(&path), "{path}: a cancelled embed must stay embedded=0");
                    assert!(catalog.chunks(&path).unwrap().is_empty());
                }
            }
            assert_eq!(stored, progress.done, "files whose text was read are kept (keyword search)");
        }
        worker.settle(&result, None);
        assert!(!cancel.load(Ordering::SeqCst));

        // Resume: the rescan reads the files the cancel skipped, the
        // backfill embeds the ones it left at embedded=0.
        let working = Arc::new(Embedder::fake(|batch| Ok(unit_vectors(batch))));
        let worker = Worker { embedder: Some(working), ..engine.detached_worker() };
        worker.scan_folders(&[(id, key(&root))], None).unwrap();
        let progress = engine.progress();
        assert!(!progress.cancelled);
        assert_eq!(progress.unread, 0);
        assert_eq!((progress.embed_done, progress.embed_total), (progress.done, progress.done), "{progress:?}");
        assert!(progress.pending_embed.is_some_and(|n| n > 0), "the cancelled files still wait: {progress:?}");
        worker.backfill_embeddings(None).unwrap();
        let progress = engine.progress();
        assert_eq!(progress.embed_done, progress.embed_total, "{progress:?}");
        assert_eq!(progress.pending_embed, Some(0), "{progress:?}");
        let catalog = engine.catalog.lock();
        assert!(catalog.unembedded_paths().unwrap().is_empty());
        for file in &files {
            assert!(!catalog.chunks(&key(file)).unwrap().is_empty(), "{} has no vectors", key(file));
        }
    }

    /// a normal scan counts text read and semantic index
    /// separately; a file without text needs no semantic index; without a
    /// model the second stage stays empty and nothing is "pending".
    #[test]
    fn progress_counts_text_and_semantic_stages_separately() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..3 {
            std::fs::write(root.join(format!("not-{i}.txt")), format!("fatura notu {i} odeme")).unwrap();
        }
        std::fs::write(root.join("bos.txt"), "").unwrap();
        let engine = test_engine(&dir.path().join("data"));
        *engine.watcher.lock() = None;
        let _scan = engine.scan.lock();
        let id = engine.ensure_folder(&key(&root)).unwrap();

        // No model: text only.
        engine.detached_worker().scan_folders(&[(id, key(&root))], None).unwrap();
        let progress = engine.progress();
        assert_eq!((progress.done, progress.text_done, progress.total), (4, 4, 4));
        assert_eq!((progress.embed_done, progress.embed_total, progress.pending_embed), (0, 0, None), "{progress:?}");
        assert!(engine.folders().unwrap().iter().all(|f| f.pending_embed_count == 0));

        // With a model: the 3 text files need (and get) a semantic index.
        for i in 0..3 {
            std::fs::write(root.join(format!("not-{i}.txt")), format!("fatura notu {i} odeme yeni")).unwrap();
        }
        // Stage 1 is reported before the embed runs: while a file embeds,
        // its text already counts, `done` does not yet.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let working = Arc::new(Embedder::fake({
            let progress = engine.progress.clone();
            let seen = seen.clone();
            move |batch| {
                let p = progress.lock().clone();
                seen.lock().push((p.text_done, p.done));
                Ok(unit_vectors(batch))
            }
        }));
        let worker = Worker { embedder: Some(working), ..engine.detached_worker() };
        worker.scan_folders(&[(id, key(&root))], None).unwrap();
        let seen = seen.lock().clone();
        assert!(!seen.is_empty() && seen.iter().all(|(text, done)| text > done), "{seen:?}");
        let progress = engine.progress();
        assert_eq!((progress.embed_done, progress.embed_total), (3, 3), "{progress:?}");
        assert_eq!(progress.pending_embed, Some(0));
        let json = serde_json::to_value(&progress).unwrap();
        for field in ["textDone", "embedDone", "embedTotal", "unread", "pendingEmbed"] {
            assert!(json.get(field).is_some(), "{field} missing in {json}");
        }
    }

    /// a scan's embed reports the running file's chunks like
    /// the backfill does (the semantic stage used to move only per finished
    /// file: one big file held it at "0 / 1" for minutes), and the chunk
    /// line is gone once the file is done.
    #[test]
    fn a_scan_reports_the_chunks_of_the_file_it_embeds() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        let text: String = (0..900).map(|i| format!("kira sözleşmesi madde {i} ödeme günü ")).collect();
        std::fs::write(root.join("uzun.txt"), &text).unwrap();
        let engine = test_engine(&dir.path().join("data"));
        *engine.watcher.lock() = None;
        let _scan = engine.scan.lock();
        let id = engine.ensure_folder(&key(&root)).unwrap();
        // What the progress said while each chunk ran.
        let seen = Arc::new(Mutex::new(Vec::new()));
        let working = Arc::new(Embedder::fake({
            let progress = engine.progress.clone();
            let seen = seen.clone();
            move |batch| {
                let p = progress.lock().clone();
                seen.lock().push((p.phase, p.current, p.chunks_done, p.chunks_total, p.embed_done));
                Ok(unit_vectors(batch))
            }
        }));
        let events = Arc::new(Mutex::new(Vec::new()));
        let record = {
            let events = events.clone();
            move |p: &IndexProgress| events.lock().push((p.chunks_done, p.chunks_total))
        };
        let observer: &(dyn Fn(&IndexProgress) + Sync) = &record;
        let worker = Worker { embedder: Some(working), ..engine.detached_worker() };
        worker.scan_folders(&[(id, key(&root))], Some(observer)).unwrap();

        let seen = seen.lock().clone();
        let chunks = seen.len();
        assert!(chunks >= 3, "a multi-chunk file: {chunks}");
        let file = key(&root.join("uzun.txt"));
        for (n, (phase, current, done, total, embed_done)) in seen.iter().enumerate() {
            assert_eq!(*phase, IndexPhase::Indexing);
            assert_eq!(current.as_deref(), Some(file.as_str()), "the embedding file is shown");
            assert_eq!((*done, *total), (Some(n), Some(chunks)), "chunk {n} of {chunks}");
            assert_eq!(*embed_done, 0, "the file is not counted before it finishes");
        }
        // The last chunk is always reported (not throttled).
        assert!(events.lock().contains(&(Some(chunks), Some(chunks))), "{:?}", events.lock());
        let progress = engine.progress();
        assert_eq!((progress.embed_done, progress.embed_total), (1, 1));
        assert!(progress.chunks_done.is_none() && progress.chunks_total.is_none() && progress.embedding_elapsed_secs.is_none());
        // Same vectors as before: one per chunk, stored with the file.
        assert_eq!(engine.catalog.lock().chunks(&file).unwrap().len(), chunks);
    }

    /// "Şimdi tamamla": the Embed job runs the backfill for the
    /// files a cancel left at embedded=0 and reports the semantic stage.
    #[test]
    fn the_embed_job_completes_pending_semantic_indexes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("arsiv");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..4 {
            std::fs::write(root.join(format!("not-{i}.txt")), format!("kira notu {i} benzersiz")).unwrap();
        }
        let engine = test_engine(&dir.path().join("data"));
        *engine.watcher.lock() = None;
        let _scan = engine.scan.lock();
        let id = engine.ensure_folder(&key(&root)).unwrap();
        // Text read, every embed failed (as after a cancel): all pending.
        let broken = Arc::new(Embedder::fake(|_| Err(anyhow!("model busy"))));
        let worker = Worker { embedder: Some(broken), ..engine.detached_worker() };
        worker.scan_folders(&[(id, key(&root))], None).unwrap();
        let progress = engine.progress();
        assert_eq!((progress.done, progress.embed_done, progress.embed_total), (4, 0, 4), "{progress:?}");
        assert_eq!(progress.pending_embed, Some(4));
        let working = Arc::new(Embedder::fake(|batch| Ok(unit_vectors(batch))));
        let worker = Worker { embedder: Some(working), ..engine.detached_worker() };
        worker.handle(Job::Embed).unwrap();
        let progress = engine.progress();
        assert_eq!(progress.phase, IndexPhase::Idle);
        assert_eq!((progress.done, progress.total, progress.embed_done, progress.embed_total), (4, 4, 4, 4), "{progress:?}");
        assert_eq!(progress.pending_embed, Some(0));
        assert_eq!(engine.catalog.lock().unembedded_count().unwrap(), 0);
    }

    #[test]
    fn a_discarded_index_is_rebuilt_from_the_catalog_without_re_extraction() {
        let dir = tempfile::tempdir().unwrap();
        let docs = dir.path().join("belgeler");
        let data = dir.path().join("data");
        std::fs::create_dir_all(&docs).unwrap();
        let file = docs.join("kira.docx");
        write_docx(&file, "Kira sozlesmesi tablo");
        {
            let engine = test_engine(&data);
            engine.index_folder_blocking(docs.to_str().unwrap(), &|_| {}).unwrap();
            assert_eq!(engine.search(&request("tablo", 10, 0)).unwrap().total, 1);
        }
        // Same size and mtime, different content: only re-extraction would see it.
        let modified = std::fs::metadata(&file).unwrap().modified().unwrap();
        write_docx(&file, "Kira sozlesmesi kalem");
        std::fs::File::options().write(true).open(&file).unwrap().set_modified(modified).unwrap();
        // Simulate an index from an older schema.
        std::fs::remove_file(data.join("index").join("arsiv-schema-version")).unwrap();

        let engine = test_engine(&data);
        assert_eq!(engine.search(&request("tablo", 10, 0)).unwrap().total, 1, "rebuilt from catalog");
        assert!(engine.search(&request("kalem", 10, 0)).is_ok_and(|r| r.total == 0), "file was re-read");
        let view = engine.document(file.to_str().unwrap(), "tablo").unwrap();
        assert_eq!(view.pages[0].highlights.len(), 1);
    }
}

/// Eval diagnostics: the raw candidate lists hybrid fusion sees, so ranking
/// rules can be swept offline (`arsiv-eval dump`). Not used by the app.
#[derive(Clone, Debug, Serialize)]
pub struct DebugCandidate {
    pub path: String,
    pub page: usize,
    pub score: f32,
    /// Query terms (index tokens) matched on this page, by term index.
    pub matched_terms: Vec<usize>,
    /// Vector side only: the file's top chunk cosines `(page, score)` —
    /// whether one window matched or several agree (diagnostics).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub chunks: Vec<(usize, f32)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct DebugCandidates {
    pub terms: Vec<String>,
    /// Files each query term matches on its own (document frequency).
    pub term_files: Vec<usize>,
    pub total_files: usize,
    pub bm25: Vec<DebugCandidate>,
    /// Vector side at the configured dim, cosine floor ignored.
    pub vectors: Vec<DebugCandidate>,
}

impl Engine {
    /// Exact query embedding for cross-process diagnostics. Uses the same
    /// embedder and quote cleanup as `search`, without loading ORT twice.
    pub fn debug_query_vector(&self, query: &str) -> Result<Vec<f32>> {
        let embedder = self.embedder.as_ref().ok_or_else(|| EngineError::new("semantic_unavailable", "No embedding model is bundled"))?;
        let text = query.replace(['"', '“', '”'], " ");
        embedder.embed_query(text.trim())
    }

    pub fn debug_candidates(&self, query: &str, depth: usize) -> Result<DebugCandidates> {
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let terms = query_terms(query);
        let options = self.index.options();
        let results = self.index.search_counted(query, &filter, depth)?;
        let term_files = terms
            .iter()
            .map(|t| self.index.count_files(t, &filter).unwrap_or(0))
            .collect();
        let matchers: Vec<Matcher> = terms.iter().map(|t| Matcher::new(std::slice::from_ref(t), &options)).collect();
        let catalog = self.catalog.lock();
        let matched = |path: &str, page: usize| -> Vec<usize> {
            match catalog.page_text(path, page) {
                Ok(Some(text)) => (0..matchers.len()).filter(|&i| !matchers[i].ranges(&text).is_empty()).collect(),
                _ => Vec::new(),
            }
        };
        let bm25 = results
            .hits
            .iter()
            .map(|h| DebugCandidate { path: h.path.clone(), page: h.page, score: h.score, matched_terms: matched(&h.path, h.page), chunks: Vec::new() })
            .collect();
        let mut hybrid = *self.hybrid.read();
        hybrid.min_cosine = -1.0;
        hybrid.candidates = depth;
        let vectors = match &self.embedder {
            Some(embedder) if !self.vectors.is_empty() => {
                let text = query.replace(['"', '“', '”'], " ");
                match embedder.embed_query(text.trim()) {
                    Ok(q) => {
                        let chunks = self.vectors.search_chunks(&q, &filter, &hybrid, 4);
                        self.vectors
                            .search(&q, &filter, &hybrid)
                            .into_iter()
                            .map(|h| DebugCandidate {
                                path: h.path.clone(),
                                page: h.page,
                                score: h.score,
                                matched_terms: Vec::new(),
                                chunks: chunks.get(&h.path).cloned().unwrap_or_default(),
                            })
                            .collect()
                    }
                    Err(_) => Vec::new(),
                }
            }
            _ => Vec::new(),
        };
        Ok(DebugCandidates { terms, term_files, total_files: results.total_files, bm25, vectors })
    }
}
