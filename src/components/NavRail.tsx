import type { AppInfo } from "../api/types";
import { t } from "../i18n";
import { formatCount } from "../lib/format";
import { AlertIcon, FolderIcon, InfoIcon, SearchIcon, SlidersIcon } from "./Icons";

export type View = "search" | "folders" | "issues" | "settings" | "about";

interface Props {
  view: View | null;
  onView: (view: View) => void;
  folderCount: number;
  issueCount: number;
  wholeFailures: number;
  info: AppInfo | null;
  disabled: boolean;
}


/** Left navigation: the three places of the app, the system's health, then Settings and About. */
export function NavRail({ view, onView, folderCount, issueCount, wholeFailures, info, disabled }: Props) {
  const item = (id: View, icon: React.ReactNode, label: string, count?: React.ReactNode) => (
    <li>
      <button
        className="nav-item"
        aria-current={view === id ? "page" : undefined}
        onClick={() => onView(id)}
        disabled={disabled}
      >
        {icon}
        <span className="nav-label">{label}</span>
        {count}
      </button>
    </li>
  );

  const state = (ok: boolean | undefined, okText: string, badText: string, detail?: string) => (
    <dd className={ok ? "ok" : "bad"} title={detail}>
      <span className="state-dot" aria-hidden="true" />
      {info ? (ok ? okText : badText) : "…"}
    </dd>
  );

  return (
    <nav className="nav" aria-label={t.nav.label}>
      <ul className="nav-list">
        {item("search", <SearchIcon size={17} />, t.nav.search)}
        {item("folders", <FolderIcon size={17} />, t.nav.folders, folderCount > 0 && <span className="count">{folderCount}</span>)}
        {item(
          "issues",
          <AlertIcon size={17} />,
          t.nav.issues,
          issueCount > 0 && <span className={`count${wholeFailures > 0 ? " alert" : ""}`}>{issueCount}</span>,
        )}
      </ul>
      <section className="system" aria-label={t.nav.system}>
        <h2>{t.nav.system}</h2>
        <dl>
          <div>
            <dt>{t.nav.documents}</dt>
            <dd className="num">{info ? formatCount(info.documentCount) : "…"}</dd>
          </div>
          <div>
            <dt>{t.nav.ocr}</dt>
            {state(info?.ocrReady, t.nav.ready, t.nav.unavailable, info?.ocrDetail)}
          </div>
          <div>
            <dt>{t.nav.semantic}</dt>
            {state(info?.semanticReady, t.nav.on, t.nav.off, info?.semanticDetail)}
          </div>
        </dl>
        <p className="offline">{t.nav.offline}</p>
      </section>
      <ul className="nav-list nav-foot">
        {item("settings", <SlidersIcon size={17} />, t.nav.settings)}
        {item("about", <InfoIcon size={17} />, t.nav.about)}
      </ul>
    </nav>
  );
}
