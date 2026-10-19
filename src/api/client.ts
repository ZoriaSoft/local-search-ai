// Typed access to the Rust backend. Outside Tauri (plain `vite` in a browser)
// an in-memory mock is used so the UI can be developed and screenshotted.

import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { openUrl } from "@tauri-apps/plugin-opener";

import { createMockBackend } from "./mock";
import type {
  ApiError,
  AppInfo,
  DocumentView,
  ErrorCode,
  FailureInfo,
  FileKind,
  FolderInfo,
  IndexProgress,
  LegalTexts,
  OcrLanguageInfo,
  PageImage,
  UpdateInfo,
  SearchRequest,
  SearchResponse,
} from "./types";

export interface Backend {
  appInfo(): Promise<AppInfo>;
  listFolders(): Promise<FolderInfo[]>;
  addFolder(path: string, kinds: FileKind[]): Promise<FolderInfo>;
  setFolderKinds(id: number, kinds: FileKind[]): Promise<FolderInfo>;
  cancelIndexing(): Promise<boolean>;
  removeFolder(id: number): Promise<void>;
  reindex(folderId: number | null): Promise<void>;
  completeEmbeddings(): Promise<void>;
  indexStatus(): Promise<IndexProgress>;
  listFailures(folderId: number | null): Promise<FailureInfo[]>;
  retryFailures(folderId: number | null): Promise<number>;
  search(request: SearchRequest): Promise<SearchResponse>;
  getDocument(path: string, query: string): Promise<DocumentView>;
  getPageImage(path: string, page: number, width: number): Promise<PageImage>;
  checkUpdate(): Promise<UpdateInfo>;
  installUpdate(): Promise<void>;
  openPath(path: string): Promise<void>;
  revealPath(path: string): Promise<void>;
  /** A web page (the source code) in the default browser. */
  openUrl(url: string): Promise<void>;
  /** About page: bundled license notices and the terms of use. */
  legalTexts(): Promise<LegalTexts>;
  /** Settings: document language for scanned pages. */
  ocrLanguage(): Promise<OcrLanguageInfo>;
  setOcrLanguage(language: string): Promise<OcrLanguageInfo>;
  /** Folders the user picked (several at once allowed); empty when cancelled. */
  pickFolders(): Promise<string[]>;
  onProgress(handler: (progress: IndexProgress) => void): Promise<() => void>;
  onChanged(handler: (documentCount: number) => void): Promise<() => void>;
}

const CODES: readonly ErrorCode[] = [
  "folder_missing",
  "folder_duplicate",
  "folder_nested",
  "kinds_empty",
  "file_missing",
  "not_indexed",
  "index_busy",
  "index_io",
  "ocr_unavailable",
  "pdf_unavailable",
  "query_empty",
  "preview_unavailable",
  "update_failed",
  "ocr_language_unavailable",
  "internal",
];

function isErrorCode(value: unknown): value is ErrorCode {
  return typeof value === "string" && CODES.some((code) => code === value);
}

/** Normalize anything thrown across the IPC boundary into an ApiError. */
export function toApiError(error: unknown): ApiError {
  if (error && typeof error === "object" && "code" in error) {
    const message = "message" in error ? String(error.message) : "";
    return { code: isErrorCode(error.code) ? error.code : "internal", message };
  }
  return { code: "internal", message: String(error) };
}

function tauriBackend(): Backend {
  const call = async <T>(command: string, args?: Record<string, unknown>): Promise<T> => {
    try {
      return await invoke<T>(command, args);
    } catch (error) {
      throw toApiError(error);
    }
  };
  return {
    appInfo: () => call("app_info"),
    listFolders: () => call("list_folders"),
    addFolder: (path, kinds) => call("add_folder", { path, kinds }),
    setFolderKinds: (id, kinds) => call("set_folder_kinds", { id, kinds }),
    cancelIndexing: () => call("cancel_indexing"),
    removeFolder: (id) => call("remove_folder", { id }),
    reindex: (folderId) => call("reindex", { folderId }),
    completeEmbeddings: () => call("complete_embeddings"),
    indexStatus: () => call("index_status"),
    listFailures: (folderId) => call("list_failures", { folderId }),
    retryFailures: (folderId) => call("retry_failures", { folderId }),
    search: (request) => call("search", { request }),
    getDocument: (path, query) => call("get_document", { path, query }),
    getPageImage: (path, page, width) => call("get_page_image", { path, page, width }),
    checkUpdate: () => call("check_update"),
    installUpdate: () => call("install_update"),
    openPath: (path) => call("open_path", { path }),
    revealPath: (path) => call("reveal_path", { path }),
    openUrl: async (url) => {
      try {
        await openUrl(url);
      } catch (error) {
        throw toApiError(error);
      }
    },
    legalTexts: async () => {
      const raw = await call<{ json: string; file: string; terms: string; termsEn: string; license: string }>(
        "third_party_notices",
      );
      return {
        notices: JSON.parse(raw.json),
        noticesFile: raw.file,
        terms: raw.terms,
        termsEn: raw.termsEn,
        license: raw.license,
      };
    },
    ocrLanguage: () => call("ocr_language"),
    setOcrLanguage: (language) => call("set_ocr_language", { language }),
    pickFolders: async () => {
      const picked = await open({ directory: true, multiple: true });
      if (picked === null) return [];
      return Array.isArray(picked) ? picked : [picked];
    },
    onProgress: (handler) => listen<IndexProgress>("index://progress", (e) => handler(e.payload)),
    onChanged: (handler) =>
      listen<{ documentCount: number }>("index://changed", (e) => handler(e.payload.documentCount)),
  };
}

let backend: Backend | null = null;

export function getBackend(): Backend {
  backend ??= "__TAURI_INTERNALS__" in window ? tauriBackend() : createMockBackend();
  return backend;
}
