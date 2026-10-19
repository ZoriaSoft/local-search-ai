//! Headless CLI over the real app engine.
//!
//! `arsiv-eval index <data_dir> <folder> --pdfium <dir> --tesseract <exe> --tessdata <dir>`
//!   indexes a folder exactly like the desktop app and prints a summary.
//! `arsiv-eval search <data_dir> <query> ...` runs one query.
//! Search A/B flags (defaults = production): `--no-expand`, `--no-legacy-stems`, `--stem-boost X`,
//! `--phrase-boost X`, `--pair-boost X`, `--phrase-slop N`, `--synonym-boost X`.
//! Hybrid: `--vec-weight --rrf-k --bm25-head --head-coverage --min-cosine --vec-only --vec-dim`.
//!   `--code-cosine X` (vector-only floor for digit/code queries; >1 drops them),
//!   `--short-chars N --short-penalty X` (cosine penalty for short pages; 0 = off).
//! `arsiv-eval nomatch <data_dir> <queries.txt> <out.json>` counts hits for queries no file
//!   contains (numbers/codes); the expected answer is an empty list.
//! `ARSIV_SEARCH_TIMING=1` prints per-stage search milliseconds to stderr.
//! `arsiv-eval dump ...` (same positionals as `lab`) writes the raw fusion inputs per query (JSONL) for offline sweeps.
//! OCR input: deskew is on like in the app; `--no-deskew` turns it off.
//! `--ocr-jobs N` caps concurrent Tesseract processes (default max(1, cores-1)).
//! `arsiv-eval querybench <data_dir> <corpus_dir> <load_dir> --models DIR --ort FILE [--load backfill|scan]`
//!   search latency per stage (embed, bm25, coverage, fusion, snippets, catalog lock wait) while
//!   idle and while `load_dir`'s semantic index is built.
//! `arsiv-eval querysnapshot <data_dir> <queries.jsonl> <out.json> --models DIR --ort FILE`
//!   records every query vector's exact f32 bits and the real search's first 20 results.
//! `arsiv-eval lab <data_dir> <queries.jsonl> <map.json> <out.json> ...`
//!   replays the laboratory's authored queries/qrels against the app index.
//!   map.json maps each indexed file path to the lab logical document id.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use arsiv_core::engine::{Engine, EnginePaths, SearchRequest};
use arsiv_core::index::SearchOptions;
use arsiv_core::ocr::OcrConfig;
use serde::Deserialize;
use serde_json::{json, Value};

const CUTOFFS: [usize; 4] = [1, 5, 10, 20];

/// Process peak RSS in KiB (VmHWM), for indexing-memory reports on Linux.
#[cfg(not(windows))]
fn peak_rss_kib() -> Option<u64> {
    std::fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:")?.trim().strip_suffix(" kB")?.replace(' ', "").parse().ok())
}

/// Process peak working set in KiB (Windows).
#[cfg(windows)]
fn peak_rss_kib() -> Option<u64> {
    #[repr(C)]
    #[derive(Default)]
    struct Counters {
        cb: u32,
        page_fault_count: u32,
        peak_working_set_size: usize,
        working_set_size: usize,
        quota_peak_paged_pool_usage: usize,
        quota_paged_pool_usage: usize,
        quota_peak_non_paged_pool_usage: usize,
        quota_non_paged_pool_usage: usize,
        pagefile_usage: usize,
        peak_pagefile_usage: usize,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetCurrentProcess() -> *mut std::ffi::c_void;
        fn K32GetProcessMemoryInfo(process: *mut std::ffi::c_void, counters: *mut Counters, cb: u32) -> i32;
    }
    let mut counters = Counters { cb: std::mem::size_of::<Counters>() as u32, ..Default::default() };
    // SAFETY: the pseudo-handle is always valid; `counters` is a properly sized out-parameter.
    let ok = unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    (ok != 0).then(|| counters.peak_working_set_size as u64 / 1024)
}

struct Options {
    positional: Vec<String>,
    pdfium: PathBuf,
    tesseract: PathBuf,
    tessdata: PathBuf,
    search: SearchOptions,
    ocr_jobs: usize,
    deskew: bool,
    /// Semantic pieces; `None` = pure BM25 (A/B baseline).
    embed: Option<arsiv_core::embed::EmbedConfig>,
    hybrid: arsiv_core::vectors::HybridOptions,
    named: HashMap<String, String>,
}

impl Options {
    fn named_usize(&self, key: &str) -> Option<usize> {
        self.named.get(key).and_then(|v| v.parse().ok())
    }
}

