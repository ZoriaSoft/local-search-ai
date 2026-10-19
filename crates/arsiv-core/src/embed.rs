//! Offline semantic embeddings: EmbeddingGemma 2 text encoder (ONNX) via
//! ONNX Runtime, loaded from files bundled with the app. The runtime library,
//! the model (`onnx/*.onnx` + `.onnx_data`) and `tokenizer.json` are resolved
//! like pdfium/tessdata; nothing is ever downloaded at runtime.
//!
//! Prompt format follows the model card: queries are wrapped in
//! `task: search result | query: …`, document chunks in
//! `title: {file stem} | text: …`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use ort::session::Session;
use ort::value::Tensor;
use tokenizers::Tokenizer;

use crate::model::{FileKind, PageText};
use crate::ocr::Cancelled;
/// Native output width of `sentence_embedding`; chunks are stored at this
/// size and shorter Matryoshka dims are produced by truncating + renorming.
pub const NATIVE_DIM: usize = 768;
/// Search prompt (model card, "Web / document search", asymmetric).
pub const QUERY_PREFIX: &str = "task: search result | query: ";
/// Document chunk target: page text is cut into windows of ~this many
/// tokens so a 8192-token model still sees local context.
const CHUNK_TOKENS: usize = 400;
/// Neighbouring chunks share this many tokens; context does not stop at the
/// cut, and a cut mid-sentence does not lose the match.
const CHUNK_OVERLAP: usize = 80;
/// Document embedding compute. Every chunk is one model run
/// (batch 1): on CPU a 400-token chunk already fills the matrix kernels, so
/// batching only added padding (every row padded to the longest chunk, +19%
/// tokens on a 4-chunk file) and was 13-20% slower than single runs on the
/// a 48-thread Windows PC at every thread count; a single run also gives per-chunk progress
/// and a cancel point per chunk. Throughput comes from *several* document
/// sessions running chunks side by side: ORT's intra-op scaling flattens
/// early (48-thread Windows PC, s/chunk at batch 1: 2 threads 1.46, 4 0.85,
/// 12 0.39-0.49, 24 0.43) while separate sessions add up (two 12-thread sessions in two
/// processes: 0.23 s/chunk combined). The old fixed single 2-thread session
/// used 2 of 48 threads: ~70-80 s for a 48-chunk file.
///
/// Budget: half the logical CPUs (the PC stays usable while indexing),
/// split into sessions of at most this many intra-op threads...
const DOC_THREADS_PER_SESSION: usize = 6;
/// ...and at most this many sessions (each holds its own copy of the model,
/// ~200 MB resident). Measured in one process on a 48-thread Windows PC (48-chunk
/// file): 1x2 threads 82 s, 1x12 23.5 s, 2x12 17.4 s, 4x6 15.2 s, 8x3
/// 14.6 s, 4x12 12.6 s; turning intra-op spinning off was slower (16.9 s).
const MAX_DOC_SESSIONS: usize = 4;

/// `(document sessions, intra-op threads each)` for a machine with
/// `logical` CPUs: 48 -> 4x6, 16 -> 2x4, 8 -> 1x4, 4 -> 1x2.
fn document_pool_shape(logical: usize) -> (usize, usize) {
    let budget = (logical / 2).max(1);
    let sessions = budget.div_ceil(DOC_THREADS_PER_SESSION).clamp(1, MAX_DOC_SESSIONS);
    (sessions, (budget / sessions).max(1))
}

/// A/B: eight query threads reduced latency without changing query
/// vectors or ranked results. Keep the existing budget below 16 logical
/// CPUs: the document pool already uses roughly half the machine, and
/// increasing query concurrency on smaller PCs has not been measured.
fn query_thread_count(logical: usize) -> usize {
    if logical >= 16 { 8 } else { logical.clamp(1, 4) }
}

/// Measurement knobs for A/B runs on a target PC (never needed in normal
/// use): `ARSIV_EMBED_SESSIONS`, `ARSIV_EMBED_THREADS` (per document
/// session). `ARSIV_EMBED_TIMING=1` prints per-step ms to stderr.
fn env_usize(key: &str) -> Option<usize> {
    std::env::var(key).ok()?.trim().parse().ok().filter(|&n| n > 0)
}

fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("ARSIV_EMBED_TIMING").is_ok_and(|v| v == "1"))
}

/// Document-session compute runs below normal priority (Windows; like
/// Tesseract): a search, the UI or the user's own programs
/// preempt it and indexing takes what is left. With a 24-thread pool at
/// normal priority a search during the backfill took ~285 ms (median) vs
/// ~150 ms idle. `ARSIV_EMBED_PRIORITY=normal` turns it off (A/B).
fn background_priority() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("ARSIV_EMBED_PRIORITY").map_or(true, |v| v != "normal"))
}

/// Lowers the calling thread's scheduling priority (no-op off Windows).
fn lower_thread_priority() {
    #[cfg(windows)]
    {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentThread() -> *mut std::ffi::c_void;
            fn SetThreadPriority(thread: *mut std::ffi::c_void, priority: i32) -> i32;
        }
        const THREAD_PRIORITY_BELOW_NORMAL: i32 = -1;
        // SAFETY: the pseudo-handle of the current thread is always valid.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_BELOW_NORMAL);
        }
    }
}

/// Creates the document sessions' intra-op threads at background priority.
struct BackgroundThreads;

impl ort::environment::ThreadManager for BackgroundThreads {
    type Thread = std::thread::JoinHandle<()>;

    fn create(&self, work: impl FnOnce() + Send + 'static) -> ort::Result<Self::Thread> {
        std::thread::Builder::new()
            .name("arsiv-embed".into())
            .spawn(move || {
                lower_thread_priority();
                work()
            })
            .map_err(|e| ort::Error::new(e.to_string()))
    }

    fn join(thread: Self::Thread) -> ort::Result<()> {
        thread.join().map_err(|_| ort::Error::new("embedding thread panicked"))
    }
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
/// Chunks embedded per document (~256 x 320 new tokens, roughly the first
/// 150 pages). Very long texts (build logs, mapping files, books) would
/// otherwise keep the backfill busy for hours; BM25 still indexes every
/// page, only the meaning side stops here.
const MAX_CHUNKS_PER_DOCUMENT: usize = 256;
/// Chunk budget for `Text`/`Log` files. Plain-text pages are a single huge
/// "page" and their token count is dominated by markup noise (XML tags,
/// JSON punctuation, base64 IDs); after `clean_for_embedding` they need a
/// fraction of the document budget. The chunks are spread across the whole
/// file so a tail-of-log match is still found.
const TEXT_CHUNK_BUDGET: usize = 48;
/// The bundled BPE vocabulary consumes at most 93 UTF-8 bytes per token
/// (including literal ▁ markers in the input). Allow 96, plus a whole extra
/// chunk of lookahead, before tokenization. This preserves ordinary
/// documents and bounds a 50 MB log to about 7.9 MB even when all
/// 256 chunks remain; later pages get only the remaining chunk budget.
const CHUNK_BYTES_PER_TOKEN: usize = 96;
/// The media-feature inputs are required but unused for text-only input:
/// `(0, 512)` empty tensors satisfy the graph without an encoder.
const EMPTY_MEDIA: [i64; 2] = [0, 512];

/// Where the semantic pieces live, resolved like pdfium/tessdata.
#[derive(Clone, Debug)]
pub struct EmbedConfig {
    /// `libonnxruntime.so`/`onnxruntime.dll` (the directory is fine too —
    /// `library()` appends the platform name).
    pub runtime: PathBuf,
    /// Directory holding `model_q4.onnx` + `model_q4.onnx_data` and
    /// `tokenizer.json` (bundled: `resources/models`). The external-data
    /// file name is baked into the graph (`<model file>_data`), so the
    /// upstream names are kept verbatim.
    pub models: PathBuf,
}

impl EmbedConfig {
    fn library(&self) -> PathBuf {
        let name = if cfg!(windows) {
            "onnxruntime.dll"
        } else if cfg!(target_os = "macos") {
            "libonnxruntime.dylib"
        } else {
            "libonnxruntime.so"
        };
        if self.runtime.is_dir() {
            self.runtime.join(name)
        } else {
            self.runtime.clone()
        }
    }

