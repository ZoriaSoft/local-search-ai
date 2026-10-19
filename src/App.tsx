import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { getBackend, toApiError } from "./api/client";
import type { ApiError, AppInfo, FailureInfo, FileKind, FolderInfo, IndexProgress, SearchHit, UpdateInfo } from "./api/types";
import { AddFolderDialog } from "./components/AddFolderDialog";
import { CloseIcon } from "./components/Icons";
import { AboutView } from "./components/AboutView";
import { FoldersView } from "./components/FoldersView";
import { IssuesView } from "./components/IssuesView";
import { NavRail, type View } from "./components/NavRail";
import { SearchView, type DateRange } from "./components/SearchView";
import { SettingsView } from "./components/SettingsView";
import { TopBar } from "./components/TopBar";
import { Welcome } from "./components/Welcome";
import { defaultOcrLanguage, language, ocrChosen, t, type Language } from "./i18n";
import { createRequestGate } from "./lib/requestGate";

const PAGE = 30;
const IDLE: IndexProgress = {
  phase: "idle", total: 0, done: 0, failed: 0, current: null, startedAt: null, finishedAt: null, watching: false,
  cancelling: false, cancelled: false, textDone: 0, embedDone: 0, embedTotal: 0, unread: 0, pendingEmbed: null,
};

function rangeStart(range: DateRange): number | null {
  const now = new Date();
  if (range === "week") return Math.floor(now.getTime() / 1000) - 7 * 86_400;
  if (range === "month") return Math.floor(now.getTime() / 1000) - 30 * 86_400;
  if (range === "year") return Math.floor(new Date(now.getFullYear(), 0, 1).getTime() / 1000);
  return null;
}

interface AppProps {
  /** The page to open on: Settings after a language switch remounts the app. */
  initialView?: View;
  onLanguage: (language: Language, view: View) => void;
}

