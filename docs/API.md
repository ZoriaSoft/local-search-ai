# Zoria Arşiv command contract (UI ↔ Rust)

Single source of truth for the Tauri boundary. TypeScript mirror: `src/api/types.ts`.
All offsets sent to the UI are **UTF-16 code-unit offsets** (JavaScript string indices).
Timestamps are Unix seconds (UTC). Errors reject the `invoke` promise with `ApiError`.

## Types

```ts
type FileKind = "pdf" | "image" | "word" | "excel" | "text" | "log";
// pdf .pdf · image .jpg .jpeg .png .tif .tiff .bmp · word .docx · excel .xlsx .xlsm
// text .txt .csv .md .json .xml · log .log (not in a new folder's default selection)

interface ApiError { code: ErrorCode; message: string }   // message: English diagnostic
type ErrorCode =
  | "folder_missing" | "folder_duplicate" | "folder_nested" | "kinds_empty"
  | "file_missing" | "not_indexed"
  | "index_busy" | "index_io" | "ocr_unavailable" | "pdf_unavailable"
  | "query_empty" | "preview_unavailable" | "update_failed"
  | "ocr_language_unavailable" | "internal";

interface FolderInfo {
  id: number; path: string;
  fileCount: number;      // supported files found at last scan
  indexedCount: number;   // successfully indexed
  failedCount: number;    // extraction failed (see listFailures)
  lowConfidenceCount: number; // files with ≥1 low-confidence OCR page
  partialCount: number;   // indexed files with ≥1 unreadable page (see listFailures)
  lastScan: number | null;
  missing: boolean;       // folder itself not found (deleted/renamed/unplugged); records kept
  kinds: FileKind[];      // kinds indexed in this folder (never empty; default: all)
  pendingEmbedCount: number; // text read, semantic index pending (embedded=0); 0 without a model
}

type IndexPhase = "idle" | "scanning" | "indexing" | "embedding" | "committing";
interface IndexProgress {
  phase: IndexPhase;
  total: number;          // files queued in this run
  done: number;           // processed (ok + failed)
  failed: number;
  current: string | null; // path being processed (while chunks are reported: the file being embedded)
  chunksDone?: number;    // chunks of the file being embedded, in the `embedding` backfill and while a
  chunksTotal?: number;   // scan embeds a file (one file at a time); absent otherwise
  embeddingElapsedSecs?: number; // seconds that file has been embedding
  startedAt: number | null;
  finishedAt: number | null;
  watching: boolean;      // live folder watcher active
  cancelling: boolean;    // cancel requested; running OCR is being stopped
  cancelled: boolean;     // the last run was cancelled before it finished (reset when a run starts)
  textDone: number;       // first stage: files whose text is read (counted before their embed; `done` waits for it)
  embedDone: number;      // second stage: files whose semantic index finished in this run…
  embedTotal: number;     // …of the files needing one (readable text + working model; grows while
                          // text is read; in the `embedding` backfill = the pending list). 0 without a model
  unread: number;         // files a cancelled scan never read (0 otherwise)
  pendingEmbed?: number;  // only while idle: catalogued files waiting for their semantic index;
                          // absent without a model
}

interface SearchRequest {
  query: string;
  kinds: FileKind[];          // empty = all
  modifiedAfter: number | null;
  modifiedBefore: number | null;
  limit: number;              // 1..200
  offset: number;
}

interface Snippet {
  page: number | null;        // 0-based page, null if unknown
  text: string;
  highlights: [number, number][]; // [start, end) UTF-16 offsets into text
}

interface SearchHit {
  path: string; fileName: string; folder: string; kind: FileKind;
  size: number; modified: number;
  pageCount: number;
  lowConfidencePages: number;  // count of OCR pages under the confidence floor
  ocrPages: number;            // pages whose text came from OCR
  failedPages: number;         // pages that could not be read (render/OCR error); not searchable
  semanticOnly: boolean;       // in results only via vector similarity ("Anlamca benzer")
  nameOnly: boolean;           // found only by its file name; listed before content matches, newest first
  score: number;
  snippets: Snippet[];         // up to 3, best first
}

interface SearchResponse {
  total: number;      // exact number of files matching query + filters (not capped by limit/offset)
  hits: SearchHit[];  // the requested window, best first
  tookMs: number;
}

type TextSource = "text_layer" | "ocr" | "native";
interface PageView {
  index: number; text: string; source: TextSource;
  confidence: number | null;   // OCR mean word confidence 0..1
  lowConfidence: boolean;
  highlights: [number, number][]; // query term matches, UTF-16 offsets
  error: string | null;        // set when the page could not be read (text is empty)
}
interface DocumentView {
  path: string; fileName: string; kind: FileKind; size: number; modified: number;
  pages: PageView[];
}

interface PageImage {
  page: number; width: number; height: number;
  dataUrl: string;             // "data:image/jpeg;base64,…" (CSP img-src data:)
}

interface UpdateInfo {
  enabled: boolean;            // false: build has no updater public key → never checks
  available: boolean;
  version: string | null;
  notes: string | null;
}

interface FailureInfo {
  reason: "encrypted" | "access_denied" | "too_large" | "timeout" | "engine_missing" | "damaged" | "unsupported" | "ocr_failed" | "other";
  path: string; error: string; at: number;
  failedPages: number | null;  // partially read file: unreadable page count; null = whole file failed
  pageCount: number | null;
}

interface AppInfo {
  version: string;
  ocrReady: boolean;   ocrDetail: string;   // Tesseract read a built-in test image with the tur model
                                            // (same --tessdata-dir as real pages); probed once, cached
  pdfReady: boolean;   pdfDetail: string;   // bundled pdfium loadable
  semanticReady: boolean; semanticDetail: string; // EmbeddingGemma 2 loaded → hybrid search on
  indexDir: string;
  documentCount: number;
}
```

