import { useRef, useState } from "react";

import type { Backend } from "../api/client";
import { toApiError } from "../api/client";
import type { FileKind, FolderInfo, IndexProgress } from "../api/types";
import { t } from "../i18n";
import { formatCount, formatDateTime } from "../lib/format";
import { holdRatio, progressRatio, type ShownRatio } from "../lib/progress";
import { AlertIcon, CheckIcon, FolderIcon, FolderPlusIcon, RefreshIcon, WarnIcon } from "./Icons";
import { KindPicker, kindSummary } from "./KindPicker";

interface Props {
  backend: Backend;
  folders: FolderInfo[];
  progress: IndexProgress;
  semantic: boolean; // the model loaded: the bar also counts the semantic index
  issueCount: number;
  onAddFolder: () => void;
  /** Folders picked and being added: shown at once, before the backend answers. */
  adding: string[];
  onFoldersChanged: () => void;
  onShowIssues: () => void;
  onError: (message: string) => void;
}


/** Folder management: the running scan, then one row per watched folder. */
export function FoldersView({ backend, folders, adding, progress, semantic, issueCount, onAddFolder, onFoldersChanged, onShowIssues, onError }: Props) {
  // Folder whose kinds are being edited, and the unsaved choice.
  const [editing, setEditing] = useState<{ id: number; kinds: FileKind[] } | null>(null);
  const busy = progress.phase !== "idle";
  // Bar fill: both stages, never moving back within a run.
  const shown = useRef<ShownRatio | null>(null);
  shown.current = busy ? holdRatio(shown.current, progress.startedAt, progressRatio(progress, semantic)) : null;
  const ratio = shown.current?.ratio ?? 0;

  const fail = (error: unknown) => onError(t.errors[toApiError(error).code]);

  // Folders being removed: their rows dim at once and lose their buttons.
  const [removing, setRemoving] = useState<Set<number>>(() => new Set());
  const remove = async (folder: FolderInfo) => {
    if (!window.confirm(t.index.removeConfirm(folder.path))) return;
    setRemoving((current) => new Set(current).add(folder.id));
    try {
      await backend.removeFolder(folder.id);
    } catch (error) {
      fail(error);
    } finally {
      setRemoving((current) => {
        const next = new Set(current);
        next.delete(folder.id);
        return next;
      });
      onFoldersChanged();
    }
  };

  const reindex = (id: number | null) => backend.reindex(id).catch(fail);

  // "Şimdi tamamla" after a cancel: only semantic indexes left → embed them
  // now; unread files (or nothing known) → a rescan, which embeds after.
  const unread = progress.unread ?? 0;
  const pendingEmbed = progress.pendingEmbed ?? 0;
  const complete = () => (unread > 0 || pendingEmbed === 0 ? reindex(null) : backend.completeEmbeddings().catch(fail));

  // Returns at once; the run reports `cancelling`, then `cancelled`, via progress events.
  const cancel = () => backend.cancelIndexing().catch(fail);

  const saveKinds = async (folder: FolderInfo, kinds: FileKind[]) => {
    const dropped = folder.kinds.filter((k) => !kinds.includes(k));
    if (dropped.length > 0 && !window.confirm(t.kinds.removeWarning(dropped.map((k) => t.kind[k])))) return;
    try {
      await backend.setFolderKinds(folder.id, kinds);
      setEditing(null);
      onFoldersChanged();
    } catch (error) {
      fail(error);
    }
  };

  // Two stages, told apart: text read (keyword search works) and the
  // semantic index (embedding), which can trail far behind.
  const showText = progress.phase !== "embedding" && progress.total > 0;
  const showEmbed = (progress.embedTotal ?? 0) > 0;
  // The chunk/seconds line stays apart from the path: the path line is
  // right-to-left (ellipsis on the left), which garbled "0 / 3" into "3 / 0".
  const embedDetail =
    (progress.phase === "embedding" || progress.phase === "indexing") && progress.embeddingElapsedSecs != null
      ? t.index.embeddingDetail(progress.chunksDone ?? 0, progress.chunksTotal ?? null, progress.embeddingElapsedSecs)
      : null;

  return (
    <div className="page-view">
      <header className="page-header">
        <div>
          <h1>{t.folders.title}</h1>
          <p>{t.folders.lead}</p>
        </div>
        <div className="page-actions">
          {folders.length > 0 && (
            <button className="btn" onClick={() => reindex(null)} disabled={busy}>
              <RefreshIcon size={15} /> {t.index.reindexAll}
            </button>
          )}
          <button className="btn primary" onClick={onAddFolder}>
            <FolderPlusIcon size={15} /> {t.index.add}
          </button>
        </div>
      </header>

      {busy && (
        <section className="panel progress" role="status" aria-live="polite">
          <div className="progress-head">
            <span className="spinner" aria-hidden="true" />
            <strong>{progress.cancelling ? t.index.cancelling : t.index.phase[progress.phase]}</strong>
            <span className="progress-pct">%{Math.round(ratio * 100)}</span>
          </div>
          <div className="progress-bar" aria-hidden="true">
            <div style={{ transform: `scaleX(${ratio})` }} />
          </div>
          {(showText || showEmbed) && (
            <dl className="progress-stages">
              {showText && (
                <div>
                  <dt>{t.index.textStage}</dt>
                  <dd>{t.index.progress(Math.max(progress.textDone ?? 0, progress.done), progress.total)}</dd>
                </div>
              )}
              {showEmbed && (
                <div>
                  <dt>{t.index.embedStage}</dt>
                  <dd>{t.index.progress(progress.embedDone ?? 0, progress.embedTotal ?? 0)}</dd>
                </div>
              )}
              {embedDetail && (
                <div>
                  <dt>{t.index.embedDetailLabel}</dt>
                  <dd>{embedDetail}</dd>
                </div>
              )}
            </dl>
          )}
          {progress.current && (
            <div className="progress-file" title={progress.current}>
              <bdi>{progress.current}</bdi>
            </div>
          )}
          <div className="progress-actions">
            {!progress.cancelling && (
              <button className="btn small" onClick={cancel} title={t.index.cancelHelp}>
                {t.index.cancel}
              </button>
            )}
          </div>
        </section>
      )}

      {!busy && progress.cancelled && (
        <div className="notice block" role="status">
          <WarnIcon size={15} />
          <span>
            <strong>{t.index.cancelled}.</strong>{" "}
            {unread > 0 || pendingEmbed > 0 ? t.index.stoppedDetail(unread, pendingEmbed) : t.index.cancelledHelp}
          </span>
          <button className="btn small" onClick={complete}>
            <RefreshIcon size={14} /> {t.index.resume}
          </button>
        </div>
      )}

      {issueCount > 0 && !busy && (
        <div className="notice bad block" role="status">
          <AlertIcon size={15} />
          <span>{t.folders.issues(issueCount)}</span>
          <button className="btn small" onClick={onShowIssues}>
            {t.folders.showIssues}
          </button>
        </div>
      )}

      <section className="panel table" aria-label={t.folders.title}>
        <div className="table-head folder-grid" aria-hidden="true">
          <span>{t.folders.columns.folder}</span>
          <span>{t.folders.columns.files}</span>
          <span>{t.folders.columns.status}</span>
          <span>{t.folders.columns.lastScan}</span>
          <span />
        </div>
        {folders.length === 0 && adding.length === 0 && <p className="table-empty">{t.folders.empty}</p>}
        {folders.map((folder) => {
          const pending = folder.pendingEmbedCount ?? 0;
          const problems = folder.failedCount + folder.partialCount;
          if (removing.has(folder.id)) return <PendingRow key={folder.id} path={folder.path} label={t.folders.removing} />;
          return (
            <div className="table-row" key={folder.id}>
              <div className="folder-grid">
                <div className="cell-folder">
                  <FolderIcon size={16} />
                  <div>
                    <div className="folder-path" title={folder.path}>
                      <bdi>{folder.path}</bdi>
                    </div>
                    <div className="folder-kinds">{kindSummary(folder.kinds)}</div>
                  </div>
                </div>
                <div className="num">
                  {formatCount(folder.indexedCount)}
                  <span className="muted"> / {formatCount(folder.fileCount)}</span>
                </div>
                <div className="cell-status">
                  {folder.missing ? (
                    <span className="tag bad">{t.index.missing}</span>
                  ) : problems > 0 ? (
                    <button className="tag bad link" onClick={onShowIssues}>
                      {t.folders.problemTag(problems)}
                    </button>
                  ) : (
                    <span className="tag ok">
                      <CheckIcon size={12} /> {t.folders.healthy}
                    </span>
                  )}
                  {folder.lowConfidenceCount > 0 && <span className="tag warn">{t.index.low(folder.lowConfidenceCount)}</span>}
                  {!busy && pending > 0 && <span className="tag">{t.index.pendingEmbed(pending)}</span>}
                </div>
                <div className="muted">{folder.lastScan ? formatDateTime(folder.lastScan) : t.index.neverScanned}</div>
                <div className="row-actions">
                  <button className="btn small" onClick={() => reindex(folder.id)} disabled={busy}>
                    {t.index.reindex}
                  </button>
                  <button className="btn small" onClick={() => setEditing(editing?.id === folder.id ? null : { id: folder.id, kinds: folder.kinds })}>
                    {t.kinds.edit}
                  </button>
                  <button className="btn small quiet-danger" onClick={() => remove(folder)}>
                    {t.index.remove}
                  </button>
                </div>
              </div>
              {folder.missing && (
                <div className="notice bad">
                  <WarnIcon size={14} />
                  <span>{t.index.missingHelp}</span>
                </div>
              )}
              {editing?.id === folder.id && (
                <form
                  className="kind-editor"
                  onSubmit={(event) => {
                    event.preventDefault();
                    if (editing.kinds.length > 0) void saveKinds(folder, editing.kinds);
                  }}
                >
                  <KindPicker value={editing.kinds} onChange={(kinds) => setEditing({ id: folder.id, kinds })} />
                  <div className="form-actions">
                    <button type="button" className="btn small" onClick={() => setEditing(null)}>
                      {t.kinds.cancel}
                    </button>
                    <button type="submit" className="btn small primary" disabled={editing.kinds.length === 0}>
                      {t.kinds.save}
                    </button>
                  </div>
                </form>
              )}
            </div>
          );
        })}
        {adding
          .filter((path) => !folders.some((f) => f.path === path))
          .map((path) => (
            <PendingRow key={`adding:${path}`} path={path} label={t.folders.adding} />
          ))}
      </section>
    </div>
  );
}

/** A row whose folder is being added or removed: shown at once, no actions. */
function PendingRow({ path, label }: { path: string; label: string }) {
  return (
    <div className="table-row pending" aria-busy="true">
      <div className="folder-grid">
        <div className="cell-folder">
          <FolderIcon size={16} />
          <div>
            <div className="folder-path" title={path}>
              <bdi>{path}</bdi>
            </div>
          </div>
        </div>
        <div className="num muted">–</div>
        <div className="cell-status">
          <span className="tag">
            <span className="spinner" aria-hidden="true" /> {label}
          </span>
        </div>
        <div className="muted">–</div>
        <div />
      </div>
    </div>
  );
}
