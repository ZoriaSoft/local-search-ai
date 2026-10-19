// The active UI language. `t` is a live binding: after `setLanguage` every
// module that imported it reads the new strings, and the root re-renders
// the app (see main.tsx).

import { en } from "./en";
import { tr, type Strings } from "./tr";

export type Language = "tr" | "en";
export const LANGUAGES: Language[] = ["tr", "en"];

const STRINGS: Record<Language, Strings> = { tr, en };
const LOCALES: Record<Language, string> = { tr: "tr-TR", en: "en-US" };
const STORAGE_KEY = "arsiv.language";

/** Windows' display language (WebView2 reports it), Turkish or else English. */
export function systemLanguage(languages: readonly string[] = navigator.languages ?? [navigator.language]): Language {
  return languages.some((l) => l.toLowerCase().startsWith("tr")) ? "tr" : "en";
}

function storedLanguage(): Language | null {
  try {
    const value = localStorage.getItem(STORAGE_KEY);
    return value === "tr" || value === "en" ? value : null;
  } catch {
    return null;
  }
}

/** True until the user picks a language; the system language is used meanwhile. */
export function languageChosen(): boolean {
  return storedLanguage() !== null;
}

export let language: Language = storedLanguage() ?? systemLanguage();
export let t: Strings = STRINGS[language];
/** BCP 47 locale for numbers and dates in the active language. */
export let locale: string = LOCALES[language];

function apply(next: Language) {
  language = next;
  t = STRINGS[next];
  locale = LOCALES[next];
  document.documentElement.lang = next;
}
apply(language);

/** Switches the UI language and remembers the choice. */
export function setLanguage(next: Language) {
  try {
    localStorage.setItem(STORAGE_KEY, next);
  } catch {
    // Private storage unavailable: the choice lasts for this session.
  }
  apply(next);
}

const OCR_CHOSEN_KEY = "arsiv.ocrChosen";

/** The user picked a document language in Settings: stop following the UI language. */
export function markOcrChosen() {
  try {
    localStorage.setItem(OCR_CHOSEN_KEY, "1");
  } catch {
    // Without storage the default simply follows the UI language.
  }
}

export function ocrChosen(): boolean {
  try {
    return localStorage.getItem(OCR_CHOSEN_KEY) === "1";
  } catch {
    return false;
  }
}

/** Tesseract document language that suits a UI language on first run. */
export function defaultOcrLanguage(lang: Language): string {
  return lang === "tr" ? "tur" : "eng";
}
