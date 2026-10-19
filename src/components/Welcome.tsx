import { LANGUAGES, language, t, type Language } from "../i18n";
import { FolderPlusIcon } from "./Icons";

interface Props {
  onAddFolder: () => void;
  onLanguage: (language: Language) => void;
}

/** First run: what the product does, in three steps, and one action. The
 * language switch is here too: Settings is not reachable before a folder. */
export function Welcome({ onAddFolder, onLanguage }: Props) {
  return (
    <section className="welcome" aria-labelledby="welcome-title">
      <div className="welcome-card">
        <div className="welcome-language segmented" role="group" aria-label={t.settings.language}>
          {LANGUAGES.map((lang) => (
            <button
              key={lang}
              lang={lang}
              aria-pressed={language === lang}
              onClick={() => language !== lang && onLanguage(lang)}
            >
              {t.settings.languages[lang]}
            </button>
          ))}
        </div>
        <h1 id="welcome-title">{t.welcome.title}</h1>
        <p className="lead">{t.welcome.lead}</p>
        <ol className="steps">
          {t.welcome.steps.map((step, i) => (
            <li key={step.title}>
              <span className="step-no" aria-hidden="true">
                {i + 1}
              </span>
              <div>
                <strong>{step.title}</strong>
                <p>{step.body}</p>
              </div>
            </li>
          ))}
        </ol>
        <button className="btn primary large" onClick={onAddFolder}>
          <FolderPlusIcon /> {t.welcome.action}
        </button>
        <p className="fineprint">{t.nav.offline}</p>
      </div>
    </section>
  );
}
