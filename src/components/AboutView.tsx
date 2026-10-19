import { useEffect, useMemo, useState } from "react";

import type { Backend } from "../api/client";
import { toApiError } from "../api/client";
import type { AppInfo, LegalTexts, NoticeItem } from "../api/types";
import { language, locale, t } from "../i18n";
import { ArchiveIcon, ChevronIcon, ExternalIcon } from "./Icons";

interface Props {
  backend: Backend;
  info: AppInfo | null;
  onError: (message: string) => void;
}

/** Terms text → sections: a line like "2. Gizlilik" starts a new section. */
function sections(terms: string): { title: string; body: string[] }[] {
  const out: { title: string; body: string[] }[] = [];
  for (const raw of terms.split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    if (/^\d+\.\s/.test(line)) out.push({ title: line, body: [] });
    else out.at(-1)?.body.push(line);
  }
  return out;
}

const key = (groupId: string, item: NoticeItem) => `${groupId}/${item.name}/${item.version}`;

/** The public repository (README, issues, releases). */
export const SOURCE_URL = "https://github.com/ZoriaSoft/local-search-ai";

/** Notices are written in English with a Turkish copy for the Turkish UI. */
const localized = (english: string, turkish: string | undefined) => (language === "tr" && turkish ? turkish : english);

/** Version, terms of use and every bundled open-source component with its license text. */
export function AboutView({ backend, info, onError }: Props) {
  const [legal, setLegal] = useState<LegalTexts | null>(null);
  const [failed, setFailed] = useState(false);
  const [filter, setFilter] = useState("");
  const [openGroups, setOpenGroups] = useState<Set<string>>(new Set());
  const [openItem, setOpenItem] = useState<string | null>(null);
  const [licenseShown, setLicenseShown] = useState(false);

  useEffect(() => {
    let alive = true;
    backend.legalTexts().then(
      (texts) => alive && setLegal(texts),
      () => alive && setFailed(true),
    );
    return () => {
      alive = false;
    };
  }, [backend]);

  const total = legal ? legal.notices.groups.reduce((n, g) => n + g.items.length, 0) : 0;
  const needle = filter.trim().toLocaleLowerCase(locale);
  const groups = useMemo(() => {
    if (!legal) return [];
    return legal.notices.groups
      .map((g) => ({
        ...g,
        items: needle
          ? g.items.filter((i) => `${i.name} ${i.license}`.toLocaleLowerCase(locale).includes(needle))
          : g.items,
      }))
      .filter((g) => g.items.length > 0);
  }, [legal, needle]);

  const toggleGroup = (id: string) =>
    setOpenGroups((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });

  const openSource = () => void backend.openUrl(SOURCE_URL).catch(() => onError(t.errors.internal));

  const openFile = async () => {
    if (!legal) return;
    try {
      await backend.openPath(legal.noticesFile);
    } catch (error) {
      onError(t.errors[toApiError(error).code]);
    }
  };

  return (
    <div className="page-view about">
      <header className="page-header">
        <div>
          <h1>{t.about.title}</h1>
          <p>{t.about.lead}</p>
        </div>
      </header>

      <section className="panel about-hero">
        <div className="about-mark" aria-hidden="true">
          <ArchiveIcon size={26} />
        </div>
        <div>
          <h2>Zoria Arşiv</h2>
          <p className="about-version">{info ? t.about.version(info.version) : "…"}</p>
          <p className="muted">{t.about.copyright}</p>
        </div>
        <ul className="about-tags">
          {t.about.tags.map((tag) => (
            <li key={tag}>{tag}</li>
          ))}
        </ul>
      </section>

      {failed && (
        <section className="panel about-failed" role="alert">
          {t.about.loadFailed}
        </section>
      )}

      {legal && (
        <section className="panel about-terms" aria-labelledby="about-terms">
          <header>
            <h2 id="about-terms">{t.about.terms}</h2>
          </header>
          <div className="about-terms-body">
            {sections(language === "tr" ? legal.terms : legal.termsEn).map((s) => (
              <div key={s.title}>
                <h3>{s.title}</h3>
                {s.body.map((p) => (
                  <p key={p}>{p}</p>
                ))}
              </div>
            ))}
          </div>
        </section>
      )}

      {legal && (
        <section className="panel about-license" aria-labelledby="about-license">
          <header>
            <div>
              <h2 id="about-license">{t.about.license}</h2>
              <p>{t.about.licenseLead}</p>
            </div>
            <button className="btn small" onClick={openSource}>
              <ExternalIcon size={14} /> {t.about.sourceCode}
            </button>
          </header>
          <p className="about-source">
            <bdi>{SOURCE_URL}</bdi>
          </p>
          <button className="btn small ghost" aria-expanded={licenseShown} onClick={() => setLicenseShown((v) => !v)}>
            <ChevronIcon size={14} className={licenseShown ? "open" : undefined} />{" "}
            {licenseShown ? t.about.hideLicense : t.about.showLicense}
          </button>
          {licenseShown && <pre className="about-license-text">{legal.license}</pre>}
        </section>
      )}

      {legal && (
        <section className="panel about-oss" aria-labelledby="about-oss">
          <header>
            <div>
              <h2 id="about-oss">
                {t.about.components}
                <span className="count">{t.about.count(total)}</span>
              </h2>
              <p>{t.about.componentsLead(total)}</p>
              <p>{t.about.copyleft}</p>
            </div>
            <button className="btn small" onClick={openFile}>
              <ExternalIcon size={14} /> {t.about.openFile}
            </button>
          </header>
          <div className="about-ack">
            {(language === "tr" ? (legal.notices.acknowledgements_tr ?? legal.notices.acknowledgements) : legal.notices.acknowledgements).map((a) => (
              <p key={a}>{a}</p>
            ))}
          </div>
          <div className="about-filter">
            <input
              type="search"
              value={filter}
              onChange={(e) => setFilter(e.target.value)}
              placeholder={t.about.filter}
              aria-label={t.about.filter}
            />
          </div>
          {groups.length === 0 && <p className="about-empty">{t.about.noMatch}</p>}
          {groups.map((g) => {
            const expanded = needle !== "" || openGroups.has(g.id);
            return (
              <div className="oss-group" key={g.id}>
                <button className="oss-group-head" aria-expanded={expanded} onClick={() => toggleGroup(g.id)}>
                  <ChevronIcon size={14} className={expanded ? "open" : undefined} />
                  <span>{localized(g.title, g.title_tr)}</span>
                  <span className="count">{t.about.count(g.items.length)}</span>
                </button>
                {expanded && (
                  <ul>
                    {g.items.map((item) => {
                      const k = key(g.id, item);
                      const shown = openItem === k;
                      return (
                        <li key={k}>
                          <div className="oss-row">
                            <div className="oss-main">
                              <span className="oss-name">{item.name}</span>
                              {item.version && <span className="oss-version">{item.version}</span>}
                              {item.note && <div className="oss-note">{localized(item.note, item.note_tr)}</div>}
                            </div>
                            <span className="oss-license" title={item.license}>
                              {item.license}
                            </span>
                            <button
                              className="btn small ghost"
                              aria-expanded={shown}
                              onClick={() => setOpenItem(shown ? null : k)}
                            >
                              {shown ? t.about.hideText : t.about.showText}
                            </button>
                          </div>
                          {shown && (
                            <div className="oss-texts">
                              {item.url && (
                                <p className="oss-url">
                                  {t.about.source}: <bdi>{item.url}</bdi>
                                </p>
                              )}
                              {item.texts.map((id) => (
                                <div key={id}>
                                  <h4>{legal.notices.texts[id]?.title}</h4>
                                  <pre>{legal.notices.texts[id]?.text}</pre>
                                </div>
                              ))}
                            </div>
                          )}
                        </li>
                      );
                    })}
                  </ul>
                )}
              </div>
            );
          })}
          <p className="about-generated">{t.about.generated(legal.notices.generated)}</p>
        </section>
      )}
    </div>
  );
}
