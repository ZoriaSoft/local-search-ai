//! In-memory vector index over the catalog's chunk embeddings. The corpus is
//! small (hundreds of files, thousands of chunks), so a flat cosine scan is
//! the right structure: exact, no parameters, and fast enough
//! (20k chunks × 768d ≈ 15M FLOPs ≈ a few ms). HNSW can replace `search`
//! transparently if a deployment ever makes the scan show up in `took_ms`.
//!
//! Fusion with BM25 is reciprocal-rank (RRF): each side contributes
//! `weight / (rrf_k + rank)` per file. Ranks are scale-free, so BM25 and
//! cosine never need score normalization.

use std::collections::HashMap;
use std::sync::RwLock;

use crate::index::{PageHit, SearchFilter};
use crate::model::FileKind;

/// RRF smoothing constant (the usual literature default).
/// 20, not the literature's 60: the lab (2026-10-15) favours a sharper
/// rank curve once weak BM25 hits are no longer protected.
pub const DEFAULT_RRF_K: f32 = 20.0;
/// How many best-scored files each side contributes to the fused list. The
/// lab ranks 20; a deeper pool only widens the recall net RRF can reorder.
pub const HYBRID_CANDIDATES: usize = 200;
/// Vector-side hits below this cosine are treated as noise and dropped
/// before fusion. Calibrated on the lab corpus.
pub const MIN_COSINE: f32 = 0.3;

/// Hybrid ranking switches, lab-measured.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HybridOptions {
    /// RRF weight of the vector list; `0` disables hybrid ranking.
    pub vec_weight: f32,
    /// RRF smoothing constant.
    pub rrf_k: f32,
    /// Embedding dims used at query time (Matryoshka truncation; vectors
    /// are stored at 768 and both sides are renormalized after slicing).
    pub dim: usize,
    /// Best files each ranking contributes.
    pub candidates: usize,
    /// Cosine floor applied to vector candidates.
    pub min_cosine: f32,
    /// Extra damping for files ONLY the vector side found (semantic-only
    /// hits). These carry most of the value for paraphrase queries but
    /// also most of the noise, so they enter the fused list damped.
    pub vec_only_factor: f32,
    /// Up to this many of the first BM25 hits are kept verbatim — but only
    /// the confident ones (see `head_coverage`); everything else, and the
    /// vector candidates, compete on RRF scores after them.
    pub bm25_head: usize,
    /// A head BM25 hit is protected only if its page covers at least this
    /// share of the query's IDF mass (`term_weights` / `coverage`). A hit
    /// that matched a single common word ("ücret") of a natural-language
    /// query is weak keyword evidence and must not outrank meaning. `0`
    /// protects every head hit (the pre-2026-10-15 behaviour).
    pub head_coverage: f32,
    /// Queries with a digit-bearing token (amount, date, plate, document
    /// number) are exact lookups: a file only the vector side found is shown
    /// only at or above this cosine. Above 1 (default 2) drops them all, so a
    /// number no file contains returns an empty list instead of look-alikes.
    pub code_vec_only_cosine: f32,
    /// Pages shorter than this many characters (OCR of a mostly-graphic
    /// image) get a cosine penalty growing to `short_text_penalty` as the
    /// text shrinks to nothing; `0` disables it.
    pub short_text_chars: usize,
    pub short_text_penalty: f32,
}

impl Default for HybridOptions {
    fn default() -> Self {
        // Lab sweep (2026-10-15):
        // a fixed BM25 head of 5 let one common query word push meaning
        // matches below rank 6. Now up to 10 head hits stay verbatim only
        // when they cover ≥40% of the query's IDF mass; the rest fuse by RRF
        // (k=20) with a strong vector weight, vector-only files damped.
        // 512 dims beat 768 on aggregate within noise.
        Self {
            vec_weight: 3.0,
            rrf_k: DEFAULT_RRF_K,
            dim: 512,
            candidates: HYBRID_CANDIDATES,
            min_cosine: 0.45,
            vec_only_factor: 0.3,
            bm25_head: 10,
            head_coverage: 0.4,
            code_vec_only_cosine: 2.0,
            // Lab sweep 2026-10-15: 800 chars / 0.03 lifts
            // semantic R@10 0.267 -> 0.272 with every guard line held.
            short_text_chars: 800,
            short_text_penalty: 0.03,
        }
    }
}

/// One indexed file's vector data: per-chunk `(page, vector)` plus the
/// filter metadata a search needs without touching the catalog.
struct FileVecs {
    kind: FileKind,
    modified: i64,
    page_of_chunk: Vec<usize>,
    vectors: Vec<Vec<f32>>,
}

