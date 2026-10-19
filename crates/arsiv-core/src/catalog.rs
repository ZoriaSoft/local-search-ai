//! SQLite catalog: watched folders, indexed files (for incremental rescans and
//! document views) and extraction failures. Page text is stored here so the
//! preview never re-runs OCR.

use std::path::Path;

use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::model::{FileKind, PageText, TextSource};

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderInfo {
    pub id: i64,
    pub path: String,
    pub file_count: i64,
    pub indexed_count: i64,
    pub failed_count: i64,
    pub low_confidence_count: i64,
    /// Indexed files with at least one unreadable page.
    pub partial_count: i64,
    pub last_scan: Option<i64>,
    /// The folder itself is gone (deleted, renamed, drive unplugged). Its
    /// records are kept — they reappear on the next scan once it is back.
    #[serde(default)]
    pub missing: bool,
    /// File kinds indexed in this folder (never empty; all kinds by default).
    pub kinds: Vec<FileKind>,
    /// Files whose text is read but whose semantic index is pending
    /// (embedded=0). The engine reports 0 when no model is in use.
    #[serde(default)]
    pub pending_embed_count: i64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FailureInfo {
    pub path: String,
    pub error: String,
    /// `failure_reason(error)`: what the UI explains to the user.
    pub reason: &'static str,
    pub at: i64,
    /// Set for partially read files: how many pages failed, out of how many.
    /// `None` means the whole file failed and is not searchable.
    pub failed_pages: Option<i64>,
    pub page_count: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct FileRecord {
    pub path: String,
    pub folder_id: i64,
    pub kind: FileKind,
    pub size: i64,
    pub modified: i64,
    pub page_count: i64,
    pub ocr_pages: i64,
    pub low_confidence_pages: i64,
    pub failed_pages: i64,
    /// blake3 of the file bytes; identical files share one extraction.
    pub content_hash: Option<String>,
    /// Tesseract language of the OCR pages (`None` without OCR pages).
    pub ocr_language: Option<String>,
}

/// Schema migrations, applied in order; `PRAGMA user_version` records how
/// many have run. Append only — never edit a shipped step.
const MIGRATIONS: &[&str] = &[
    // 1: per-page extraction failures (a failed page no longer fails the file).
    "ALTER TABLE pages ADD COLUMN error TEXT;
     ALTER TABLE files ADD COLUMN failed_pages INTEGER NOT NULL DEFAULT 0;
     ALTER TABLE files ADD COLUMN indexed_at INTEGER NOT NULL DEFAULT 0;",
    // 2: content hash, so a copied/moved file reuses stored pages (no re-OCR).
    "ALTER TABLE files ADD COLUMN content_hash TEXT;
     CREATE INDEX files_content_hash ON files(content_hash);",
    // 3: per-folder file kinds to index; NULL = every kind (also kinds added
    // by later builds), otherwise comma-separated `FileKind::as_str` values.
    "ALTER TABLE folders ADD COLUMN kinds TEXT;",
    // 4: semantic embeddings. One row per text chunk (~400 tokens) of an
    // indexed file; `files.embedded` marks files whose chunks are complete
    // so interrupted/failed embeds are finished by the backfill pass.
    "ALTER TABLE files ADD COLUMN embedded INTEGER NOT NULL DEFAULT 0;
     CREATE TABLE chunks(
       path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE,
       idx INTEGER NOT NULL, page INTEGER NOT NULL, vector BLOB NOT NULL,
      PRIMARY KEY(path, idx));",
    // 5: per-page character count so search never decodes multi-MB texts
    // just to measure them (vector short-page penalty, snippet sizing).
    "ALTER TABLE pages ADD COLUMN chars INTEGER;
     UPDATE pages SET chars = length(text) WHERE chars IS NULL;",
    // 6: covering index so `page_chars` never opens the row payload — any
    // column access past `text` reads the whole multi-MB value with it.
    "CREATE INDEX pages_chars ON pages(path, idx, chars);",
    // 7: a cancelled scan-time embed used to store text files
    // as embedded=1 with no chunks, so the backfill never embedded them.
    // Hand every such file with readable text back to the backfill; files
    // without text (blank scans, images with no OCR text) stay done.
    "UPDATE files SET embedded=0
      WHERE embedded=1
        AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.path = files.path)
        AND EXISTS (SELECT 1 FROM pages p WHERE p.path = files.path AND p.error IS NULL
                    AND length(trim(p.text, ' ' || char(9, 10, 11, 12, 13, 160))) > 0);",
    // 8: which extractor generation recorded a failure. A failure only
    // keeps a file "current" for the generation that wrote it, so a build
    // that reads more (Unicode paths, empty files) retries old failures on
    // its first scan instead of showing them as unreadable forever.
    "ALTER TABLE failures ADD COLUMN version INTEGER NOT NULL DEFAULT 0;",
    // 9: document language. `ocr_lang` is the Tesseract language a file's
    // OCR pages were read with (every older build read Turkish only); a
    // file read with another language than the current setting is read
    // again. `settings` holds user choices such as that language.
    "ALTER TABLE files ADD COLUMN ocr_lang TEXT;
     UPDATE files SET ocr_lang='tur' WHERE ocr_pages>0;
     CREATE TABLE settings(key TEXT PRIMARY KEY, value TEXT NOT NULL);",
];

/// Bump when extraction learns to read files it used to fail on; every
/// failure recorded by an older generation is retried on the next scan.
pub const EXTRACT_VERSION: i64 = 1;

/// Why a file could not be read, as a stable code the UI explains in
/// Turkish (the raw error stays available as detail).
pub fn failure_reason(error: &str) -> &'static str {
    let e = error.to_lowercase();
    if e.contains("password") || e.contains("encrypt") || e.contains("parola") {
        "encrypted"
    } else if e.contains("os error 5") || e.contains("access is denied") || e.contains("permission denied") || e.contains("os error 32") {
        "access_denied"
    } else if e.contains("too large") {
        "too_large"
    } else if e.contains("timed out") {
        "timeout"
    } else if e.contains("ocr unavailable") || e.contains("pdf support unavailable") {
        "engine_missing"
    } else if e.contains("not a valid docx")
        || e.contains("cannot open workbook")
        || e.contains("cannot open pdf")
        || e.contains("cannot decode image")
        || e.contains("invalid zip")
        || e.contains("no word/document.xml")
    {
        "damaged"
    } else if e.contains("unsupported") {
        "unsupported"
    } else if e.contains("tesseract") || e.contains("ocr") {
        "ocr_failed"
    } else {
        "other"
    }
}

pub struct Catalog {
    db: Connection,
}

