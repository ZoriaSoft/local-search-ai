//! Tantivy BM25 index with Turkish normalization.
//!
//! One Tantivy document per page. `text` uses Turkish lowercase tokens; `folded`
//! adds ASCII-folded tokens so `sozlesme` finds `sözleşme`. Results are grouped
//! per file (best page wins), matching the lab's per-document evaluation.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::OnceLock;

use parking_lot::RwLock;

use anyhow::{Context, Result};
use serde::Serialize;
use tantivy::collector::{Collector, SegmentCollector, TopDocs};
use tantivy::columnar::StrColumn;
use tantivy::query::{BooleanQuery, BoostQuery, FuzzyTermQuery, Occur, PhraseQuery, Query, RegexQuery, TermQuery};
use tantivy::schema::{
    Field, IndexRecordOption, NumericOptions, Schema, TextFieldIndexing, TextOptions, Value,
    FAST, STORED, STRING,
};
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};
use tantivy::{
    doc, DocId, Index, IndexReader, IndexWriter, ReloadPolicy, Score, SegmentOrdinal, SegmentReader,
    TantivyDocument, Term,
};

use crate::model::{ExtractedDocument, FileKind};

/// How long `SearchIndex::writer` waits out a transiently held writer lock.
const WRITER_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(2);
use crate::turkish::{ascii_fold, token_spans, tokenize, turkish_lower};

/// Bump whenever the schema or tokenization changes. An index written by
/// another version is discarded on open and rebuilt from the catalog, which
/// stores every page's text — no file is re-read and nothing is re-OCR'd.
/// 1: `path` became a fast field (exact distinct-file counts).
/// 2: `stem` field (Snowball Turkish stems, ASCII-folded).
pub const SCHEMA_VERSION: u32 = 2;
const VERSION_FILE: &str = "arsiv-schema-version";
const PATH_FIELD: &str = "path";

const TURKISH: &str = "turkish";
const TURKISH_FOLDED: &str = "turkish_folded";
const TURKISH_STEM: &str = "turkish_stem";
/// Folded-field matches score below exact Turkish matches.
const FOLDED_BOOST: f32 = 0.6;
/// Query-term stems stripped of one Turkish suffix rank below folded matches.
const STEM_BOOST: f32 = 0.4;
/// Terms continuing a query term (Turkish suffixes attach at the end) rank low.
const PREFIX_BOOST: f32 = 0.25;
/// One-edit typo variants rank below prefix matches.
const TYPO_BOOST: f32 = 0.15;
/// Prefix matching is unsafe below 4 letters (`ev` would eat `evrak`).
const MIN_PREFIX_LEN: usize = 4;
/// A stripped stem must keep at least this many letters to be trusted.
const MIN_STEM_LEN: usize = 4;

/// Folded query-side suffixes, longest first so `si` wins over `i`.
/// Vowel-harmony alternates collapse under folding (ı→i, ü→u), so one
/// spelling per shape suffices. Stems are approximate by design; ranking
/// keeps exact matches on top and the evaluation set decides the cut.
const SUFFIXES: &[&str] = &[
    "lariniz", "leriniz", "larimiz", "lerimiz", "lardan", "lerden", "larinda", "lerinde",
    "sindan", "sinden", "sinda", "sinde", "sina", "sine", "sini", "sinin",
    "mizdan", "mizden", "mizda", "mizde", "miza", "mize", "mizi", "mizin",
    "nizdan", "nizden", "nizda", "nizde", "niza", "nize", "nizi", "nizin",
    "umuz", "unuz", "iniz", "imiz", "inin", "unun",
    "lara", "lere", "lari", "leri", "larin", "lerin", "larim", "lerim",
    "ndan", "nden", "nda", "nde", "nin", "nun",
    "dan", "den", "tan", "ten", "yla", "yle",
    "ina", "ine", "una", "une", "cen",
    "li", "lu", "siz", "suz",
    "la", "le", "ya", "ye", "na", "ne",
    "si", "su", "sa", "se", "ci", "cu",
    "in", "un",
    "a", "e", "i", "u",
];

/// `sozlesmesi` -> `sozlesme`: strip at most one suffix, keeping a real stem.
/// Operates on the folded form so ı/i/u/ü alternations collapse.
fn stem_variants(folded: &str) -> Vec<String> {
    let mut out = Vec::new();
    for suffix in SUFFIXES {
        if let Some(head) = folded.strip_suffix(suffix) {
            let head = head.trim_end_matches(['\'', '’']);
            if head.chars().count() >= MIN_STEM_LEN && !out.iter().any(|v| v == head) {
                out.push(head.to_string());
            }
        }
    }
    out
}

/// English inflections stripped query-side, so an English query meets the
/// base form its documents use: `invoices`→`invoice`, `companies`→`company`,
/// `approved`→`approve`/`approv`, `processing`→`process`. Like the Turkish
/// list it runs on the folded term, keeps at least `MIN_STEM_LEN` letters
/// and ranks as a stem; Turkish words seldom end this way (`-us`/`-is`/`-ss`
/// endings such as `otobus`, `giris` are left alone).
fn english_variants(folded: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |head: String| {
        if head.chars().count() >= MIN_STEM_LEN && head != folded && !out.contains(&head) {
            out.push(head);
        }
    };
    if let Some(head) = folded.strip_suffix("ies") {
        push(format!("{head}y"));
    }
    if let Some(head) = folded.strip_suffix("es") {
        push(head.to_string());
    }
    if let Some(head) = folded.strip_suffix('s') {
        if !["ss", "us", "is"].iter().any(|end| folded.ends_with(end)) {
            push(head.to_string());
        }
    }
    if let Some(head) = folded.strip_suffix("ed") {
        push(format!("{head}e"));
        push(head.to_string());
    }
    if let Some(head) = folded.strip_suffix("ing") {
        push(head.to_string());
    }
    out
}

/// Folded term form used by index and query: apostrophes dropped so
/// `Ankara'da` and `ankarada` meet in the same term.
fn folded_term(term: &str) -> String {
    ascii_fold(term).replace(['\'', '’'], "")
}

/// Snowball Turkish stem of a Turkish-lowercased term, ASCII-folded so
/// diacritic-free queries meet it. After an apostrophe everything is
/// inflection (`Ankara'nın`), so only the head is stemmed — exactly like the
/// bare query `ankara`.
pub fn stem_term(lower: &str) -> String {
    let head = lower.split(['\'', '’']).next().unwrap_or(lower);
    let stemmer = rust_stemmers::Stemmer::create(rust_stemmers::Algorithm::Turkish);
    folded_term(&stemmer.stem(head))
}

/// Query-time matching switches. The defaults are the measured production
/// configuration (measured on the lab corpus); eval flips them for A/B runs.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchOptions {
    /// Prefix continuation, single-suffix stems and one-edit typos.
    pub expand: bool,
    /// The hand-written single-suffix stem clauses (part of `expand`).
    pub legacy_stems: bool,
    /// Boost of the Snowball `stem` field clause; 0 disables it.
    pub stem_boost: f32,
    /// Bonus for multi-word queries whose words all occur, in order, within
    /// `phrase_slop` positions of each other; 0 disables it.
    pub phrase_boost: f32,
    pub phrase_slop: u32,
    /// The same bonus per adjacent word pair, so long queries ("firma adı +
    /// konu") still earn partial credit; 0 disables it.
    pub pair_boost: f32,
    /// Boost of office synonyms (`synonyms.txt`) added to the query; 0 = off.
    pub synonym_boost: f32,
}

/// Lab-measured (2026-10-14): each value is the best of
/// its A/B sweep that lowered no aggregate metric.
impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            expand: true,
            legacy_stems: true,
            // Tiny on purpose: the clause is for recall (inflections the
            // suffix list misses); larger boosts reorder exact hits.
            stem_boost: 0.02,
            phrase_boost: 1.0,
            phrase_slop: 1,
            pair_boost: 0.4,
            synonym_boost: 0.8,
        }
    }
}

/// Hand-written office synonym groups (Turkish-lowercased), parsed once.
fn synonym_groups() -> &'static [Vec<String>] {
    static GROUPS: OnceLock<Vec<Vec<String>>> = OnceLock::new();
    GROUPS.get_or_init(|| {
        include_str!("synonyms.txt")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| line.split(',').map(|w| turkish_lower(w.trim())).filter(|w| !w.is_empty()).collect())
            .filter(|group: &Vec<String>| group.len() > 1)
            .collect()
    })
}

/// The other members of every synonym group containing `term`. Inflected
/// queries (`faturası`, `faturaları`) are reduced with the single-suffix
/// stripper; Snowball stems are too irregular for lookup (`fatura`→`fatur`
/// but `faturaları`→`fatura`).
pub fn synonyms_of(term: &str) -> Vec<String> {
    let folded = folded_term(term);
    let mut forms = stem_variants(&folded);
    forms.push(folded);
    let mut out = Vec::new();
    for group in synonym_groups() {
        let hit = group.iter().any(|w| forms.contains(&folded_term(w)));
        if hit {
            for word in group {
                if !forms.contains(&folded_term(word)) && !out.contains(word) {
                    out.push(word.clone());
                }
            }
        }
    }
    out
}

/// Quote-aware segments of a query: `(unquoted, quoted)`, with straight and
/// curly double quotes normalised to `"` and a trailing unclosed quote treated
/// as unquoted text. Quoted parts are required phrases (`quoted_phrases`);
/// unquoted parts can still carry exact requirements of their own
/// (`digit_part_clauses`).
fn quote_segments(query: &str) -> (Vec<String>, Vec<String>) {
    let normalized: String = query.chars().map(|c| if matches!(c, '“' | '”' | '„' | '«' | '»') { '"' } else { c }).collect();
    let parts: Vec<&str> = normalized.split('"').collect();
    // Odd-indexed parts are inside quotes; a trailing unclosed one is not.
    let closed = if parts.len() % 2 == 0 { parts.len() - 1 } else { parts.len() };
    let mut unquoted: Vec<String> = parts[..closed].iter().step_by(2).map(|s| s.to_string()).collect();
    if closed < parts.len() {
        unquoted.push(parts[closed].to_string());
    }
    let quoted = parts[..closed].iter().skip(1).step_by(2).map(|s| s.to_string()).collect();
    (unquoted, quoted)
}

/// Quoted parts of a query (`"kira sözleşmesi"`): each must occur as an exact
/// phrase (diacritic-insensitive, apostrophes folded). Straight and curly
/// double quotes are accepted; an unclosed quote is ignored.
pub fn quoted_phrases(query: &str) -> Vec<Vec<String>> {
    quote_segments(query)
        .1
        .into_iter()
        .map(|part| tokenize(&part).into_iter().map(|(s, e)| folded_term(&turkish_lower(&part[s..e]))).collect::<Vec<_>>())
        .filter(|tokens: &Vec<String>| !tokens.is_empty())
        .collect()
}