export function App({ initialView = "search", onLanguage }: AppProps) {
  const backend = useMemo(getBackend, []);
  const [view, setView] = useState<View>(initialView);
  const [info, setInfo] = useState<AppInfo | null>(null);
  const [folders, setFolders] = useState<FolderInfo[]>([]);
  const [foldersLoaded, setFoldersLoaded] = useState(false);
  const [failures, setFailures] = useState<FailureInfo[]>([]);
  const [progress, setProgress] = useState<IndexProgress>(IDLE);
  const [query, setQuery] = useState("");
  const [submitted, setSubmitted] = useState("");
  const [kinds, setKinds] = useState<FileKind[]>([]);
  const [range, setRange] = useState<DateRange>("all");
  const [hits, setHits] = useState<SearchHit[]>([]);
  const [total, setTotal] = useState(0);
  const [tookMs, setTookMs] = useState(0);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<ApiError | null>(null);
  const [selected, setSelected] = useState<string | null>(null);
  const [toast, setToast] = useState<string | null>(null);
  const [announce, setAnnounce] = useState("");
  const [update, setUpdate] = useState<UpdateInfo | null>(null);
  const [installing, setInstalling] = useState(false);
  // Folder picked, waiting for the file-kind choice.
  // Picked folders waiting for the file-kind choice, then the ones being added.
  const [pendingFolders, setPendingFolders] = useState<string[] | null>(null);
  const [adding, setAdding] = useState<string[]>([]);
  const input = useRef<HTMLInputElement>(null);
  const list = useRef<HTMLUListElement>(null);
  const searchId = useRef(0);
  const metaGate = useRef(createRequestGate());
  const busy = progress.phase !== "idle";
  const switchLanguage = (next: Language) => onLanguage(next, view);

  const refreshMeta = useCallback(() => {
    const current = metaGate.current.next();
    backend.appInfo().then((value) => { if (current()) setInfo(value); }, () => {});
    backend.listFolders().then(
      (value) => {
        if (!current()) return;
        setFolders(value);
        setFoldersLoaded(true);
      },
      () => { if (current()) setFoldersLoaded(true); },
    );
    backend.listFailures(null).then((value) => { if (current()) setFailures(value); }, () => {});
  }, [backend]);

  const runSearch = useCallback(
    async (text: string, offset: number) => {
      const id = ++searchId.current;
      if (!text.trim()) {
        setHits([]);
        setTotal(0);
        setError(null);
        // A search still in flight is now stale and will not clear it.
        setLoading(false);
        return;
      }
      setLoading(true);
      try {
        const response = await backend.search({
          query: text, kinds, modifiedAfter: rangeStart(range), modifiedBefore: null, limit: PAGE, offset,
        });
        if (id !== searchId.current) return;
        setError(null);
        setTotal(response.total);
        setTookMs(response.tookMs);
        setHits((previous) => (offset === 0 ? response.hits : [...previous, ...response.hits]));
        if (offset === 0) {
          setSelected((current) =>
            current && response.hits.some((h) => h.path === current) ? current : (response.hits[0]?.path ?? null),
          );
          setAnnounce(t.search.results(response.total));
        }
      } catch (e) {
        if (id !== searchId.current) return;
        const apiError = toApiError(e);
        if (apiError.code === "query_empty") {
          setHits([]);
          setTotal(0);
          setError(null);
        } else setError(apiError);
      } finally {
        if (id === searchId.current) setLoading(false);
      }
    },
    [backend, kinds, range],
  );

  useEffect(() => {
    let live = true;
    let eventSeen = false;
    refreshMeta();
    const offProgress = backend.onProgress((p) => {
      if (!live) return;
      eventSeen = true;
      setProgress(p);
      if (p.phase === "idle") {
        refreshMeta();
        setAnnounce(p.cancelled ? t.index.cancelled : t.index.phase.idle);
      }
    });
    // Subscribe before requesting the initial snapshot so a terminal event
    // cannot fall between the snapshot and listener registration. Once events
    // have started, keep their state instead of restoring the snapshot.
    offProgress.catch(() => undefined).then(() => live ? backend.indexStatus() : undefined).then((p) => {
      if (p && live && !eventSeen) setProgress(p);
    }, () => {});
    input.current?.focus();
    return () => {
      live = false;
      metaGate.current.invalidate();
      void offProgress.then((off) => off(), () => {});
    };
  }, [backend, refreshMeta]);

  // Until the user picks a document language, an empty archive follows the
  // UI language (Turkish UI → Turkish scans, otherwise English).
  useEffect(() => {
    if (foldersLoaded && folders.length === 0 && !ocrChosen()) {
      void backend.setOcrLanguage(defaultOcrLanguage(language)).catch(() => undefined);
    }
  }, [backend, foldersLoaded, folders.length]);

  // One quiet update check shortly after start; silent when disabled or offline.
  useEffect(() => {
    const timer = setTimeout(() => {
      backend.checkUpdate().then(
        (info) => info.enabled && info.available && setUpdate(info),
        () => {},
      );
    }, 5000);
    return () => clearTimeout(timer);
  }, [backend]);

  // Ctrl+F / Ctrl+K anywhere: back to the search box.
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && (event.key === "f" || event.key === "k")) {
        event.preventDefault();
        setView("search");
        input.current?.focus();
        input.current?.select();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  const startAddFolder = async () => {
    const paths = await backend.pickFolders();
    if (paths.length > 0) setPendingFolders(paths);
  };

  const addFolders = async (paths: string[], kinds: FileKind[]) => {
    setPendingFolders(null);
    // The rows show at once on the Klasörler page, where reading progress lives.
    setAdding(paths);
    setView("folders");
    const skipped: { path: string; message: string }[] = [];
    for (const path of paths) {
      try {
        await backend.addFolder(path, kinds);
      } catch (e) {
        skipped.push({ path, message: t.errors[toApiError(e).code] });
      }
      setAdding((current) => current.filter((p) => p !== path));
      refreshMeta();
    }
    if (skipped.length === 1 && paths.length === 1) setToast(skipped[0]?.message ?? null);
    else if (skipped.length > 0)
      setToast(t.folders.addedSome(paths.length - skipped.length, skipped.map((s) => `${s.path} (${s.message})`)));
  };

  const installUpdate = async () => {
    if (!update?.version || !window.confirm(t.status.updateConfirm(update.version))) return;
    setInstalling(true);
    try {
      await backend.installUpdate();
    } catch (e) {
      setToast(t.errors[toApiError(e).code]);
    } finally {
      setInstalling(false);
    }
  };

  // Debounced search on typing / filter change.
  useEffect(() => {
    const timer = setTimeout(() => setSubmitted(query), 250);
    return () => clearTimeout(timer);
  }, [query]);
  useEffect(() => {
    void runSearch(submitted, 0);
  }, [submitted, runSearch]);

  // Re-run the visible search when the index changes.
  useEffect(() => {
    let live = true;
    const off = backend.onChanged(() => {
      // A pending unsubscription must not start an older query again.
      if (!live) return;
      refreshMeta();
      void runSearch(submitted, 0);
    });
    return () => {
      live = false;
      void off.then((f) => f(), () => {});
    };
  }, [backend, refreshMeta, runSearch, submitted]);

  const move = (delta: number) => {
    if (!hits.length) return;
    const index = Math.max(0, hits.findIndex((h) => h.path === selected));
    const next = hits[Math.min(hits.length - 1, Math.max(0, index + delta))];
    if (!next) return;
    setSelected(next.path);
    list.current?.querySelector(`[data-path="${CSS.escape(next.path)}"]`)?.scrollIntoView({ block: "nearest" });
  };

  const open = (path: string, reveal = false) =>
    void (reveal ? backend.revealPath(path) : backend.openPath(path)).catch((e: unknown) =>
      setToast(t.errors[toApiError(e).code]),
    );

  const onKey = (event: React.KeyboardEvent) => {
    // Result navigation belongs to the search box and the result list only:
    // arrows must still work in selects and dialogs, and Enter on a focused
    // button must press that button, not open the selected result as well.
    const target = event.target as Node;
    if (target !== input.current && !list.current?.contains(target)) return;
    if (view !== "search") return;
    if (event.key === "ArrowDown") {
      event.preventDefault();
      move(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      move(-1);
    } else if (event.key === "Enter" && selected && event.target !== input.current) {
      open(selected, event.ctrlKey);
    } else if (event.key === "Enter" && event.target === input.current) {
      // Search now instead of after the typing pause; when the text changed,
      // the `submitted` effect runs it (calling it here too searched twice).
      if (query === submitted) void runSearch(query, 0);
      else setSubmitted(query);
    } else if (event.key === "Escape" && event.target === input.current) {
      setQuery("");
    }
  };

  const onQuery = (text: string) => {
    setQuery(text);
    if (view !== "search") setView("search");
  };

  const firstRun = foldersLoaded && folders.length === 0 && adding.length === 0 && !busy;
  const wholeFailures = failures.filter((f) => f.failedPages === null).length;
  const issueCount = failures.length;

  let content: React.ReactNode;
  if (firstRun) {
    content = <Welcome onAddFolder={startAddFolder} onLanguage={switchLanguage} />;
  } else if (view === "folders") {
    content = (
      <FoldersView
        backend={backend}
        folders={folders}
        adding={adding}
        progress={progress}
        semantic={info?.semanticReady ?? false}
        issueCount={issueCount}
        onAddFolder={startAddFolder}
        onFoldersChanged={refreshMeta}
        onShowIssues={() => setView("issues")}
        onError={setToast}
      />
    );
  } else if (view === "settings") {
    content = (
      <SettingsView
        backend={backend}
        onLanguage={switchLanguage}
        onError={setToast}
        indexDone={progress.done}
        busy={busy}
      />
    );
  } else if (view === "about") {
    content = <AboutView backend={backend} info={info} onError={setToast} />;
  } else if (view === "issues") {
    content = (
      <IssuesView
        backend={backend}
        failures={failures}
        busy={busy}
        onReveal={(path) => open(path, true)}
        onChanged={refreshMeta}
        onError={setToast}
        onNotice={setToast}
      />
    );
  } else {
    content = (
      <SearchView
        backend={backend}
        query={submitted}
        kinds={kinds}
        onKinds={setKinds}
        range={range}
        onRange={setRange}
        hits={hits}
        total={total}
        tookMs={tookMs}
        loading={loading}
        error={error}
        selected={selected}
        onSelect={setSelected}
        onOpen={open}
        onRetry={() => runSearch(submitted, 0)}
        onMore={() => runSearch(submitted, hits.length)}
        documentCount={info?.documentCount ?? 0}
        listRef={list}
        onError={setToast}
      />
    );
  }

  return (
    <div className="app" onKeyDown={onKey}>
      <TopBar
        inputRef={input}
        query={query}
        onQuery={onQuery}
        progress={progress}
        disabled={firstRun}
        onActivity={() => setView("folders")}
      />
      <div className="body">
        <NavRail
          view={firstRun ? null : view}
          onView={setView}
          folderCount={folders.length}
          issueCount={issueCount}
          wholeFailures={wholeFailures}
          info={info}
          disabled={firstRun}
        />
        <main className="content" aria-live="off">
          {toast && (
            <div className="toast" role="alert">
              <span>{toast}</span>
              <button className="icon-btn" onClick={() => setToast(null)} aria-label={t.close} title={t.close}>
                <CloseIcon />
              </button>
            </div>
          )}
          {content}
        </main>
      </div>
      <footer className="statusbar">
        <span>{t.status.documents(info?.documentCount ?? 0)}</span>
        {info && !info.ocrReady && (
          <span className="warn" title={info.ocrDetail}>
            {t.status.ocrMissing}
          </span>
        )}
        {info && !info.pdfReady && (
          <span className="warn" title={info.pdfDetail}>
            {t.status.pdfMissing}
          </span>
        )}
        <span className="watch">
          <span className={`dot${progress.watching ? " on" : ""}`} aria-hidden="true" />
          {progress.watching ? t.index.watching : t.index.notWatching}
        </span>
        {update?.version && (
          <span className="update" role="status" title={update.notes ?? undefined}>
            {installing ? t.status.updateInstalling : t.status.updateAvailable(update.version)}
            {!installing && (
              <button className="btn small primary" onClick={installUpdate}>
                {t.status.updateInstall}
              </button>
            )}
          </span>
        )}
        <span className="push">{info ? `v${info.version}` : ""}</span>
      </footer>
      {pendingFolders !== null && (
        <AddFolderDialog
          paths={pendingFolders}
          onConfirm={(kinds) => void addFolders(pendingFolders, kinds)}
          onCancel={() => setPendingFolders(null)}
        />
      )}
      <div className="sr-only" aria-live="polite">
        {announce}
      </div>
    </div>
  );
}