impl Catalog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut db = Connection::open(path)?;
        db.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;
             CREATE TABLE IF NOT EXISTS folders(
               id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
               file_count INTEGER NOT NULL DEFAULT 0, last_scan INTEGER);
             CREATE TABLE IF NOT EXISTS files(
               path TEXT PRIMARY KEY, folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
               kind TEXT NOT NULL, size INTEGER NOT NULL, modified INTEGER NOT NULL,
               page_count INTEGER NOT NULL, ocr_pages INTEGER NOT NULL, low_confidence_pages INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS pages(
               path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE, idx INTEGER NOT NULL,
               text TEXT NOT NULL, source TEXT NOT NULL, confidence REAL,
               PRIMARY KEY(path, idx));
             CREATE TABLE IF NOT EXISTS failures(
               path TEXT PRIMARY KEY, folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
               size INTEGER NOT NULL, modified INTEGER NOT NULL, error TEXT NOT NULL, at INTEGER NOT NULL);",
        )?;
        migrate(&mut db)?;
        Ok(Self { db })
    }

    #[cfg(test)]
    pub fn db_for_tests(&self) -> &Connection {
        &self.db
    }

    pub fn schema_version(&self) -> Result<usize> {
        Ok(self.db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize)
    }

    pub fn add_folder(&self, path: &str) -> Result<i64> {
        self.add_folder_with_kinds(path, &FileKind::DEFAULT)
    }

    /// Adds a folder that indexes only `kinds` (error `kinds_empty` if none).
    pub fn add_folder_with_kinds(&self, path: &str, kinds: &[FileKind]) -> Result<i64> {
        let stored = encode_kinds(kinds)?;
        let normalized = normalize_folder(path);
        for existing in self.folder_paths()? {
            let existing = normalize_folder(&existing);
            if existing == normalized {
                bail!("folder_duplicate");
            }
            if is_within(&normalized, &existing) || is_within(&existing, &normalized) {
                bail!("folder_nested");
            }
        }
        self.db.execute("INSERT INTO folders(path, kinds) VALUES (?1, ?2)", params![path, stored])?;
        Ok(self.db.last_insert_rowid())
    }

    /// Id of an already-registered folder for `path` (same normalization
    /// `add_folder` dedups on), or `None`.
    pub fn folder_id(&self, path: &str) -> Result<Option<i64>> {
        let normalized = normalize_folder(path);
        let mut statement = self.db.prepare("SELECT id, path FROM folders")?;
        let rows = statement.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (id, existing) = row?;
            if normalize_folder(&existing) == normalized {
                return Ok(Some(id));
            }
        }
        Ok(None)
    }

    fn folder_paths(&self) -> Result<Vec<String>> {
        let mut statement = self.db.prepare("SELECT path FROM folders")?;
        let rows = statement.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn folder_path(&self, id: i64) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT path FROM folders WHERE id=?1", [id], |r| r.get(0))
            .optional()?)
    }

    /// The kinds a folder indexes; `None` when the folder does not exist.
    pub fn folder_kinds(&self, id: i64) -> Result<Option<Vec<FileKind>>> {
        let stored: Option<Option<String>> =
            self.db.query_row("SELECT kinds FROM folders WHERE id=?1", [id], |r| r.get(0)).optional()?;
        Ok(stored.map(|kinds| decode_kinds(kinds.as_deref())))
    }

    /// Replaces a folder's kinds (error `kinds_empty` if none). Returns false
    /// when the folder does not exist.
    pub fn set_folder_kinds(&self, id: i64, kinds: &[FileKind]) -> Result<bool> {
        let stored = encode_kinds(kinds)?;
        Ok(self.db.execute("UPDATE folders SET kinds=?2 WHERE id=?1", params![id, stored])? == 1)
    }

    /// Catalogued paths (indexed or failed) of a folder whose kind is not
    /// in `kinds` — records to drop after the selection changed.
    pub fn paths_outside_kinds(&self, folder_id: i64, kinds: &[FileKind]) -> Result<Vec<String>> {
        Ok(self
            .file_paths(folder_id)?
            .into_iter()
            .filter(|path| FileKind::from_path(Path::new(path)).map_or(true, |kind| !kinds.contains(&kind)))
            .collect())
    }

    pub fn remove_folder(&self, id: i64) -> Result<Vec<String>> {
        let paths = self.file_paths(id)?;
        self.db.execute("DELETE FROM folders WHERE id=?1", [id])?;
        Ok(paths)
    }

    pub fn folders(&self) -> Result<Vec<FolderInfo>> {
        let mut statement = self.db.prepare(
            // file_count is the last scan's on-disk count, kept current by
            // the watcher and kind purges; never shown below what is recorded.
            "SELECT f.id, f.path,
               MAX(f.file_count, (SELECT COUNT(*) FROM files WHERE folder_id=f.id)
                                 + (SELECT COUNT(*) FROM failures WHERE folder_id=f.id)),
               f.last_scan,
               (SELECT COUNT(*) FROM files WHERE folder_id=f.id),
               (SELECT COUNT(*) FROM failures WHERE folder_id=f.id),
               (SELECT COUNT(*) FROM files WHERE folder_id=f.id AND low_confidence_pages>0),
               (SELECT COUNT(*) FROM files WHERE folder_id=f.id AND failed_pages>0),
               f.kinds,
               (SELECT COUNT(*) FROM files WHERE folder_id=f.id AND embedded=0)
             FROM folders f ORDER BY f.path COLLATE NOCASE",
        )?;
        let rows = statement.query_map([], |r| {
            Ok(FolderInfo {
                id: r.get(0)?,
                path: r.get(1)?,
                file_count: r.get(2)?,
                last_scan: r.get(3)?,
                indexed_count: r.get(4)?,
                failed_count: r.get(5)?,
                low_confidence_count: r.get(6)?,
                partial_count: r.get(7)?,
                missing: false,
                kinds: decode_kinds(r.get::<_, Option<String>>(8)?.as_deref()),
                pending_embed_count: r.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn folder_ids(&self) -> Result<Vec<(i64, String)>> {
        let mut statement = self.db.prepare("SELECT id, path FROM folders ORDER BY id")?;
        let rows = statement.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn finish_scan(&self, id: i64, file_count: i64, at: i64) -> Result<()> {
        self.db
            .execute("UPDATE folders SET file_count=?2, last_scan=?3 WHERE id=?1", params![id, file_count, at])?;
        Ok(())
    }

    /// Shifts a folder's on-disk file count between full scans (watcher
    /// adds/removes, kind purges); never below zero.
    pub fn adjust_file_count(&self, id: i64, delta: i64) -> Result<()> {
        if delta != 0 {
            self.db
                .execute("UPDATE folders SET file_count=MAX(0, file_count+?2) WHERE id=?1", params![id, delta])?;
        }
        Ok(())
    }

    /// True when the path has any record, indexed or failed.
    pub fn knows(&self, path: &str) -> Result<bool> {
        let found: Option<i64> = self
            .db
            .query_row(
                "SELECT 1 FROM files WHERE path=?1 UNION ALL SELECT 1 FROM failures WHERE path=?1 LIMIT 1",
                [path],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    pub fn file_paths(&self, folder_id: i64) -> Result<Vec<String>> {
        let mut statement =
            self.db.prepare("SELECT path FROM files WHERE folder_id=?1 UNION SELECT path FROM failures WHERE folder_id=?1")?;
        let rows = statement.query_map([folder_id], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Known paths (indexed or failed) equal to `path` or below it as a
    /// directory, using the same case-insensitive key as folder nesting.
    pub fn paths_under(&self, folder_id: i64, path: &str) -> Result<Vec<String>> {
        let prefix = normalize_folder(path);
        Ok(self
            .file_paths(folder_id)?
            .into_iter()
            .filter(|known| normalize_folder(known).starts_with(&prefix))
            .collect())
    }

    /// True when the file is already indexed or failed with this exact
    /// size+mtime. An indexed file with OCR pages is current only if they
    /// were read with `ocr_language`, the current document language.
    pub fn is_current(&self, path: &str, size: i64, modified: i64, ocr_language: &str) -> Result<bool> {
        let indexed: Option<i64> = self
            .db
            .query_row(
                "SELECT 1 FROM files WHERE path=?1 AND size=?2 AND modified=?3 AND (ocr_pages=0 OR ocr_lang IS ?4)",
                params![path, size, modified, ocr_language],
                |r| r.get(0),
            )
            .optional()?;
        let failed: Option<i64> = self
            .db
            .query_row(
                "SELECT 1 FROM failures WHERE path=?1 AND size=?2 AND modified=?3 AND version=?4",
                params![path, size, modified, EXTRACT_VERSION],
                |r| r.get(0),
            )
            .optional()?;
        Ok(indexed.is_some() || failed.is_some())
    }

    /// Stores the file record, its pages and its chunk embeddings in one
    /// transaction. `chunks` is `(page, vector)` in chunk order;
    /// `copy_chunks_from` clones another file's stored chunks instead
    /// (identical-bytes reuse) — and only after re-checking inside the same
    /// transaction that the donor's `content_hash` still matches this file's:
    /// a parallel worker may have re-stored or removed the donor after the
    /// caller read its pages, and stale vectors must not be cloned. `embedded`
    /// becomes 1 whenever the file ends up with chunks *or* with nothing to
    /// embed (no readable text); a refused copy or a skipped/cancelled embed
    /// keeps 0 so the backfill pass picks the file up. Returns whether the
    /// donor's chunks were actually cloned (false also when there was no
    /// donor).
    pub fn store(
        &mut self,
        record: &FileRecord,
        pages: &[PageText],
        chunks: &[(usize, Vec<f32>)],
        copy_chunks_from: Option<&str>,
        embedded: bool,
        at: i64,
    ) -> Result<bool> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM failures WHERE path=?1", [&record.path])?;
        tx.execute("DELETE FROM files WHERE path=?1", [&record.path])?;
        // A requested copy happens only if the donor still stores identical
        // content at commit time; a refused copy leaves the file unembedded
        // for the caller's fresh embed or the backfill pass.
        let mut cloned = false;
        if let Some(source) = copy_chunks_from {
            let unchanged: Option<String> = tx
                .query_row("SELECT content_hash FROM files WHERE path=?1", [source], |r| r.get(0))
                .optional()?
                .flatten();
            cloned = unchanged.as_deref() == record.content_hash.as_deref();
        }
        let embedded = embedded && (copy_chunks_from.is_none() || cloned);
        tx.execute(
            "INSERT INTO files(path, folder_id, kind, size, modified, page_count, ocr_pages,
                               low_confidence_pages, failed_pages, indexed_at, content_hash, embedded, ocr_lang)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            params![
                record.path,
                record.folder_id,
                record.kind.as_str(),
                record.size,
                record.modified,
                record.page_count,
                record.ocr_pages,
                record.low_confidence_pages,
                record.failed_pages,
                at,
                record.content_hash,
                embedded as i64,
                record.ocr_language.as_deref().filter(|_| record.ocr_pages > 0),
            ],
        )?;
        {
            let mut insert =
                tx.prepare("INSERT INTO pages(path, idx, text, source, confidence, error, chars) VALUES (?1,?2,?3,?4,?5,?6,?7)")?;
            for page in pages {
                insert.execute(params![
                    record.path,
                    page.index as i64,
                    page.text,
                    source_name(page.source),
                    page.confidence,
                    page.error,
                    page.text.chars().count() as i64,
                ])?;
            }
        }
        if cloned {
            let source = copy_chunks_from.expect("cloned implies a donor");
            tx.execute(
                "INSERT INTO chunks(path, idx, page, vector)
                 SELECT ?1, idx, page, vector FROM chunks WHERE path=?2",
                params![record.path, source],
            )?;
        } else {
            let mut insert = tx.prepare("INSERT INTO chunks(path, idx, page, vector) VALUES (?1,?2,?3,?4)")?;
            for (idx, (page, vector)) in chunks.iter().enumerate() {
                insert.execute(params![record.path, idx as i64, *page as i64, vector_blob(vector)])?;
            }
        }
        tx.commit()?;
        Ok(cloned)
    }

    /// Files whose chunk embeddings are missing (never embedded, or the
    /// embed pass was cancelled) — the backfill pass works through these.
    pub fn unembedded_paths(&self) -> Result<Vec<String>> {
        let mut statement = self.db.prepare("SELECT path FROM files WHERE embedded=0 ORDER BY path")?;
        let rows = statement.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// How many files wait for the embedding backfill (`unembedded_paths`).
    pub fn unembedded_count(&self) -> Result<usize> {
        let n: i64 = self.db.query_row("SELECT COUNT(*) FROM files WHERE embedded=0", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    /// Stored chunk vectors of one file as `(page, f32 vector)`.
    pub fn chunks(&self, path: &str) -> Result<Vec<(usize, Vec<f32>)>> {
        let mut statement = self.db.prepare("SELECT page, vector FROM chunks WHERE path=?1 ORDER BY idx")?;
        let rows = statement.query_map([path], |r| {
            let blob: Vec<u8> = r.get(1)?;
            let vector: Vec<f32> = blob
                .chunks_exact(4)
                .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("4 bytes")))
                .collect();
            Ok((r.get::<_, i64>(0)? as usize, vector))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Every file's stored chunks (VectorIndex warm-up after open).
    pub fn all_chunks(&self) -> Result<Vec<(String, FileKind, i64, usize, Vec<f32>)>> {
        let mut statement = self.db.prepare(
            "SELECT c.path, f.kind, f.modified, c.page, c.vector
               FROM chunks c JOIN files f ON f.path = c.path ORDER BY c.path, c.idx",
        )?;
        let rows = statement.query_map([], |r| {
            let kind: String = r.get(1)?;
            let blob: Vec<u8> = r.get(4)?;
            let vector: Vec<f32> = blob
                .chunks_exact(4)
                .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("4 bytes")))
                .collect();
            Ok((
                r.get::<_, String>(0)?,
                FileKind::parse(&kind).unwrap_or(FileKind::Pdf),
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)? as usize,
                vector,
            ))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Marks a backfilled file's chunks complete (or clears them on retry).
    pub fn store_chunks(&mut self, path: &str, chunks: &[(usize, Vec<f32>)]) -> Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM chunks WHERE path=?1", [path])?;
        {
            let mut insert = tx.prepare("INSERT INTO chunks(path, idx, page, vector) VALUES (?1,?2,?3,?4)")?;
            for (idx, (page, vector)) in chunks.iter().enumerate() {
                insert.execute(params![path, idx as i64, *page as i64, vector_blob(vector)])?;
            }
        }
        tx.execute("UPDATE files SET embedded=1 WHERE path=?1", [path])?;
        tx.commit()?;
        Ok(())
    }

    pub fn record_failure(&self, folder_id: i64, path: &str, size: i64, modified: i64, error: &str, at: i64) -> Result<()> {
        self.db.execute("DELETE FROM files WHERE path=?1", [path])?;
        self.db.execute(
            "INSERT OR REPLACE INTO failures(path, folder_id, size, modified, error, at, version) VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![path, folder_id, size, modified, error, at, EXTRACT_VERSION],
        )?;
        Ok(())
    }

    /// "Tekrar dene": makes every unreadable and partly read file of the
    /// folder (or of all folders) not current, so the next scan reads it
    /// again. Partly read files keep their searchable pages until then.
    /// Returns the paths, for an immediate targeted re-read.
    pub fn retry_failures(&self, folder_id: Option<i64>) -> Result<Vec<String>> {
        let mut paths: Vec<String> = Vec::new();
        {
            let mut statement = self.db.prepare(
                "SELECT path FROM failures WHERE ?1 IS NULL OR folder_id=?1
                 UNION SELECT path FROM files WHERE failed_pages>0 AND (?1 IS NULL OR folder_id=?1)",
            )?;
            for path in statement.query_map([folder_id], |r| r.get::<_, String>(0))? {
                paths.push(path?);
            }
        }
        self.db.execute("UPDATE failures SET version=-1 WHERE ?1 IS NULL OR folder_id=?1", [folder_id])?;
        self.db.execute("UPDATE files SET modified=-1 WHERE failed_pages>0 AND (?1 IS NULL OR folder_id=?1)", [folder_id])?;
        Ok(paths)
    }

    /// Indexed files whose file name contains every folded query word, with
    /// their kind and modification time, for file-name search.
    pub fn files_named(&self, words: &[String]) -> Result<Vec<(String, FileKind, i64)>> {
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let mut statement = self.db.prepare_cached("SELECT path, kind, modified FROM files")?;
        let mut out = Vec::new();
        let rows = statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        for row in rows {
            let (path, kind, modified) = row?;
            let name = path.rsplit(['\\', '/']).next().unwrap_or(&path);
            let folded = crate::turkish::ascii_fold(&crate::turkish::turkish_lower(name));
            if words.iter().all(|w| folded.contains(w.as_str())) {
                out.push((path, FileKind::parse(&kind).unwrap_or(FileKind::Pdf), modified));
            }
        }
        Ok(out)
    }

    pub fn forget(&self, path: &str) -> Result<()> {
        self.db.execute("DELETE FROM files WHERE path=?1", [path])?;
        self.db.execute("DELETE FROM failures WHERE path=?1", [path])?;
        Ok(())
    }

    /// Unreadable files, then partially read files (first failing page's
    /// error), newest first.
    pub fn failures(&self, folder_id: Option<i64>) -> Result<Vec<FailureInfo>> {
        let mut statement = self.db.prepare(
            "SELECT path, error, at, NULL, NULL FROM failures WHERE ?1 IS NULL OR folder_id=?1
             UNION ALL
             SELECT f.path,
                    COALESCE((SELECT error FROM pages WHERE path=f.path AND error IS NOT NULL ORDER BY idx LIMIT 1), ''),
                    f.indexed_at, f.failed_pages, f.page_count
               FROM files f WHERE f.failed_pages>0 AND (?1 IS NULL OR f.folder_id=?1)
             ORDER BY 3 DESC, 1",
        )?;
        let rows = statement.query_map([folder_id], |r| {
            let error: String = r.get(1)?;
            Ok(FailureInfo {
                path: r.get(0)?,
                reason: failure_reason(&error),
                error,
                at: r.get(2)?,
                failed_pages: r.get(3)?,
                page_count: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn file(&self, path: &str) -> Result<Option<FileRecord>> {
        Ok(self
            .db
            .query_row(
                "SELECT path, folder_id, kind, size, modified, page_count, ocr_pages, low_confidence_pages, failed_pages,
                        content_hash, ocr_lang
                   FROM files WHERE path=?1",
                [path],
                |r| {
                    let kind: String = r.get(2)?;
                    Ok(FileRecord {
                        path: r.get(0)?,
                        folder_id: r.get(1)?,
                        kind: FileKind::parse(&kind).unwrap_or(FileKind::Pdf),
                        size: r.get(3)?,
                        modified: r.get(4)?,
                        page_count: r.get(5)?,
                        ocr_pages: r.get(6)?,
                        low_confidence_pages: r.get(7)?,
                        failed_pages: r.get(8)?,
                        content_hash: r.get(9)?,
                        ocr_language: r.get(10)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn pages(&self, path: &str) -> Result<Vec<PageText>> {
        let mut statement =
            self.db.prepare("SELECT idx, text, source, confidence, error FROM pages WHERE path=?1 ORDER BY idx")?;
        let rows = statement.query_map([path], |r| {
            let source: String = r.get(2)?;
            Ok(PageText {
                index: r.get::<_, i64>(0)? as usize,
                text: r.get(1)?,
                source: parse_source(&source),
                confidence: r.get(3)?,
                error: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Another fully read file with identical bytes, if any, with its
    /// embedding status — identical files reuse stored pages AND stored
    /// chunk vectors. Files with failed pages are not reused (a retry must
    /// stay possible); an embedded source is preferred so the copy brings
    /// its vectors along. OCR pages read with another language than
    /// `ocr_language` (the current setting) are not reused either.
    pub fn reusable_source(
        &self,
        content_hash: &str,
        kind: FileKind,
        except_path: &str,
        ocr_language: &str,
    ) -> Result<Option<(String, bool)>> {
        Ok(self
            .db
            .query_row(
                "SELECT path, embedded FROM files
                  WHERE content_hash=?1 AND kind=?2 AND failed_pages=0 AND path<>?3
                    AND (ocr_pages=0 OR ocr_lang IS ?4)
                  ORDER BY embedded DESC LIMIT 1",
                params![content_hash, kind.as_str(), except_path, ocr_language],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)),
            )
            .optional()?)
    }

    /// Every successfully indexed file (used to rebuild the search index).
    pub fn indexed_paths(&self) -> Result<Vec<String>> {
        let mut statement = self.db.prepare("SELECT path FROM files ORDER BY path")?;
        let rows = statement.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn page_text(&self, path: &str, index: usize) -> Result<Option<String>> {
        Ok(self
            .db
            .query_row("SELECT text FROM pages WHERE path=?1 AND idx=?2", params![path, index as i64], |r| r.get(0))
            .optional()?)
    }

    /// Character count of one stored page, without decoding the text.
    /// Served by the `pages_chars` covering index (migration 6): asking for
    /// `chars` alone never touches the row payload, while even
    /// `COALESCE(chars, length(text))` read the whole multi-MB text.
    pub fn page_chars(&self, path: &str, index: usize) -> Result<Option<usize>> {
        Ok(self
            .db
            .query_row("SELECT chars FROM pages WHERE path=?1 AND idx=?2", params![path, index as i64], |r| {
                r.get::<_, Option<i64>>(0)
            })
            .optional()?
            .flatten()
            .map(|n| n.max(0) as usize))
    }

    /// Page text capped at `max_bytes`: big stored pages (multi-MB logs,
    /// mappings) are only ever needed in full for the document view; a
    /// snippet never sees past it, so the search reads a bounded prefix.
    /// Page text capped at `max_bytes`, returned with `truncated` when the
    /// stored text is longer. Read through the incremental blob API:
    /// `substr(text,1,N)` costs a *full* row read in SQLite (measured: 80 ms
    /// for a 15 MB page — as slow as `SELECT text`), while `blob_open` +
    /// `read_at` touches only the B-tree pages holding the prefix.
    pub fn page_text_prefix(&self, path: &str, index: usize, max_bytes: usize) -> Result<Option<(String, bool)>> {
        self.page_text_slice(path, index, 0, max_bytes)
    }


    /// Incremental read of `[from, from+max)` bytes of one page's text.
    fn page_text_slice(&self, path: &str, index: usize, from: usize, max: usize) -> Result<Option<(String, bool)>> {
        let Some(rowid) = self
            .db
            .query_row("SELECT rowid FROM pages WHERE path=?1 AND idx=?2", params![path, index as i64], |r| r.get::<_, i64>(0))
            .optional()?
        else {
            return Ok(None);
        };
        let blob = self.db.blob_open("main", "pages", "text", rowid, true)?;
        let size = blob.len();
        if from >= size {
            return Ok(Some((String::new(), false)));
        }
        // A byte-range read may start inside a multi-byte character; skip
        // those continuation bytes so the returned text is valid UTF-8.
        let mut start = from;
        let mut probe = [0u8; 4];
        while start < size {
            let n = blob.read_at(&mut probe[..1], start)?;
            if n == 0 || (probe[0] & 0xC0) != 0x80 {
                break;
            }
            start += 1;
        }
        if start >= size {
            return Ok(Some((String::new(), false)));
        }
        let mut buf = vec![0u8; (size - start).min(max)];
        let n = blob.read_at(&mut buf, start)?;
        buf.truncate(n);
        // Stored values are valid UTF-8; a byte cut mid-codepoint at the end
        // is trimmed (error_len is always the truncated tail sequence).
        let valid = match std::str::from_utf8(&buf) {
            Ok(_) => buf.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(e) => bail!("stored page text is not UTF-8: {e}"),
        };
        buf.truncate(valid);
        let text = String::from_utf8(buf).map_err(|e| anyhow::anyhow!("stored page text is not UTF-8: {e}"))?;
        let truncated = start + text.len() < size;
        Ok(Some((text, truncated)))
    }

    /// A stored user setting (see migration 9), if set.
    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.db.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        self.db.execute(
            "INSERT INTO settings(key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Indexed files whose OCR pages were read with another language than
    /// `ocr_language`: the scans a language change reads again.
    pub fn files_to_reread(&self, ocr_language: &str) -> Result<i64> {
        Ok(self.db.query_row(
            "SELECT COUNT(*) FROM files WHERE ocr_pages>0 AND ocr_lang IS NOT ?1",
            [ocr_language],
            |r| r.get(0),
        )?)
    }

    pub fn document_count(&self) -> Result<i64> {
        Ok(self.db.query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))?)
    }

    pub fn folder_for(&self, path: &str) -> Result<Option<(i64, String)>> {
        let normalized = normalize_folder(path);
        Ok(self
            .folder_ids()?
            .into_iter()
            .find(|(_, folder)| is_within(&normalized, &normalize_folder(folder))))
    }
}

/// Brings an existing catalog up to the current schema without touching its
/// data. Each step runs in its own transaction together with the version bump.
fn migrate(db: &mut Connection) -> Result<()> {
    let version = db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))? as usize;
    if version > MIGRATIONS.len() {
        bail!(
            "catalog.db schema {version} is newer than this build supports ({}); refusing to modify it",
            MIGRATIONS.len()
        );
    }
    for (step, sql) in MIGRATIONS.iter().enumerate().skip(version) {
        let tx = db.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", (step + 1) as i64)?;
        tx.commit()?;
    }
    Ok(())
}

/// Validates a kind selection for storage: `NULL` for the default selection
/// (every kind but `Log`); anything else is stored as an explicit list.
fn encode_kinds(kinds: &[FileKind]) -> Result<Option<String>> {
    if kinds.is_empty() {
        bail!("kinds_empty");
    }
    if FileKind::ALL.iter().all(|kind| kinds.contains(kind) == FileKind::DEFAULT.contains(kind)) {
        return Ok(None);
    }
    let selected: Vec<&str> = FileKind::ALL.iter().filter(|k| kinds.contains(k)).map(|k| k.as_str()).collect();
    Ok(Some(selected.join(",")))
}

fn decode_kinds(stored: Option<&str>) -> Vec<FileKind> {
    let kinds: Vec<FileKind> = stored
        .map(|value| value.split(',').filter_map(FileKind::parse).collect())
        .unwrap_or_default();
    if kinds.is_empty() {
        FileKind::DEFAULT.to_vec()
    } else {
        kinds
    }
}

fn source_name(source: TextSource) -> &'static str {
    match source {
        TextSource::TextLayer => "text_layer",
        TextSource::Ocr => "ocr",
        TextSource::Native => "native",
    }
}

fn parse_source(value: &str) -> TextSource {
    match value {
        "ocr" => TextSource::Ocr,
        "native" => TextSource::Native,
        _ => TextSource::TextLayer,
    }
}

/// f32 vector → little-endian BLOB (no extra crate; matches `chunks`/`all_chunks` decode).
fn vector_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Case-insensitive (Windows) path key with a trailing separator.
pub fn normalize_folder(path: &str) -> String {
    let mut value = path.replace('/', "\\").to_lowercase();
    if !value.ends_with('\\') {
        value.push('\\');
    }
    value
}

pub fn is_within(child: &str, parent: &str) -> bool {
    child.starts_with(parent) && child != parent
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_and_nested_folders_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = Catalog::open(&dir.path().join("c.db")).unwrap();
        catalog.add_folder(r"C:\Arşiv").unwrap();
        assert_eq!(catalog.add_folder(r"c:\arşiv\").unwrap_err().to_string(), "folder_duplicate");
        assert_eq!(catalog.add_folder(r"C:\Arşiv\Faturalar").unwrap_err().to_string(), "folder_nested");
        assert_eq!(catalog.add_folder(r"C:\").unwrap_err().to_string(), "folder_nested");
        catalog.add_folder(r"C:\Arşiv2").unwrap();
    }

    #[test]
    fn chunkless_files_with_text_go_back_to_the_backfill_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("c.db");
        let mut catalog = Catalog::open(&db).unwrap();
        let folder = catalog.add_folder(r"C:\Arşiv").unwrap();
        let page = |text: &str, error: Option<&str>| PageText {
            index: 0,
            text: text.into(),
            source: TextSource::Ocr,
            confidence: None,
            error: error.map(Into::into),
        };
        let record = |name: &str, kind: FileKind| FileRecord {
            path: format!(r"C:\Arşiv\{name}"),
            folder_id: folder,
            kind,
            size: 1,
            modified: 1,
            page_count: 1,
            ocr_pages: 0,
            low_confidence_pages: 0,
            failed_pages: 0,
            content_hash: None,
            ocr_language: None,
        };
        let vector = vec![(0usize, vec![1.0f32, 0.0])];
        // Bug victim: text, no chunks, embedded=1.
        catalog.store(&record("iptal.txt", FileKind::Text), &[page("kira notu", None)], &[], None, true, 1).unwrap();
        // Done for real: has chunks.
        catalog.store(&record("tamam.txt", FileKind::Text), &[page("kira notu", None)], &vector, None, true, 1).unwrap();
        // No text: blank scan, whitespace-only OCR, failed page.
        catalog.store(&record("bos.png", FileKind::Image), &[page("", None)], &[], None, true, 1).unwrap();
        catalog.store(&record("bosluk.png", FileKind::Image), &[page(" \n\t ", None)], &[], None, true, 1).unwrap();
        catalog.store(&record("bozuk.png", FileKind::Image), &[page("x", Some("ocr failed"))], &[], None, true, 1).unwrap();
        // Text-bearing image victim.
        catalog.store(&record("yazili.png", FileKind::Image), &[page("fatura", None)], &[], None, true, 1).unwrap();
        assert!(catalog.unembedded_paths().unwrap().is_empty());
        // Re-run step 7 as an older catalog would on upgrade.
        catalog.db.pragma_update(None, "user_version", 6i64).unwrap();
        // Later steps run again too; undo steps 8 and 9 so they can.
        catalog
            .db
            .execute_batch(
                "ALTER TABLE failures DROP COLUMN version;
                 ALTER TABLE files DROP COLUMN ocr_lang;
                 DROP TABLE settings;",
            )
            .unwrap();
        drop(catalog);
        let mut catalog = Catalog::open(&db).unwrap();
        assert_eq!(catalog.schema_version().unwrap(), MIGRATIONS.len());
        assert_eq!(
            catalog.unembedded_paths().unwrap(),
            [r"C:\Arşiv\iptal.txt".to_string(), r"C:\Arşiv\yazili.png".to_string()]
        );
        // Once embedded they stay embedded: the step does not run again.
        catalog.store_chunks(r"C:\Arşiv\iptal.txt", &vector).unwrap();
        catalog.store_chunks(r"C:\Arşiv\yazili.png", &vector).unwrap();
        drop(catalog);
        let catalog = Catalog::open(&db).unwrap();
        assert!(catalog.unembedded_paths().unwrap().is_empty());
    }

    /// the folder summary and the post-cancel notice count the
    /// files whose semantic index is pending — embedded=0 only.
    #[test]
    fn pending_embed_counts_follow_the_embedded_flag_per_folder() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(&dir.path().join("c.db")).unwrap();
        let a = catalog.add_folder(r"C:\A").unwrap();
        let b = catalog.add_folder(r"C:\B").unwrap();
        let page = PageText { index: 0, text: "kira notu".into(), source: TextSource::Native, confidence: None, error: None };
        let record = |folder: i64, path: &str| FileRecord {
            path: path.into(),
            folder_id: folder,
            kind: FileKind::Text,
            size: 1,
            modified: 1,
            page_count: 1,
            ocr_pages: 0,
            low_confidence_pages: 0,
            failed_pages: 0,
            content_hash: None,
            ocr_language: None,
        };
        let vector = vec![(0usize, vec![1.0f32, 0.0])];
        catalog.store(&record(a, r"C:\A\1.txt"), &[page.clone()], &[], None, false, 1).unwrap();
        catalog.store(&record(a, r"C:\A\2.txt"), &[page.clone()], &[], None, false, 1).unwrap();
        catalog.store(&record(a, r"C:\A\3.txt"), &[page.clone()], &vector, None, true, 1).unwrap();
        catalog.store(&record(b, r"C:\B\1.txt"), &[page.clone()], &vector, None, true, 1).unwrap();
        let counts = |catalog: &Catalog| -> Vec<(String, i64)> {
            catalog.folders().unwrap().into_iter().map(|f| (f.path, f.pending_embed_count)).collect()
        };
        assert_eq!(catalog.unembedded_count().unwrap(), 2);
        assert_eq!(counts(&catalog), [(r"C:\A".to_string(), 2), (r"C:\B".to_string(), 0)]);
        catalog.store_chunks(r"C:\A\1.txt", &vector).unwrap();
        assert_eq!(catalog.unembedded_count().unwrap(), 1);
        assert_eq!(counts(&catalog), [(r"C:\A".to_string(), 1), (r"C:\B".to_string(), 0)]);
        let json = serde_json::to_value(&catalog.folders().unwrap()[0]).unwrap();
        assert_eq!(json["pendingEmbedCount"], 1);
    }

    /// Exact schema shipped in 0.1.0 (before `user_version` was used).
    const SCHEMA_V0: &str = "
        CREATE TABLE folders(
          id INTEGER PRIMARY KEY, path TEXT NOT NULL UNIQUE,
          file_count INTEGER NOT NULL DEFAULT 0, last_scan INTEGER);
        CREATE TABLE files(
          path TEXT PRIMARY KEY, folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
          kind TEXT NOT NULL, size INTEGER NOT NULL, modified INTEGER NOT NULL,
          page_count INTEGER NOT NULL, ocr_pages INTEGER NOT NULL, low_confidence_pages INTEGER NOT NULL);
        CREATE TABLE pages(
          path TEXT NOT NULL REFERENCES files(path) ON DELETE CASCADE, idx INTEGER NOT NULL,
          text TEXT NOT NULL, source TEXT NOT NULL, confidence REAL,
          PRIMARY KEY(path, idx));
        CREATE TABLE failures(
          path TEXT PRIMARY KEY, folder_id INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
          size INTEGER NOT NULL, modified INTEGER NOT NULL, error TEXT NOT NULL, at INTEGER NOT NULL);";

    fn record(path: &str, pages: i64, failed: i64) -> FileRecord {
        FileRecord {
            path: path.into(),
            folder_id: 1,
            kind: FileKind::Pdf,
            size: 10,
            modified: 20,
            page_count: pages,
            ocr_pages: pages - failed,
            low_confidence_pages: 0,
            failed_pages: failed,
            content_hash: None,
            ocr_language: Some("tur".into()),
        }
    }

    #[test]
    fn a_v0_catalog_is_migrated_in_place_without_losing_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        {
            let db = Connection::open(&path).unwrap();
            db.execute_batch(SCHEMA_V0).unwrap();
            db.execute_batch(
                "INSERT INTO folders(id, path, file_count, last_scan) VALUES (1, 'C:\\Arşiv', 1, 5);
                 INSERT INTO files VALUES ('C:\\Arşiv\\a.pdf', 1, 'pdf', 10, 20, 1, 1, 0);
                 INSERT INTO pages VALUES ('C:\\Arşiv\\a.pdf', 0, 'kira sözleşmesi', 'ocr', 0.9);
                 INSERT INTO failures VALUES ('C:\\Arşiv\\b.pdf', 1, 3, 4, 'Cannot open PDF', 6);",
            )
            .unwrap();
        }
        let catalog = Catalog::open(&path).unwrap();
        assert_eq!(catalog.schema_version().unwrap(), MIGRATIONS.len());
        let file = catalog.file(r"C:\Arşiv\a.pdf").unwrap().unwrap();
        assert_eq!((file.page_count, file.failed_pages), (1, 0));
        assert_eq!(file.ocr_language.as_deref(), Some("tur"), "older builds read Turkish only");
        let pages = catalog.pages(r"C:\Arşiv\a.pdf").unwrap();
        assert_eq!(pages[0].text, "kira sözleşmesi");
        assert_eq!(pages[0].error, None);
        assert_eq!(catalog.page_chars(r"C:\Arşiv\a.pdf", 0).unwrap(), Some("kira sözleşmesi".chars().count()));
        let failures = catalog.failures(None).unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].failed_pages, None);
        drop(catalog);
        // Re-opening is a no-op.
        assert_eq!(Catalog::open(&path).unwrap().schema_version().unwrap(), MIGRATIONS.len());
    }

    #[test]
    fn identical_content_reuses_fully_read_pages_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(&dir.path().join("c.db")).unwrap();
        catalog.add_folder(r"C:\Arşiv").unwrap();
        let pages = [PageText::new(0, "taranmış fatura".into(), TextSource::Ocr, Some(0.9))];
        let original = FileRecord { content_hash: Some("abc".into()), ..record(r"C:\Arşiv\a.png", 1, 0) };
        catalog.store(&FileRecord { kind: FileKind::Image, ..original }, &pages, &[], None, true, 1).unwrap();
        let (source, source_embedded) = catalog.reusable_source("abc", FileKind::Image, r"C:\Arşiv\kopya.png", "tur").unwrap().unwrap();
        assert_eq!(source, r"C:\Arşiv\a.png");
        assert!(source_embedded, "stored with embedded=true");
        let reused = catalog.pages(&source).unwrap();
        assert_eq!(reused[0].text, "taranmış fatura");
        assert!(catalog.reusable_source("abc", FileKind::Image, r"C:\Arşiv\a.png", "tur").unwrap().is_none(), "not itself");
        assert!(catalog.reusable_source("abc", FileKind::Pdf, "x", "tur").unwrap().is_none(), "kind must match");

        let partial = [PageText::new(0, "a".into(), TextSource::Ocr, Some(0.9)), PageText::failed(1, "timeout".into())];
        let broken = FileRecord { content_hash: Some("def".into()), ..record(r"C:\Arşiv\b.pdf", 2, 1) };
        catalog.store(&broken, &partial, &[], None, true, 1).unwrap();
        assert!(catalog.reusable_source("def", FileKind::Pdf, "y", "tur").unwrap().is_none(), "failed pages are retried");
    }

    /// Changing the document language makes every scan read with another
    /// language stale (read again, never reused as a copy's donor); files
    /// without OCR pages stay current whatever the language.
    #[test]
    fn scans_read_with_another_language_are_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(&dir.path().join("c.db")).unwrap();
        catalog.add_folder(r"C:\Arşiv").unwrap();
        let scan = [PageText::new(0, "taranmış fatura".into(), TextSource::Ocr, Some(0.9))];
        let typed = [PageText::new(0, "yazılmış not".into(), TextSource::Native, None)];
        let record_scan = FileRecord { kind: FileKind::Image, content_hash: Some("abc".into()), ..record(r"C:\Arşiv\a.png", 1, 0) };
        catalog.store(&record_scan, &scan, &[], None, true, 1).unwrap();
        let record_text = FileRecord {
            kind: FileKind::Text,
            ocr_pages: 0,
            ocr_language: None,
            ..record(r"C:\Arşiv\not.txt", 1, 0)
        };
        catalog.store(&record_text, &typed, &[], None, true, 1).unwrap();

        assert!(catalog.is_current(r"C:\Arşiv\a.png", 10, 20, "tur").unwrap());
        assert!(!catalog.is_current(r"C:\Arşiv\a.png", 10, 20, "tur+eng").unwrap());
        assert!(!catalog.is_current(r"C:\Arşiv\a.png", 10, 20, "eng").unwrap());
        assert!(catalog.is_current(r"C:\Arşiv\not.txt", 10, 20, "eng").unwrap());
        assert_eq!(catalog.files_to_reread("tur").unwrap(), 0);
        assert_eq!(catalog.files_to_reread("eng").unwrap(), 1);
        assert!(catalog.reusable_source("abc", FileKind::Image, r"C:\Arşiv\b.png", "tur").unwrap().is_some());
        assert!(catalog.reusable_source("abc", FileKind::Image, r"C:\Arşiv\b.png", "eng").unwrap().is_none());

        // Read again with English: current for English, and the stored
        // language is the one the pages were read with.
        let reread = FileRecord { ocr_language: Some("eng".into()), ..record_scan };
        catalog.store(&reread, &scan, &[], None, true, 2).unwrap();
        assert!(catalog.is_current(r"C:\Arşiv\a.png", 10, 20, "eng").unwrap());
        assert_eq!(catalog.file(r"C:\Arşiv\a.png").unwrap().unwrap().ocr_language.as_deref(), Some("eng"));
    }

    #[test]
    fn settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.db");
        let catalog = Catalog::open(&path).unwrap();
        assert_eq!(catalog.setting("ocr_language").unwrap(), None);
        catalog.set_setting("ocr_language", "eng").unwrap();
        catalog.set_setting("ocr_language", "tur+eng").unwrap();
        drop(catalog);
        assert_eq!(Catalog::open(&path).unwrap().setting("ocr_language").unwrap().as_deref(), Some("tur+eng"));
    }

    /// A copy's chunk clone is only as good as the donor's content at commit
    /// time: a parallel worker can re-store the donor between the caller's
    /// `reusable_source` lookup and `store`. The in-transaction hash check
    /// must then refuse the copy and leave the file unembedded for backfill.
    #[test]
    fn a_changed_donor_does_not_donate_its_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(&dir.path().join("c.db")).unwrap();
        catalog.add_folder(r"C:\Arşiv").unwrap();
        let pages = [PageText::new(0, "taranmış fatura".into(), TextSource::Ocr, Some(0.9))];
        let chunks = [(0usize, vec![0.5f32, 0.5])];
        catalog
            .store(
                &FileRecord { content_hash: Some("abc".into()), kind: FileKind::Image, ..record(r"C:\Arşiv\a.png", 1, 0) },
                &pages,
                &chunks,
                None,
                true,
                1,
            )
            .unwrap();

        // Commit-time identity still matches: the chunks are cloned.
        let copy = FileRecord { content_hash: Some("abc".into()), kind: FileKind::Image, ..record(r"C:\Arşiv\b.png", 1, 0) };
        assert!(catalog.store(&copy, &pages, &[], Some(r"C:\Arşiv\a.png"), true, 2).unwrap());
        assert_eq!(catalog.chunks(r"C:\Arşiv\b.png").unwrap().len(), 1);
        assert!(!catalog.unembedded_paths().unwrap().contains(&r"C:\Arşiv\b.png".to_string()));

        // The donor is re-stored under new content before the second copy
        // commits — exactly the parallel-worker race.
        catalog
            .store(
                &FileRecord { content_hash: Some("xyz".into()), kind: FileKind::Image, ..record(r"C:\Arşiv\a.png", 1, 0) },
                &pages,
                &[],
                None,
                false,
                3,
            )
            .unwrap();
        let copy2 = FileRecord { content_hash: Some("abc".into()), kind: FileKind::Image, ..record(r"C:\Arşiv\c.png", 1, 0) };
        assert!(!catalog.store(&copy2, &pages, &[], Some(r"C:\Arşiv\a.png"), true, 4).unwrap(), "stale donor refused");
        assert!(catalog.chunks(r"C:\Arşiv\c.png").unwrap().is_empty(), "changed content cloned nothing");
        assert!(catalog.unembedded_paths().unwrap().contains(&r"C:\Arşiv\c.png".to_string()), "backfill picks it up");

        // A donor deleted outright is refused the same way.
        catalog.forget(r"C:\Arşiv\a.png").unwrap();
        let copy3 = FileRecord { content_hash: Some("abc".into()), kind: FileKind::Image, ..record(r"C:\Arşiv\d.png", 1, 0) };
        assert!(!catalog.store(&copy3, &pages, &[], Some(r"C:\Arşiv\a.png"), true, 5).unwrap());
        assert!(catalog.unembedded_paths().unwrap().contains(&r"C:\Arşiv\d.png".to_string()));
    }

    #[test]
    fn folder_kinds_default_to_all_but_logs_and_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.db");
        let catalog = Catalog::open(&path).unwrap();
        let all = catalog.add_folder(r"C:\Arşiv").unwrap();
        assert_eq!(catalog.folder_kinds(all).unwrap().unwrap(), FileKind::DEFAULT);
        let scans = catalog.add_folder_with_kinds(r"D:\Taramalar", &[FileKind::Image, FileKind::Pdf]).unwrap();
        // Stored in canonical order, whatever order they were given in.
        assert_eq!(catalog.folder_kinds(scans).unwrap().unwrap(), [FileKind::Pdf, FileKind::Image]);
        let stored: Option<String> =
            catalog.db.query_row("SELECT kinds FROM folders WHERE id=?1", [scans], |r| r.get(0)).unwrap();
        assert_eq!(stored.as_deref(), Some("pdf,image"));

        assert_eq!(catalog.add_folder_with_kinds(r"E:\Bos", &[]).unwrap_err().to_string(), "kinds_empty");
        assert_eq!(catalog.set_folder_kinds(scans, &[]).unwrap_err().to_string(), "kinds_empty");
        assert!(catalog.set_folder_kinds(scans, &FileKind::DEFAULT).unwrap());
        let stored: Option<String> =
            catalog.db.query_row("SELECT kinds FROM folders WHERE id=?1", [scans], |r| r.get(0)).unwrap();
        assert_eq!(stored, None, "the default selection is stored as NULL");
        assert!(catalog.set_folder_kinds(scans, &FileKind::ALL).unwrap());
        let stored: Option<String> =
            catalog.db.query_row("SELECT kinds FROM folders WHERE id=?1", [scans], |r| r.get(0)).unwrap();
        assert_eq!(stored.as_deref(), Some("pdf,image,word,excel,text,log"), "logs opted in explicitly");
        assert_eq!(catalog.folder_kinds(scans).unwrap().unwrap(), FileKind::ALL);
        assert!(catalog.set_folder_kinds(scans, &[FileKind::Image, FileKind::Pdf]).unwrap());
        assert!(!catalog.set_folder_kinds(99, &[FileKind::Pdf]).unwrap());
        assert_eq!(catalog.folder_kinds(99).unwrap(), None);

        catalog.set_folder_kinds(all, &[FileKind::Word, FileKind::Text]).unwrap();
        drop(catalog);
        let catalog = Catalog::open(&path).unwrap();
        let folders = catalog.folders().unwrap();
        assert_eq!(folders[0].kinds, [FileKind::Word, FileKind::Text], "kept across restarts");
        assert_eq!(folders[1].kinds, [FileKind::Pdf, FileKind::Image]);
    }

    #[test]
    fn records_outside_the_selected_kinds_are_listed_for_removal() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(&dir.path().join("c.db")).unwrap();
        let id = catalog.add_folder(r"C:\Arşiv").unwrap();
        let page = [PageText::new(0, "metin".into(), TextSource::Native, None)];
        catalog.store(&record(r"C:\Arşiv\a.pdf", 1, 0), &page, &[], None, true, 1).unwrap();
        catalog.store(&FileRecord { kind: FileKind::Text, ..record(r"C:\Arşiv\not.txt", 1, 0) }, &page, &[], None, true, 1).unwrap();
        catalog.record_failure(id, r"C:\Arşiv\bozuk.XLSX", 1, 1, "Cannot open workbook", 1).unwrap();
        let mut outside = catalog.paths_outside_kinds(id, &[FileKind::Pdf]).unwrap();
        outside.sort();
        assert_eq!(outside, [r"C:\Arşiv\bozuk.XLSX", r"C:\Arşiv\not.txt"], "failures too, extension case-insensitive");
        assert!(catalog.paths_outside_kinds(id, &FileKind::ALL).unwrap().is_empty());
    }

    #[test]
    fn a_catalog_from_a_newer_build_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("catalog.db");
        Catalog::open(&path).unwrap().db.pragma_update(None, "user_version", 999).unwrap();
        let error = Catalog::open(&path).err().expect("newer schema must be refused");
        assert!(error.to_string().contains("newer than this build"), "{error}");
    }

    #[test]
    fn partially_read_files_are_listed_with_failures() {
        let dir = tempfile::tempdir().unwrap();
        let mut catalog = Catalog::open(&dir.path().join("c.db")).unwrap();
        let folder = catalog.add_folder(r"C:\Arşiv").unwrap();
        assert_eq!(folder, 1);
        let pages = [
            PageText::new(0, "metin".into(), TextSource::TextLayer, None),
            PageText::failed(1, "Tesseract timed out after 120 s".into()),
            PageText::failed(2, "Cannot render PDF page 3".into()),
        ];
        catalog.store(&record(r"C:\Arşiv\karma.pdf", 3, 2), &pages, &[], None, true, 50).unwrap();
        catalog.store(&record(r"C:\Arşiv\temiz.pdf", 1, 0), &pages[..1], &[], None, true, 51).unwrap();
        catalog.record_failure(1, r"C:\Arşiv\bozuk.pdf", 1, 1, "Cannot open PDF", 40).unwrap();

        let failures = catalog.failures(Some(1)).unwrap();
        let summary: Vec<_> = failures.iter().map(|f| (f.path.as_str(), f.failed_pages, f.page_count)).collect();
        assert_eq!(
            summary,
            [(r"C:\Arşiv\karma.pdf", Some(2), Some(3)), (r"C:\Arşiv\bozuk.pdf", None, None)]
        );
        assert_eq!(failures[0].error, "Tesseract timed out after 120 s");
        let stored = catalog.pages(r"C:\Arşiv\karma.pdf").unwrap();
        assert_eq!(stored[2].error.as_deref(), Some("Cannot render PDF page 3"));
        let folder = &catalog.folders().unwrap()[0];
        assert_eq!((folder.indexed_count, folder.failed_count, folder.partial_count), (2, 1, 1));
    }

    #[test]
    fn failure_reasons_are_classified() {
        assert_eq!(failure_reason("Not a valid DOCX (zip) file: invalid Zip archive"), "damaged");
        assert_eq!(failure_reason("Cannot open PDF: PdfiumLibraryInternalError(PasswordError)"), "encrypted");
        assert_eq!(failure_reason("Cannot read x: Access is denied. (os error 5)"), "access_denied");
        assert_eq!(failure_reason("Text file too large: 20 MB (limit 16 MB)"), "too_large");
        assert_eq!(failure_reason("No page could be read: Tesseract timed out after 120 s"), "timeout");
        assert_eq!(failure_reason("Tesseract exited with exit code: 1"), "ocr_failed");
        assert_eq!(failure_reason("something else"), "other");
    }
}