/// Required clauses for all-digit space-separated query parts (amounts
/// `8.158,00`, dates `13.11.2024`, document numbers `2031/4417`, plate
/// numbers `37`/`955`): the default tokenizer splits such a part into
/// pieces (`9.731,45` → `9 731 45`) that BM25 then matches independently,
/// so a page holding the pieces anywhere — never adjacent — ranked as a
/// hit. A numeric part must instead occur the way the user wrote it:
/// several digits as a slop-0 phrase on `folded` (any separator, no
/// interleaved word), a single one as a term. Letter parts next to them
/// (`TL`, `TEST`) stay optional `Should`s.
///
/// Parts whose sub-tokens include letters (`ÖR-CAHNFY-27454`,
/// `TC-2024-8871`) get no clause: the lab measured a strict phrase here
/// costs numeric recall (OCR corrupts code letters, `OGĞRDN`→`OGĞRODN`,
/// `O`→`0`), and a required digit run alone still loses codes OCR broke
/// numerically (`66337`→`66VT`). Their rare letter tokens already
/// discriminate, so they keep the plain OR.
/// The boost is 0: the clause only filters which pages may hit, it does
/// not re-score the ones that do.
fn digit_part_clauses(query: &str, folded: Field) -> Vec<Box<dyn Query>> {
    let mut clauses: Vec<Box<dyn Query>> = Vec::new();
    for segment in quote_segments(query).0 {
        for part in segment.split_whitespace() {
            if !part.bytes().any(|b| b.is_ascii_digit()) {
                continue;
            }
            let terms: Vec<String> = tokenize(part)
                .into_iter()
                .map(|(s, e)| folded_term(&turkish_lower(&part[s..e])))
                .collect();
            // Only parts made of digits alone: a letter anywhere in the
            // part makes it a code, which must not be pinned (see doc).
            if terms.is_empty() || !terms.iter().all(|t| t.bytes().all(|b| b.is_ascii_digit())) {
                continue;
            }
            let clause: Box<dyn Query> = match terms.as_slice() {
                [] => continue,
                [term] => Box::new(TermQuery::new(Term::from_field_text(folded, term), IndexRecordOption::Basic)),
                _ => Box::new(PhraseQuery::new(terms.iter().map(|t| Term::from_field_text(folded, t)).collect())),
            };
            clauses.push(Box::new(BoostQuery::new(clause, 0.0)));
        }
    }
    clauses
}

/// Folded query tokens in order, duplicates kept (for proximity).
fn folded_sequence(query: &str) -> Vec<String> {
    tokenize(query).into_iter().map(|(s, e)| folded_term(&turkish_lower(&query[s..e]))).collect()
}

/// Escapes the folded term for use as a regex literal (terms are lowercase
/// alphanumerics, but `+`/`(`-style chars could sneak through `is_word`).
fn regex_syntax_escape(term: &str) -> String {
    term.chars()
        .flat_map(|c| {
            if c.is_alphanumeric() {
                vec![c]
            } else {
                vec!['\\', c]
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
enum TokenForm {
    Lower,
    Folded,
    Stem,
}

#[derive(Clone)]
struct TurkishTokenizer {
    form: TokenForm,
}

struct TurkishStream {
    tokens: Vec<Token>,
    index: usize,
}

impl Tokenizer for TurkishTokenizer {
    type TokenStream<'a> = TurkishStream;
    fn token_stream<'a>(&'a mut self, text: &'a str) -> TurkishStream {
        let tokens = tokenize(text)
            .into_iter()
            .enumerate()
            .map(|(position, (start, end))| {
                let lower = turkish_lower(&text[start..end]);
                Token {
                    offset_from: start,
                    offset_to: end,
                    position,
                    text: match self.form {
                        TokenForm::Lower => lower,
                        TokenForm::Folded => folded_term(&lower),
                        TokenForm::Stem => stem_term(&lower),
                    },
                    position_length: 1,
                }
            })
            .collect();
        TurkishStream { tokens, index: 0 }
    }
}

impl TokenStream for TurkishStream {
    fn advance(&mut self) -> bool {
        self.index += 1;
        self.index <= self.tokens.len()
    }
    fn token(&self) -> &Token {
        &self.tokens[self.index - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.index - 1]
    }
}

/// Query terms exactly as the indexers produce them.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for (s, e) in tokenize(query) {
        let term = turkish_lower(&query[s..e]);
        if !seen.contains(&term) {
            seen.push(term);
        }
    }
    seen
}

#[derive(Clone, Copy)]
struct Fields {
    path: Field,
    page: Field,
    kind: Field,
    modified: Field,
    text: Field,
    folded: Field,
    stem: Field,
}

pub struct SearchIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
    options: RwLock<SearchOptions>,
    /// The on-disk index was from another schema version and was discarded.
    discarded_stale: bool,
}

/// Ranked best-page hits for the requested window plus the exact number of
/// distinct files matching the query and filters.
#[derive(Clone, Debug)]
pub struct SearchResults {
    pub hits: Vec<PageHit>,
    pub total_files: usize,
    /// Every distinct file the query matched (not just the window) — the
    /// hybrid fusion uses it to tell vector-only hits from buried BM25 ones.
    pub matched_paths: HashSet<String>,
}

/// Collects the distinct `path` values of matching pages (one file has many
/// page documents, so a plain Count over-counts).
struct DistinctPaths;

struct DistinctPathsInSegment {
    column: Option<StrColumn>,
    ords: HashSet<u64>,
}

impl Collector for DistinctPaths {
    type Fruit = HashSet<String>;
    type Child = DistinctPathsInSegment;

    fn for_segment(&self, _: SegmentOrdinal, segment: &SegmentReader) -> tantivy::Result<Self::Child> {
        Ok(DistinctPathsInSegment { column: segment.fast_fields().str(PATH_FIELD)?, ords: HashSet::new() })
    }

    fn requires_scoring(&self) -> bool {
        false
    }

    fn merge_fruits(&self, fruits: Vec<HashSet<String>>) -> tantivy::Result<HashSet<String>> {
        Ok(fruits.into_iter().flatten().collect())
    }
}

impl SegmentCollector for DistinctPathsInSegment {
    type Fruit = HashSet<String>;

    fn collect(&mut self, doc: DocId, _: Score) {
        if let Some(column) = &self.column {
            self.ords.extend(column.term_ords(doc));
        }
    }

    fn harvest(self) -> HashSet<String> {
        let Some(column) = self.column else { return HashSet::new() };
        let mut out = HashSet::with_capacity(self.ords.len());
        for ord in self.ords {
            let mut path = String::new();
            if column.ord_to_str(ord, &mut path).unwrap_or(false) {
                out.insert(path);
            }
        }
        out
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageHit {
    pub path: String,
    pub page: usize,
    pub score: f32,
}

#[derive(Clone, Debug)]
pub struct SearchFilter<'a> {
    pub kinds: &'a [FileKind],
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
}

impl SearchIndex {
    pub fn open(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        let version = std::fs::read_to_string(dir.join(VERSION_FILE))
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok());
        let discarded_stale = dir.join("meta.json").exists() && version != Some(SCHEMA_VERSION);
        if discarded_stale {
            for entry in std::fs::read_dir(dir)? {
                let path = entry?.path();
                if path.is_dir() {
                    std::fs::remove_dir_all(&path)?;
                } else {
                    std::fs::remove_file(&path)?;
                }
            }
        }
        let text_options = |tokenizer: &str| {
            TextOptions::default().set_indexing_options(
                TextFieldIndexing::default()
                    .set_tokenizer(tokenizer)
                    .set_index_option(IndexRecordOption::WithFreqsAndPositions),
            )
        };
        let mut builder = Schema::builder();
        let fields = Fields {
            path: builder.add_text_field(PATH_FIELD, STRING | STORED | FAST),
            page: builder.add_u64_field("page", STORED),
            kind: builder.add_text_field("kind", STRING | STORED),
            modified: builder.add_i64_field(
                "modified",
                NumericOptions::default().set_indexed().set_fast().set_stored(),
            ),
            text: builder.add_text_field("text", text_options(TURKISH)),
            folded: builder.add_text_field("folded", text_options(TURKISH_FOLDED)),
            stem: builder.add_text_field("stem", text_options(TURKISH_STEM)),
        };
        let schema = builder.build();
        let directory = tantivy::directory::MmapDirectory::open(dir)?;
        let index = Index::open_or_create(directory, schema).context("Cannot open search index")?;
        index.tokenizers().register(TURKISH, TurkishTokenizer { form: TokenForm::Lower });
        index.tokenizers().register(TURKISH_FOLDED, TurkishTokenizer { form: TokenForm::Folded });
        index.tokenizers().register(TURKISH_STEM, TurkishTokenizer { form: TokenForm::Stem });
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()?;
        std::fs::write(dir.join(VERSION_FILE), SCHEMA_VERSION.to_string())?;
        Ok(Self { index, reader, fields, options: RwLock::new(SearchOptions::default()), discarded_stale })
    }

    /// True when `open` threw away an index from another schema version; the
    /// caller must re-add every document (the engine does so from the catalog).
    pub fn discarded_stale(&self) -> bool {
        self.discarded_stale
    }

