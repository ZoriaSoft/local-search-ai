// TypeScript mirror of docs/API.md. Offsets are UTF-16 code units.

export type FileKind = "pdf" | "image" | "word" | "excel" | "text" | "log";
export const FILE_KINDS: FileKind[] = ["pdf", "image", "word", "excel", "text", "log"];
/** Kinds a new folder starts with: everything but `.log` (can be huge). */
export const DEFAULT_KINDS: FileKind[] = ["pdf", "image", "word", "excel", "text"];

export type ErrorCode =
  | "folder_missing"
  | "folder_duplicate"
  | "folder_nested"
  | "kinds_empty"
  | "file_missing"
  | "not_indexed"
  | "index_busy"
  | "index_io"
  | "ocr_unavailable"
  | "pdf_unavailable"
  | "query_empty"
  | "preview_unavailable"
  | "update_failed"
  | "ocr_language_unavailable"
  | "internal";

export interface ApiError {
  code: ErrorCode;
  message: string;
}

export interface FolderInfo {
  id: number;
  path: string;
  fileCount: number;
  indexedCount: number;
  failedCount: number;
  lowConfidenceCount: number;
  partialCount: number; // indexed files with ≥1 unreadable page
  lastScan: number | null;
  missing: boolean; // the folder itself is gone; its records are kept
  kinds: FileKind[]; // kinds indexed in this folder; never empty
  pendingEmbedCount?: number; // text read, semantic index pending (0 without a model)
}

export type IndexPhase = "idle" | "scanning" | "indexing" | "embedding" | "committing";

export interface IndexProgress {
  phase: IndexPhase;
  total: number;
  done: number;
  failed: number;
  current: string | null;
  chunksDone?: number | null; // current embedding file; done/total remain file counts
  chunksTotal?: number | null;
  embeddingElapsedSecs?: number | null; // heartbeat during a blocking model run
  startedAt: number | null;
  finishedAt: number | null;
  watching: boolean;
  cancelling: boolean; // cancel requested, running OCR is being stopped
  cancelled: boolean; // last run was cancelled; a rescan resumes it
  textDone?: number; // first stage: files whose text is read (before their embed); `done` waits for it
  embedDone?: number; // second stage: files whose semantic index is done…
  embedTotal?: number; // …of the files that need one (0 without a model)
  unread?: number; // files a cancelled scan never read
  pendingEmbed?: number | null; // when idle: files waiting for their semantic index
}

export interface SearchRequest {
  query: string;
  kinds: FileKind[];
  modifiedAfter: number | null;
  modifiedBefore: number | null;
  limit: number;
  offset: number;
}

export type Range = [number, number];

export interface Snippet {
  page: number | null;
  text: string;
  highlights: Range[];
}

export interface SearchHit {
  path: string;
  fileName: string;
  folder: string;
  kind: FileKind;
  size: number;
  modified: number;
  pageCount: number;
  lowConfidencePages: number;
  ocrPages: number;
  failedPages: number; // pages that could not be read (not searchable)
  semanticOnly: boolean; // in the results only via vector similarity ("anlamca benzer")
  nameOnly?: boolean; // found only by its file name (no page text matched)
  score: number;
  snippets: Snippet[];
}

export interface SearchResponse {
  total: number;
  hits: SearchHit[];
  tookMs: number;
}

export type TextSource = "text_layer" | "ocr" | "native";

export interface PageView {
  index: number;
  text: string;
  source: TextSource;
  confidence: number | null;
  lowConfidence: boolean;
  highlights: Range[];
  error: string | null; // set when this page could not be read
}

export interface DocumentView {
  path: string;
  fileName: string;
  kind: FileKind;
  size: number;
  modified: number;
  pages: PageView[];
}

export interface PageImage {
  page: number;
  width: number;
  height: number;
  dataUrl: string; // data:image/jpeg;base64,…
}

export interface UpdateInfo {
  enabled: boolean; // false: this build has no updater key, checks are off
  available: boolean;
  version: string | null;
  notes: string | null;
}

export type FailureReason =
  | "encrypted"
  | "access_denied"
  | "too_large"
  | "timeout"
  | "engine_missing"
  | "damaged"
  | "unsupported"
  | "ocr_failed"
  | "other";

export interface FailureInfo {
  path: string;
  error: string;
  reason?: FailureReason; // why, for the Turkish explanation (raw `error` is the detail)
  at: number;
  failedPages: number | null; // partial file: unreadable pages; null = whole file failed
  pageCount: number | null;
}

export interface AppInfo {
  version: string;
  ocrReady: boolean;
  ocrDetail: string;
  pdfReady: boolean;
  pdfDetail: string;
  semanticReady: boolean; // EmbeddingGemma 2 loaded → hybrid search is on
  semanticDetail: string;
  indexDir: string;
  documentCount: number;
}

/** One open-source component shipped inside the app. */
export interface NoticeItem {
  name: string;
  version: string;
  license: string; // SPDX expression or the package's own wording
  url: string;
  note?: string; // English
  note_tr?: string;
  texts: string[]; // keys into NoticeData.texts
}

export interface NoticeGroup {
  id: string;
  title: string; // English
  title_tr?: string;
  items: NoticeItem[];
}

/** resources/licenses/third-party.json (scripts/gen-notices.py). */
export interface NoticeData {
  generated: string;
  acknowledgements: string[]; // English
  acknowledgements_tr?: string[];
  groups: NoticeGroup[];
  texts: Record<string, { title: string; text: string }>;
}

export interface LegalTexts {
  notices: NoticeData;
  noticesFile: string; // THIRD_PARTY_NOTICES.txt in the install dir
  terms: string; // KULLANIM_KOSULLARI.txt
  termsEn: string; // TERMS.txt
  license: string; // the app's own LICENSE (Apache-2.0)
}

/** Document language for scanned pages (Settings). */
export interface OcrLanguageInfo {
  language: string; // tur | eng | tur+eng
  available: string[];
  pendingRereads: number;
}