/// Per-file chunk embeddings kept in memory (catalog `chunks` is the
/// durable copy; this is rebuilt lazily on open and kept in step by the
/// indexer).
pub struct VectorIndex {
    files: RwLock<HashMap<String, FileVecs>>,
}

impl VectorIndex {
    pub fn empty() -> Self {
        Self { files: RwLock::new(HashMap::new()) }
    }

    /// Replaces the file's vectors (empty `pages`/`vectors` removes it).
    pub fn set(&self, path: &str, kind: FileKind, modified: i64, chunks: &[(usize, Vec<f32>)]) {
        let mut files = self.files.write().unwrap_or_else(|e| e.into_inner());
        if chunks.is_empty() {
            files.remove(path);
        } else {
            files.insert(
                path.to_string(),
                FileVecs {
                    kind,
                    modified,
                    page_of_chunk: chunks.iter().map(|(page, _)| *page).collect(),
                    vectors: chunks.iter().map(|(_, vector)| vector.clone()).collect(),
                },
            );
        }
    }

    pub fn remove(&self, path: &str) {
        self.files.write().unwrap_or_else(|e| e.into_inner()).remove(path);
    }

    pub fn is_empty(&self) -> bool {
        self.files.read().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    pub fn chunk_count(&self) -> usize {
        self.files
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|f| f.vectors.len())
            .sum()
    }

    /// Best page per file by cosine against the query vector, honouring the
    /// same kind/date filter as the BM25 side. `dim` truncates Matryoshka-
    /// style: both sides are sliced, then L2-renormalized, before the dot
    /// product — truncating a unit vector does not leave a unit vector.
    pub fn search(&self, query: &[f32], filter: &SearchFilter, options: &HybridOptions) -> Vec<PageHit> {
        let dim = options.dim.min(super::embed::NATIVE_DIM);
        let mut truncated_query = query[..dim.min(query.len())].to_vec();
        super::embed::normalize(&mut truncated_query);
        let query = &truncated_query[..];
        let files = self.files.read().unwrap_or_else(|e| e.into_inner());
        let mut hits: Vec<PageHit> = Vec::new();
        for (path, file) in files.iter() {
            if !filter.kinds.is_empty() && !filter.kinds.contains(&file.kind) {
                continue;
            }
            if let Some(after) = filter.modified_after {
                if file.modified < after {
                    continue;
                }
            }
            if let Some(before) = filter.modified_before {
                if file.modified > before {
                    continue;
                }
            }
            let mut best: Option<(usize, f32)> = None;
            for (idx, vector) in file.vectors.iter().enumerate() {
                let truncated = &vector[..dim.min(vector.len())];
                let norm: f32 = truncated.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm == 0.0 {
                    continue;
                }
                let score: f32 = query.iter().zip(truncated).map(|(q, d)| q * d).sum::<f32>() / norm;
                if best.map_or(true, |(_, s)| score > s) {
                    best = Some((file.page_of_chunk[idx], score));
                }
            }
            if let Some((page, score)) = best {
                if score >= options.min_cosine {
                    hits.push(PageHit { path: path.clone(), page, score });
                }
            }
        }
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
        hits.truncate(options.candidates);
        hits
    }
    /// Per-file chunk scores for diagnostics: every candidate file maps to its
    /// highest `top_k` (page, cosine) pairs — the engine reports best-chunk
    /// only, but whether the second chunk also agrees is a useful signal for
    /// "is this a real semantic match or a lone noisy window".
    pub fn search_chunks(&self, query: &[f32], filter: &SearchFilter, options: &HybridOptions, top_k: usize) -> HashMap<String, Vec<(usize, f32)>> {
        let dim = options.dim.min(super::embed::NATIVE_DIM);
        let mut truncated_query = query[..dim.min(query.len())].to_vec();
        super::embed::normalize(&mut truncated_query);
        let query = &truncated_query[..];
        let files = self.files.read().unwrap_or_else(|e| e.into_inner());
        let mut out = HashMap::new();
        for (path, file) in files.iter() {
            if !filter.kinds.is_empty() && !filter.kinds.contains(&file.kind) {
                continue;
            }
            if let Some(after) = filter.modified_after {
                if file.modified < after {
                    continue;
                }
            }
            if let Some(before) = filter.modified_before {
                if file.modified > before {
                    continue;
                }
            }
            let mut scored: Vec<(usize, f32)> = Vec::with_capacity(file.vectors.len());
            for (idx, vector) in file.vectors.iter().enumerate() {
                let truncated = &vector[..dim.min(vector.len())];
                let norm: f32 = truncated.iter().map(|v| v * v).sum::<f32>().sqrt();
                if norm == 0.0 {
                    continue;
                }
                let score: f32 = query.iter().zip(truncated).map(|(q, d)| q * d).sum::<f32>() / norm;
                scored.push((file.page_of_chunk[idx], score));
            }
            scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            scored.truncate(top_k);
            if !scored.is_empty() {
                out.insert(path.clone(), scored);
            }
        }
        out
    }
}


