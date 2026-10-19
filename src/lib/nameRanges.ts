import { asciiFold, foldedTerm, queryTerms, turkishLower } from "./turkish.ts";

/** Where the query's words sit in a file name (case- and diacritic-free). */
export function nameRanges(name: string, query: string): [number, number][] {
  const folded = asciiFold(turkishLower(name));
  // Folding keeps one char per char for Turkish names; anything else
  // (combining marks, ligatures) would shift offsets, so no highlight.
  if (folded.length !== name.length) return [];
  const ranges: [number, number][] = [];
  for (const term of queryTerms(query)) {
    const needle = foldedTerm(term);
    if (!needle) continue;
    for (let at = folded.indexOf(needle); at >= 0; at = folded.indexOf(needle, at + needle.length)) {
      ranges.push([at, at + needle.length]);
    }
  }
  ranges.sort((a, b) => a[0] - b[0]);
  return ranges.filter((r, i) => i === 0 || r[0] >= (ranges[i - 1]?.[1] ?? 0));
}
