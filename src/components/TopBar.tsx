import type { RefObject } from "react";

import type { IndexProgress } from "../api/types";
import { t } from "../i18n";
import { CheckIcon, CloseIcon, SearchIcon } from "./Icons";

interface Props {
  inputRef: RefObject<HTMLInputElement | null>;
  query: string;
  onQuery: (text: string) => void;
  progress: IndexProgress;
  disabled: boolean; // first run: nothing to search yet
  onActivity: () => void;
}

/** Product bar: brand, the one search box, and what the indexer is doing. */
export function TopBar({ inputRef, query, onQuery, progress, disabled, onActivity }: Props) {
  const busy = progress.phase !== "idle";
  const done = Math.max(progress.textDone ?? 0, progress.done);
  const label = !busy
    ? t.activity.idle
    : progress.phase === "embedding"
      ? t.activity.embedding
      : t.activity.busy(done, progress.total);

  return (
    <header className="topbar">
      <div className="brand">
        <span className="brand-mark" aria-hidden="true">
          <svg viewBox="0 0 24 24" width="18" height="18" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
            <rect x="3.5" y="4" width="17" height="5" rx="1" />
            <path d="M5.5 9v9.5a1 1 0 0 0 1 1h11a1 1 0 0 0 1-1V9M10 13h4" />
          </svg>
        </span>
        <span className="brand-text">
          <strong>{t.appName}</strong>
          <small>{t.appTagline}</small>
        </span>
      </div>
      <div className="searchbox">
        <SearchIcon className="lead" size={18} />
        <input
          ref={inputRef}
          type="search"
          value={query}
          onChange={(e) => onQuery(e.target.value)}
          placeholder={t.search.placeholder}
          aria-label={t.search.label}
          spellCheck={false}
          autoComplete="off"
          disabled={disabled}
        />
        {query ? (
          <button className="icon-btn clear" onClick={() => onQuery("")} aria-label={t.search.clear} title={t.search.clear}>
            <CloseIcon />
          </button>
        ) : (
          <kbd className="hint" aria-hidden="true">
            {t.search.shortcut}
          </kbd>
        )}
      </div>
      <button
        className={`activity${busy ? " busy" : ""}`}
        onClick={onActivity}
        title={t.activity.open}
        disabled={disabled}
      >
        {busy ? <span className="spinner" aria-hidden="true" /> : <CheckIcon size={15} />}
        <span>{progress.cancelling ? t.index.cancelling : label}</span>
      </button>
    </header>
  );
}