    /// The single index writer. Tantivy's writer lock is an `flock` on
    /// `.tantivy-writer.lock`; on Unix a child process being spawned at that
    /// moment (Tesseract, any `Command`) holds a copy of the descriptor until
    /// its `exec`, so the lock can outlive the dropped writer for a few
    /// milliseconds. `LockBusy` is therefore retried for up to 2 s before it
    /// is reported; a writer really held elsewhere still fails.
    pub fn writer(&self) -> Result<IndexWriter> {
        let deadline = std::time::Instant::now() + WRITER_LOCK_WAIT;
        loop {
            match self.index.writer(100_000_000) {
                Err(tantivy::TantivyError::LockFailure(tantivy::directory::error::LockError::LockBusy, _))
                    if std::time::Instant::now() < deadline =>
                {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                result => return Ok(result?),
            }
        }
    }

    pub fn reload(&self) -> Result<()> {
        Ok(self.reader.reload()?)
    }

    pub fn delete_path(&self, writer: &IndexWriter, path: &str) {
        writer.delete_term(Term::from_field_text(self.fields.path, path));
    }

    pub fn add(&self, writer: &IndexWriter, path: &str, modified: i64, document: &ExtractedDocument) -> Result<()> {
        self.delete_path(writer, path);
        // Failed pages carry no text; the catalog keeps them for the UI.
        for page in document.pages.iter().filter(|p| !p.is_failed()) {
            writer.add_document(doc!(
                self.fields.path => path,
                self.fields.page => page.index as u64,
                self.fields.kind => document.kind.as_str(),
                self.fields.modified => modified,
                self.fields.text => page.text.as_str(),
                self.fields.folded => page.text.as_str(),
                self.fields.stem => page.text.as_str(),
            ))?;
        }
        Ok(())
    }

    pub fn page_count(&self) -> u64 {
        self.reader.searcher().num_docs()
    }
    /// Enables/disables inflection expansion (stem + prefix clauses).
    pub fn set_expansion(&self, enabled: bool) {
        self.options.write().expand = enabled;
    }

    pub fn options(&self) -> SearchOptions {
        *self.options.read()
    }

    pub fn set_options(&self, options: SearchOptions) {
        *self.options.write() = options;
    }

    /// BM25 OR-query over Turkish + folded fields; best page per file, ranked.
    /// With expansion on, each query term also matches its single-suffix stem
    /// (`sözleşmesi`→`sözleşme`) and indexed terms that continue it
    /// (`sözleşme`→`sözleşmesi/sözleşmenin/…`), all on the folded field.
    pub fn search(&self, query: &str, filter: &SearchFilter, max_files: usize) -> Result<Vec<PageHit>> {
        Ok(self.search_counted(query, filter, max_files)?.hits)
    }

    /// A word and its inflections on the folded field: strict prefix for
    /// ≥4 letters (Turkish suffixes attach at the end), exact otherwise.
    fn word_forms(&self, folded: &str) -> Box<dyn Query> {
        if folded.chars().count() >= MIN_PREFIX_LEN {
            if let Ok(query) = RegexQuery::from_pattern(&format!("{}.*", regex_syntax_escape(folded)), self.fields.folded) {
                return Box::new(query);
            }
        }
        Box::new(TermQuery::new(Term::from_field_text(self.fields.folded, folded), IndexRecordOption::WithFreqs))
    }

    /// The clauses shared by ranked search and exact file counts. Keeping
    /// them together makes coverage count the same matches as normal search.
    fn matching_query(&self, query: &str, filter: &SearchFilter) -> Option<BooleanQuery> {
        let terms = query_terms(query);
        if terms.is_empty() {
            return None;
        }
        let options = self.options();
        let mut clauses: Vec<(Occur, Box<dyn Query>)> = Vec::new();
        for term in &terms {
            clauses.push((
                Occur::Should,
                Box::new(TermQuery::new(
                    Term::from_field_text(self.fields.text, term),
                    IndexRecordOption::WithFreqs,
                )),
            ));
            let folded = folded_term(term);
            clauses.push((
                Occur::Should,
                Box::new(BoostQuery::new(
                    Box::new(TermQuery::new(
                        Term::from_field_text(self.fields.folded, &folded),
                        IndexRecordOption::WithFreqs,
                    )),
                    FOLDED_BOOST,
                )),
            ));
            if options.stem_boost > 0.0 {
                clauses.push((
                    Occur::Should,
                    Box::new(BoostQuery::new(
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.stem, &stem_term(term)),
                            IndexRecordOption::WithFreqs,
                        )),
                        options.stem_boost,
                    )),
                ));
            }
            if options.expand {
                // Strict prefix (edit-free): `kira` reaches `kirası` but never `kiraz`.
                // Applied to the term AND each stem so `sözleşmesi` (stem
                // `sözleşme`) still finds `sözleşmenin`.
                let mut bases = vec![folded.clone()];
                if options.legacy_stems {
                    for variant in stem_variants(&folded).into_iter().chain(english_variants(&folded)) {
                        if !bases.contains(&variant) {
                            bases.push(variant);
                        }
                    }
                }
                for base in bases {
                    clauses.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(
                            Box::new(TermQuery::new(
                                Term::from_field_text(self.fields.folded, &base),
                                IndexRecordOption::WithFreqs,
                            )),
                            STEM_BOOST,
                        )),
                    ));
                    if base.chars().count() >= MIN_PREFIX_LEN {
                        let pattern = format!("{}.*", regex_syntax_escape(&base));
                        if let Ok(query) = RegexQuery::from_pattern(&pattern, self.fields.folded) {
                            clauses.push((
                                Occur::Should,
                                Box::new(BoostQuery::new(Box::new(query), PREFIX_BOOST)),
                            ));
                        }
                    }
                }
                // One-edit typo tolerance on the folded term, ranked lowest.
                if folded.chars().count() >= 5 {
                    clauses.push((
                        Occur::Should,
                        Box::new(BoostQuery::new(
                            Box::new(FuzzyTermQuery::new(
                                Term::from_field_text(self.fields.folded, &folded),
                                1,
                                true,
                            )),
                            TYPO_BOOST,
                        )),
                    ));
                }
            }
        }
        if options.synonym_boost > 0.0 {
            let mut added: Vec<String> = Vec::new();
            for term in &terms {
                for synonym in synonyms_of(term) {
                    let folded = folded_term(&synonym);
                    if terms.iter().any(|t| folded_term(t) == folded) || added.contains(&folded) {
                        continue;
                    }
                    clauses.push((Occur::Should, Box::new(BoostQuery::new(self.word_forms(&folded), options.synonym_boost))));
                    added.push(folded);
                }
            }
        }
        let sequence = folded_sequence(query);
        let mut proximity: Vec<(&[String], f32)> = Vec::new();
        if options.phrase_boost > 0.0 && sequence.len() >= 2 {
            proximity.push((&sequence[..], options.phrase_boost));
        }
        if options.pair_boost > 0.0 && sequence.len() >= 3 {
            proximity.extend(sequence.windows(2).map(|pair| (pair, options.pair_boost)));
        }
        for (words, boost) in proximity {
            let terms = words.iter().map(|t| Term::from_field_text(self.fields.folded, t)).collect();
            let mut phrase = PhraseQuery::new(terms);
            phrase.set_slop(options.phrase_slop);
            clauses.push((Occur::Should, Box::new(BoostQuery::new(Box::new(phrase), boost))));
        }
        let text_query: Box<dyn Query> = Box::new(BooleanQuery::new(clauses));
        let mut must: Vec<(Occur, Box<dyn Query>)> = vec![(Occur::Must, text_query)];
        for phrase in quoted_phrases(query) {
            let mut terms: Vec<Term> = phrase.iter().map(|t| Term::from_field_text(self.fields.folded, t)).collect();
            let required: Box<dyn Query> = if terms.len() == 1 {
                Box::new(TermQuery::new(terms.remove(0), IndexRecordOption::Basic))
            } else {
                Box::new(PhraseQuery::new(terms))
            };
            must.push((Occur::Must, required));
        }
        for clause in digit_part_clauses(query, self.fields.folded) {
            must.push((Occur::Must, clause));
        }
        if !filter.kinds.is_empty() {
            let kinds = filter
                .kinds
                .iter()
                .map(|k| {
                    (
                        Occur::Should,
                        Box::new(TermQuery::new(
                            Term::from_field_text(self.fields.kind, k.as_str()),
                            IndexRecordOption::Basic,
                        )) as Box<dyn Query>,
                    )
                })
                .collect();
            must.push((Occur::Must, Box::new(BooleanQuery::new(kinds))));
        }
        if filter.modified_after.is_some() || filter.modified_before.is_some() {
            let lower = filter.modified_after.unwrap_or(i64::MIN);
            let upper = filter.modified_before.unwrap_or(i64::MAX);
            must.push((
                Occur::Must,
                Box::new(tantivy::query::RangeQuery::new(
                    std::ops::Bound::Included(Term::from_field_i64(self.fields.modified, lower)),
                    std::ops::Bound::Included(Term::from_field_i64(self.fields.modified, upper)),
                )),
            ));
        }
        Some(BooleanQuery::new(must))
    }

    /// Exact distinct-file count with the same query semantics as search.
    /// Coverage needs membership only: no BM25 scores, top-page window, or
    /// stored page documents need to be computed or loaded here.
    pub fn count_files(&self, query: &str, filter: &SearchFilter) -> Result<usize> {
        let Some(query) = self.matching_query(query, filter) else {
            return Ok(0);
        };
        Ok(self.reader.searcher().search(&query, &DistinctPaths)?.len())
    }

    /// `search` plus the exact number of matching files.
    pub fn search_counted(&self, query: &str, filter: &SearchFilter, max_files: usize) -> Result<SearchResults> {
        let Some(query) = self.matching_query(query, filter) else {
            return Ok(SearchResults { hits: Vec::new(), total_files: 0, matched_paths: HashSet::new() });
        };
        let searcher = self.reader.searcher();
        // Pages, not files, are scored; widen the window until enough files.
        // The distinct-file count rides along on the first pass.
        let mut window = (max_files * 8).max(64);
        let mut matched_paths: Option<HashSet<String>> = None;
        loop {
            let top = if matched_paths.is_none() {
                let (top, files) =
                    searcher.search(&query, &(TopDocs::with_limit(window).order_by_score(), DistinctPaths))?;
                matched_paths = Some(files);
                top
            } else {
                searcher.search(&query, &TopDocs::with_limit(window).order_by_score())?
            };
            let exhausted = top.len() < window;
            let mut best: HashMap<String, PageHit> = HashMap::new();
            let mut order = Vec::new();
            for (score, address) in top {
                let stored: TantivyDocument = searcher.doc(address)?;
                let path = stored
                    .get_first(self.fields.path)
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let page = stored.get_first(self.fields.page).and_then(|v| v.as_u64()).unwrap_or(0) as usize;
                if !best.contains_key(&path) {
                    order.push(path.clone());
                    best.insert(path.clone(), PageHit { path, page, score });
                }
            }
            if order.len() >= max_files || exhausted {
                order.truncate(max_files);
                let matched = matched_paths.unwrap_or_default();
                return Ok(SearchResults {
                    hits: order.into_iter().filter_map(|p| best.remove(&p)).collect(),
                    total_files: matched.len(),
                    matched_paths: matched,
                });
            }
            window *= 4;
        }
    }
}

/// Decides which tokens of a text a query matched, mirroring the search
/// clauses: exact, folded, single-suffix stem, a continuation of a ≥4-letter
/// query word, a shared Snowball stem, or an office synonym. Built once per
/// query; reused for every snippet and page.
pub struct Matcher {
    terms: Vec<String>,
    folded: Vec<String>,
    /// Folded forms that match exactly (suffix stems, short synonyms).
    exact: Vec<String>,
    /// Folded prefixes whose continuations match.
    prefixes: Vec<String>,
    snowball: Vec<String>,
    /// Each Snowball stem minus its last letter. Snowball only strips
    /// suffixes (and may soften the final consonant, `kitabı`→`kitap`), so a
    /// token can share a stem only if it starts with this; checking it first
    /// keeps the stemmer off almost every token of a page.
    snowball_heads: Vec<String>,
    /// Fast byte search for pages past `SNIPPET_SCAN_BYTES` (see `anchors`),
    /// built on first use: anchors of ≥3 bytes, then the shorter ones.
    /// Both `None`: the query has no usable anchor.
    locator: OnceLock<[Option<aho_corasick::AhoCorasick>; 2]>,
}

impl Matcher {
    pub fn new(terms: &[String], options: &SearchOptions) -> Self {
        let folded: Vec<String> = terms.iter().map(|t| folded_term(t)).collect();
        let mut exact = Vec::new();
        let mut prefixes = Vec::new();
        if options.expand {
            prefixes.extend(folded.iter().filter(|t| t.chars().count() >= MIN_PREFIX_LEN).cloned());
            if options.legacy_stems {
                exact.extend(folded.iter().flat_map(|t| stem_variants(t).into_iter().chain(english_variants(t))));
            }
        }
        if options.synonym_boost > 0.0 {
            for synonym in terms.iter().flat_map(|t| synonyms_of(t)) {
                let synonym = folded_term(&synonym);
                if synonym.chars().count() >= MIN_PREFIX_LEN {
                    prefixes.push(synonym);
                } else {
                    exact.push(synonym);
                }
            }
        }
        let snowball: Vec<String> =
            if options.stem_boost > 0.0 { terms.iter().map(|t| stem_term(t)).collect() } else { Vec::new() };
        // A digit is never a softened consonant, so a numeric stem is its own
        // head. Without this a one-digit term (`8` of `8.158,00`) has an
        // empty head and every token of a page goes through the stemmer —
        // the main cost of number queries on large log pages.
        let snowball_heads = snowball
            .iter()
            .map(|stem| {
                if !stem.is_empty() && stem.bytes().all(|b| b.is_ascii_digit()) {
                    stem.clone()
                } else {
                    stem.chars().take(stem.chars().count().saturating_sub(1)).collect()
                }
            })
            .collect();
        Self { terms: terms.to_vec(), folded, exact, prefixes, snowball, snowball_heads, locator: OnceLock::new() }
    }