fn parse() -> Result<Options> {
    let mut positional = Vec::new();
    let mut named: HashMap<String, String> = HashMap::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if let Some(flag) = arg.strip_prefix("--no-").filter(|f| ["expand", "legacy-stems"].contains(f)) {
            named.insert(format!("no-{flag}"), "1".into());
        } else if arg == "--no-deskew" {
            named.insert("no-deskew".into(), "1".into());
        } else if arg == "--no-vec" {
            named.insert("no-vec".into(), "1".into());
        } else if let Some(key) = arg.strip_prefix("--") {
            named.insert(key.to_string(), args.next().context("missing option value")?);
        } else {
            positional.push(arg);
        }
    }
    let take = |key: &str| named.get(key).map(PathBuf::from).with_context(|| format!("--{key} is required"));
    Ok(Options {
        positional,
        pdfium: take("pdfium")?,
        tesseract: take("tesseract")?,
        tessdata: take("tessdata")?,
        search: {
            let mut search = SearchOptions::default();
            search.expand = !named.contains_key("no-expand");
            search.legacy_stems = !named.contains_key("no-legacy-stems");
            if let Some(value) = named.get("stem-boost") {
                search.stem_boost = value.parse().context("--stem-boost")?;
            }
            if let Some(value) = named.get("phrase-boost") {
                search.phrase_boost = value.parse().context("--phrase-boost")?;
            }
            if let Some(value) = named.get("pair-boost") {
                search.pair_boost = value.parse().context("--pair-boost")?;
            }
            if let Some(value) = named.get("phrase-slop") {
                search.phrase_slop = value.parse().context("--phrase-slop")?;
            }
            if let Some(value) = named.get("synonym-boost") {
                search.synonym_boost = value.parse().context("--synonym-boost")?;
            }
            search
        },
        // Same as the app: deskew on unless measuring without it.
        deskew: !named.contains_key("no-deskew"),
        ocr_jobs: named.get("ocr-jobs").map(|v| v.parse()).transpose().context("--ocr-jobs")?.unwrap_or(0),
        // `--models DIR --ort FILE` point at the bundled EmbeddingGemma 2
        // pieces; `--no-vec` keeps the BM25 baseline. Omitting `--models`
        // also means no semantic side (embedder never loads).
        embed: if named.contains_key("no-vec") {
            None
        } else {
            match (named.get("ort"), named.get("models")) {
                (Some(ort), Some(models)) => Some(arsiv_core::embed::EmbedConfig {
                    runtime: PathBuf::from(ort),
                    models: PathBuf::from(models),
                }),
                _ => None,
            }
        },
        hybrid: {
            let mut hybrid = arsiv_core::vectors::HybridOptions::default();
            if let Some(value) = named.get("vec-weight") {
                hybrid.vec_weight = value.parse().context("--vec-weight")?;
            }
            if let Some(value) = named.get("rrf-k") {
                hybrid.rrf_k = value.parse().context("--rrf-k")?;
            }
            if let Some(value) = named.get("vec-dim") {
                hybrid.dim = value.parse().context("--vec-dim")?;
            }
            if let Some(value) = named.get("vec-candidates") {
                hybrid.candidates = value.parse().context("--vec-candidates")?;
            }
            if let Some(value) = named.get("min-cosine") {
                hybrid.min_cosine = value.parse().context("--min-cosine")?;
            }
            if let Some(value) = named.get("vec-only") {
                hybrid.vec_only_factor = value.parse().context("--vec-only")?;
            }
            if let Some(value) = named.get("head-coverage") {
                hybrid.head_coverage = value.parse().context("--head-coverage")?;
            }
            if let Some(value) = named.get("bm25-head") {
                hybrid.bm25_head = value.parse().context("--bm25-head")?;
            }
            if let Some(value) = named.get("code-cosine") {
                hybrid.code_vec_only_cosine = value.parse().context("--code-cosine")?;
            }
            if let Some(value) = named.get("short-chars") {
                hybrid.short_text_chars = value.parse().context("--short-chars")?;
            }
            if let Some(value) = named.get("short-penalty") {
                hybrid.short_text_penalty = value.parse().context("--short-penalty")?;
            }
            hybrid
        },
        named: named.clone(),
    })
}

fn open(options: &Options, data_dir: &str) -> Result<Arc<Engine>> {
    let engine = Engine::open(
        EnginePaths {
            data_dir: PathBuf::from(data_dir),
            pdfium_dir: options.pdfium.clone(),
            ocr: OcrConfig {
                binary: options.tesseract.clone(),
                tessdata: options.tessdata.clone(),
                language: "tur".into(),
                timeout: Duration::from_secs(120),
                threads: 1,
            },
            ocr_jobs: options.ocr_jobs,
            deskew: options.deskew,
            embed: options.embed.clone(),
        },
        Arc::new(|_| {}),
        Arc::new(|_| {}),
    )?;
    engine.set_hybrid_options(options.hybrid);
    engine.set_search_options(options.search);
    // A schema bump rebuilds the index from the catalog in the background;
    // measurements must see the whole index.
    engine.wait_ready();
    Ok(engine)
}