/// A fused search hit: the file, the page to open, the RRF score, and
/// whether the vector side was the only list that ranked it (the UI badge
/// "anlamca benzer").
#[derive(Clone, Debug)]
pub struct FusedHit {
    pub path: String,
    pub page: usize,
    pub score: f32,
    /// No BM25 rank — this file is in the results only because it is
    /// semantically similar.
    pub semantic_only: bool,
}

/// Reciprocal-rank fusion of the BM25 and vector candidate lists (already
/// best-page-per-file). The first `bm25_head` BM25 hits keep their exact
/// order — the lab showed rank changes at the top cost more MRR than the
/// vectors recover — while the tail and the vector-only files compete on
/// RRF scores after it. `usize::MAX` as `bm25_head` is "BM25 only".
///
/// `coverage[i]` is BM25 hit `i`'s share of the query's IDF mass
/// (`idf_coverage`); a head hit below `options.head_coverage` loses its
/// protection and joins the RRF tail at its BM25 rank. Missing entries
/// count as fully covered.
pub fn fuse(
    bm25: &[PageHit],
    coverage: &[f32],
    vectors: &[PageHit],
    options: &HybridOptions,
    limit: usize,
) -> Vec<FusedHit> {
    let head_len = options.bm25_head.min(bm25.len());
    let protected: Vec<bool> = (0..bm25.len())
        .map(|i| i < head_len && coverage.get(i).map_or(true, |c| *c >= options.head_coverage))
        .collect();
    let mut fused: Vec<FusedHit> = Vec::with_capacity(head_len + vectors.len());
    for (hit, _) in bm25.iter().zip(&protected).filter(|(_, p)| **p) {
        fused.push(FusedHit { path: hit.path.clone(), page: hit.page, score: f32::MAX, semantic_only: false });
    }
    let mut scores: HashMap<&str, FusedHit> = HashMap::with_capacity(bm25.len() + vectors.len());
    for (rank, hit) in bm25.iter().enumerate().filter(|(i, _)| !protected[*i]) {
        scores.insert(
            hit.path.as_str(),
            FusedHit {
                path: hit.path.clone(),
                page: hit.page,
                score: 1.0 / (options.rrf_k + rank as f32 + 1.0),
                semantic_only: false,
            },
        );
    }
    for (rank, hit) in vectors.iter().enumerate() {
        if fused.iter().any(|f| f.path == hit.path) {
            continue; // a protected head file: keep its BM25 page and rank
        }
        let contribution = options.vec_weight / (options.rrf_k + rank as f32 + 1.0);
        match scores.get_mut(hit.path.as_str()) {
            // Both sides found it: keep the BM25 page, add the vector score.
            Some(fused) => fused.score += contribution,
            None => {
                scores.insert(
                    hit.path.as_str(),
                    FusedHit {
                        path: hit.path.clone(),
                        page: hit.page,
                        score: contribution * options.vec_only_factor,
                        semantic_only: true,
                    },
                );
            }
        }
    }
    let mut tail: Vec<FusedHit> = scores.into_values().collect();
    tail.sort_by(|a, b| b.score.total_cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    fused.extend(tail);
    fused.truncate(limit);
    fused
}

/// Cosine after the short-page penalty: `penalty * (1 - chars / short)` for
/// pages under `short` characters, nothing otherwise.
pub fn short_text_adjusted(cosine: f32, chars: usize, options: &HybridOptions) -> f32 {
    if options.short_text_chars == 0 || options.short_text_penalty <= 0.0 || chars >= options.short_text_chars {
        return cosine;
    }
    cosine - options.short_text_penalty * (1.0 - chars as f32 / options.short_text_chars as f32)
}

/// True when a query token carries a digit: amounts (`8.158,00`), dates,
/// plate-style test codes (`00 TEST 123`), document numbers. Such queries are exact lookups.
pub fn is_code_query(query: &str) -> bool {
    query.split_whitespace().any(|token| token.bytes().any(|b| b.is_ascii_digit()))
}

/// IDF weight of each query term: `ln((files + 1) / (df + 1))`. A term no
/// file contains gets the largest weight — a query whose words the archive
/// does not know is a paraphrase, and keyword evidence for it is weak.
pub fn term_weights(term_files: &[usize], files: usize) -> Vec<f32> {
    term_files.iter().map(|df| (((files + 1) as f32) / ((*df + 1) as f32)).ln().max(0.0) + 1e-4).collect()
}

/// Share of the query's IDF mass a page covers (`present[i]`: term `i`
/// occurs on it).
pub fn idf_coverage(weights: &[f32], present: &[bool]) -> f32 {
    let total: f32 = weights.iter().sum();
    if total <= 0.0 {
        return 1.0;
    }
    weights.iter().zip(present).filter(|(_, p)| **p).map(|(w, _)| *w).sum::<f32>() / total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::SearchFilter;

    fn hit(path: &str, page: usize, score: f32) -> PageHit {
        PageHit { path: path.into(), page, score }
    }
    fn filter() -> SearchFilter<'static> {
        SearchFilter { kinds: &[], modified_after: None, modified_before: None }
    }

    fn options() -> HybridOptions {
        HybridOptions::default()
    }

    #[test]
    fn code_queries_and_short_page_penalty() {
        assert!(is_code_query("1548"));
        assert!(is_code_query("fatura 8.158,00 TL"));
        assert!(is_code_query("00 TEST 123"));
        assert!(is_code_query("2024/117-B"));
        assert!(!is_code_query("oturduğum evin ücret anlaşması"));
        let mut options = options();
        assert!(short_text_adjusted(0.7, 10, &options) < 0.7, "on by default");
        options.short_text_chars = 0;
        assert_eq!(short_text_adjusted(0.7, 10, &options), 0.7, "0 disables it");
        options.short_text_chars = 400;
        options.short_text_penalty = 0.1;
        assert_eq!(short_text_adjusted(0.7, 400, &options), 0.7);
        assert!((short_text_adjusted(0.7, 0, &options) - 0.6).abs() < 1e-6);
        assert!((short_text_adjusted(0.7, 200, &options) - 0.65).abs() < 1e-6);
    }

    #[test]
    fn a_vector_only_file_is_marked_semantic() {
        let bm25 = vec![hit("/a.pdf", 0, 5.0)];
        let vectors = vec![hit("/a.pdf", 1, 0.9), hit("/b.pdf", 2, 0.8)];
        let fused = fuse(&bm25, &[], &vectors, &options(), 10);
        assert_eq!(fused.len(), 2);
        // /a.pdf ranks top: BM25 rank 1 + vector rank 1 beats vector rank 2.
        assert_eq!(fused[0].path, "/a.pdf");
        assert!(!fused[0].semantic_only);
        let b = fused.iter().find(|f| f.path == "/b.pdf").unwrap();
        assert!(b.semantic_only);
        // The BM25 page wins for the shared file.
        assert_eq!(fused[0].page, 0);
    }

    #[test]
    fn the_vector_side_can_outrank_the_bm25_tail() {
        // With the head unprotected, BM25's second file — the vector's top
        // hit — reorders above BM25's first at a strong vector weight.
        let bm25 = vec![hit("/a.pdf", 0, 8.0), hit("/b.pdf", 0, 1.0)];
        let vectors = vec![hit("/b.pdf", 0, 0.95), hit("/c.pdf", 0, 0.9)];
        let mut options = options();
        options.bm25_head = 0;
        options.vec_weight = 3.0;
        let fused = fuse(&bm25, &[], &vectors, &options, 10);
        assert_eq!(fused[0].path, "/b.pdf");
        // The shared file also gets the full vector boost; the vector-only
        // file is damped (vec_only_factor), so the BM25 rank-1 edges it out.
        assert_eq!(fused[1].path, "/a.pdf");
        assert_eq!(fused[2].path, "/c.pdf");
        assert!(fused[2].semantic_only);
    }

    #[test]
    fn the_protected_head_keeps_its_exact_bm25_order() {
        let bm25 = vec![hit("/a.pdf", 0, 8.0), hit("/b.pdf", 0, 1.0)];
        // The vector strongly prefers /b.pdf, which sits inside the head:
        // it must not be promoted over /a.pdf.
        let vectors = vec![hit("/b.pdf", 0, 0.95)];
        let mut options = options();
        options.bm25_head = 1;
        options.vec_weight = 3.0;
        let fused = fuse(&bm25, &[], &vectors, &options, 10);
        assert_eq!(fused[0].path, "/a.pdf");
        assert_eq!(fused[0].page, 0);
        assert_eq!(fused[1].path, "/b.pdf");
    }

    #[test]
    fn a_weak_head_hit_loses_its_protection() {
        // /common.pdf matched one common word of the query (low coverage),
        // /lease.pdf is BM25 rank 3 but the vectors' clear favourite.
        let bm25 = vec![hit("/common.pdf", 0, 9.0), hit("/exact.pdf", 0, 8.0), hit("/lease.pdf", 0, 2.0)];
        let coverage = [0.2, 1.0, 0.3];
        let vectors = vec![hit("/lease.pdf", 0, 0.8), hit("/other.pdf", 0, 0.7)];
        let fused = fuse(&bm25, &coverage, &vectors, &options(), 10);
        // The fully covered hit stays first; the weak one falls below the
        // meaning match.
        assert_eq!(fused[0].path, "/exact.pdf");
        assert_eq!(fused[1].path, "/lease.pdf");
        let common = fused.iter().position(|f| f.path == "/common.pdf").unwrap();
        assert!(common > 1);
    }

    #[test]
    fn idf_coverage_weighs_rare_and_unknown_terms() {
        // 100 files: "ücret" is everywhere, "kira" rare, "oturduğum" unknown.
        let weights = term_weights(&[90, 3, 0], 100);
        assert!(weights[2] > weights[1] && weights[1] > weights[0]);
        assert!(idf_coverage(&weights, &[true, false, false]) < 0.1);
        assert!(idf_coverage(&weights, &[true, true, true]) > 0.99);
        // A single known term query is fully covered by any page with it.
        let one = term_weights(&[5], 100);
        assert!(idf_coverage(&one, &[true]) > 0.99);
    }

    #[test]
    fn vector_search_respects_the_filter_and_floor() {
        let index = VectorIndex::empty();
        index.set("/a.pdf", FileKind::Pdf, 100, &[(0, vec![1.0, 0.0])]);
        index.set("/b.docx", FileKind::Word, 100, &[(1, vec![0.0, 1.0])]);
        index.set("/old.pdf", FileKind::Pdf, 10, &[(0, vec![1.0, 0.0])]);
        let filter = SearchFilter { kinds: &[FileKind::Pdf], modified_after: Some(50), modified_before: None };
        let hits = index.search(&[1.0, 0.0], &filter, &options());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/a.pdf");
        assert_eq!(hits[0].page, 0);
        // Orthogonal queries score 0 — below the floor, nothing survives.
        let hits = index.search(&[0.0, 1.0], &filter, &options());
        assert!(hits.is_empty());
    }

    #[test]
    fn shorter_dims_use_matryoshka_truncation() {
        let index = VectorIndex::empty();
        index.set("/a.pdf", FileKind::Pdf, 100, &[(0, vec![1.0, 0.0])]);
        index.set("/b.pdf", FileKind::Pdf, 100, &[(0, vec![0.9, 0.436])]); // ≈60° at 2d, far at 1d? second dim only
        let filter = filter();
        let mut options = options();
        options.dim = 1;
        options.min_cosine = 0.0;
        let hits = index.search(&[1.0, 0.0], &filter, &options);
        // At 1d both docs project to (1.0) after renorm → tie, path order.
        assert_eq!(hits[0].path, "/a.pdf");
        assert!((hits[0].score - 1.0).abs() < 1e-6);
        assert!((hits[1].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn a_truncated_query_is_renormalized_before_cosine() {
        let index = VectorIndex::empty();
        index.set("/a.pdf", FileKind::Pdf, 100, &[(0, vec![1.0, 0.0])]);
        let filter = filter();
        let mut options = options();
        options.dim = 1;
        options.min_cosine = 0.5;
        // The dropped Matryoshka tail carried norm 0.5: without renormalizing
        // the truncated query the aligned document scores 0.8 and is dropped
        // by the floor instead of the true cosine 1.0.
        let hits = index.search(&[0.8, 0.5], &filter, &options);
        assert_eq!(hits.len(), 1);
        assert!((hits[0].score - 1.0).abs() < 1e-6);
    }

    #[test]
    fn set_replaces_and_remove_drops() {
        let index = VectorIndex::empty();
        index.set("/a.pdf", FileKind::Pdf, 100, &[(0, vec![1.0, 0.0])]);
        index.set("/a.pdf", FileKind::Pdf, 101, &[(0, vec![0.0, 1.0]), (1, vec![0.0, 1.0])]);
        assert_eq!(index.chunk_count(), 2);
        index.remove("/a.pdf");
        assert!(index.is_empty());
        // Empty chunk list also removes.
        index.set("/b.pdf", FileKind::Pdf, 100, &[(0, vec![1.0, 0.0])]);
        index.set("/b.pdf", FileKind::Pdf, 100, &[]);
        assert!(index.is_empty());
    }
}