// Document language for scanned pages (Settings).
interface OcrLanguageInfo {
  language: string;        // Tesseract code: "tur" | "eng" | "tur+eng"
  available: string[];     // choices whose models are installed, in that order
  pendingRereads: number;  // indexed scans read with another language, still to be read again
}

// About page texts.
interface Notices {
  json: string;            // resources/licenses/third-party.json (groups/notes in English, `*_tr` copies)
  file: string;            // absolute path of licenses\THIRD_PARTY_NOTICES.txt
  terms: string;           // Turkish terms of use
  termsEn: string;         // English terms of use
  license: string;         // the app's own LICENSE (Apache-2.0)
}

## Commands (`invoke(name, args)`)

| Command | Args | Returns |
|---|---|---|
| `app_info` | – | `AppInfo` |
| `list_folders` | – | `FolderInfo[]` |
| `add_folder` | `{ path: string, kinds?: FileKind[] }` | `FolderInfo` (starts indexing that folder; `kinds` omitted = all; `[]` → `kinds_empty`) |
| `set_folder_kinds` | `{ id: number, kinds: FileKind[] }` | `FolderInfo` — stored per folder; records of unselected kinds are dropped, newly selected kinds are scanned; `[]` → `kinds_empty`, unknown id → `folder_missing` |
| `cancel_indexing` | – | `boolean` — false when nothing was running; returns at once, outcome via `index://progress` |
| `remove_folder` | `{ id: number }` | `void` (removes its documents from the index) |
| `reindex` | `{ folderId: number \| null }` | `void` (rescan one or all; incremental by size+mtime) |
| `complete_embeddings` | – | `void` — starts the embedding backfill now (files with `embedded=0`); returns at once, progress via `index://progress` (`phase: "embedding"`) |
| `ocr_language` | – | `OcrLanguageInfo` |
| `set_ocr_language` | `{ language: string }` | `OcrLanguageInfo` — saved in the catalog; scans read with another language are read again in the background (their old text stays searchable until then); unknown or not installed → `ocr_language_unavailable` |
| `index_status` | – | `IndexProgress` |
| `list_failures` | `{ folderId: number \| null }` | `FailureInfo[]` |
| `retry_failures` | `{ folderId: number \| null }` | `number` (files queued for an immediate re-read) |
| `search` | `{ request: SearchRequest }` | `SearchResponse` |
| `get_document` | `{ path: string, query: string }` | `DocumentView` |
| `get_page_image` | `{ path: string, page: number, width: number }` | `PageImage` — PDF page (pdfium) or the image itself, scaled to `width` (clamped 200–2400) px; Word/Excel → `preview_unavailable`; LRU-cached |
| `check_update` | – | `UpdateInfo` (GitHub Releases `latest.json`; `enabled:false` without a configured key) |
| `install_update` | – | `void` — downloads, verifies the signature, installs, restarts |
| `open_path` | `{ path: string }` | `void` (default app) |
| `reveal_path` | `{ path: string }` | `void` (Explorer, file selected) |
| `third_party_notices` | – | `Notices` |