fn engine_paths(options: &Options, data_dir: &str, embed: Option<arsiv_core::embed::EmbedConfig>) -> EnginePaths {
    EnginePaths {
        data_dir: PathBuf::from(data_dir),
        pdfium_dir: options.pdfium.clone(),
        ocr: OcrConfig {
            binary: options.tesseract.clone(),
            tessdata: options.tessdata.clone(),
            language: "tur".into(),
            timeout: Duration::from_secs(120),
            threads: 1,
        },
        ocr_jobs: options.ocr_jobs,
        deskew: options.deskew,
        embed,
    }
}

/// Median / 90th percentile / max of one stage over the samples.
fn spread(values: &mut [f64]) -> Value {
    if values.is_empty() {
        return json!(null);
    }
    values.sort_by(|a, b| a.total_cmp(b));
    let at = |p: f64| values[((values.len() - 1) as f64 * p).round() as usize];
    json!({"median": (at(0.5) * 10.0).round() / 10.0, "p90": (at(0.9) * 10.0).round() / 10.0, "max": (values[values.len() - 1] * 10.0).round() / 10.0})
}

fn timing_report(samples: &[arsiv_core::engine::SearchTiming]) -> Value {
    let stage = |get: &dyn Fn(&arsiv_core::engine::SearchTiming) -> f64| spread(&mut samples.iter().map(get).collect::<Vec<_>>());
    let mut terms: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for timing in samples {
        for (term, elapsed) in &timing.coverage_terms {
            terms.entry(term.clone()).or_default().push(*elapsed);
        }
    }
    let term_times = terms.into_iter().map(|(term, mut times)| (term, spread(&mut times))).collect::<serde_json::Map<_, _>>();
    json!({
        "n": samples.len(),
        "total": stage(&|t| t.total),
        "embed": stage(&|t| t.embed),
        "bm25": stage(&|t| t.bm25),
        "coverage": stage(&|t| t.coverage),
        "coverage_counts": stage(&|t| t.coverage_counts),
        "coverage_reads": stage(&|t| t.coverage_reads),
        "coverage_match": stage(&|t| t.coverage_match),
        "coverage_terms": term_times,
        "coverage_page_reads": stage(&|t| t.coverage_page_reads as f64),
        "coverage_bytes": stage(&|t| t.coverage_bytes as f64),
        "fusion": stage(&|t| t.fusion),
        "vectors": stage(&|t| t.vectors),
        "snippets": stage(&|t| t.snippets),
        "catalog_wait": stage(&|t| t.catalog_wait),
    })
}

