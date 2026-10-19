import { useState } from "react";

import type { Backend } from "../api/client";
import { toApiError } from "../api/client";
import type { FailureInfo, FailureReason } from "../api/types";
import { t } from "../i18n";
import { formatDateTime } from "../lib/format";
import { CheckIcon, FolderIcon, RefreshIcon } from "./Icons";

interface Props {
  backend: Backend;
  failures: FailureInfo[];
  busy: boolean;
  onReveal: (path: string) => void;
  onChanged: () => void;
  onError: (message: string) => void;
  onNotice: (message: string) => void;
}

const ORDER: FailureReason[] = [
  "damaged", "encrypted", "access_denied", "ocr_failed", "timeout", "too_large", "engine_missing", "unsupported", "other",
];

function split(path: string): { name: string; folder: string } {
  const at = Math.max(path.lastIndexOf("\\"), path.lastIndexOf("/"));
  return at < 0 ? { name: path, folder: "" } : { name: path.slice(at + 1), folder: path.slice(0, at) };
}

/** Unreadable files grouped by why, each group with what the user can do. */
export function IssuesView({ backend, failures, busy, onReveal, onChanged, onError, onNotice }: Props) {
  const [retrying, setRetrying] = useState(false);
  const [open, setOpen] = useState<string | null>(null);

  const retry = async () => {
    setRetrying(true);
    try {
      const count = await backend.retryFailures(null);
      onNotice(t.issues.retryStarted(count));
      onChanged();
    } catch (error) {
      onError(t.errors[toApiError(error).code]);
    } finally {
      setRetrying(false);
    }
  };

  const groups = ORDER.map((reason) => ({
    reason,
    items: failures.filter((f) => (f.reason ?? "other") === reason),
  })).filter((g) => g.items.length > 0);

  return (
    <div className="page-view">
      <header className="page-header">
        <div>
          <h1>{t.issues.title}</h1>
          <p>{t.issues.lead}</p>
        </div>
        <div className="page-actions">
          <button className="btn primary" onClick={retry} disabled={failures.length === 0 || busy || retrying}>
            <RefreshIcon size={15} /> {retrying || busy ? t.issues.retrying : t.issues.retryAll}
          </button>
        </div>
      </header>

      {failures.length === 0 && (
        <section className="panel empty-ok">
          <CheckIcon size={20} />
          <p>{t.issues.none}</p>
        </section>
      )}

      {groups.map(({ reason, items }) => (
        <section className="panel issue-group" key={reason} aria-labelledby={`issue-${reason}`}>
          <header>
            <h2 id={`issue-${reason}`}>
              {t.issues.reasons[reason].title}
              <span className="count">{t.issues.count(items.length)}</span>
            </h2>
            <p>{t.issues.reasons[reason].help}</p>
          </header>
          <ul>
            {items.map((item) => {
              const { name, folder } = split(item.path);
              const expanded = open === item.path;
              return (
                <li key={item.path}>
                  <div className="issue-row">
                    <div className="issue-main">
                      <div className="issue-name" title={item.path}>
                        {name}
                      </div>
                      <div className="issue-folder" title={folder}>
                        <bdi>{folder}</bdi>
                      </div>
                    </div>
                    <div className="issue-meta">
                      <span>
                        {item.failedPages !== null && item.pageCount !== null
                          ? t.issues.partial(item.failedPages, item.pageCount)
                          : t.issues.whole}
                      </span>
                      {item.at > 0 && <span className="muted">{formatDateTime(item.at)}</span>}
                    </div>
                    <div className="row-actions">
                      <button className="btn small" onClick={() => onReveal(item.path)}>
                        <FolderIcon size={14} /> {t.issues.reveal}
                      </button>
                      <button className="btn small ghost" aria-expanded={expanded} onClick={() => setOpen(expanded ? null : item.path)}>
                        {t.issues.detail}
                      </button>
                    </div>
                  </div>
                  {expanded && <pre className="issue-detail">{item.error || "—"}</pre>}
                </li>
              );
            })}
          </ul>
        </section>
      ))}
    </div>
  );
}
