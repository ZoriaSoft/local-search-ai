import { locale } from "../i18n";

// Formatters follow the active UI language (rebuilt when it changes).
let cached: { locale: string; date: Intl.DateTimeFormat; dateTime: Intl.DateTimeFormat; number: Intl.NumberFormat } | null =
  null;

function formats() {
  if (cached?.locale !== locale) {
    cached = {
      locale,
      date: new Intl.DateTimeFormat(locale, { day: "2-digit", month: "2-digit", year: "numeric" }),
      dateTime: new Intl.DateTimeFormat(locale, { dateStyle: "short", timeStyle: "short" }),
      number: new Intl.NumberFormat(locale, { maximumFractionDigits: 1 }),
    };
  }
  return cached;
}

export const formatDate = (unix: number) => formats().date.format(unix * 1000);
export const formatDateTime = (unix: number) => formats().dateTime.format(unix * 1000);
/** Whole numbers with the active language's digit grouping. */
export const formatCount = (n: number) => new Intl.NumberFormat(locale).format(n);

export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${formats().number.format(value)} ${units[unit]}`;
}
