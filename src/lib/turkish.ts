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

const WORD = /[\p{L}\p{N}][\p{L}\p{N}\p{M}]*(?:['’][\p{L}\p{N}][\p{L}\p{N}\p{M}]*)*/gu;

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

function pointWidth(text: string, at: number): number {
  return (text.codePointAt(at) ?? 0) > 0xffff ? 2 : 1;
}

function equalTail(left: string, a: number, right: string, b: number): boolean {
  if (left.length - a !== right.length - b) return false;
  while (a < left.length) {
    if (left.charCodeAt(a++) !== right.charCodeAt(b++)) return false;
  }
  return true;
}

/** Unicode one-edit distance, with an adjacent transposition costing one. */
export function oneEditApart(left: string, right: string): boolean {
  let a = 0;
  let b = 0;
  while (a < left.length && b < right.length) {
    const x = left.codePointAt(a);
    const y = right.codePointAt(b);
    const nextA = a + pointWidth(left, a);
    const nextB = b + pointWidth(right, b);
    if (x !== y) {
      return equalTail(left, nextA, right, nextB) || equalTail(left, nextA, right, b) || equalTail(left, a, right, nextB) || (
        nextA < left.length && nextB < right.length &&
        left.codePointAt(nextA) === y && right.codePointAt(nextB) === x &&
        equalTail(left, nextA + pointWidth(left, nextA), right, nextB + pointWidth(right, nextB))
      );
    }
    a = nextA;
    b = nextB;
  }
  return (a === left.length && (b === right.length || b + pointWidth(right, b) === right.length)) ||
    (b === right.length && a + pointWidth(left, a) === left.length);
}

export function codePointLength(text: string): number {
  let length = 0;
  for (let at = 0; at < text.length; at += (text.codePointAt(at) ?? 0) > 0xffff ? 2 : 1) length += 1;
  return length;
}

/** Token spans matching any term: exact, folded, stem, prefix, or one edit. */
export function matchRanges(text: string, terms: string[]): [number, number][] {
  const foldedTerms = terms.map(foldedTerm);
  const stems = foldedTerms.flatMap(stemVariants);
  const prefixes = [...foldedTerms, ...stems].filter((term) => codePointLength(term) >= MIN_PREFIX_LEN);
  const fuzzy = foldedTerms.filter((term) => codePointLength(term) >= 5);
  return tokenSpans(text).filter(([s, e]) => {
    const lower = turkishLower(text.slice(s, e));
    if (terms.includes(lower)) return true;
    const folded = foldedTerm(lower);
    return (
      foldedTerms.includes(folded) ||
      stems.includes(folded) ||
      prefixes.some((term) => folded.startsWith(term)) ||
      fuzzy.some((term) => oneEditApart(term, folded))
    );
  });
}