    /// Byte strings that every matching token must contain (ASCII case
    /// aside). A token matches when its folded form equals, or starts with,
    /// one of the folded needles (`folded`, `exact`, `prefixes`,
    /// `snowball_heads`). Of each needle we keep its longest run of letters
    /// and digits that folding cannot produce from another character — every
    /// ASCII alphanumeric except `c g i o s u` (ç ğ ı/İ ö ş ü fold to them).
    /// Such a run appears verbatim in the raw page, so an Aho-Corasick scan
    /// finds every candidate without tokenizing the text. `None` when some
    /// needle has no such run (e.g. `su`): then nothing may be skipped.
    /// Known blind spot: an apostrophe inside the run (`ab'c` folds to `abc`)
    /// and `â/î/û` written for `a` — both rare inside query words.
    fn anchors(&self) -> Option<Vec<String>> {
        let mut anchors = Vec::new();
        let needles = self.folded.iter().chain(&self.exact).chain(&self.prefixes).chain(&self.snowball_heads);
        for needle in needles {
            let lower = needle.to_lowercase();
            let best = lower
                .split(|ch: char| !ch.is_ascii_alphanumeric() || "cgiosu".contains(ch))
                .max_by_key(|run| run.len())
                .unwrap_or("");
            if best.is_empty() {
                return None;
            }
            anchors.push(best.to_string());
        }
        anchors.sort();
        anchors.dedup();
        (!anchors.is_empty()).then_some(anchors)
    }

    /// Strong (≥3-byte) and weak anchor automata. A short anchor such as
    /// `00` occurs on every log line; searching the strong ones first over
    /// the whole remainder finds `158` of `8.158,00` at the end of a page
    /// before the weak pass spends its unit budget.
    fn locators(&self) -> &[Option<aho_corasick::AhoCorasick>; 2] {
        self.locator.get_or_init(|| {
            let Some(anchors) = (if self.terms.is_empty() { None } else { self.anchors() }) else {
                return [None, None];
            };
            let build = |strong: bool| {
                let set: Vec<&String> = anchors.iter().filter(|a| (a.len() >= 3) == strong).collect();
                if set.is_empty() {
                    return None;
                }
                aho_corasick::AhoCorasick::builder().ascii_case_insensitive(true).build(set).ok()
            };
            [build(true), build(false)]
        })
    }

    /// Matches in `text[from..]`, found by jumping between anchor hits
    /// instead of tokenizing: each hit's whitespace-delimited unit is matched
    /// exactly like `snippets` matches units, then the search resumes after
    /// it. At most `MAX_ANCHOR_UNITS` units are examined, so a very common
    /// anchor cannot turn this back into a full-page scan. Empty when the
    /// query has no anchors.
    fn ranges_from<'a>(&'a self, text: &'a str, from: usize) -> impl Iterator<Item = (usize, usize)> + 'a {
        let [strong, weak] = self.locators();
        self.anchored(text, from, strong.as_ref()).chain(self.anchored(text, from, weak.as_ref()))
    }

    fn anchored<'a>(
        &'a self,
        text: &'a str,
        from: usize,
        locator: Option<&'a aho_corasick::AhoCorasick>,
    ) -> impl Iterator<Item = (usize, usize)> + 'a {
        let mut position = from.min(text.len());
        let mut units = 0usize;
        let mut pending: std::vec::IntoIter<(usize, usize)> = Vec::new().into_iter();
        std::iter::from_fn(move || loop {
            if let Some(range) = pending.next() {
                return Some(range);
            }
            let locator = locator?;
            if units >= MAX_ANCHOR_UNITS || position >= text.len() {
                return None;
            }
            let found = locator.find(aho_corasick::Input::new(text).range(position..))?;
            let start = text[position..found.start()]
                .char_indices()
                .rev()
                .find(|(_, ch)| ch.is_whitespace())
                .map_or(position, |(i, ch)| position + i + ch.len_utf8());
            let end = text[found.end()..].find(char::is_whitespace).map_or(text.len(), |i| found.end() + i);
            units += 1;
            position = end;
            let unit = &text[start..end];
            pending = self.iter_ranges(unit).map(|(s, e)| (start + s, start + e)).collect::<Vec<_>>().into_iter();
        })
    }

    /// Highlights for a full page view. Pages up to `SNIPPET_SCAN_BYTES` get
    /// every match; larger pages get the matches of the first
    /// `SNIPPET_SCAN_BYTES` plus anchor-located ones after it, at most
    /// `MAX_PAGE_HIGHLIGHTS` — never a token pass over a multi-MB page.
    pub fn page_ranges(&self, text: &str) -> Vec<(usize, usize)> {
        if text.len() <= SNIPPET_SCAN_BYTES {
            return self.ranges(text);
        }
        let (prefix, resume) = self.prefix_ranges(text);
        let mut ranges: Vec<(usize, usize)> = prefix.take(MAX_PAGE_HIGHLIGHTS).collect();
        let mut seen: HashSet<(usize, usize)> = HashSet::new();
        for range in self.ranges_from(text, resume) {
            if ranges.len() >= MAX_PAGE_HIGHLIGHTS {
                break;
            }
            if seen.insert(range) {
                ranges.push(range);
            }
        }
        ranges.sort_unstable();
        ranges
    }

    /// Matches inside the first `SNIPPET_SCAN_BYTES` (whole units only; a
    /// unit cut by the budget is left out) and the offset where the cut unit
    /// starts, i.e. where `ranges_from` has to resume.
    fn prefix_ranges<'a>(&'a self, text: &'a str) -> (impl Iterator<Item = (usize, usize)> + 'a, usize) {
        let scan_end = floor_char(text, text.len().min(SNIPPET_SCAN_BYTES));
        let truncated = scan_end < text.len();
        let resume = if truncated && !text[scan_end..].starts_with(char::is_whitespace) {
            text[..scan_end].rfind(char::is_whitespace).map_or(0, |i| i + text[i..].chars().next().map_or(1, char::len_utf8))
        } else {
            scan_end
        };
        let mut position = 0;
        let ranges = text[..resume]
            .split_inclusive(char::is_whitespace)
            .map(move |part| {
                let offset = position;
                position += part.len();
                (offset, part)
            })
            .flat_map(move |(offset, part)| self.iter_ranges(part).map(move |(s, e)| (offset + s, offset + e)));
        (ranges, resume)
    }

    /// Byte ranges of matching tokens in `text` (full-page highlighting).
    pub fn ranges(&self, text: &str) -> Vec<(usize, usize)> {
        self.iter_ranges(text).collect()
    }

    /// Matching stays lazy: snippets can stop before visiting the next token.
    fn iter_ranges<'a>(&'a self, text: &'a str) -> impl Iterator<Item = (usize, usize)> + 'a {
        let input = self.scanned(text);
        token_spans(input).filter(move |&(start, end)| self.matches(&lower_for_match(&input[start..end])))
    }

    /// The part of `text` a token pass visits: nothing for a query without
    /// terms (an empty query must never walk a multi-MB page).
    fn scanned<'a>(&self, text: &'a str) -> &'a str {
        if self.terms.is_empty() {
            ""
        } else {
            text
        }
    }

    fn matches(&self, lower: &str) -> bool {
        if self.terms.iter().any(|t| t == lower) {
            return true;
        }
        // ASCII tokens without apostrophes are already folded. Most log
        // words/numbers therefore need no allocation on the rejected path.
        let folded = if lower.is_ascii() && !lower.contains('\'') {
            Cow::Borrowed(lower)
        } else {
            Cow::Owned(folded_term(lower))
        };
        self.folded.iter().any(|t| t == folded.as_ref())
            || self.exact.iter().any(|t| t == folded.as_ref())
            || self.prefixes.iter().any(|p| folded.starts_with(p.as_str()))
            || (self.snowball_heads.iter().any(|head| folded.starts_with(head.as_str()))
                && self.snowball.contains(&stem_term(lower)))
    }
}

/// Character ranges of query-term matches inside `text` (byte offsets), with
/// the production options. Prefer one `Matcher` when scanning many texts.
pub fn match_ranges(text: &str, terms: &[String]) -> Vec<(usize, usize)> {
    Matcher::new(terms, &SearchOptions::default()).ranges(text)
}

pub fn match_ranges_with(text: &str, terms: &[String], options: &SearchOptions) -> Vec<(usize, usize)> {
    Matcher::new(terms, options).ranges(text)
}

/// Convert a byte range in `text` to UTF-16 code-unit offsets for JavaScript.
pub fn utf16_range(text: &str, start: usize, end: usize) -> (usize, usize) {
    let a = text[..start].encode_utf16().count();
    (a, a + text[start..end].encode_utf16().count())
}

/// Hard limit for snippet matching and lead-text emptiness checks. The
/// catalog/full document view still keeps and highlights the complete page.
const SNIPPET_SCAN_BYTES: usize = 256 * 1024;
/// Anchor hits examined past `SNIPPET_SCAN_BYTES` (snippets and page view).
const MAX_ANCHOR_UNITS: usize = 20_000;
/// Highlights returned for one page larger than `SNIPPET_SCAN_BYTES`.
const MAX_PAGE_HIGHLIGHTS: usize = 2_000;

/// `utf16_range` for many ascending ranges in one pass over `text`.
pub fn utf16_ranges(text: &str, ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut out = Vec::with_capacity(ranges.len());
    let (mut byte, mut unit) = (0usize, 0usize);
    for &(start, end) in ranges {
        if start < byte {
            out.push(utf16_range(text, start, end));
            continue;
        }
        unit += text[byte..start].encode_utf16().count();
        let len = text[start..end].encode_utf16().count();
        out.push((unit, unit + len));
        unit += len;
        byte = end;
    }
    out
}

fn lower_for_match(token: &str) -> Cow<'_, str> {
    if token.is_ascii() {
        if !token.bytes().any(|b| b.is_ascii_uppercase()) {
            return Cow::Borrowed(token);
        }
        // Turkish uppercase I must become dotless ı, including in ASCII
        // input. Other ASCII capitals can use the cheaper lowercase path.
        if !token.contains('I') {
            return Cow::Owned(token.to_ascii_lowercase());
        }
    }
    Cow::Owned(turkish_lower(token))
}

/// The existing no-match fallback: byte-width prefix, whitespace compacted,
/// no highlights. Check only a bounded prefix for text, so whitespace-only
/// multi-megabyte pages also stay cheap.
pub fn lead_snippet(text: &str, width: usize) -> Vec<(String, Vec<(usize, usize)>)> {
    let scan_end = floor_char(text, text.len().min(SNIPPET_SCAN_BYTES));
    if text[..scan_end].trim().is_empty() {
        return Vec::new();
    }
    let end = ceil_char(text, width.min(scan_end));
    vec![(text[..end].split_whitespace().collect::<Vec<_>>().join(" "), Vec::new())]
}