/// `querybench <data_dir> <corpus_dir> <load_dir> --models … --ort …`
/// Search latency per stage while idle and while the
/// semantic index of `load_dir` is built — `--load backfill` (default: its
/// text is read without a model first, then a fresh engine embeds it in the
/// start-up backfill, like the app after a cancel) or `--load scan` (a scan
/// that reads and embeds it). `--queries "a|b"`, `--rounds N` (idle passes),
/// `--gap-ms N` (pause between searches, default 200 like a typing user).
/// The load folder is removed afterwards so every run embeds it again.
fn querybench(options: &Options) -> Result<()> {
    let [_, data, corpus, load] = options.positional.as_slice() else {
        bail!("querybench <data_dir> <corpus_dir> <load_dir>")
    };
    let embed = options.embed.clone().context("querybench needs --models and --ort")?;
    let queries: Vec<String> = options
        .named
        .get("queries")
        .map(|q| q.split('|').map(str::trim).filter(|q| !q.is_empty()).map(String::from).collect())
        .unwrap_or_else(|| {
            ["kira sözleşmesi", "oturduğum evin ücret anlaşması", "00 TEST 123", "fatura 8.158,00", "depozito iadesi"]
                .map(String::from)
                .to_vec()
        });
    if queries.is_empty() {
        bail!("querybench needs at least one non-empty query");
    }
    let rounds = options.named_usize("rounds").unwrap_or(5).max(1);
    let gap = Duration::from_millis(options.named_usize("gap-ms").unwrap_or(200) as u64);
    let mode = options.named.get("load").map(String::as_str).unwrap_or("backfill");
    let searches = |engine: &Engine, keep_going: &dyn Fn(usize) -> bool| -> Result<Vec<_>> {
        let mut samples = Vec::new();
        let mut n = 0usize;
        while keep_going(n) {
            let query = &queries[n % queries.len()];
            let (_, timing) = engine.search_timed(&request(query, 20)).with_context(|| format!("querybench search {query:?}"))?;
            samples.push((n % queries.len(), timing));
            n += 1;
            std::thread::sleep(gap);
        }
        Ok(samples)
    };
    let forget_load = |engine: &Engine| -> Result<()> {
        let normalized = arsiv_core::catalog::normalize_folder(load);
        for folder in engine.folders()? {
            if arsiv_core::catalog::normalize_folder(&folder.path) == normalized {
                engine.remove_folder(folder.id)?;
            }
        }
        Ok(())
    };

    // One embedder per process: ONNX Runtime is loaded once, a second
    // engine with a model in the same process cannot load it again.
    let (engine, idle, busy, base_chunks, embed_secs, phase_secs) = match mode {
        "scan" => {
            let engine = open(options, data)?;
            forget_load(&*engine)?;
            engine.index_folder_blocking_full(corpus, &|_| {})?;
            let base_chunks = engine.vector_chunk_count();
            let idle = searches(&*engine, &|n| n < rounds * queries.len())?;
            let finished = std::sync::atomic::AtomicBool::new(false);
            let started = Instant::now();
            let (result, busy) = std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let result = engine.index_folder_blocking_full(load, &|_| {});
                    finished.store(true, std::sync::atomic::Ordering::SeqCst);
                    result
                });
                let busy = searches(&*engine, &|_| !finished.load(std::sync::atomic::Ordering::SeqCst));
                (worker.join().expect("scan thread"), busy)
            });
            result?;
            let busy = busy?;
            let secs = started.elapsed().as_secs_f64();
            (engine, idle, busy, base_chunks, secs, secs)
        }
        "backfill" => {
            {
                // Text only, no model: the load files wait at embedded=0
                // (run `--load scan` once first so the corpus has vectors).
                let plain = Engine::open(engine_paths(options, data, None), Arc::new(|_| {}), Arc::new(|_| {}))?;
                plain.wait_ready();
                forget_load(&*plain)?;
                plain.index_folder_blocking(corpus, &|_| {})?;
                plain.index_folder_blocking(load, &|_| {})?;
            }
            let opened = Instant::now();
            let engine = Engine::open(engine_paths(options, data, Some(embed)), Arc::new(|_| {}), Arc::new(|_| {}))?;
            engine.set_hybrid_options(options.hybrid);
            engine.set_search_options(options.search);
            let base_chunks = engine.vector_chunk_count();
            let deadline = Instant::now() + Duration::from_secs(600);
            while engine.progress().phase != arsiv_core::engine::IndexPhase::Embedding {
                if Instant::now() > deadline {
                    bail!("the backfill never started (semantic: {:?})", engine.semantic_status());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            let started = Instant::now();
            let busy = searches(&*engine, &|_| engine.progress().phase == arsiv_core::engine::IndexPhase::Embedding)?;
            let phase = started.elapsed().as_secs_f64();
            let total = opened.elapsed().as_secs_f64();
            engine.wait_ready();
            // Idle reference on the same engine, once the backfill is done.
            let idle = searches(&*engine, &|n| n < rounds * queries.len())?;
            (engine, idle, busy, base_chunks, total, phase)
        }
        other => bail!("--load {other}: scan or backfill"),
    };
    let chunks = engine.vector_chunk_count().saturating_sub(base_chunks);
    let after = searches(&*engine, &|n| n < rounds * queries.len())?;
    forget_load(&*engine)?;
    let round = |x: f64, places: i32| (x * 10f64.powi(places)).round() / 10f64.powi(places);
    // Whole-search medians per query, idle vs busy.
    let per_query = |samples: &[(usize, arsiv_core::engine::SearchTiming)]| -> Value {
        queries
            .iter()
            .enumerate()
            .map(|(i, q)| {
                let timings = samples.iter().filter(|(n, _)| *n == i).map(|(_, timing)| timing.clone()).collect::<Vec<_>>();
                (q.clone(), timing_report(&timings))
            })
            .collect::<serde_json::Map<_, _>>()
            .into()
    };
    let only = |samples: &[(usize, arsiv_core::engine::SearchTiming)]| samples.iter().map(|(_, t)| t.clone()).collect::<Vec<_>>();
    let report = json!({
        "mode": mode,
        "queries": queries,
        "semantic": engine.semantic_status().ok(),
        "load_chunks": chunks,
        "embed_secs": round(embed_secs, 1),
        "busy_phase_secs": round(phase_secs, 1),
        "secs_per_chunk": if chunks > 0 { round(phase_secs / chunks as f64, 3) } else { 0.0 },
        "idle": timing_report(&only(&idle)),
        "busy": timing_report(&only(&busy)),
        "idle_after": timing_report(&only(&after)),
        "idle_by_query": per_query(&after),
        "busy_by_query": per_query(&busy),
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}

/// Read both the small A/B input (`query`) and the authored lab input (`text`).
/// Vector components are stored as u32 bits so JSON float formatting cannot
/// hide reduction-order differences between separate ORT processes.
fn querysnapshot(options: &Options) -> Result<()> {
    let [_, data, queries, out] = options.positional.as_slice() else {
        bail!("querysnapshot <data_dir> <queries.jsonl> <out.json>")
    };
    if options.embed.is_none() {
        bail!("querysnapshot needs --models and --ort");
    }
    #[derive(Deserialize)]
    struct SnapshotQuery {
        #[serde(alias = "text")]
        query: String,
        query_id: Option<String>,
        #[serde(rename = "type")]
        kind: Option<String>,
    }
    let input = std::fs::read_to_string(queries).with_context(|| format!("read {queries}"))?;
    let queries = input
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(line, text)| serde_json::from_str::<SnapshotQuery>(text).with_context(|| format!("query line {}", line + 1)))
        .collect::<Result<Vec<_>>>()?;
    if queries.is_empty() {
        bail!("querysnapshot needs at least one query");
    }
    let engine = open(options, data)?;
    engine.semantic_status().map_err(anyhow::Error::msg)?;
    if engine.vector_chunk_count() == 0 {
        bail!("querysnapshot needs an index with document vectors");
    }
    let mut rows = Vec::with_capacity(queries.len());
    for query in queries {
        let vector = engine.debug_query_vector(&query.query).with_context(|| format!("embed query {:?}", query.query))?;
        let response = engine.search(&request(&query.query, 20)).with_context(|| format!("search query {:?}", query.query))?;
        rows.push(json!({
            "query": query.query,
            "query_id": query.query_id,
            "type": query.kind,
            "vector_bits": vector.iter().map(|v| v.to_bits()).collect::<Vec<_>>(),
            "score_bits": response.hits.iter().map(|hit| hit.score.to_bits()).collect::<Vec<_>>(),
            "response": response,
        }));
    }
    let report = json!({
        "query_threads": std::env::var("ARSIV_QUERY_THREADS").ok(),
        "vector_chunks": engine.vector_chunk_count(),
        "rows": rows,
    });
    std::fs::write(out, serde_json::to_string_pretty(&report)?).with_context(|| format!("write {out}"))?;
    eprintln!("querysnapshot: {} queries written to {out}", report["rows"].as_array().map_or(0, Vec::len));
    Ok(())
}

fn main() -> Result<()> {
    let options = parse()?;
    let command = options.positional.first().map(String::as_str).unwrap_or("");
    match command {
        "index" => {
            let [_, data, folder] = options.positional.as_slice() else { bail!("index <data_dir> <folder>") };
            let engine = open(&options, data)?;
            println!("ocr: {:?}\npdf: {:?}\nsemantic: {:?}", engine.ocr_status(), engine.pdf_status(), engine.semantic_status());
            let rss_before = peak_rss_kib();
            let started = Instant::now();
            let last = std::sync::Mutex::new(Instant::now());
            // `--cancel-after-ms N`: "İptal" N ms into the run,
            // then report what the cancel left (text read, semantic index
            // pending, files never read).
            let cancel_after = options.named_usize("cancel-after-ms").map(|ms| Duration::from_millis(ms as u64));
            let finished = std::sync::atomic::AtomicBool::new(false);
            let result = std::thread::scope(|scope| {
                if let Some(after) = cancel_after {
                    let engine = &engine;
                    let finished = &finished;
                    let started = &started;
                    scope.spawn(move || {
                        while started.elapsed() < after && !finished.load(std::sync::atomic::Ordering::SeqCst) {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        if !finished.load(std::sync::atomic::Ordering::SeqCst) {
                            let p = engine.progress();
                            eprintln!("cancel at {:.1} s: text {}/{} embed {}/{}", started.elapsed().as_secs_f64(), p.text_done, p.total, p.embed_done, p.embed_total);
                            engine.cancel_indexing();
                        }
                    });
                }
                // The blocking scan embeds inline; anything it skipped is
                // filled by the worker's post-ready pass — `wait_ready`
                // already covered the start-up pass, so this is only for our
                // own leftovers.
                let result = engine.index_folder_blocking_full(folder, &|p| {
                    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
                    if last.elapsed() >= Duration::from_secs(10) || p.done == p.total {
                        *last = Instant::now();
                        eprintln!("{:?} {}/{} failed={}", p.phase, p.done, p.total, p.failed);
                    }
                });
                finished.store(true, std::sync::atomic::Ordering::SeqCst);
                result
            });
            let seconds = started.elapsed().as_secs_f64();
            if cancel_after.is_some() {
                let p = engine.progress();
                println!(
                    "after cancel: stopped in {seconds:.1} s, cancelled={} text_done={} total={} embed_done={} unread={} pending_embed={:?}",
                    p.cancelled, p.text_done, p.total, p.embed_done, p.unread, p.pending_embed
                );
            }
            if let Err(error) = result {
                // Cancellation is an expected outcome only for this explicit
                // diagnostic. Extraction/catalog/index errors must still fail.
                if cancel_after.is_none() || !arsiv_core::ocr::is_cancelled(&error) {
                    return Err(error);
                }
            }
            engine.wait_ready();
            let folders = engine.folders()?;
            println!("seconds: {seconds:.1}");
            println!("vector chunks: {}", engine.vector_chunk_count());
            match (rss_before, peak_rss_kib()) {
                (Some(before), Some(after)) => println!("peak RSS: {:.0} MiB (was {:.0} MiB)", after as f64 / 1024.0, before as f64 / 1024.0),
                _ => {}
            }
            println!("{}", serde_json::to_string_pretty(&folders)?);
            for failure in engine.failures(None)?.iter().take(20) {
                println!("FAILED {} :: {}", failure.path, failure.error);
            }
        }
        "search" => {
            let [_, data, query] = options.positional.as_slice() else { bail!("search <data_dir> <query>") };
            let engine = open(&options, data)?;
            let response = engine.search(&request(query, 10))?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            if let Some(hit) = response.hits.first() {
                let view = engine.document(&hit.path, query)?;
                println!("first page highlights: {:?}", view.pages.first().map(|p| &p.highlights));
            }
        }
        "skew" => {
            // Diagnostics: the angle deskew would correct per OCR page.
            let (data, file, dump) = match options.positional.as_slice() {
                [_, data, file] => (data, file, None),
                [_, data, file, dump] => (data, file, Some(std::path::Path::new(dump))),
                _ => bail!("skew <data_dir> <file> [dump_dir]"),
            };
            let engine = open(&options, data)?;
            for (page, angle) in engine.skew_estimates(std::path::Path::new(file), dump)?.iter().enumerate() {
                match angle {
                    Some(angle) => println!("page {}: {angle:+.1}°", page + 1),
                    None => println!("page {}: text layer", page + 1),
                }
            }
        }
        "lab" => {
            let [_, data, queries, map, out] = options.positional.as_slice() else {
                bail!("lab <data_dir> <queries.jsonl> <map.json> <out.json>")
            };
            let engine = open(&options, data)?;
            lab(&engine, queries, map, out)?;
        }
        "nomatch" => {
            // Numbers/codes no file contains: the right answer is an empty
            // (or near-empty) list, not semantically similar look-alikes.
            let [_, data, queries, out] = options.positional.as_slice() else {
                bail!("nomatch <data_dir> <queries.txt> <out.json>")
            };
            let engine = open(&options, data)?;
            let mut rows = Vec::new();
            for query in std::fs::read_to_string(queries)?.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                let response = engine.search(&request(query, 50))?;
                let semantic = response.hits.iter().filter(|h| h.semantic_only).count();
                rows.push(json!({"query": query, "hits": response.hits.len(), "semantic_only": semantic, "ms": response.took_ms}));
            }
            let empty = rows.iter().filter(|r| r["hits"] == 0).count();
            let report = json!({"queries": rows.len(), "empty": empty, "rows": rows});
            std::fs::write(out, serde_json::to_string_pretty(&report)?)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        "dump" => {
            // Raw fusion inputs per lab query, for offline ranking sweeps.
            let [_, data, queries, map, out] = options.positional.as_slice() else {
                bail!("dump <data_dir> <queries.jsonl> <map.json> <out.jsonl>")
            };
            let engine = open(&options, data)?;
            let path_to_doc: HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(map)?)?;
            let mut lines = String::new();
            for line in std::fs::read_to_string(queries)?.lines().filter(|l| !l.trim().is_empty()) {
                let query: LabQuery = serde_json::from_str(line)?;
                let mut c = serde_json::to_value(engine.debug_candidates(&query.text, 200)?)?;
                for side in ["bm25", "vectors"] {
                    if let Some(list) = c[side].as_array_mut() {
                        for hit in list {
                            let doc = hit["path"].as_str().and_then(|p| path_to_doc.get(p)).cloned();
                            hit["doc"] = json!(doc);
                        }
                    }
                }
                c["query_id"] = json!(query.query_id);
                c["type"] = json!(query.kind);
                c["text"] = json!(query.text);
                c["qrels"] = json!(query.qrels);
                lines.push_str(&serde_json::to_string(&c)?);
                lines.push('\n');
            }
            std::fs::write(out, lines)?;
        }
        "embedbench" => {
            // Embedding cost of single files, outside the app: extract once,
            // then embed `--repeat` times (ARSIV_EMBED_TIMING=1 for per-run
            // detail; ARSIV_EMBED_THREADS/BATCH for A/B). `--parallel N`
            // embeds N copies of each file at once on the shared session
            // (the scan's queueing).
            let files = &options.positional[1..];
            let embed = options.embed.clone().context("embedbench needs --models and --ort")?;
            let embedder = Arc::new(arsiv_core::embed::Embedder::new(embed));
            let load = Instant::now();
            println!("check: {:?} ({:.0} ms)", embedder.check(), load.elapsed().as_secs_f64() * 1000.0);
            let scratch = std::env::temp_dir().join("arsiv-embedbench");
            std::fs::create_dir_all(&scratch)?;
            let extractor = arsiv_core::extract::Extractor::new(
                &options.pdfium,
                OcrConfig { binary: options.tesseract.clone(), tessdata: options.tessdata.clone(), language: "tur".into(), timeout: Duration::from_secs(120), threads: 1 },
                scratch,
                1,
            );
            let repeat = options.named_usize("repeat").unwrap_or(1).max(1);
            let parallel = options.named_usize("parallel").unwrap_or(1).max(1);
            if let Some(queries) = options.named.get("same-vector") {
                // Is a query's vector bit-for-bit the same on every run
                // (a cached vector would then be exactly a fresh one)?
                for query in queries.split('|') {
                    let first = embedder.embed_query(query)?;
                    let differing = (0..repeat.max(2))
                        .map(|_| embedder.embed_query(query).map(|v| v.iter().zip(&first).filter(|(a, b)| a.to_bits() != b.to_bits()).count()))
                        .collect::<Result<Vec<_>>>()?;
                    // Fingerprint of the exact bits, to compare across
                    // processes (e.g. ARSIV_QUERY_THREADS=4 vs 8).
                    let bits = first.iter().fold(0xcbf29ce484222325u64, |h, v| (h ^ v.to_bits() as u64).wrapping_mul(0x100000001b3));
                    println!("same-vector query={query:?} runs={} differing_dims={differing:?} bits={bits:016x}", differing.len());
                }
            }
            let never = std::sync::atomic::AtomicBool::new(false);
            for file in files {
                let path = std::path::Path::new(file);
                let t = Instant::now();
                let document = extractor.extract(path)?;
                let extract_ms = t.elapsed().as_secs_f64() * 1000.0;
                let title = arsiv_core::embed::document_title(file);
                let query_load = options.named.contains_key("query-load");
                for round in 0..repeat {
                    let t = Instant::now();
                    let running = std::sync::atomic::AtomicBool::new(true);
                    let query_ms = std::sync::Mutex::new(Vec::new());
                    let counts: Vec<usize> = std::thread::scope(|scope| {
                        if query_load {
                            // A search's query embed while the document runs.
                            scope.spawn(|| {
                                while running.load(std::sync::atomic::Ordering::Relaxed) {
                                    let q = Instant::now();
                                    let _ = embedder.embed_query("kira sözleşmesi depozito");
                                    query_ms.lock().unwrap().push(q.elapsed().as_secs_f64() * 1000.0);
                                    std::thread::sleep(Duration::from_millis(200));
                                }
                            });
                        }
                        let handles: Vec<_> = (0..parallel)
                            .map(|_| scope.spawn(|| embedder.embed_document(&title, document.kind, &document.pages, &never).map(|v| v.len())))
                            .collect();
                        let counts: Vec<usize> = handles.into_iter().map(|h| h.join().expect("embed thread").unwrap_or(0)).collect();
                        running.store(false, std::sync::atomic::Ordering::Relaxed);
                        counts
                    });
                    let mut q = query_ms.into_inner().unwrap();
                    if !q.is_empty() {
                        q.sort_by(|a, b| a.total_cmp(b));
                        println!("query embeds during the run: n={} median_ms={:.0} max_ms={:.0}", q.len(), q[q.len() / 2], q[q.len() - 1]);
                    }
                    let secs = t.elapsed().as_secs_f64();
                    let chunks: usize = counts.iter().sum();
                    println!(
                        "file={file} kind={:?} extract_ms={extract_ms:.0} round={round} parallel={parallel} chunks={chunks} secs={secs:.2} s_per_chunk={:.3}",
                        document.kind,
                        secs / chunks.max(1) as f64
                    );
                }
            }
        }
        "querybench" => querybench(&options)?,
        "querysnapshot" => querysnapshot(&options)?,
        _ => bail!("usage: arsiv-eval index|search|lab|nomatch|dump|skew|embedbench|querybench|querysnapshot ... --pdfium DIR --tesseract EXE --tessdata DIR"),
    }
    Ok(())
}

fn request(query: &str, limit: usize) -> SearchRequest {
    SearchRequest { query: query.into(), kinds: vec![], modified_after: None, modified_before: None, limit, offset: 0 }
}

#[derive(Deserialize)]
struct LabQuery {
    query_id: String,
    #[serde(rename = "type")]
    kind: String,
    text: String,
    qrels: BTreeMap<String, i64>,
}

/// Same metric definitions as turkish_doc_lab.search._query_metrics.
fn metrics(query: &LabQuery, ranked: &[String]) -> BTreeMap<String, f64> {
    let relevant: HashSet<&String> = query.qrels.iter().filter(|(_, g)| **g > 0).map(|(d, _)| d).collect();
    let mut out = BTreeMap::new();
    for cutoff in CUTOFFS {
        let found = ranked.iter().take(cutoff).filter(|d| relevant.contains(d)).count();
        out.insert(format!("recall@{cutoff}"), if relevant.is_empty() { 0.0 } else { found as f64 / relevant.len() as f64 });
    }
    let mrr = ranked.iter().position(|d| relevant.contains(d)).map_or(0.0, |i| 1.0 / (i + 1) as f64);
    out.insert("mrr@20".into(), mrr);
    let gain = |grade: i64| 2f64.powi(grade as i32) - 1.0;
    let dcg: f64 = ranked
        .iter()
        .enumerate()
        .map(|(i, d)| gain(*query.qrels.get(d).unwrap_or(&0)) / ((i + 2) as f64).log2())
        .sum();
    let mut grades: Vec<i64> = query.qrels.values().copied().collect();
    grades.sort_unstable_by(|a, b| b.cmp(a));
    let ideal: f64 = grades.iter().take(20).enumerate().map(|(i, g)| gain(*g) / ((i + 2) as f64).log2()).sum();
    out.insert("ndcg@20".into(), if ideal > 0.0 { dcg / ideal } else { 0.0 });
    out
}

fn mean(rows: &[&BTreeMap<String, f64>]) -> Value {
    let mut sums: BTreeMap<String, f64> = BTreeMap::new();
    for row in rows {
        for (key, value) in row.iter() {
            *sums.entry(key.clone()).or_default() += value;
        }
    }
    let count = rows.len().max(1) as f64;
    let mut object: serde_json::Map<String, Value> =
        sums.into_iter().map(|(k, v)| (k, json!(v / count))).collect();
    object.insert("query_count".into(), json!(rows.len()));
    Value::Object(object)
}

fn lab(engine: &Engine, queries: &str, map: &str, out: &str) -> Result<()> {
    let path_to_doc: HashMap<String, String> = serde_json::from_str(&std::fs::read_to_string(map)?)?;
    let queries: Vec<LabQuery> = std::fs::read_to_string(queries)?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(serde_json::from_str)
        .collect::<Result<_, _>>()?;
    let mut rows = Vec::new();
    let mut unmapped = 0usize;
    let mut latency_ms: Vec<f64> = Vec::new();
    let started = Instant::now();
    for query in &queries {
        // Lab ranks logical documents; the app ranks files. Scanned/digital
        // variants of one logical document collapse to its first-ranked file.
        let search_started = Instant::now();
        let ranked = match engine.search(&request(&query.text, 200)) {
            Ok(response) => {
                latency_ms.push(search_started.elapsed().as_secs_f64() * 1000.0);
                let mut seen = HashSet::new();
                let mut ranked = Vec::new();
                for hit in response.hits {
                    match path_to_doc.get(&hit.path) {
                        Some(doc) if seen.insert(doc.clone()) => ranked.push(doc.clone()),
                        Some(_) => {}
                        None => unmapped += 1,
                    }
                    if ranked.len() == 20 {
                        break;
                    }
                }
                ranked
            }
            Err(_) => Vec::new(),
        };
        rows.push((query.kind.clone(), query.query_id.clone(), metrics(query, &ranked)));
    }
    let percentile = |p: f64| {
        if latency_ms.is_empty() {
            return 0.0;
        }
        let mut sorted = latency_ms.clone();
        sorted.sort_by(|a, b| a.total_cmp(b));
        let index = ((sorted.len() - 1) as f64 * p).round() as usize;
        sorted[index]
    };
    let all: Vec<&BTreeMap<String, f64>> = rows.iter().map(|(_, _, m)| m).collect();
    let mut by_type: BTreeMap<String, Vec<&BTreeMap<String, f64>>> = BTreeMap::new();
    for (kind, _, m) in &rows {
        by_type.entry(kind.clone()).or_default().push(m);
    }
    let report = json!({
        "engine": "zoria-arsiv tantivy BM25 + EmbeddingGemma 2 vector side (RRF hybrid)",
        "search_options": engine.search_options(),
        "hybrid_options": engine.hybrid_options(),
        "semantic_status": engine.semantic_status().ok(),
        "vector_chunks": engine.vector_chunk_count(),
        "query_count": queries.len(),
        "unmapped_hits": unmapped,
        "seconds": started.elapsed().as_secs_f64(),
        "latency_ms": {"p50": percentile(0.5), "p95": percentile(0.95), "count": latency_ms.len()},
        "aggregate": mean(&all),
        "by_query_type": by_type.iter().map(|(k, v)| (k.clone(), mean(v))).collect::<BTreeMap<_, _>>(),
        "queries": rows.iter().map(|(kind, id, m)| json!({"query_id": id, "type": kind, "metrics": m})).collect::<Vec<_>>(),
    });
    std::fs::write(out, serde_json::to_string_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&json!({"aggregate": report["aggregate"], "by_query_type": report["by_query_type"], "unmapped_hits": unmapped}))?);
    Ok(())
}
