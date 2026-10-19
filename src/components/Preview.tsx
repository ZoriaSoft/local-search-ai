import { useEffect, useRef, useState } from "react";

import type { Backend } from "../api/client";
import { toApiError } from "../api/client";
import type { ApiError, DocumentView, PageImage, Range } from "../api/types";
import { t } from "../i18n";
import { formatDate, formatSize } from "../lib/format";
import { Highlighted } from "./Highlighted";
import { ExternalIcon, FolderIcon, WarnIcon } from "./Icons";
import { KindTile } from "./KindTile";

interface Props {
  backend: Backend;
  path: string | null;
  query: string;
  onError: (message: string) => void;
}

type Mode = "text" | "page";

/** Word tables and blank scan margins often start a page with dozens of
 *  empty lines; show the text from its first visible character, with the
 *  highlight ranges shifted to match. */
function trimLead(text: string, ranges: Range[]): { text: string; ranges: Range[] } {
  const cut = text.length - text.trimStart().length;
  if (cut === 0) return { text, ranges };
  return {
    text: text.slice(cut),
    ranges: ranges.filter(([start]) => start >= cut).map(([start, end]) => [start - cut, end - cut] as Range),
  };
}

/** Render widths are bucketed so the backend's LRU cache is reused. */
function renderWidth(cssWidth: number): number {
  const pixels = cssWidth * (window.devicePixelRatio || 1);
  return Math.min(2400, Math.max(400, Math.ceil(pixels / 200) * 200));
}

function PageImageView({ backend, doc, initialPage }: { backend: Backend; doc: DocumentView; initialPage: number }) {
  const [page, setPage] = useState(initialPage);
  const [image, setImage] = useState<PageImage | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [attempt, setAttempt] = useState(0);
  const box = useRef<HTMLDivElement>(null);
  const total = doc.pages.length;

  useEffect(() => setPage(initialPage), [doc.path, initialPage]);

  useEffect(() => {
    let live = true;
    setError(null);
    setImage(null);
    const width = renderWidth(box.current?.clientWidth ?? 800);
    backend.getPageImage(doc.path, page, width).then(
      (value) => live && setImage(value),
      (e: unknown) => live && setError(toApiError(e)),
    );
    return () => {
      live = false;
    };
  }, [backend, doc.path, page, attempt]);

  return (
    <div className="page-image" ref={box}>
      {total > 1 && (
        <div className="pager">
          <button className="btn small" onClick={() => setPage((p) => Math.max(0, p - 1))} disabled={page === 0} aria-label={t.preview.prevPage}>
            ‹
          </button>
          <span aria-live="polite">{t.preview.pageCounter(page, total)}</span>
          <button
            className="btn small"
            onClick={() => setPage((p) => Math.min(total - 1, p + 1))}
            disabled={page >= total - 1}
            aria-label={t.preview.nextPage}
          >
            ›
          </button>
        </div>
      )}
      {error ? (
        <div className="error-box" role="alert">
          {t.errors[error.code]}
          <button className="btn small" onClick={() => setAttempt((n) => n + 1)}>
            {t.retry}
          </button>
        </div>
      ) : image ? (
        <img src={image.dataUrl} width={image.width} height={image.height} alt={t.preview.pageImageAlt(page, doc.fileName)} />
      ) : (
        <div className="skeleton page-skeleton" aria-busy="true">
          <i />
          <i />
          <i />
        </div>
      )}
    </div>
  );
}

