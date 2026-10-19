import type { RefObject } from "react";

import type { Backend } from "../api/client";
import type { ApiError, FileKind, SearchHit } from "../api/types";
import { FILE_KINDS } from "../api/types";
import { t } from "../i18n";
import { formatCount, formatDate } from "../lib/format";
import { nameRanges } from "../lib/nameRanges";
import { Highlighted } from "./Highlighted";
import { KindTile } from "./KindTile";
import { SearchIcon, WarnIcon } from "./Icons";
import { Preview } from "./Preview";

export type DateRange = keyof typeof t.filters.dateOptions;

interface Props {
  backend: Backend;
  query: string;
  kinds: FileKind[];
  onKinds: (kinds: FileKind[]) => void;
  range: DateRange;
  onRange: (range: DateRange) => void;
  hits: SearchHit[];
  total: number;
  tookMs: number;
  loading: boolean;
  error: ApiError | null;
  selected: string | null;
  onSelect: (path: string) => void;
  onOpen: (path: string, reveal?: boolean) => void;
  onRetry: () => void;
  onMore: () => void;
  documentCount: number;
  listRef: RefObject<HTMLUListElement | null>;
  onError: (message: string) => void;
}


export function SearchView(props: Props) {
  const { backend, query, kinds, onKinds, range, onRange, hits, total, tookMs, loading, error, selected } = props;
  const filtersActive = kinds.length > 0 || range !== "all";
  const hasMore = total > hits.length;

  let body: React.ReactNode;
  if (error) {
    body = (
      <div className="notice bad block" role="alert">
        <WarnIcon size={15} />
        <span>{t.errors[error.code]}</span>
        <button className="btn small" onClick={props.onRetry}>
          {t.retry}
        </button>
      </div>
    );
  } else if (!query.trim()) {
    body = (
      <div className="state">
        <div>
          <span className="state-icon" aria-hidden="true">
            <SearchIcon size={26} />
          </span>
          <h2>{t.empty.readyTitle}</h2>
          <p>{t.empty.readyBody(props.documentCount)}</p>
          <ul className="tips">
            {t.empty.tips.map((tip) => (
              <li key={tip}>{tip}</li>
            ))}
          </ul>
        </div>
      </div>
    );
  } else if (loading && hits.length === 0) {
    body = (
      <div aria-busy="true">
        {[0, 1, 2, 3, 4].map((i) => (
          <div className="skeleton" key={i}>
            <i />
            <i />
            <i />
          </div>
        ))}
      </div>
    );
  } else if (hits.length === 0) {
    body = (
      <div className="state">
        <div>
          <h2>{t.empty.noResultsTitle}</h2>
          <p>{t.empty.noResultsBody}</p>
          {filtersActive && (
            <button
              className="btn"
              onClick={() => {
                onKinds([]);
                onRange("all");
              }}
            >
              {t.empty.clearFilters}
            </button>
          )}
        </div>
      </div>
    );
  } else {
    body = (
      <>
        <ul className="results" role="listbox" aria-label={t.search.resultsLabel} ref={props.listRef} tabIndex={0}>
          {hits.map((hit) => (
            <li
              key={hit.path}
              data-path={hit.path}
              role="option"
              aria-selected={hit.path === selected}
              className="hit"
              onClick={() => props.onSelect(hit.path)}
              onDoubleClick={() => props.onOpen(hit.path)}
            >
              <KindTile kind={hit.kind} />
              <div className="hit-main">
                <div className="hit-name" title={hit.fileName}>
                  <Highlighted text={hit.fileName} ranges={nameRanges(hit.fileName, query)} />
                </div>
                <div className="hit-path" title={hit.folder}>
                  <bdi>{hit.folder}</bdi>
                </div>
                {hit.snippets.map((snippet, i) => (
                  <p className="snippet" key={i}>
                    {snippet.page !== null && hit.pageCount > 1 && <span className="pg">{t.search.pageOf(snippet.page)}</span>}
                    <Highlighted text={snippet.text} ranges={snippet.highlights} />
                  </p>
                ))}
                <div className="tags">
                  {hit.nameOnly && (
                    <span className="tag info" title={t.search.nameOnlyHelp}>
                      {t.search.nameOnly}
                    </span>
                  )}
                  {hit.semanticOnly && <span className="tag info">{t.search.semanticOnly}</span>}
                  {hit.ocrPages > 0 && <span className="tag">{t.search.ocrPages(hit.ocrPages, hit.pageCount)}</span>}
                  {hit.failedPages > 0 && (
                    <span className="tag bad">
                      <WarnIcon size={12} /> {t.search.failedPages(hit.failedPages)}
                    </span>
                  )}
                  {hit.lowConfidencePages > 0 && (
                    <span className="tag warn">
                      <WarnIcon size={12} /> {t.search.lowConfidence(hit.lowConfidencePages)}
                    </span>
                  )}
                </div>
              </div>
              <div className="hit-side">
                <span>{formatDate(hit.modified)}</span>
                <span>{t.search.pages(hit.pageCount)}</span>
              </div>
            </li>
          ))}
        </ul>
        {hasMore && (
          <div className="more">
            <button className="btn" onClick={props.onMore} disabled={loading}>
              {t.search.loadMore}
            </button>
          </div>
        )}
      </>
    );
  }

  return (
    <div className="search-view">
      <div className="toolbar" role="toolbar" aria-label={t.filters.kinds}>
        <div className="segmented" role="group" aria-label={t.filters.kinds}>
          <button aria-pressed={kinds.length === 0} onClick={() => onKinds([])}>
            {t.filters.allKinds}
          </button>
          {FILE_KINDS.map((kind) => (
            <button
              key={kind}
              aria-pressed={kinds.includes(kind)}
              onClick={() => onKinds(kinds.includes(kind) ? kinds.filter((x) => x !== kind) : [...kinds, kind])}
            >
              {t.kind[kind]}
            </button>
          ))}
        </div>
        <label className="select">
          <span>{t.filters.date}</span>
          <select value={range} onChange={(e) => onRange(e.target.value as DateRange)}>
            {(Object.keys(t.filters.dateOptions) as DateRange[]).map((key) => (
              <option key={key} value={key}>
                {t.filters.dateOptions[key]}
              </option>
            ))}
          </select>
        </label>
        {query.trim() && !error && (
          <span className="result-meta">
            <strong>{formatCount(total)}</strong> {t.search.resultWord}
            {t.search.showing(hits.length, total)} · {t.search.took(tookMs)}
          </span>
        )}
      </div>
      <div className="split">
        <section className="results-pane" aria-label={t.search.resultsLabel}>
          {body}
        </section>
        <Preview
          backend={backend}
          path={selected && hits.some((h) => h.path === selected) ? selected : null}
          query={query}
          onError={props.onError}
        />
      </div>
    </div>
  );
}
