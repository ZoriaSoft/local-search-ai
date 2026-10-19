import { useEffect, useState } from "react";

import type { Backend } from "../api/client";
import { toApiError } from "../api/client";
import type { OcrLanguageInfo } from "../api/types";
import { LANGUAGES, language, markOcrChosen, t, type Language } from "../i18n";

interface Props {
  backend: Backend;
  onLanguage: (language: Language) => void;
  onError: (message: string) => void;
  /** Files done in the running scan: re-reads move `pendingRereads`. */
  indexDone: number;
  busy: boolean;
}

const OCR_ORDER = ["tur", "eng", "tur+eng"];

/** App language and the document language scanned pages are read in. */
export function SettingsView({ backend, onLanguage, onError, indexDone, busy }: Props) {
  const [ocr, setOcr] = useState<OcrLanguageInfo | null>(null);
  const [saving, setSaving] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  useEffect(() => {
    let alive = true;
    backend.ocrLanguage().then(
      (info) => alive && setOcr(info),
      (error: unknown) => alive && onError(t.errors[toApiError(error).code]),
    );
    return () => {
      alive = false;
    };
  }, [backend, onError, indexDone, busy]);

  const pickOcr = async (next: string) => {
    if (!ocr || next === ocr.language || saving) return;
    setSaving(next);
    setSaved(false);
    try {
      setOcr(await backend.setOcrLanguage(next));
      markOcrChosen();
      setSaved(true);
    } catch (error) {
      onError(t.errors[toApiError(error).code]);
    } finally {
      setSaving(null);
    }
  };

  return (
    <div className="page-view settings">
      <header className="page-header">
        <div>
          <h1>{t.settings.title}</h1>
          <p>{t.settings.lead}</p>
        </div>
      </header>

      <section className="panel settings-section">
        <fieldset>
          <legend>{t.settings.language}</legend>
          <p className="settings-help">{t.settings.languageHelp}</p>
          <div className="segmented" role="group" aria-label={t.settings.language}>
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
        </fieldset>
      </section>

      <section className="panel settings-section">
        <fieldset disabled={!ocr || saving !== null}>
          <legend>{t.settings.ocr}</legend>
          <p className="settings-help">{t.settings.ocrHelp}</p>
          {!ocr && <p className="muted">{t.settings.loading}</p>}
          {ocr && (
            <div className="option-list">
              {OCR_ORDER.map((code) => {
                const option = t.settings.ocrOptions[code];
                const installed = ocr.available.includes(code);
                if (!option) return null;
                return (
                  <label key={code} className={`option${installed ? "" : " unavailable"}`}>
                    <input
                      type="radio"
                      name="ocr-language"
                      value={code}
                      checked={ocr.language === code}
                      disabled={!installed}
                      onChange={() => void pickOcr(code)}
                    />
                    <span className="option-text">
                      <span className="option-title">{option.title}</span>
                      <span className="option-help">{installed ? option.help : t.settings.ocrMissing}</span>
                    </span>
                  </label>
                );
              })}
            </div>
          )}
          <p className="settings-status" role="status">
            {ocr && ocr.pendingRereads > 0 ? t.settings.rereading(ocr.pendingRereads) : saved ? t.settings.saved : ""}
          </p>
        </fieldset>
      </section>
    </div>
  );
}