/// Which single-term matchers (one per query term, as `Matcher::new(&[t])`)
/// occur in the first `SNIPPET_SCAN_BYTES` of `text`. One token pass,
/// stopping as soon as every term was seen — bounded on multi-MB pages.
pub fn terms_present(text: &str, matchers: &[Matcher]) -> Vec<bool> {
    let mut found = vec![false; matchers.len()];
    let mut left = matchers.iter().filter(|m| !m.terms.is_empty()).count();
    if left == 0 {
        return found;
    }
    let end = floor_char(text, text.len().min(SNIPPET_SCAN_BYTES));
    // Each first visit checks every matcher still missing. Finding other
    // terms later cannot make this exact raw token provide new information.
    // Borrow tokens and cap the memo: large logs often repeat their words,
    // while ordinary pages and early hits should allocate nothing here.
    const MEMO_LIMIT: usize = 2048;
    const MEMO_SAMPLE: usize = 128;
    let mut memo_on = end >= 16 * 1024;
    let mut sampled = 0usize;
    let mut repeats = 0usize;
    let mut seen: Option<HashSet<&str>> = None;
    for (n, (start, stop)) in token_spans(&text[..end]).enumerate() {
        let token = &text[start..stop];
        if memo_on && n >= 64 {
            let repeated = {
                let seen = seen.get_or_insert_with(|| HashSet::with_capacity(MEMO_LIMIT));
                if seen.len() < MEMO_LIMIT {
                    !seen.insert(token)
                } else {
                    seen.contains(token)
                }
            };
            // Unique-heavy pages do not repay a lookup for every token.
            // Sample visits, including repeats, and stop memoizing those
            // pages; all later tokens then follow the original scan.
            if sampled < MEMO_SAMPLE {
                sampled += 1;
                repeats += repeated as usize;
                if sampled == MEMO_SAMPLE && repeats < 16 {
                    memo_on = false;
                    seen = None;
                }
            }
            if repeated {
                continue;
            }
            // Once full, unknown tokens still take the normal matching path.
        }
        let lower = lower_for_match(token);
        for (i, matcher) in matchers.iter().enumerate() {
            if !found[i] && !matcher.terms.is_empty() && matcher.matches(&lower) {
                found[i] = true;
                left -= 1;
            }
        }
        if left == 0 {
            break;
        }
    }
    found
}

/// Up to `max` snippets around matches, each ~`width` bytes. Matching
/// tokenizes at most 256 KiB, ending on a complete whitespace-delimited unit
/// (never a partial word); beyond that, matches are located by anchor search
/// (`Matcher::ranges_from`) without tokenizing the rest of the page.
pub fn snippets(text: &str, matcher: &Matcher, max: usize, width: usize) -> Vec<(String, Vec<(usize, usize)>)> {
    if max == 0 || matcher.terms.is_empty() {
        return lead_snippet(text, width);
    }
    // The first SNIPPET_SCAN_BYTES are matched unit by unit; after that,
    // anchor search jumps straight to candidate units (`ranges_from`), so a
    // match at the end of a multi-MB page is still shown.
    let (prefix, resume) = matcher.prefix_ranges(text);
    let ranges = prefix.chain(matcher.ranges_from(text, resume));
    let mut out = Vec::new();
    let mut covered_until = 0usize;
    for (start, _) in ranges {
        if start < covered_until {
            continue;
        }
        let window_start = floor_char(text, start.saturating_sub(width / 3));
        let window_start = text[window_start..start]
            .char_indices()
            .find(|(_, ch)| ch.is_whitespace())
            .map_or(window_start, |(i, ch)| window_start + i + ch.len_utf8())
            .min(start);
        let window_end = ceil_char(text, start.saturating_add(width).min(text.len()));
        let slice = &text[window_start..window_end];
        let compact: String = slice.split_whitespace().collect::<Vec<_>>().join(" ");
        let highlights = matcher
            .ranges(&compact)
            .into_iter()
            .map(|(s, e)| utf16_range(&compact, s, e))
            .collect();
        out.push((compact, highlights));
        // Break here: asking the iterator for another match first could
        // scan the entire remaining prefix after the final wanted snippet.
        if out.len() >= max {
            break;
        }
        covered_until = window_end;
    }
    if out.is_empty() {
        return lead_snippet(text, width);
    }
    out
}