    fn model(&self) -> PathBuf {
        self.models.join("model_q4.onnx")
    }

    fn tokenizer(&self) -> PathBuf {
        self.models.join("tokenizer.json")
    }
}

/// A document chunk's embedding: which catalog page the text came from and
/// the normalized 768-d vector.
#[derive(Clone, Debug)]
pub struct ChunkVector {
    pub page: usize,
    pub vector: Vec<f32>,
}

/// Lazily-initialised embedder. The session and tokenizer are loaded on the
/// first real use (`embed`/`check`), and the verdict is cached in a
/// `OnceLock` exactly like `OcrConfig`: a missing model reports
/// `semanticReady=false` but never blocks indexing or search.
pub struct Embedder {
    config: EmbedConfig,
    state: OnceLock<Result<Backend, String>>,
    /// `check` verdict, probed once (like OCR): `app_info` must not wait
    /// for the session while a backfill holds it.
    verdict: OnceLock<Result<String, String>>,
}

/// Everything a ready embedder needs. `Session::run` needs `&mut`, so a
/// session is used by one run at a time — and search queries get their
/// own session: a backfill embedding thousands of chunks (hours on a big
/// archive) keeps the document sessions busy run after run, and a query
/// must never wait behind it (it did: >100 s on a 48-thread Windows PC, 2026-10-15).
struct Backend {
    documents: SessionPool,
    queries: Mutex<Box<dyn Inference + Send>>,
    tokenizer: Tokenizer,
}

/// The document sessions. A run leases any free one; a document hands out
/// its chunks to all of them, and the next document starts as soon as the
/// last chunk of the current one has a session — files finish in order and
/// no session idles at a file boundary.
struct SessionPool {
    free: parking_lot::Mutex<Vec<Box<dyn Inference + Send>>>,
    returned: parking_lot::Condvar,
    size: usize,
    /// True while a document is handing out chunks.
    dispatching: parking_lot::Mutex<bool>,
    dispatch_done: parking_lot::Condvar,
}

impl SessionPool {
    fn new(sessions: Vec<Box<dyn Inference + Send>>) -> Self {
        Self {
            size: sessions.len().max(1),
            free: parking_lot::Mutex::new(sessions),
            returned: parking_lot::Condvar::new(),
            dispatching: parking_lot::Mutex::new(false),
            dispatch_done: parking_lot::Condvar::new(),
        }
    }

    /// Waits for a free session; it goes back to the pool when the lease drops.
    fn lease(&self) -> Lease<'_> {
        let mut free = self.free.lock();
        loop {
            if let Some(session) = free.pop() {
                return Lease { pool: self, session: Some(session) };
            }
            self.returned.wait(&mut free);
        }
    }

    /// Waits until no other document is handing out chunks.
    fn turn(&self) -> Turn<'_> {
        let mut busy = self.dispatching.lock();
        while *busy {
            self.dispatch_done.wait(&mut busy);
        }
        *busy = true;
        Turn { pool: self, held: AtomicBool::new(true) }
    }
}

struct Lease<'a> {
    pool: &'a SessionPool,
    session: Option<Box<dyn Inference + Send>>,
}

impl Lease<'_> {
    fn run(&mut self, ids: Vec<i64>, mask: Vec<i64>, batch: usize, width: usize) -> Result<Vec<Vec<f32>>> {
        self.session.as_mut().expect("leased session").run(ids, mask, batch, width)
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Some(session) = self.session.take() {
            self.pool.free.lock().push(session);
            self.pool.returned.notify_one();
        }
    }
}

/// A document's right to hand out chunks; released (once) by whichever
/// worker claims the last chunk, on error/cancel, or on drop.
struct Turn<'a> {
    pool: &'a SessionPool,
    held: AtomicBool,
}