export function Preview({ backend, path, query, onError }: Props) {
  const [doc, setDoc] = useState<DocumentView | null>(null);
  const [error, setError] = useState<ApiError | null>(null);
  const [attempt, setAttempt] = useState(0);
  // Default is the text view; the choice then sticks for the session.
  const [mode, setMode] = useState<Mode>("text");
  // A picture with no text opens on the picture; the user's own tab click
  // on this document wins (reset when another document is shown).
  const [textChosen, setTextChosen] = useState(false);
  useEffect(() => setTextChosen(false), [path]);
  const pane = useRef<HTMLElement>(null);
  const firstMatchNode = useRef<HTMLElement | null>(null);

  // Jump to the first match once per loaded document (and query), else to
  // the top. It used to run in a ref callback on every render: progress
  // events re-render the app ~5x/s while indexing, and each one pulled the
  // reader back to the first match.
  useEffect(() => {
    if (!doc || mode !== "text") return;
    if (firstMatchNode.current) firstMatchNode.current.scrollIntoView({ block: "start" });
    else pane.current?.scrollTo({ top: 0 });
  }, [doc, mode]);

  useEffect(() => {
    if (!path) {
      setDoc(null);
      return;
    }
    let live = true;
    setError(null);
    backend.getDocument(path, query).then(
      (value) => live && setDoc(value),
      (e: unknown) => live && setError(toApiError(e)),
    );
    return () => {
      live = false;
    };
  }, [backend, path, query, attempt]);

  if (!path) {
    return (
      <section className="pane preview" aria-label={t.preview.title}>
        <div className="state">
          <p>{t.preview.empty}</p>
        </div>
      </section>
    );
  }
  if (error) {
    return (
      <section className="pane preview" aria-label={t.preview.title}>
        <div className="error-box" role="alert">
          {t.errors[error.code]}
          <button className="btn small" onClick={() => setAttempt((n) => n + 1)}>
            {t.retry}
          </button>
        </div>
      </section>
    );
  }
  if (!doc || doc.path !== path) {
    return (
      <section className="pane preview" aria-label={t.preview.title} aria-busy="true">
        {[0, 1, 2].map((i) => (
          <div className="skeleton" key={i}>
            <i />
            <i />
            <i />
          </div>
        ))}
      </section>
    );
  }

  const act = (run: (p: string) => Promise<void>) => () =>
    run(doc.path).catch((e: unknown) => onError(t.errors[toApiError(e).code]));
  const firstMatch = doc.pages.find((p) => p.highlights.length > 0)?.index;
  const failedPages = doc.pages.filter((p) => p.error !== null).length;
  // Word/Excel have no page image: the tab is hidden and text is shown.
  const canRender = doc.kind === "pdf" || doc.kind === "image";
  const textless = doc.pages.every((p) => p.error !== null || !p.text.trim());
  const shownMode: Mode = canRender && (mode === "page" || (textless && !textChosen)) ? "page" : "text";
  const showPage = shownMode === "page";
  const folder = doc.path.slice(0, Math.max(doc.path.lastIndexOf("\\"), doc.path.lastIndexOf("/")));

  return (
    <section className="pane preview" aria-label={doc.fileName} ref={pane}>
      <header className="preview-head">
        <div className="preview-title">
          <KindTile kind={doc.kind} />
          <h2 title={doc.fileName}>{doc.fileName}</h2>
        </div>
        <div className="preview-actions" role="toolbar" aria-label={t.preview.actions}>
          <button className="btn primary small" onClick={act(backend.openPath)}>
            <ExternalIcon size={14} /> {t.preview.open}
          </button>
          <button className="btn small" onClick={act(backend.revealPath)}>
            <FolderIcon size={14} /> {t.preview.reveal}
          </button>
        </div>
        <dl className="doc-meta">
          <div>
            <dt>{t.preview.kind}</dt>
            <dd>{t.kind[doc.kind]}</dd>
          </div>
          <div>
            <dt>{t.preview.modified}</dt>
            <dd>{formatDate(doc.modified)}</dd>
          </div>
          <div>
            <dt>{t.preview.size}</dt>
            <dd>{formatSize(doc.size)}</dd>
          </div>
          <div>
            <dt>{t.search.columns.pages}</dt>
            <dd>{doc.pages.length}</dd>
          </div>
          <div className="wide">
            <dt>{t.preview.location}</dt>
            <dd title={folder}>
              <bdi>{folder}</bdi>
            </dd>
          </div>
        </dl>
        {canRender && (
          <div className="tabs" role="tablist" aria-label={t.preview.tabs}>
            {(["text", "page"] as const).map((m) => (
              <button
                key={m}
                role="tab"
                className="tab"
                aria-selected={shownMode === m}
                onClick={() => {
                  setMode(m);
                  setTextChosen(m === "text");
                }}
              >
                {m === "text" ? t.preview.tabText : t.preview.tabPage}
              </button>
            ))}
          </div>
        )}
      </header>
      {failedPages > 0 && (
        <div className="notice bad" role="note">
          <WarnIcon size={14} />
          <span>{t.preview.failedPages(failedPages)}</span>
        </div>
      )}
      {showPage && <PageImageView backend={backend} doc={doc} initialPage={firstMatch ?? 0} />}
      {!showPage && doc.pages.map((page) => (
        <article
          className="page"
          key={page.index}
          ref={page.index === firstMatch && page.index > 0 ? firstMatchNode : undefined}
        >
          <div className="page-head">
            <strong>{t.search.pageOf(page.index)}</strong>
            <span>{t.source[page.source]}</span>
            {page.error === null && page.confidence !== null && <span>{t.preview.confidence(page.confidence)}</span>}
            {page.error === null && <span>{t.preview.matches(page.highlights.length)}</span>}
          </div>
          {page.error !== null && (
            <div className="notice bad" title={page.error}>
              <WarnIcon size={14} />
              <span>{t.preview.pageFailed}</span>
            </div>
          )}
          {page.lowConfidence && (
            <div className="notice">
              <WarnIcon size={14} />
              <span>{t.preview.lowConfidence}</span>
            </div>
          )}
          {page.error !== null ? null : page.text.trim() ? (
            <pre className="page-text">
              <Highlighted {...trimLead(page.text, page.highlights)} />
            </pre>
          ) : (
            <p className="page-text">{t.preview.noText}</p>
          )}
        </article>
      ))}
    </section>
  );
}
