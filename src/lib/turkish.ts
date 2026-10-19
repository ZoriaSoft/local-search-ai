// Turkish-aware normalization matching crates/arsiv-core/src/turkish.rs
// and crates/arsiv-core/src/index.rs (stem/prefix expansion rules).

const ASCII: Record<string, string> = {
  ç: "c", ğ: "g", ı: "i", ö: "o", ş: "s", ü: "u", â: "a", î: "i", û: "u",
};

export function turkishLower(text: string): string {
  return text.normalize("NFC").replace(/I/g, "ı").replace(/İ/g, "i").toLowerCase().normalize("NFC");
}

export function asciiFold(text: string): string {
  return text.replace(/[çğıöşüâîû]/g, (ch) => ASCII[ch] ?? ch);
}

const WORD = /[\p{L}\p{N}]+(?:['’][\p{L}\p{N}]+)*/gu;

/** Folded term form: ASCII-folded with apostrophes dropped. */
export function foldedTerm(term: string): string {
  return asciiFold(term).replace(/['’]/g, "");
}

const MIN_PREFIX_LEN = 4;
const MIN_STEM_LEN = 4;

// Folded suffixes, longest first. Must match index.rs::SUFFIXES.
const SUFFIXES = [
  "lariniz", "leriniz", "larimiz", "lerimiz", "lardan", "lerden", "larinda", "lerinde",
  "sindan", "sinden", "sinda", "sinde", "sina", "sine", "sini", "sinin",
  "mizdan", "mizden", "mizda", "mizde", "miza", "mize", "mizi", "mizin",
  "nizdan", "nizden", "nizda", "nizde", "niza", "nize", "nizi", "nizin",
  "umuz", "unuz", "iniz", "imiz", "inin", "unun",
  "lara", "lere", "lari", "leri", "larin", "lerin", "larim", "lerim",
  "ndan", "nden", "nda", "nde", "nin", "nun",
  "dan", "den", "tan", "ten", "yla", "yle",
  "ina", "ine", "una", "une", "cen",
  "li", "lu", "siz", "suz",
  "la", "le", "ya", "ye", "na", "ne",
  "si", "su", "sa", "se", "ci", "cu",
  "in", "un",
  "a", "e", "i", "u",
];

/** Folded stems after stripping a single suffix (`sozlesmesi`→`sozlesme`). */
export function stemVariants(folded: string): string[] {
  const out: string[] = [];
  for (const suffix of SUFFIXES) {
    if (folded.endsWith(suffix)) {
      const head = folded.slice(0, -suffix.length).replace(/['’]+$/, "");
      if (head.length >= MIN_STEM_LEN && !out.includes(head)) out.push(head);
    }
  }
  return out;
}

/** Word token spans as UTF-16 [start, end) offsets. */
export function tokenSpans(text: string): [number, number][] {
  return Array.from(text.matchAll(WORD), (m) => [m.index, m.index + m[0].length]);
}

export function queryTerms(query: string): string[] {
  return [...new Set(Array.from(query.matchAll(WORD), (m) => turkishLower(m[0])))];
}

/** Token spans matching any term: exact, folded, stem, or strict prefix. */
export function matchRanges(text: string, terms: string[]): [number, number][] {
  const foldedTerms = terms.map(foldedTerm);
  const stems = foldedTerms.flatMap(stemVariants);
  return tokenSpans(text).filter(([s, e]) => {
    const lower = turkishLower(text.slice(s, e));
    if (terms.includes(lower)) return true;
    const folded = foldedTerm(lower);
    return (
      foldedTerms.includes(folded) ||
      stems.includes(folded) ||
      foldedTerms.some((t) => t.length >= MIN_PREFIX_LEN && folded.startsWith(t))
    );
  });
}