impl Turn<'_> {
    fn release(&self) {
        if self.held.swap(false, Ordering::SeqCst) {
            *self.pool.dispatching.lock() = false;
            self.pool.dispatch_done.notify_one();
        }
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

/// One ONNX run as a trait so the lock/cancel discipline in `Backend::embed`
/// can be tested without a model: the session behind the lock only needs to
/// answer one padded batch.
trait Inference: Send {
    /// Runs `batch` × `width` token ids + attention mask, returns normalized
    /// `NATIVE_DIM` vectors (one per row).
    fn run(&mut self, ids: Vec<i64>, mask: Vec<i64>, batch: usize, width: usize) -> Result<Vec<Vec<f32>>>;
}

impl Inference for Session {
    fn run(&mut self, ids: Vec<i64>, mask: Vec<i64>, batch: usize, width: usize) -> Result<Vec<Vec<f32>>> {
        let media = || Tensor::from_array((EMPTY_MEDIA.to_vec(), Vec::<f32>::new()));
        let inputs = ort::inputs![
            "input_ids" => Tensor::from_array((vec![batch, width], ids))?,
            "attention_mask" => Tensor::from_array((vec![batch, width], mask))?,
            "image_features" => media()?,
            "video_features" => media()?,
            "audio_features" => media()?,
        ];
        let outputs = Session::run(self, inputs)?;
        let embedding = outputs["sentence_embedding"].try_extract_array::<f32>()?;
        if embedding.shape() != [batch, NATIVE_DIM] {
            bail!("unexpected embedding shape {:?}", embedding.shape());
        }
        let mut vectors = Vec::with_capacity(batch);
        for row in embedding.rows() {
            let mut vector: Vec<f32> = row.to_owned().into_raw_vec_and_offset().0;
            normalize(&mut vector);
            vectors.push(vector);
        }
        Ok(vectors)
    }
}

/// Cancel flag for calls that cannot be cancelled (a lone query embed):
/// `embed_query` shares the post-lock check with document batches, whose
/// queue wait is what made a cancel expensive.
static NEVER_CANCELLED: AtomicBool = AtomicBool::new(false);

/// Which session a run uses.
#[derive(Clone, Copy, PartialEq)]
enum Caller {
    Query,
    /// Single document runs outside `embed_chunks` (tests).
    #[cfg_attr(not(test), allow(dead_code))]
    Document,
}

impl Embedder {
    pub fn new(config: EmbedConfig) -> Self {
        Self { config, state: OnceLock::new(), verdict: OnceLock::new() }
    }

    /// Probes the real path: the runtime library loads, the model parses,
    /// the tokenizer loads and a tiny probe string embeds to `NATIVE_DIM`.
    /// Same contract as `OcrConfig::check`: the status light must prove the
    /// model actually works, not just that the files exist.
    pub fn check(&self) -> Result<String, String> {
        self.verdict.get_or_init(|| self.probe()).clone()
    }

    fn probe(&self) -> Result<String, String> {
        match self.backend() {
            Ok(backend) => {
                let embedded = backend.embed(&[QUERY_PREFIX.to_string() + "probe"], Caller::Query, &NEVER_CANCELLED);
                match embedded {
                    Ok(vectors) if vectors.first().map(|v| v.len() == NATIVE_DIM).unwrap_or(false) => {
                        Ok("EmbeddingGemma 2 (q4, 768d) verified".into())
                    }
                    Ok(_) => Err("Embedder loaded but produced no embedding".into()),
                    Err(e) => Err(format!("Embedder loaded but the probe failed: {e:#}")),
                }
            }
            Err(detail) => Err(detail.clone()),
        }
    }

    /// An embedder whose document session is `run(batch)` instead of the
    /// ONNX model (engine tests); the bundled tokenizer chunks the text.
    #[cfg(test)]
    pub(crate) fn fake(run: impl FnMut(usize) -> Result<Vec<Vec<f32>>> + Send + 'static) -> Self {
        struct Fake<R>(R);
        impl<R: FnMut(usize) -> Result<Vec<Vec<f32>>> + Send> Inference for Fake<R> {
            fn run(&mut self, _ids: Vec<i64>, _mask: Vec<i64>, batch: usize, _width: usize) -> Result<Vec<Vec<f32>>> {
                (self.0)(batch)
            }
        }
        let tokenizer = Tokenizer::from_file(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/models/tokenizer.json"))
            .expect("bundled tokenizer.json");
        let state = OnceLock::new();
        let _ = state.set(Ok(Backend {
            documents: SessionPool::new(vec![Box::new(Fake(run))]),
            queries: Mutex::new(Box::new(Fake(|batch| Ok(vec![vec![0.0; NATIVE_DIM]; batch])))),
            tokenizer,
        }));
        Self { config: EmbedConfig { runtime: PathBuf::new(), models: PathBuf::new() }, state, verdict: OnceLock::new() }
    }

    /// True when the embedder is usable (files present and the session
    /// loaded). Never embeds on first call — use `check` for a verdict with
    /// detail.
    pub fn usable(&self) -> bool {
        self.backend().is_ok()
    }

    /// True once loading was tried and failed. Never loads (cheap for
    /// progress/folder counts); `false` before the first use.
    pub fn failed(&self) -> bool {
        matches!(self.state.get(), Some(Err(_)))
    }

    fn backend(&self) -> Result<&Backend, &String> {
        self.state.get_or_init(|| self.load()).as_ref()
    }

    fn load(&self) -> Result<Backend, String> {
        let library = self.config.library();
        if !library.is_file() {
            return Err(format!("ONNX Runtime library not found: {}", library.display()));
        }
        if !self.config.model().is_file() {
            return Err(format!("Embedding model not found: {}", self.config.model().display()));
        }
        if !self.config.tokenizer().is_file() {
            return Err(format!("Tokenizer not found: {}", self.config.tokenizer().display()));
        }
        let build = (|| -> Result<Backend> {
            // ort's builder errors are !Send/!Sync; stringify eagerly.
            let env = ort::init_from(&library)
                .map_err(|e| anyhow!("{e}"))?
                .build()
                .map_err(|e| anyhow!("{e}"))?;
            let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
            // Queries are latency-critical; retain the A/B override.
            // Documents: see `document_pool_shape`.
            let query_threads = env_usize("ARSIV_QUERY_THREADS").unwrap_or(query_thread_count(logical));
            let (shape_sessions, shape_threads) = document_pool_shape(logical);
            let sessions = env_usize("ARSIV_EMBED_SESSIONS").unwrap_or(shape_sessions);
            let doc_threads = env_usize("ARSIV_EMBED_THREADS").unwrap_or(shape_threads);
            let model = self.config.model();
            let session = |threads: usize, background: bool| -> Result<Session> {
                let mut builder = Session::builder(&env)
                    .map_err(|e| anyhow!("{e}"))?
                    .with_intra_threads(threads)
                    .map_err(|e| anyhow!("{e}"))?;
                if background {
                    builder = builder.with_thread_manager(BackgroundThreads).map_err(|e| anyhow!("{e}"))?;
                }
                builder.commit_from_file(&model).map_err(|e| anyhow!("{e}"))
            };
            let started = Instant::now();
            // Each session parses the model itself; load them side by side.
            let session = &session;
            let mut built = std::thread::scope(|scope| {
                let background = background_priority();
                let handles: Vec<_> = std::iter::once((query_threads, false))
                    .chain(std::iter::repeat((doc_threads, background)).take(sessions))
                    .map(|(threads, background)| scope.spawn(move || session(threads, background)))
                    .collect();
                handles
                    .into_iter()
                    .map(|handle| handle.join().map_err(|_| anyhow!("session load panicked"))?)
                    .collect::<Result<Vec<Session>>>()
            })?;
            if timing() {
                eprintln!(
                    "embed-timing load logical={logical} query_threads={query_threads} doc_sessions={sessions} doc_threads={doc_threads} ms={:.0}",
                    ms(started.elapsed())
                );
            }
            let queries = built.remove(0);
            let tokenizer = Tokenizer::from_file(self.config.tokenizer())
                .map_err(|e| anyhow!("tokenizer.json: {e}"))?;
            Ok(Backend {
                documents: SessionPool::new(
                    built.into_iter().map(|s| Box::new(s) as Box<dyn Inference + Send>).collect(),
                ),
                queries: Mutex::new(Box::new(queries)),
                tokenizer,
            })
        })();
        build.map_err(|e| format!("EmbeddingGemma 2 could not be loaded: {e:#}"))
    }

    /// Chunk texts (already `title: … | text: …` formatted) to embeddings;
    /// returns one vector per input, L2-normalized at `NATIVE_DIM`.
    fn embed_texts(&self, texts: &[String], caller: Caller, cancel: &AtomicBool) -> Result<Vec<Vec<f32>>> {
        let backend = self.backend().map_err(|e| anyhow!("{e}"))?;
        backend.embed(texts, caller, cancel).map_err(|e| anyhow!("{e:#}"))
    }

    /// One search query → its vector (the model-card search prefix applied).
    /// `cancel` is honoured *before* the session runs (a query embed is one
    /// batch, so there is nothing to interrupt once inside).
    pub fn embed_query(&self, query: &str) -> Result<Vec<f32>> {
        let mut vectors = self.embed_texts(&[format!("{QUERY_PREFIX}{query}")], Caller::Query, &NEVER_CANCELLED)?;
        vectors.pop().context("empty embedding output")
    }

    /// The document's readable pages → one vector per ~`CHUNK_TOKENS`-token
    /// window (overlap `CHUNK_OVERLAP`). `title` is the file stem used in the
    /// `title: {title} | text: …` prompt; `kind` chooses the text-cleaning
    /// and chunk budget (markup kinds strip XML/JSON noise and spread fewer
    /// chunks across the whole file). A cancel between files' batches
    /// aborts with `Cancelled`; a document that was never embedded simply
    /// yields no chunks (the caller leaves `embedded=0`, the backfill pass
    /// finishes it later) — it is never a file failure.
    pub fn embed_document(&self, title: &str, kind: FileKind, pages: &[PageText], cancel: &AtomicBool) -> Result<Vec<ChunkVector>> {
        self.embed_document_with_progress(title, kind, pages, cancel, &|_, _| {})
    }

    /// Same document embeddings, reporting completed/total chunks before
    /// the first model run and after every chunk (monotonic). Callers can
    /// supply a heartbeat as well: an individual ONNX run may itself exceed
    /// a second on a small PC.
    pub fn embed_document_with_progress(
        &self,
        title: &str,
        kind: FileKind,
        pages: &[PageText],
        cancel: &AtomicBool,
        on_progress: &(dyn Fn(usize, usize) + Sync),
    ) -> Result<Vec<ChunkVector>> {
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let backend = self.backend().map_err(|e| anyhow!("{e}"))?;
        let chunked = Instant::now();
        let chunks = document_chunks(&backend.tokenizer, pages, title, kind, chunk_budget(kind), cancel)?;
        if timing() {
            let bytes: usize = pages.iter().map(|p| p.text.len()).sum();
            eprintln!("embed-timing chunking title={title} kind={kind:?} bytes={bytes} chunks={} ms={:.1}", chunks.len(), ms(chunked.elapsed()));
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let total = chunks.len();
        on_progress(0, total);
        let texts: Vec<&str> = chunks.iter().map(|c| c.text.as_str()).collect();
        let embedded = match backend.embed_chunks(&texts, cancel, &|done| on_progress(done, total)) {
            Ok(vectors) => vectors,
            Err(error) if cancel.load(Ordering::Relaxed) || crate::ocr::is_cancelled(&error) => return Err(Cancelled.into()),
            Err(error) => return Err(error),
        };
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        Ok(chunks.iter().zip(embedded).map(|(chunk, vector)| ChunkVector { page: chunk.page, vector }).collect())
    }
}

/// The exact input handed to the tokenizer, bounded *before* encoding.
/// Only the last UTF-8 character can be omitted; the lookahead keeps that
/// cut well past the token windows we retain.
fn chunk_input(text: &str, remaining: usize) -> &str {
    if remaining == 0 {
        return "";
    }
    let token_budget = CHUNK_TOKENS
        .saturating_add(remaining.saturating_sub(1).saturating_mul(CHUNK_TOKENS - CHUNK_OVERLAP))
        .saturating_add(CHUNK_TOKENS);
    let mut end = text.len().min(token_budget.saturating_mul(CHUNK_BYTES_PER_TOKEN));
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// What an XML/JSON source actually *says*: tags, entities, structural
/// punctuation and long opaque IDs (hex/base64 runs) removed, prose kept.
/// Embedding a raw `ui2.xml` spends most tokens on markup the model cannot
/// use for meaning — a 48-thread Windows PC showed ~3,000 tokens/9.5 KB, ~12 s/file.
/// Applied to `Text`/`Log` kinds only; code is a heuristic, not a parser:
/// it errs toward keeping text.
fn clean_for_embedding(text: &str) -> String {
    let b = text.as_bytes();
    let mut out = String::with_capacity(b.len());
    // Byte range of the word being collected; delimiters close it.
    let mut word_start: Option<usize> = None;
    let push_word = |out: &mut String, text: &str, word: (usize, usize)| {
        let w = &text[word.0..word.1];
        // Long opaque runs (hashes, guids, base64 blobs) carry no meaning a
        // query would use; short codes (PO numbers, plates) stay.
        let opaque = w.chars().count() > 24
            && w.bytes().all(|x| x.is_ascii_alphanumeric() || matches!(x, b'+' | b'/' | b'=' | b'-' | b'_'));
        if !opaque {
            if !out.is_empty() {
                out.push(' ');
            }
            out.push_str(w);
        }
    };
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            // `<x…>` — a tag only when a letter/`/`/`!`/`?` follows (`a < b` stays text).
            b'<' if i + 1 < b.len() && matches!(b[i + 1], b'a'..=b'z' | b'A'..=b'Z' | b'/' | b'!' | b'?') => {
                if let Some(s) = word_start.take() {
                    push_word(&mut out, text, (s, i));
                }
                while i < b.len() && b[i] != b'>' {
                    i += 1;
                }
                i += usize::from(i < b.len());
            }
            // `&name;` / `&#123;` — dropped; a stray `&` stays text.
            b'&' => {
                let start = i + 1;
                let mut j = start;
                while j < b.len() && j - start <= 12 && (b[j].is_ascii_alphanumeric() || b[j] == b'#') {
                    j += 1;
                }
                if j > start && j < b.len() && b[j] == b';' {
                    if let Some(s) = word_start.take() {
                        push_word(&mut out, text, (s, i));
                    }
                    i = j + 1;
                } else {
                    word_start.get_or_insert(i);
                    i += 1;
                }
            }
            c if c.is_ascii_whitespace() => {
                if let Some(s) = word_start.take() {
                    push_word(&mut out, text, (s, i));
                }
                i += 1;
            }
            b'{' | b'}' | b'[' | b']' | b'(' | b')' | b'"' | b'\'' | b':' | b';' | b',' | b'|' | b'=' | b'`' => {
                if let Some(s) = word_start.take() {
                    push_word(&mut out, text, (s, i));
                }
                i += 1;
            }
            _ => {
                word_start.get_or_insert(i);
                i += 1;
            }
        }
    }
    if let Some(s) = word_start.take() {
        push_word(&mut out, text, (s, b.len()));
    }
    out
}

/// Evenly-spaced token windows over `text`: `count` samples taken across
/// the whole file instead of only the head, for kinds whose single "page"
/// can be megabytes long (logs, dumps, markup). Each window tokenises to
/// at most `CHUNK_TOKENS` — the same embedding unit a page chunk uses.
fn spread_chunks(tokenizer: &Tokenizer, text: &str, page: usize, title: &str, count: usize) -> Result<Vec<ChunkText>> {
    let mut chunks = Vec::with_capacity(count);
    if count == 0 || text.trim().is_empty() {
        return Ok(chunks);
    }
    // A ~1600-byte window covers a 400-token chunk for prose; markup-clean
    // text is denser so the tokeniser trims the tail either way.
    const WINDOW_BYTES: usize = CHUNK_TOKENS * CHUNK_BYTES_PER_TOKEN / 24;
    let step = (text.len() / count).max(WINDOW_BYTES);
    let mut offset = 0usize;
    for _ in 0..count {
        while offset < text.len() && !text.is_char_boundary(offset) {
            offset += 1;
        }
        if offset >= text.len() {
            break;
        }
        let mut end = (offset + WINDOW_BYTES).min(text.len());
        while end > offset && !text.is_char_boundary(end) {
            end -= 1;
        }
        let window = &text[offset..end];
        if !window.trim().is_empty() {
            let encoding = tokenizer.encode(window, false).map_err(|e| anyhow!("tokenize: {e}"))?;
            let offsets = encoding.get_offsets();
            if !offsets.is_empty() {
                let (bs, be) = (offsets[0].0.min(window.len()), offsets[(CHUNK_TOKENS - 1).min(offsets.len() - 1)].1.min(window.len()));
                chunks.push(ChunkText {
                    page,
                    text: format!("title: {title} | text: {}", window[bs..be].trim()),
                });
            }
        }
        offset += step;
    }
    Ok(chunks)
}

/// How many chunk windows a file kind budgets: markup kinds clean first,
/// then sample the whole file with a small cap; other kinds embed pages
/// with the document cap.
fn chunk_budget(kind: FileKind) -> usize {
    match kind {
        FileKind::Text | FileKind::Log => TEXT_CHUNK_BUDGET,
        _ => MAX_CHUNKS_PER_DOCUMENT,
    }
}

/// Tokenizer-only chunking: shared by production and tests, with no ONNX
/// runtime/session required. Stop before touching pages after the cap.
fn document_chunks(tokenizer: &Tokenizer, pages: &[PageText], title: &str, kind: FileKind, max: usize, cancel: &AtomicBool) -> Result<Vec<ChunkText>> {
    let markup = matches!(kind, FileKind::Text | FileKind::Log);
    let mut chunks = Vec::new();
    for page in pages {
        if chunks.len() >= max {
            break;
        }
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        if page.is_failed() {
            continue;
        }
        let remaining = max - chunks.len();
        if markup {
            let cleaned = clean_for_embedding(&page.text);
            if cleaned.trim().is_empty() {
                continue;
            }
            // Sequential windows while the file is small; once the budget
            // cannot cover the page, switch to spread windows so the tail
            // of a huge log still reaches the meaning index.
            if cleaned.len() <= chunk_input(&cleaned, remaining).len() {
                chunks.extend(chunk_page(tokenizer, &cleaned, page.index, title, remaining)?);
            } else {
                chunks.extend(spread_chunks(tokenizer, &cleaned, page.index, title, remaining)?);
            }
        } else {
            chunks.extend(chunk_page(tokenizer, &page.text, page.index, title, remaining)?);
        }
    }
    Ok(chunks)
}

/// Splits the bounded text prefix into the original prompt-formatted,
/// overlapping token windows. Ordinary pages use the exact same encoding
/// and offsets as before; the remaining budget is enforced inside the loop.
fn chunk_page(tokenizer: &Tokenizer, text: &str, page: usize, title: &str, remaining: usize) -> Result<Vec<ChunkText>> {
    let input = chunk_input(text, remaining);
    if input.trim().is_empty() {
        return Ok(Vec::new());
    }
    let encoding = tokenizer.encode(input, false).map_err(|e| anyhow!("tokenize: {e}"))?;
    let offsets = encoding.get_offsets();
    let mut chunks = Vec::new();
    let mut start = 0usize;
    while start < offsets.len() && chunks.len() < remaining {
        let end = (start + CHUNK_TOKENS).min(offsets.len());
        let (byte_start, byte_end) = (offsets[start].0, offsets[end - 1].1);
        let text = &input[byte_start.min(input.len())..byte_end.min(input.len())];
        if !text.trim().is_empty() {
            chunks.push(ChunkText {
                page,
                text: format!("title: {title} | text: {}", text.trim()),
            });
        }
        if end == offsets.len() {
            break;
        }
        start = end.saturating_sub(CHUNK_OVERLAP);
    }
    Ok(chunks)
}

#[derive(Debug, PartialEq)]
struct ChunkText {
    page: usize,
    text: String,
}

/// One tokenized model input: ids + attention mask, `batch` x `width`.
struct Encoded {
    ids: Vec<i64>,
    mask: Vec<i64>,
    batch: usize,
    width: usize,
}

impl Backend {
    /// Tokenizes `texts` (special tokens added) into one padded batch.
    fn encode<S: AsRef<str>>(&self, texts: &[S]) -> Result<Encoded> {
        let encodings: Vec<_> = texts
            .iter()
            .map(|text| self.tokenizer.encode(text.as_ref(), true))
            .collect::<Result<_, _>>()
            .map_err(|e| anyhow!("tokenize: {e}"))?;
        let batch = encodings.len();
        let width = encodings.iter().map(|e| e.len()).max().unwrap_or(1).max(1);
        let mut ids = vec![0i64; batch * width];
        let mut mask = vec![0i64; batch * width];
        for (row, encoding) in encodings.iter().enumerate() {
            for (col, &id) in encoding.get_ids().iter().enumerate() {
                ids[row * width + col] = id as i64;
                mask[row * width + col] = 1;
            }
        }
        Ok(Encoded { ids, mask, batch, width })
    }

    /// Runs one padded batch through a session; returns normalized 768-d
    /// vectors. `session.run` is `&mut`: the query session sits behind a
    /// mutex, document runs lease a pool session.
    /// `cancel` is re-checked *after* the session is taken: workers queue
    /// for document sessions, so a cancel raised while waiting must stop a
    /// fresh run instead of running one more (every
    /// queued file ran a whole batch after "İptal").
    fn embed(&self, texts: &[String], caller: Caller, cancel: &AtomicBool) -> Result<Vec<Vec<f32>>> {
        let Encoded { ids, mask, batch, width } = self.encode(texts)?;
        match caller {
            Caller::Query => {
                let mut session = self.queries.lock().map_err(|_| anyhow!("embedder session poisoned"))?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(Cancelled.into());
                }
                session.run(ids, mask, batch, width)
            }
            Caller::Document => {
                let mut session = self.documents.lease();
                if cancel.load(Ordering::Relaxed) {
                    return Err(Cancelled.into());
                }
                session.run(ids, mask, batch, width)
            }
        }
    }

    /// One document's chunks -> one vector each, in order. Every chunk is
    /// its own run (batch 1, no padding); up to `pool.size` chunks run at
    /// once on separate sessions. `on_done(n)` reports n finished chunks,
    /// serialized and increasing. A cancel is honoured after each session
    /// lease; a failed chunk stops the rest and fails the document.
    fn embed_chunks(&self, texts: &[&str], cancel: &AtomicBool, on_done: &(dyn Fn(usize) + Sync)) -> Result<Vec<Vec<f32>>> {
        let total = texts.len();
        if total == 0 {
            return Ok(Vec::new());
        }
        let encoded: Vec<Encoded> = texts.iter().map(|t| self.encode(std::slice::from_ref(t))).collect::<Result<_>>()?;
        let turn_wait = Instant::now();
        let turn = self.documents.turn();
        let waited = turn_wait.elapsed();
        if cancel.load(Ordering::Relaxed) {
            return Err(Cancelled.into());
        }
        let started = Instant::now();
        let next = AtomicUsize::new(0);
        let done = parking_lot::Mutex::new(0usize);
        let failure: parking_lot::Mutex<Option<anyhow::Error>> = parking_lot::Mutex::new(None);
        let results: Vec<parking_lot::Mutex<Option<Vec<f32>>>> = (0..total).map(|_| parking_lot::Mutex::new(None)).collect();
        let workers = self.documents.size.min(total);
        std::thread::scope(|scope| {
            for _ in 0..workers {
                scope.spawn(|| loop {
                    // The run's caller thread computes too: same priority
                    // as the session's own threads.
                    if background_priority() {
                        lower_thread_priority();
                    }
                    if failure.lock().is_some() {
                        break;
                    }
                    let leased = Instant::now();
                    let mut session = self.documents.lease();
                    let lease_wait = leased.elapsed();
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    if index + 1 >= total {
                        // Last chunk handed out: the next document may start
                        // on the sessions this one no longer needs.
                        turn.release();
                    }
                    if index >= total {
                        break;
                    }
                    if cancel.load(Ordering::Relaxed) {
                        failure.lock().get_or_insert_with(|| Cancelled.into());
                        turn.release();
                        break;
                    }
                    let input = &encoded[index];
                    let run = Instant::now();
                    match session.run(input.ids.clone(), input.mask.clone(), 1, input.width) {
                        Ok(mut vectors) if vectors.len() == 1 => {
                            *results[index].lock() = vectors.pop();
                            if timing() {
                                eprintln!(
                                    "embed-timing chunk {index}/{total} tokens={} lease_wait_ms={:.1} run_ms={:.1}",
                                    input.width,
                                    ms(lease_wait),
                                    ms(run.elapsed())
                                );
                            }
                            let mut done = done.lock();
                            *done += 1;
                            on_done(*done);
                        }
                        outcome => {
                            let error = outcome.err().unwrap_or_else(|| anyhow!("empty embedding output"));
                            failure.lock().get_or_insert(error);
                            turn.release();
                            break;
                        }
                    }
                });
            }
        });
        drop(turn);
        if let Some(error) = failure.into_inner() {
            return Err(error);
        }
        if timing() {
            eprintln!(
                "embed-timing document chunks={total} sessions={workers} turn_wait_ms={:.1} ms={:.1}",
                ms(waited),
                ms(started.elapsed())
            );
        }
        results
            .into_iter()
            .map(|slot| slot.into_inner().context("missing chunk embedding"))
            .collect()
    }
}

/// L2-normalize in place; a degenerate (zero) vector stays zero.
pub fn normalize(vector: &mut [f32]) {
    let norm: f32 = vector.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for value in vector.iter_mut() {
            *value /= norm;
        }
    }
}