## Events (`listen(name)`)

| Event | Payload | When |
|---|---|---|
| `index://progress` | `IndexProgress` | at most ~5×/s while running, and on phase change |
| `index://changed` | `{ documentCount: number }` | after each index commit (re-run the visible search) |

## Behaviour guarantees

- Search is offline BM25 over Turkish-normalized tokens (İ/ı aware) plus an
  ASCII-folded field, so `sozlesme` matches `sözleşme`. Terms are OR-combined;
  documents matching more terms rank higher. Turkish inflections (single-suffix
  stems, Snowball stems, prefix continuations), English plural/verb forms,
  one-edit typos, proximity and office synonyms add lower-ranked matches.
- Text in double quotes (`"kira sözleşmesi"`, also “…”) must occur as an exact
  phrase (diacritic-insensitive); unquoted words stay OR-combined.
- `lowConfidence` marks OCR pages whose mean Tesseract word confidence is below
  the floor `LOW_CONFIDENCE_FLOOR` (0.60); the text is still indexed.
- A file that fails extraction is never silently dropped: it appears in
  `list_failures` and in `FolderInfo.failedCount`.
- A page that fails to render or OCR does not fail its file: the page is kept
  with empty text and `PageView.error`, the other pages are indexed, and the
  file is listed by `list_failures` with `failedPages`/`pageCount` (and counted
  in `FolderInfo.partialCount`). Only a file whose every page failed is a
  whole-file failure.
- The search index carries a schema version. An index from another version
  is discarded at start-up and rebuilt from the catalog's stored page text
  (no re-extraction, no OCR); progress is reported as a normal indexing run.
- Folder watching covers directories: deleting or renaming a sub-folder drops
  (and, for a rename, re-adds under the new name) every record below it. A
  watched root that disappears is never cleaned up automatically: its records
  stay, `FolderInfo.missing` turns true and `index://changed` fires so the UI
  re-reads folders. `reindex` re-attaches the watcher once it is back.
- Every command except `index_status` runs on a background thread; the UI
  thread never waits on SQLite, Tantivy or Tesseract.
- File kinds are chosen per folder and kept in the catalog. Scans and the
  folder watcher only consider the selected kinds. Unselecting a kind removes
  that folder's records of it (indexed and failed) from catalog and index —
  also while the folder is missing, and even if a run is cancelled; newly
  selected kinds are picked up by a rescan of the folder.
- `cancel_indexing` stops the running scan cleanly: no new file starts,
  running Tesseract processes are killed, a file that did not finish is
  written neither as indexed nor as failed (a PDF is never stored with only
  some pages), files finished so far stay indexed and committed, and queued
  scans/watcher batches are dropped. `IndexProgress` goes `cancelling: true`
  → `phase: "idle", cancelled: true`. Records of files that vanished are kept
  until a full rescan. `reindex` resumes: it is incremental (size+mtime), so
  only the unfinished files are read. Text and semantic index are separate
  stages: a cancel can leave files whose text is read (keyword search works)
  but whose embedding is pending — the idle snapshot reports them in
  `pendingEmbed` and files never read in `unread`. The next scan finishes
  both; `complete_embeddings` finishes the embeddings without a rescan.