fn floor_char(text: &str, mut index: usize) -> usize {
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char(text: &str, mut index: usize) -> usize {
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// Summary numbers for one extracted document, used by the catalog.
pub struct PageStats {
    pub pages: usize,
    pub ocr: usize,
    pub low_confidence: usize,
    pub failed: usize,
}

pub fn page_stats(document: &ExtractedDocument, floor: f32) -> PageStats {
    PageStats {
        pages: document.pages.len(),
        ocr: document.ocr_pages(),
        low_confidence: document.low_confidence_pages(floor),
        failed: document.failed_pages(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{PageText, TextSource};

    /// Result and fastest wall time of `runs` calls. Bounds on cheap,
    /// bounded work are checked against the best run: one descheduled
    /// thread under the parallel test load (48 test threads) must
    /// not fail a test about how much work a call does.
    fn fastest_of<T>(runs: usize, mut work: impl FnMut() -> T) -> (T, std::time::Duration) {
        let mut best = std::time::Duration::MAX;
        let mut last = None;
        for _ in 0..runs.max(1) {
            let started = std::time::Instant::now();
            last = Some(work());
            best = best.min(started.elapsed());
        }
        (last.expect("at least one run"), best)
    }

    /// The pre-memo coverage pass, kept as a behavioral oracle. Its token
    /// boundary, normalization and matching must agree with the optimized pass.
    fn terms_present_reference(text: &str, matchers: &[Matcher]) -> Vec<bool> {
        let mut found = vec![false; matchers.len()];
        let mut left = matchers.iter().filter(|m| !m.terms.is_empty()).count();
        if left == 0 {
            return found;
        }
        let end = floor_char(text, text.len().min(SNIPPET_SCAN_BYTES));
        for (start, stop) in token_spans(&text[..end]) {
            let lower = lower_for_match(&text[start..stop]);
            for (i, matcher) in matchers.iter().enumerate() {
                if !found[i] && !matcher.terms.is_empty() && matcher.matches(&lower) {
                    found[i] = true;
                    left -= 1;
                }
            }
            if left == 0 {
                break;
            }
        }
        found
    }

    fn document(text: &str) -> ExtractedDocument {
        ExtractedDocument {
            kind: FileKind::Pdf,
            pages: vec![PageText::new(0, text.into(), TextSource::TextLayer, None)],
            ocr_language: None,
        }
    }

    #[test]
    fn turkish_case_and_ascii_folded_queries_find_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "a.pdf", 10, &document("KİRA SÖZLEŞMESİ ISPARTA şubesi")).unwrap();
        index.add(&writer, "b.pdf", 10, &document("Isparta dışında ilgisiz metin")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        for query in ["sözleşmesi", "SOZLESMESI", "Sözleşmesi"] {
            let hits = index.search(query, &filter, 10).unwrap();
            assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["a.pdf"], "{query}");
        }
        // Dotless capital I must not collapse to dotted i.
        let hits = index.search("ısparta", &filter, 10).unwrap();
        assert_eq!(hits.len(), 2);
        let hits = index.search("isparta", &filter, 10).unwrap();
        assert!(hits.len() == 2, "folded field still bridges ı/i for ASCII typists");
    }

    /// English plurals and verb forms meet the base form documents use,
    /// and the snippet highlights it; exact matches still rank first.
    #[test]
    fn english_inflections_find_the_base_form() {
        assert_eq!(english_variants("invoices"), ["invoic", "invoice"]);
        assert_eq!(english_variants("companies"), ["company", "compani", "companie"]);
        assert_eq!(english_variants("approved"), ["approve", "approv"]);
        assert_eq!(english_variants("processing"), ["process"]);
        assert!(english_variants("otobus").is_empty() && english_variants("giris").is_empty());
        assert!(english_variants("address").is_empty(), "-ss is not a plural");

        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "base.pdf", 10, &document("Monthly invoice for the company, approve and process")).unwrap();
        index.add(&writer, "plural.pdf", 10, &document("All invoices of the companies were approved")).unwrap();
        index.add(&writer, "other.pdf", 10, &document("Unrelated meeting notes")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        for (query, first) in [("invoices", "plural.pdf"), ("companies", "plural.pdf"), ("approved", "plural.pdf"), ("processing", "base.pdf")] {
            let hits: Vec<String> = index.search(query, &filter, 10).unwrap().into_iter().map(|h| h.path).collect();
            assert_eq!(hits.first().map(String::as_str), Some(first), "{query}: {hits:?}");
            assert!(hits.contains(&"base.pdf".to_string()), "{query} reaches the base form: {hits:?}");
            assert!(!hits.contains(&"other.pdf".to_string()), "{query}: {hits:?}");
        }
        let matcher = Matcher::new(&query_terms("invoices"), &SearchOptions::default());
        assert!(matcher.matches("invoice"));
        let matcher = Matcher::new(&query_terms("approved"), &SearchOptions::default());
        assert!(matcher.matches("approve"));
    }

    #[test]
    fn reindexing_a_path_replaces_its_pages() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "a.pdf", 1, &document("eski tablo")).unwrap();
        writer.commit().unwrap();
        index.add(&writer, "a.pdf", 2, &document("yeni kalem")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        assert!(index.search("tablo", &filter, 10).unwrap().is_empty());
        assert_eq!(index.search("kalem", &filter, 10).unwrap().len(), 1);
    }

    #[test]
    fn snippet_highlights_are_utf16_offsets() {
        let text = "Önceki satır. Ankara'da imzalanan sözleşme 𝄞 burada.";
        let terms = query_terms("sozlesme");
        let snips = snippets(text, &Matcher::new(&terms, &SearchOptions::default()), 3, 80);
        let (snippet, highlights) = &snips[0];
        let units: Vec<u16> = snippet.encode_utf16().collect();
        let (s, e) = highlights[0];
        assert_eq!(String::from_utf16(&units[s..e]).unwrap(), "sözleşme");
    }

    #[test]
    fn huge_page_snippets_stop_after_the_early_match() {
        let mut text = "Başlık 𝄞 Ankara'da ÜCRET 4471923 ödendi. ".to_string();
        text.push_str(&"2024-11-13 INFO order 8158 processed normally\n".repeat(125_000));
        assert!(text.len() >= 5 * 1024 * 1024);
        let matcher = Matcher::new(&query_terms("ücret"), &SearchOptions::default());
        let (result, took) = fastest_of(3, || snippets(&text, &matcher, 1, 100));
        assert!(took < std::time::Duration::from_secs(1), "early snippet scanned the huge tail: {took:?}");
        assert_eq!(result.len(), 1);
        let (excerpt, highlights) = &result[0];
        let units: Vec<u16> = excerpt.encode_utf16().collect();
        let words: Vec<String> = highlights.iter().map(|&(s, e)| String::from_utf16(&units[s..e]).unwrap()).collect();
        assert_eq!(words, ["ÜCRET"]);
        assert!(excerpt.starts_with("𝄞 Ankara'da ÜCRET 4471923 ödendi."));
    }

    #[test]
    fn matches_beyond_the_budget_are_found_by_anchor_search() {
        let mut text = "Özet\n\tburada. ".to_string();
        text.push_str(&"normal satır ".repeat(SNIPPET_SCAN_BYTES / 6));
        text.push_str("4471923 ücret");
        let matcher = Matcher::new(&query_terms("4471923"), &SearchOptions::default());
        assert_eq!(snippets(&text, &matcher, 2, 28), vec![("satır 4471923 ücret".to_string(), vec![(6, 13)])]);
        // No match anywhere: still the compacted lead text.
        let none = Matcher::new(&query_terms("9999999"), &SearchOptions::default());
        assert_eq!(snippets(&text, &none, 2, 28), vec![("Özet burada. normal satır".to_string(), vec![])]);
    }

    #[test]
    fn an_8_mb_log_shows_its_last_line_match_quickly() {
        let line = "2026-10-15 07:12:44 INFO worker=3 istek tamamlandı süre=12ms durum=200\n";
        let mut text = line.repeat(8 * 1024 * 1024 / line.len());
        text.push_str("2026-10-15 23:59:59 ERROR Fatura 8.158,00 TL 00 TEST 123 SÖZLEŞMESİ başarısız\n");
        assert!(text.len() > 8 * 1024 * 1024);
        let options = SearchOptions::default();
        for (query, word) in [("8.158,00", "158"), ("test 123", "TEST"), ("sözleşme", "SÖZLEŞMESİ"), ("sozlesme", "SÖZLEŞMESİ")] {
            let matcher = Matcher::new(&query_terms(query), &options);
            // Best of three: one descheduled thread under the full parallel
            // test load (130 ms seen, 34 ms alone) is not what this measures.
            let (result, took) = fastest_of(3, || snippets(&text, &matcher, 2, 80));
            // The 50 ms target holds for release builds (`cargo test --release`);
            // unoptimized dependencies (aho-corasick) are ~20x slower: debug
            // runs take 0.3-0.6 s on a 48-thread Windows PC under the full parallel test
            // load, so the 5 s debug budget keeps an ~8x margin.
            let budget = if cfg!(debug_assertions) { 5_000 } else { 50 };
            assert!(took < std::time::Duration::from_millis(budget), "{query}: {took:?}");
            eprintln!("8 MB log, {query}: {took:?}");
            let (excerpt, highlights) = &result[0];
            assert!(excerpt.contains("Fatura 8.158,00") || excerpt.contains("SÖZLEŞMESİ"), "{query}: {excerpt}");
            let units: Vec<u16> = excerpt.encode_utf16().collect();
            let words: Vec<String> =
                highlights.iter().map(|&(s, e)| String::from_utf16(&units[s..e]).unwrap()).collect();
            assert!(words.iter().any(|w| w == word), "{query}: {words:?}");
            let page = matcher.page_ranges(&text);
            assert!(!page.is_empty() && page.len() <= MAX_PAGE_HIGHLIGHTS);
            assert!(page.iter().all(|&(s, e)| matcher.ranges(&text[s..e]) == [(0, e - s)]), "{query}: exact tokens only");
        }
        // A word in every line: page highlights stop at the cap.
        let matcher = Matcher::new(&query_terms("worker"), &options);
        assert_eq!(matcher.page_ranges(&text).len(), MAX_PAGE_HIGHLIGHTS);
    }

    #[test]
    fn page_ranges_equal_full_ranges_on_ordinary_pages_and_utf16_batches_agree() {
        let text = "Önce 𝄞  Ankara'da\n\tÜCRET   ödendi. ücretler kira";
        let matcher = Matcher::new(&query_terms("ankara ücret"), &SearchOptions::default());
        assert_eq!(matcher.page_ranges(text), matcher.ranges(text));
        let ranges = matcher.ranges(text);
        let one_by_one: Vec<_> = ranges.iter().map(|&(s, e)| utf16_range(text, s, e)).collect();
        assert_eq!(utf16_ranges(text, &ranges), one_by_one);
        // Past the budget the page view finds the same tokens as a full scan.
        let big = " ".repeat(SNIPPET_SCAN_BYTES) + text;
        assert_eq!(matcher.page_ranges(&big), matcher.ranges(&big));
    }

    #[test]
    fn numeric_terms_match_the_same_tokens_with_their_own_stem_head() {
        let text = "8 158 00 TL 8'de 158'i 00'dan 18 80 1580 tutar=8.158,00 TLye 00 TEST 123 00'si 0 0 12 123'te têst ağ 8.";
        for query in ["8.158,00 TL", "00 TEST 123", "8", "2026-10-15", "kira 3"] {
            let matcher = Matcher::new(&query_terms(query), &SearchOptions::default());
            assert!(
                matcher.snowball.iter().zip(&matcher.snowball_heads).all(|(stem, head)| !head.is_empty() || !stem.bytes().all(|b| b.is_ascii_digit())),
                "{query}: numeric heads are never empty"
            );
            // The previous rule (every stem minus its last letter) matches
            // exactly the same tokens; only the stemmer runs less often.
            let mut previous = Matcher::new(&query_terms(query), &SearchOptions::default());
            previous.snowball_heads =
                previous.snowball.iter().map(|stem| stem.chars().take(stem.chars().count().saturating_sub(1)).collect()).collect();
            assert_eq!(matcher.ranges(text), previous.ranges(text), "{query}");
        }
    }

    #[test]
    fn coverage_token_memo_matches_the_reference_on_turkish_and_bounded_pages() {
        let ordinary = "ISPARTA İstanbul ıslak İSTANBUL Ankara'da ANKARA’DA Sözleşmelerimizin makbuz";
        let repeated = ordinary.repeat(400);
        let late = "ISPARTA ıslak ANKARA'da makbuz ".repeat(600) + "İSTANBUL Sözleşmelerimizin";
        let cut = "x ".repeat((SNIPPET_SCAN_BYTES - 1) / 2) + "ücret İstanbul";
        for options in [
            SearchOptions::default(),
            SearchOptions { expand: false, stem_boost: 0.0, synonym_boost: 0.0, ..SearchOptions::default() },
        ] {
            for query in ["ısparta istanbul ıslak", "ankara sözleşmeler fatura", "ü ücret istanbul", "00 TEST 123", ""] {
                let mut matchers: Vec<_> = query_terms(query).iter().map(|term| Matcher::new(std::slice::from_ref(term), &options)).collect();
                matchers.push(Matcher::new(&[], &options));
                for text in ["", ordinary, repeated.as_str(), late.as_str(), cut.as_str()] {
                    assert_eq!(terms_present(text, &matchers), terms_present_reference(text, &matchers),
                        "query={query:?}, bytes={}, options={options:?}", text.len());
                }
            }
        }
        let mut unique: String = (0..2300).map(|n| format!("benzersiz{n:04} ")).collect();
        unique.push_str("benzersiz0100 hedef");
        // The unique sample disables memoization. Repeats and a late match
        // must still be visited afterwards. The repeated prefix keeps the
        // other fixture's memo enabled until all 2,048 entries fill: cached
        // repeats are skipped there, while the new final match is not.
        let kept_memo = "ornek ".repeat(256) + &unique;
        let options = SearchOptions { expand: false, stem_boost: 0.0, synonym_boost: 0.0, ..SearchOptions::default() };
        let matchers: Vec<_> = query_terms("zqxvra yklmta hedef").iter().map(|term| Matcher::new(std::slice::from_ref(term), &options)).collect();
        for text in [&unique, &kept_memo] {
            assert!(text.len() > 16 * 1024 && text.len() < SNIPPET_SCAN_BYTES);
            assert_eq!(terms_present(text, &matchers), vec![false, false, true]);
            assert_eq!(terms_present(text, &matchers), terms_present_reference(text, &matchers));
        }
        assert_eq!(terms_present(&repeated, &[]), terms_present_reference(&repeated, &[]));
    }

    /// Opt-in timing evidence, without a scheduler-dependent speed assertion:
    /// ARSIV_COVERAGE_BENCH=1 cargo test --release -p arsiv-core coverage_token_memo_benchmark -- --nocapture
    #[test]
    fn coverage_token_memo_benchmark() {
        if std::env::var("ARSIV_COVERAGE_BENCH").as_deref() != Ok("1") {
            return;
        }
        let mut repeated = "2024-11-13 INFO order 8158 processed normally ".repeat(SNIPPET_SCAN_BYTES / 20);
        repeated.truncate(SNIPPET_SCAN_BYTES);
        let mut unique: String = (0..30_000).map(|n| format!("word{n:06} ")).collect();
        unique.truncate(SNIPPET_SCAN_BYTES);
        let options = SearchOptions::default();
        let matchers: Vec<_> = query_terms("00 TEST 123").iter().map(|term| Matcher::new(std::slice::from_ref(term), &options)).collect();
        for (name, text) in [("repeated", repeated), ("unique", unique)] {
            let expected = terms_present_reference(&text, &matchers);
            assert_eq!(expected, vec![false; 3], "the same three query terms miss in both fixtures");
            let mut old = Vec::new();
            let mut new = Vec::new();
            for _ in 0..3 {
                let started = std::time::Instant::now();
                let previous = terms_present_reference(std::hint::black_box(text.as_str()), &matchers);
                old.push(started.elapsed().as_secs_f64() * 1000.0);
                let started = std::time::Instant::now();
                let current = terms_present(std::hint::black_box(text.as_str()), &matchers);
                new.push(started.elapsed().as_secs_f64() * 1000.0);
                assert_eq!(previous, expected);
                assert_eq!(current, expected);
            }
            old.sort_by(f64::total_cmp);
            new.sort_by(f64::total_cmp);
            eprintln!("coverage-token-memo {name} bytes={} runs=3 old_ms_fastest={:.3} old_ms_median={:.3} new_ms_fastest={:.3} new_ms_median={:.3}",
                text.len(), old[0], old[1], new[0], new[1]);
        }
    }

    #[test]
    fn anchors_skip_letters_that_folding_can_produce() {
        let options = SearchOptions { expand: false, stem_boost: 0.0, synonym_boost: 0.0, ..SearchOptions::default() };
        let anchors = |q: &str| Matcher::new(&query_terms(q), &options).anchors();
        assert_eq!(anchors("4471923"), Some(vec!["4471923".to_string()]));
        assert_eq!(anchors("sözleşme"), Some(vec!["zle".to_string()]));
        assert_eq!(anchors("ücret"), Some(vec!["ret".to_string()]));
        assert_eq!(anchors("su"), None, "every letter has a Turkish variant");
    }

    #[test]
    fn cut_utf8_or_apostrophe_tokens_are_never_partial_matches() {
        let options = SearchOptions { expand: false, stem_boost: 0.0, synonym_boost: 0.0, ..SearchOptions::default() };
        for (tail, query, bytes_before_cut) in [("ücretinde", "ücret", 6), ("Ankara'da", "ankara", 7), ("𝄞ücretinde", "ücret", 9)] {
            let prefix = " ".repeat(SNIPPET_SCAN_BYTES - bytes_before_cut);
            let text = prefix + tail;
            let matcher = Matcher::new(&query_terms(query), &options);
            let result = snippets(&text, &matcher, 2, 80);
            assert!(result.iter().all(|(_, highlights)| highlights.is_empty()), "partial match in {tail}");
        }
    }

    #[test]
    fn huge_whitespace_and_unbroken_tokens_have_bounded_fallbacks() {
        let matcher = Matcher::new(&query_terms("ücret"), &SearchOptions::default());
        let whitespace = " ".repeat(5 * 1024 * 1024);
        let token = "x".repeat(5 * 1024 * 1024) + " ücret";
        let other = Matcher::new(&query_terms("kira"), &SearchOptions::default());
        let (_, took) = fastest_of(3, || {
            assert!(snippets(&whitespace, &matcher, 2, 80).is_empty());
            assert_eq!(snippets(&token, &matcher, 2, 80), vec![("ücret".to_string(), vec![(0, 5)])]);
            assert_eq!(snippets(&token, &other, 2, 80), vec![("x".repeat(80), vec![])]);
        });
        assert!(took < std::time::Duration::from_secs(1), "{took:?}");
    }

    #[test]
    fn ordinary_snippet_text_and_utf16_apostrophe_highlights_are_unchanged() {
        let matcher = Matcher::new(&query_terms("ankara ücret"), &SearchOptions::default());
        assert_eq!(
            snippets("Önce 𝄞  Ankara'da\n\tÜCRET   ödendi.", &matcher, 2, 80),
            vec![("𝄞 Ankara'da ÜCRET ödendi.".to_string(), vec![(3, 12), (13, 18)])],
        );
        assert_eq!(lead_snippet("  𝄞 ÜCRET\nAnkara’da", 6), vec![("𝄞".to_string(), vec![])]);
        assert_eq!(snippets("ücret var", &matcher, 0, 80), vec![("ücret var".to_string(), vec![])]);
        assert_eq!(snippets("önce\u{2003}ücret ödendi", &matcher, 1, 12)[0].1.len(), 1);
    }

    #[test]
    fn empty_queries_return_no_full_document_highlights() {
        // Structural, not a stopwatch: a 100 ms bound on a debug build
        // failed under the parallel test load. What matters
        // is that a query without terms visits none of the page.
        let matcher = Matcher::new(&[], &SearchOptions::default());
        let text = "x ".repeat(5 * 1024 * 1024 / 2);
        assert!(matcher.scanned(&text).is_empty(), "an empty query must not tokenize the page");
        assert!(matcher.ranges(&text).is_empty());
        assert!(matcher.page_ranges(&text).is_empty());
        assert!(matcher.locators().iter().all(Option::is_none), "no anchor search either");
        assert!(!Matcher::new(&query_terms("x"), &SearchOptions::default()).scanned(&text).is_empty());
    }

    #[test]
    fn ascii_match_fast_paths_preserve_turkish_and_unicode_normalization() {
        for text in ["log123", "INFO", "ORDER", "ISPARTA", "Ankara'da", "ANKARA’DA", "ÜCRET", "u\u{308}cret", "I\u{307}", "Café"] {
            assert_eq!(lower_for_match(text).as_ref(), turkish_lower(text), "{text}");
        }
        assert!(matches!(lower_for_match("log123"), Cow::Borrowed(_)));
    }

    #[test]
    fn inflected_queries_find_base_forms_and_vice_versa() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "a.pdf", 10, &document("Kira sözleşmesi imzalandı")).unwrap();
        index.add(&writer, "b.pdf", 10, &document("Sözleşmenin süresi bir yıldır")).unwrap();
        index.add(&writer, "c.pdf", 10, &document("Kiraz mevsimi geldi")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };

        // Query-side stem + prefix: "sözleşmesi" must reach "sözleşmenin" too.
        let hits = index.search("sözleşmesi", &filter, 10).unwrap();
        assert!(hits.iter().any(|h| h.path == "b.pdf"), "inflected doc missed: {hits:?}");
        // Base query "sözleşme" must find both inflected docs.
        let hits = index.search("sözleşme", &filter, 10).unwrap();
        assert_eq!(hits.len(), 2, "expected a+b, got {hits:?}");
        // "kiraz" is an accepted false-positive of strict prefix on a 4-char
        // term — it must rank below the true hit, never alone on top.
        let hits = index.search("kira", &filter, 10).unwrap();
        assert_eq!(hits.first().map(|h| h.path.as_str()), Some("a.pdf"), "kiraz outranked kira: {hits:?}");

        // Matching off: only exact/folded terms match — no doc contains the
        // bare base form, so "sözleşme" returns empty.
        let literal = SearchOptions { expand: false, stem_boost: 0.0, synonym_boost: 0.0, ..SearchOptions::default() };
        index.set_options(literal);
        assert!(index.search("sözleşme", &filter, 10).unwrap().is_empty());
        assert_eq!(index.search("sözleşmesi", &filter, 10).unwrap().len(), 1);
        index.set_options(SearchOptions::default());
    }

    fn multi_page(pages: &[&str]) -> ExtractedDocument {
        ExtractedDocument {
            kind: FileKind::Pdf,
            pages: pages
                .iter()
                .enumerate()
                .map(|(i, t)| PageText::new(i, t.to_string(), TextSource::TextLayer, None))
                .collect(),
            ocr_language: None,
        }
    }

    /// All-digit query parts must occur the way the user wrote them:
    /// `9.731,45` is `9 731 45` adjacent, not the pieces
    /// scattered over a page; letter parts (`TL`, `TEST`) stay optional and
    /// alphanumeric code parts are not pinned (OCR corrupts letters).
    #[test]
    fn digit_parts_require_their_tokens_adjacent() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "invoice.pdf", 10, &document("TOPLAM TUTAR 8.158,00 TL olarak ödenecektir")).unwrap();
        index.add(&writer, "scattered.pdf", 10, &document("Adet 8, satır 158, oran %00 TL ve başka metin")).unwrap();
        index.add(&writer, "receipt.pdf", 10, &document("Örnek ödeme belgesi ÖR-CAHNFY-27454 tutar")).unwrap();
        index.add(&writer, "scattered-code.pdf", 10, &document("ÖR kodu CAHNFY ile ve numara 27454 ayrı")).unwrap();
        index.add(&writer, "date.pdf", 10, &document("Sözleşme tarihi 13.11.2024 olarak geçiyor")).unwrap();
        index.add(&writer, "scattered-date.pdf", 10, &document("13 Kasım listesi, madde 11, yıl 2024")).unwrap();
        index.add(&writer, "parts.pdf", 10, &document("Sıra 9 sonra 731 en sonda 45 rakamları")).unwrap();
        index.add(&writer, "whole.pdf", 10, &document("Tutar 9.731,45 net ödenir")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        // Amount: adjacent hits; pieces scattered over one page do not match.
        // `TL` is not required (documents still rank without it).
        let hits = index.search("8.158,00 TL", &filter, 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["invoice.pdf"]);
        let hits = index.search("8.158,00", &filter, 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["invoice.pdf"]);
        // The reported bug: parts alone in the same document must not match.
        assert_eq!(index.search("9.731,45", &filter, 10).unwrap().iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["whole.pdf"]);
        // Alphanumeric codes are not pinned (OCR corrupts letters): the
        // exact code still ranks first, a page of scattered pieces may
        // follow behind it.
        assert_eq!(index.search("ÖR-CAHNFY-27454", &filter, 10).unwrap()[0].path, "receipt.pdf");
        // Date.
        assert_eq!(index.search("13.11.2024", &filter, 10).unwrap().iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["date.pdf"]);
    }

    /// Plate-style queries keep letter parts optional while every digit part
    /// is required: the synthetic `00 TEST 123` matches 00+123 without TEST.
    #[test]
    fn plate_queries_require_each_digit_part_only() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "plate.pdf", 10, &document("Test plakası 00 TEST 123 olarak kayıtlı")).unwrap();
        index.add(&writer, "no-code.pdf", 10, &document("00 otobüsü, kapı 123")).unwrap();
        index.add(&writer, "half.pdf", 10, &document("TEST 123 otobüsü geçti")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let hits = index.search("00 TEST 123", &filter, 10).unwrap();
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths, ["plate.pdf", "no-code.pdf"], "TEST optional, 00 and 123 required");
    }

    #[test]
    fn total_counts_distinct_files_beyond_the_requested_window() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        for i in 0..40 {
            // Three matching pages per file: a page count would say 120.
            let doc = multi_page(&["fatura bir", "fatura iki", "fatura üç", "ilgisiz"]);
            index.add(&writer, &format!("f{i}.pdf"), 10, &doc).unwrap();
        }
        index.add(&writer, "x.pdf", 10, &document("kalem")).unwrap();
        writer.commit().unwrap();
        // Second segment re-adding a file must not double count it.
        index.add(&writer, "f0.pdf", 11, &multi_page(&["fatura yeni"])).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let result = index.search_counted("fatura", &filter, 5).unwrap();
        assert_eq!(result.hits.len(), 5);
        assert_eq!(result.total_files, 40);
        assert_eq!(index.count_files("fatura", &filter).unwrap(), result.total_files);
        let word = [FileKind::Word];
        let filtered = SearchFilter { kinds: &word, modified_after: None, modified_before: None };
        assert_eq!(index.search_counted("fatura", &filtered, 5).unwrap().total_files, 0);
    }

    /// Counting skips scoring and stored-page reads, while every clause
    /// (including required phrases/digits and filters) must stay identical.
    #[test]
    fn count_only_matches_ranked_totals_for_all_query_clauses_and_filters() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "invoice.pdf", 10, &multi_page(&[
            "Kira sözleşmesi fatura 8.158,00 TL tarih 13.11.2024",
            "Fatura sözleşmelerimizin tamamı",
            "Fatura başka sayfada",
        ])).unwrap();
        index.add(&writer, "scattered.pdf", 20, &document("Kira uzun açıklama sözleşmesi tutar 8 adet 158 oran 00 TL")).unwrap();
        index.add(&writer, "receipt.pdf", 30, &document("Makbuz icar kontrat")).unwrap();
        index.add(&writer, "inflected.pdf", 40, &document("Sözleşmelerimizin tamamı ektedir")).unwrap();
        let mut word = document("Fatura kira sözleşmesi 8.158,00 TL");
        word.kind = FileKind::Word;
        index.add(&writer, "invoice.docx", 20, &word).unwrap();
        index.add(&writer, "unrelated.pdf", 50, &document("İlgisiz metin")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let pdf = [FileKind::Pdf];
        let word = [FileKind::Word];
        let filters = [
            SearchFilter { kinds: &[], modified_after: None, modified_before: None },
            SearchFilter { kinds: &pdf, modified_after: None, modified_before: None },
            SearchFilter { kinds: &word, modified_after: None, modified_before: None },
            SearchFilter { kinds: &[], modified_after: Some(20), modified_before: Some(30) },
            SearchFilter { kinds: &pdf, modified_after: Some(20), modified_before: Some(20) },
            SearchFilter { kinds: &[], modified_after: Some(50), modified_before: None },
            SearchFilter { kinds: &[], modified_after: None, modified_before: Some(10) },
        ];
        let literal = SearchOptions { expand: false, stem_boost: 0.0, synonym_boost: 0.0, ..SearchOptions::default() };
        for options in [
            SearchOptions::default(),
            literal,
            SearchOptions { stem_boost: 0.5, ..literal },
            SearchOptions { synonym_boost: 0.8, ..literal },
        ] {
            index.set_options(options);
            for filter in &filters {
                for query in [
                    "", "   ", "!!!", "fatura", "makbuz", "sözleşmeler", "sozlesmesi",
                    "kira sözleşmesi", "kira kira", "\"kira sözleşmesi\"", "“kira sözleşmesi”",
                    "8.158,00 TL", "13.11.2024", "ÖR-CAHNFY-27454", "bulunmayan",
                ] {
                    let ranked = index.search_counted(query, filter, 1).unwrap();
                    assert_eq!(index.count_files(query, filter).unwrap(), ranked.total_files,
                        "query={query:?}, options={options:?}, kinds={:?}, after={:?}, before={:?}",
                        filter.kinds, filter.modified_after, filter.modified_before);
                }
            }
        }
        index.set_options(literal);
        assert_eq!(index.count_files("fatura", &filters[0]).unwrap(), 2, "matching pages count once per file");
        assert_eq!(index.count_files("\"kira sözleşmesi\"", &filters[0]).unwrap(), 2, "phrase adjacency is required");
        assert_eq!(index.count_files("8.158,00 TL", &filters[0]).unwrap(), 2, "scattered digits are excluded");
        assert_eq!(index.count_files("fatura", &filters[4]).unwrap(), 0, "kind and inclusive date filters intersect");
        index.set_options(SearchOptions { synonym_boost: 0.8, ..literal });
        assert_eq!(index.count_files("fatura", &filters[0]).unwrap(), 3, "synonym-only matches are counted");
    }

    #[test]
    fn count_only_tracks_committed_additions_replacements_and_deletions() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let verify = |expected| {
            assert_eq!(index.count_files("kalem", &filter).unwrap(), expected);
            assert_eq!(index.search_counted("kalem", &filter, 1).unwrap().total_files, expected);
        };
        verify(0);
        index.add(&writer, "a.pdf", 10, &multi_page(&["kalem bir", "kalem iki"])).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        verify(1);
        index.add(&writer, "b.pdf", 20, &document("kalem üç")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        verify(2);
        // Replacing an existing path also removes its matching old pages.
        index.add(&writer, "a.pdf", 30, &document("defter")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        verify(1);
        index.delete_path(&writer, "b.pdf");
        writer.commit().unwrap();
        index.reload().unwrap();
        verify(0);
    }

    #[test]
    fn the_writer_waits_out_a_briefly_held_lock_but_not_a_real_one() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let held = index.writer().unwrap();
        let started = std::time::Instant::now();
        let error = index.writer().err().expect("a writer that stays alive must not be shared");
        assert!(format!("{error:#}").contains("LockBusy"), "{error:#}");
        assert!(started.elapsed() >= WRITER_LOCK_WAIT, "gave up after {:?}", started.elapsed());
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(std::time::Duration::from_millis(150));
                drop(held);
            });
            index.writer().expect("lock released while waiting");
        });
    }

    #[test]
    fn an_index_from_another_schema_version_is_discarded() {
        let dir = tempfile::tempdir().unwrap();
        {
            let index = SearchIndex::open(dir.path()).unwrap();
            assert!(!index.discarded_stale());
            let mut writer = index.writer().unwrap();
            index.add(&writer, "a.pdf", 1, &document("fatura")).unwrap();
            writer.commit().unwrap();
        }
        let reopened = SearchIndex::open(dir.path()).unwrap();
        assert!(!reopened.discarded_stale());
        assert_eq!(reopened.page_count(), 1);
        drop(reopened);
        // Simulate an index written by 0.1.0 (no version marker).
        std::fs::remove_file(dir.path().join(VERSION_FILE)).unwrap();
        let rebuilt = SearchIndex::open(dir.path()).unwrap();
        assert!(rebuilt.discarded_stale());
        assert_eq!(rebuilt.page_count(), 0);
        let version = std::fs::read_to_string(dir.path().join(VERSION_FILE)).unwrap();
        assert_eq!(version, SCHEMA_VERSION.to_string());
    }

    #[test]
    fn snowball_stems_join_inflected_forms() {
        for (a, b) in [
            ("sözleşmelerimizin", "sözleşme"),
            ("faturaları", "faturalar"),
            ("kiracının", "kiracı"),
            ("dilekçesini", "dilekçe"),
            ("ankara'nın", "ankara"),
        ] {
            assert_eq!(stem_term(a), stem_term(b), "{a} vs {b}");
        }
        // Folded on both sides: ASCII queries meet diacritic text.
        assert_eq!(stem_term("sözleşmesi"), stem_term("sozlesmesi"), "ascii query");
    }

    #[test]
    fn the_stem_field_finds_inflections_the_suffix_list_misses() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "a.pdf", 10, &document("Sözleşmelerimizin tamamı ektedir")).unwrap();
        index.add(&writer, "b.pdf", 10, &document("İlgisiz metin")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let stems_only = SearchOptions { expand: false, legacy_stems: false, stem_boost: 0.5, ..SearchOptions::default() };
        index.set_options(SearchOptions { stem_boost: 0.0, ..stems_only });
        assert!(index.search("sözleşmeler", &filter, 10).unwrap().is_empty());
        index.set_options(stems_only);
        let hits = index.search("sözleşmeler", &filter, 10).unwrap();
        assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["a.pdf"]);
        let ranges = match_ranges_with("Sözleşmelerimizin tamamı", &query_terms("sözleşmeler"), &stems_only);
        assert_eq!(ranges, [(0, "Sözleşmelerimizin".len())]);
    }

    #[test]
    fn quoted_phrases_are_parsed_and_unclosed_quotes_ignored() {
        assert_eq!(quoted_phrases(r#"fatura "Kira Sözleşmesi" 2026"#), [vec!["kira".to_string(), "sozlesmesi".into()]]);
        assert_eq!(quoted_phrases("“Ankara'da” ve \"tek\""), [vec!["ankarada".to_string()], vec!["tek".into()]]);
        assert!(quoted_phrases(r#"kira "sözleşmesi"#).is_empty());
        assert!(quoted_phrases(r#"kira "" bedeli"#).is_empty());
    }

    #[test]
    fn quoted_phrase_is_required_and_proximity_ranks_close_words_first() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        // far.pdf repeats both words (higher BM25) but never side by side.
        index.add(&writer, "far.pdf", 10, &document("kira kira kira bu yıl ödendi uzun uzun süre sonra yeni bir sözleşmesi sözleşmesi yapıldı")).unwrap();
        index.add(&writer, "near.pdf", 10, &document("Bu kira sözleşmesi bir yıllıktır ve uzun bir metnin parçasıdır burada")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let ranked = |q: &str| index.search(q, &filter, 10).unwrap().into_iter().map(|h| h.path).collect::<Vec<_>>();

        assert_eq!(ranked("\"kira sözleşmesi\""), ["near.pdf"]);
        assert_eq!(ranked("“KIRA SOZLESMESI”"), ["near.pdf"], "folded, curly quotes");
        assert_eq!(ranked("kira sözleşmesi").len(), 2, "unquoted stays OR");

        index.set_options(SearchOptions { phrase_boost: 0.0, pair_boost: 0.0, ..SearchOptions::default() });
        assert_eq!(ranked("kira sözleşmesi")[0], "far.pdf", "fixture must favour far.pdf without proximity");
        index.set_options(SearchOptions::default());
        assert_eq!(ranked("kira sözleşmesi")[0], "near.pdf", "proximity is on by default");
    }

    /// Long queries rarely occur whole; adjacent word pairs still earn credit.
    #[test]
    fn adjacent_query_pairs_earn_proximity_credit() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "far.pdf", 10, &document("kira ödendi yıl geçti uzun süre sonra sözleşmesi yenilendi kira")).unwrap();
        index.add(&writer, "near.pdf", 10, &document("yeni kira sözleşmesi imzalandı ve bir yıl uzun sürdü")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let top = |options: SearchOptions| {
            index.set_options(options);
            index.search("yıl kira sözleşmesi uzun", &filter, 10).unwrap()[0].path.clone()
        };
        let base = SearchOptions { phrase_boost: 0.0, pair_boost: 0.0, ..SearchOptions::default() };
        assert_eq!(top(base), "far.pdf", "fixture must favour far.pdf without proximity");
        assert_eq!(top(SearchOptions { pair_boost: 0.4, ..base }), "near.pdf", "pair kira+sözleşmesi");
    }

    #[test]
    fn synonym_groups_parse_and_match_inflected_queries() {
        let groups = synonym_groups();
        assert!(groups.len() >= 45, "{}", groups.len());
        let mut seen = std::collections::HashSet::new();
        for word in groups.iter().flatten() {
            assert!(seen.insert(folded_term(word)), "{word} is in two groups");
        }
        assert_eq!(synonyms_of("fatura"), ["makbuz"]);
        assert_eq!(synonyms_of("faturası"), ["makbuz"], "inflected query");
        assert_eq!(synonyms_of("SOZLESME".to_lowercase().as_str()), ["mukavele", "kontrat", "anlaşma", "mutabakat"]);
        assert_eq!(synonyms_of("faturaları"), ["makbuz"], "plural inflection");
        assert!(synonyms_of("kiraz").is_empty(), "kiraz is not kira");
        assert!(synonyms_of("ankara").is_empty());
    }

    #[test]
    fn synonyms_reach_documents_without_the_query_word_and_rank_below_it() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "makbuz.pdf", 10, &document("Tahsilat makbuzları ektedir")).unwrap();
        index.add(&writer, "fatura.pdf", 10, &document("Fatura ektedir")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let ranked = |q: &str| index.search(q, &filter, 10).unwrap().into_iter().map(|h| h.path).collect::<Vec<_>>();
        index.set_options(SearchOptions { synonym_boost: 0.0, ..SearchOptions::default() });
        assert_eq!(ranked("fatura"), ["fatura.pdf"]);
        let options = SearchOptions::default();
        assert!(options.synonym_boost > 0.0, "synonyms are on by default");
        index.set_options(options);
        assert_eq!(ranked("fatura"), ["fatura.pdf", "makbuz.pdf"]);
        let terms = query_terms("fatura");
        assert_eq!(match_ranges_with("Tahsilat makbuzları", &terms, &options), [(9, "Tahsilat makbuzları".len())]);
    }

    #[test]
    fn apostrophe_forms_fold_to_the_same_term() {
        let dir = tempfile::tempdir().unwrap();
        let index = SearchIndex::open(dir.path()).unwrap();
        let mut writer = index.writer().unwrap();
        index.add(&writer, "a.pdf", 10, &document("Sözleşme Ankara'da imzalandı")).unwrap();
        writer.commit().unwrap();
        index.reload().unwrap();
        let filter = SearchFilter { kinds: &[], modified_after: None, modified_before: None };
        let hits = index.search("ankarada", &filter, 10).unwrap();
        assert_eq!(hits.len(), 1, "apostrophe fold failed: {hits:?}");
    }
}