/// The document prompt's `title` is the file stem — the closest thing the
/// catalog has to a document title.
pub fn document_title(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "none".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_document_title_is_the_file_stem() {
        assert_eq!(document_title("/a/b/kira sözleşmesi.pdf"), "kira sözleşmesi");
        assert_eq!(document_title("fatura.docx"), "fatura");
        assert_eq!(document_title(""), "none");
    }

    #[test]
    fn a_missing_runtime_reports_not_ready() {
        let embedder = Embedder::new(EmbedConfig {
            runtime: PathBuf::from("/does/not/exist/libonnxruntime.so"),
            models: PathBuf::from("/does/not/exist"),
        });
        assert!(!embedder.usable());
        let detail = embedder.check().unwrap_err();
        assert!(detail.contains("ONNX Runtime library not found"), "{detail}");
    }

    #[test]
    fn a_missing_model_reports_not_ready() {
        // Point runtime at a real file (any file) so the check passes the
        // file-exists gate and reaches the model check.
        let fake = std::env::temp_dir().join("fake-onnxruntime.so");
        std::fs::write(&fake, b"not a library").unwrap();
        let embedder = Embedder::new(EmbedConfig {
            runtime: fake,
            models: PathBuf::from("/does/not/exist"),
        });
        // Loading fails at dlopen of the fake .so — which surfaces as
        // "could not be loaded", not "not found".
        let detail = embedder.check().unwrap_err();
        assert!(
            detail.contains("could not be loaded") || detail.contains("not found"),
            "{detail}"
        );
    }

    fn bundled_tokenizer() -> &'static Tokenizer {
        static TOKENIZER: OnceLock<Tokenizer> = OnceLock::new();
        TOKENIZER.get_or_init(|| {
            let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/models/tokenizer.json");
            Tokenizer::from_file(path).expect("bundled tokenizer.json must be available for chunk regression tests")
        })
    }

    /// Reference: whole-page tokenization before the former chunk cap.
    fn uncapped_chunks(page: &PageText, title: &str) -> Vec<ChunkText> {
        let encoding = bundled_tokenizer().encode(page.text.as_str(), false).unwrap();
        let offsets = encoding.get_offsets();
        let mut expected = Vec::new();
        let mut start = 0;
        while start < offsets.len() {
            let end = (start + CHUNK_TOKENS).min(offsets.len());
            let text = page.text[offsets[start].0..offsets[end - 1].1].trim();
            if !text.is_empty() {
                expected.push(ChunkText { page: page.index, text: format!("title: {title} | text: {text}") });
            }
            if end == offsets.len() {
                break;
            }
            start = end - CHUNK_OVERLAP;
        }
        expected
    }

    #[test]
    fn chunk_byte_budget_covers_the_bundled_vocabulary() {
        let vocab = bundled_tokenizer().get_vocab(true);
        let max_bytes = vocab.keys().map(String::len).max().unwrap();
        assert!(max_bytes <= CHUNK_BYTES_PER_TOKEN, "vocabulary grew beyond the prefix safety margin: {max_bytes}");
        assert!(chunk_input(&"x".repeat(50 * 1024 * 1024), MAX_CHUNKS_PER_DOCUMENT).len() < 8 * 1024 * 1024);
    }

    #[test]
    fn ordinary_multilingual_chunks_equal_whole_page_tokenization() {
        let text = "İSTANBUL Ankara'da ücret ödendi. Cafe\u{301} 𝄞 日本語 中文 العربية ────────────────\n".repeat(90);
        let page = PageText::new(4, text, crate::model::TextSource::Native, None);
        let expected = uncapped_chunks(&page, "normal");
        assert!(expected.len() > 1 && expected.len() < MAX_CHUNKS_PER_DOCUMENT);
        assert_eq!(chunk_input(&page.text, MAX_CHUNKS_PER_DOCUMENT), page.text);
        assert_eq!(chunk_page(bundled_tokenizer(), &page.text, page.index, "normal", MAX_CHUNKS_PER_DOCUMENT).unwrap(), expected);
    }

    #[test]
    fn literal_tokenizer_space_markers_under_the_cap_are_not_cut() {
        // A vocabulary token can contain 31 literal markers (93 input
        // bytes); treating every marker as one ASCII space underestimates
        // this normal four-chunk document's required prefix.
        let page = PageText::new(0, "▁".repeat(31 * 1_300), crate::model::TextSource::Native, None);
        let expected = uncapped_chunks(&page, "markers");
        assert!(expected.len() <= 4);
        assert_eq!(chunk_input(&page.text, 4), page.text);
        assert_eq!(chunk_page(bundled_tokenizer(), &page.text, page.index, "markers", 4).unwrap(), expected);
    }

    #[test]
    fn a_fifty_mb_page_is_cut_before_encoding_and_keeps_the_same_first_chunks() {
        let line = "2024-11-13 INFO order 8158 processed ücret Ankara'da 日本語.\n";
        let page = PageText::new(
            8,
            line.repeat(50 * 1024 * 1024 / line.len() + 1),
            crate::model::TextSource::Native,
            None,
        );
        let input = chunk_input(&page.text, 2);
        assert_eq!(input.as_ptr(), page.text.as_ptr());
        assert!(input.len() <= (CHUNK_TOKENS * 2 + CHUNK_TOKENS - CHUNK_OVERLAP) * CHUNK_BYTES_PER_TOKEN);
        assert!(input.len() < page.text.len() / 100);
        // An independently encoded complete short page supplies a reference
        // without ever tokenizing the generated 50 MB tail in the test.
        let short = PageText::new(8, line.repeat(200), crate::model::TextSource::Native, None);
        let mut expected = uncapped_chunks(&short, "log");
        expected.truncate(2);
        assert_eq!(chunk_page(bundled_tokenizer(), &page.text, page.index, "log", 2).unwrap(), expected);
    }

    #[test]
    fn document_chunk_cap_is_shared_between_pages_and_honours_cancel() {
        let pages = vec![
            PageText::new(0, "İlk sayfa özeti".into(), crate::model::TextSource::Native, None),
            PageText::failed(1, "unreadable".into()),
            PageText::new(2, "ücret order 12345 processed ".repeat(2_000), crate::model::TextSource::Native, None),
            PageText::new(3, "bu sayfa bütçe dışında".into(), crate::model::TextSource::Native, None),
        ];
        let cancel = AtomicBool::new(false);
        let chunks = document_chunks(bundled_tokenizer(), &pages, "pages", FileKind::Pdf, 3, &cancel).unwrap();
        let mut expected = uncapped_chunks(&pages[0], "pages");
        expected.extend(uncapped_chunks(&pages[2], "pages").into_iter().take(2));
        assert_eq!(chunks, expected);
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks.iter().map(|c| c.page).collect::<Vec<_>>(), [0, 2, 2]);
        cancel.store(true, Ordering::Relaxed);
        assert!(crate::ocr::is_cancelled(&document_chunks(bundled_tokenizer(), &pages, "pages", FileKind::Pdf, 3, &cancel).unwrap_err()));
    }

    #[test]
    fn chunk_prefix_is_utf8_safe_and_zero_budget_does_no_work() {
        let text = "ü𝄞".repeat(20_000);
        let prefix = chunk_input(&text, 1);
        assert!(prefix.len() < text.len());
        assert!(text.is_char_boundary(prefix.len()));
        assert!(chunk_input(&text, 0).is_empty());
        assert!(document_chunks(bundled_tokenizer(), &[], "empty", FileKind::Pdf, 0, &AtomicBool::new(false)).unwrap().is_empty());
    }

    #[test]
    fn markup_is_cleaned_to_words_before_chunking() {
        let xml = r#"<?xml version="1.0"?><LinearLayout android:id="@+id/hero"><TextView>Evin ücret anlaşması</TextView><Label>kira bedeli</Label></LinearLayout>"#;
        let cleaned = clean_for_embedding(xml);
        assert!(!cleaned.contains('<'), "{cleaned}");
        assert!(cleaned.contains("Evin ücret anlaşması"), "{cleaned}");
        assert!(cleaned.contains("kira bedeli"), "{cleaned}");
        // Tag/attribute names are dropped wholesale.
        assert!(!cleaned.contains("android"), "{cleaned}");
        assert!(!cleaned.contains("LinearLayout"), "{cleaned}");
        let json = r#"{"id":"a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4","amount":8158,"note":"kira ödemesi","tags":["ev","ay"]}"#;
        let cleaned = clean_for_embedding(json);
        assert_eq!(cleaned, "id amount 8158 note kira ödemesi tags ev ay");
        // `a < b` prose keeps its operator text; `&` without `;` stays.
        assert!(clean_for_embedding("değer 3 < 5 oldu").contains("5"));
        assert!(clean_for_embedding("a&b@x.com").contains("b@x.com"));
    }

    #[test]
    fn the_document_pool_uses_half_the_machine_in_small_sessions() {
        assert_eq!(document_pool_shape(48), (4, 6)); // 48 threads: was 1 x 2 threads
        assert_eq!(document_pool_shape(64), (4, 8));
        assert_eq!(document_pool_shape(16), (2, 4));
        assert_eq!(document_pool_shape(12), (1, 6));
        assert_eq!(document_pool_shape(8), (1, 4));
        assert_eq!(document_pool_shape(4), (1, 2));
        assert_eq!(document_pool_shape(1), (1, 1));
    }

    #[test]
    fn query_threads_preserve_small_pc_budgets_and_use_eight_on_larger_pcs() {
        for (logical, expected) in [(0, 1), (1, 1), (2, 2), (4, 4), (8, 4), (12, 4), (16, 8), (48, 8)] {
            assert_eq!(query_thread_count(logical), expected, "logical CPUs: {logical}");
        }
    }

    /// A fake session that returns `[width, 0, ...]` (identifies the chunk)
    /// and tracks how many runs overlap.
    struct Probe {
        running: std::sync::Arc<AtomicUsize>,
        peak: std::sync::Arc<AtomicUsize>,
        runs: std::sync::Arc<AtomicUsize>,
        on_run: std::sync::Arc<dyn Fn() + Send + Sync>,
    }

    impl Inference for Probe {
        fn run(&mut self, ids: Vec<i64>, _mask: Vec<i64>, batch: usize, width: usize) -> Result<Vec<Vec<f32>>> {
            assert_eq!((batch, ids.len()), (1, width), "one unpadded chunk per run");
            let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            self.runs.fetch_add(1, Ordering::SeqCst);
            (self.on_run)();
            std::thread::sleep(std::time::Duration::from_millis(15));
            self.running.fetch_sub(1, Ordering::SeqCst);
            let mut vector = vec![0.0; NATIVE_DIM];
            vector[0] = width as f32;
            Ok(vec![vector])
        }
    }

    fn probe_backend(sessions: usize, on_run: std::sync::Arc<dyn Fn() + Send + Sync>) -> (Backend, [std::sync::Arc<AtomicUsize>; 2]) {
        let peak = std::sync::Arc::new(AtomicUsize::new(0));
        let runs = std::sync::Arc::new(AtomicUsize::new(0));
        let running = std::sync::Arc::new(AtomicUsize::new(0));
        let pool = (0..sessions)
            .map(|_| {
                Box::new(Probe { running: running.clone(), peak: peak.clone(), runs: runs.clone(), on_run: on_run.clone() })
                    as Box<dyn Inference + Send>
            })
            .collect();
        let backend = Backend {
            documents: SessionPool::new(pool),
            queries: Mutex::new(Box::new(Probe { running, peak: peak.clone(), runs: runs.clone(), on_run })),
            tokenizer: bundled_tokenizer().clone(),
        };
        (backend, [peak, runs])
    }

    #[test]
    fn document_chunks_run_side_by_side_and_come_back_in_order() {
        // The first run holds its session until a second one is running
        // (bounded): "several sessions at once" must not depend on how fast
        // the OS starts the other workers under a loaded test run.
        let started = std::sync::Arc::new(AtomicUsize::new(0));
        let first = std::sync::Arc::new(AtomicBool::new(true));
        let on_run: std::sync::Arc<dyn Fn() + Send + Sync> = {
            let (started, first) = (started.clone(), first.clone());
            std::sync::Arc::new(move || {
                started.fetch_add(1, Ordering::SeqCst);
                if first.swap(false, Ordering::SeqCst) {
                    let deadline = Instant::now() + Duration::from_secs(30);
                    while started.load(Ordering::SeqCst) < 2 && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
            })
        };
        let (backend, [peak, runs]) = probe_backend(3, on_run);
        // Distinct lengths: each vector names its chunk by token width.
        let texts: Vec<String> = (1..=10).map(|n| format!("title: t | text: {}", "kira ".repeat(n * 3))).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let expected: Vec<f32> = texts.iter().map(|t| bundled_tokenizer().encode(t.as_str(), true).unwrap().len() as f32).collect();
        let progress = Mutex::new(Vec::new());
        let vectors = backend.embed_chunks(&refs, &AtomicBool::new(false), &|n| progress.lock().unwrap().push(n)).unwrap();
        assert_eq!(vectors.iter().map(|v| v[0]).collect::<Vec<_>>(), expected);
        assert_eq!(runs.load(Ordering::SeqCst), 10);
        assert!(peak.load(Ordering::SeqCst) >= 2, "chunks must use several sessions at once");
        assert!(peak.load(Ordering::SeqCst) <= 3);
        assert_eq!(progress.into_inner().unwrap(), (1..=10).collect::<Vec<_>>(), "progress is per chunk and increasing");
        // The pool is whole again and a second document runs.
        assert_eq!(backend.documents.free.lock().len(), 3);
        assert_eq!(backend.embed_chunks(&refs[..1], &AtomicBool::new(false), &|_| {}).unwrap().len(), 1);
    }

    #[test]
    fn a_cancel_stops_the_document_after_the_runs_in_flight() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let (backend, [_, runs]) = probe_backend(3, std::sync::Arc::new(move || flag.store(true, Ordering::SeqCst)));
        let texts: Vec<String> = (0..40).map(|n| format!("title: t | text: parça {n}")).collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let outcome = backend.embed_chunks(&refs, &cancel, &|_| {});
        assert!(crate::ocr::is_cancelled(&outcome.unwrap_err()));
        assert!(runs.load(Ordering::SeqCst) <= 3, "only runs already leased may finish: {}", runs.load(Ordering::SeqCst));
        assert_eq!(backend.documents.free.lock().len(), 3, "every session goes back to the pool");
        assert!(!*backend.documents.dispatching.lock(), "the dispatch turn is released");
    }

    #[test]
    fn normalize_keeps_zero_vectors() {
        let mut v = vec![0.0f32; 4];
        normalize(&mut v);
        assert_eq!(v, vec![0.0; 4]);
        let mut v = vec![3.0, 4.0];
        normalize(&mut v);
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-6);
    }

    /// A worker queued on the document session lock must not run a
    /// batch when the cancel flag was set while it waited. Deterministic:
    /// the first fake run blocks on a channel until the cancel lands, so
    /// the second call acquires the lock only after the flag is set.
    #[test]
    fn a_waiting_embed_returns_cancelled_without_running_the_model() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::mpsc;
        use std::sync::Arc;

        struct Held {
            runs: Arc<AtomicUsize>,
            entered: Option<mpsc::Sender<()>>,
            release: Option<mpsc::Receiver<()>>,
        }
        impl Inference for Held {
            fn run(&mut self, _ids: Vec<i64>, _mask: Vec<i64>, batch: usize, _width: usize) -> Result<Vec<Vec<f32>>> {
                self.runs.fetch_add(1, Ordering::Relaxed);
                if let Some(entered) = self.entered.take() {
                    let _ = entered.send(());
                }
                // Hold the lock until the test releases it, like an ONNX batch.
                if let Some(release) = self.release.take() {
                    let _ = release.recv();
                }
                Ok(vec![vec![0.0; NATIVE_DIM]; batch])
            }
        }

        let runs = Arc::new(AtomicUsize::new(0));
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let held = Held { runs: runs.clone(), entered: Some(entered_tx), release: Some(release_rx) };
        let idle = Held { runs: runs.clone(), entered: None, release: None };
        let backend = Arc::new(Backend {
            documents: SessionPool::new(vec![Box::new(held)]),
            queries: Mutex::new(Box::new(idle)),
            tokenizer: bundled_tokenizer().clone(),
        });
        let texts = vec!["title: t | text: ilk parça".to_string()];
        let cancel = Arc::new(AtomicBool::new(false));

        // t1 holds the document session inside `run` until released.
        let (runner_done, runner_wait) = mpsc::channel();
        {
            let backend = backend.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let _ = backend.embed(&texts, Caller::Document, &cancel);
                let _ = runner_done.send(());
            });
        }
        entered_rx.recv().expect("the first embed must enter run");

        // t2 queues on the same lock; cancel lands while it waits.
        let (done_tx, done_rx) = mpsc::channel();
        {
            let backend = backend.clone();
            let cancel = cancel.clone();
            std::thread::spawn(move || {
                let outcome = backend.embed(&["title: t | text: ikinci".to_string()], Caller::Document, &cancel);
                let _ = done_tx.send(outcome);
            });
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
        cancel.store(true, Ordering::Relaxed);
        let _ = release_tx.send(());

        let outcome = done_rx.recv().expect("waiting embed returns");
        assert!(crate::ocr::is_cancelled(&outcome.unwrap_err()), "queued embed must report Cancelled");
        runner_wait.recv().expect("first embed finishes");
        assert_eq!(runs.load(Ordering::Relaxed), 1, "no model run may start after the cancel");
    }
}
