import { codePointLength, foldedTerm, oneEditApart, queryTerms, tokenSpans, turkishLower } from "./turkish.ts";

const graphemes = new Intl.Segmenter(undefined, { granularity: "grapheme" });

/** Folded text with each UTF-16 unit mapped back to its original grapheme. */
function foldedName(name: string) {
  let text = "";
  const starts: number[] = [];
  const ends: number[] = [];
  for (const { segment, index } of graphemes.segment(name)) {
    const folded = foldedTerm(turkishLower(segment));
    text += folded;
    for (let unit = 0; unit < folded.length; unit += 1) {
      starts.push(index);
      ends.push(index + segment.length);
    }
  }
  return { text, starts, ends };
}

/** Filename substring matches, preserving original apostrophes/NFD offsets. */
export function nameRanges(name: string, query: string): [number, number][] {
  const folded = foldedName(name);
  const ranges: [number, number][] = [];
  for (const term of queryTerms(query)) {
    const needle = foldedTerm(term);
    if (!needle) continue;
    for (let at = folded.text.indexOf(needle); at >= 0; at = folded.text.indexOf(needle, at + needle.length)) {
      const start = folded.starts[at];
      const end = folded.ends[at + needle.length - 1];
      if (start !== undefined && end !== undefined) ranges.push([start, end]);
    }
    // Content may have matched a one-edit typo even though the filename has
    // the correctly spelled word. Highlight that word using the same edit
    // semantics; literal substrings above keep their narrower highlight.
    if (codePointLength(needle) >= 5) {
      for (const [start, end] of tokenSpans(name)) {
        const token = foldedTerm(turkishLower(name.slice(start, end)));
        if (!token.includes(needle) && oneEditApart(needle, token)) ranges.push([start, end]);
      }
    }
  }
  ranges.sort((a, b) => a[0] - b[0] || a[1] - b[1]);
  const merged: [number, number][] = [];
  for (const range of ranges) {
    const previous = merged.at(-1);
    if (previous && range[0] < previous[1]) previous[1] = Math.max(previous[1], range[1]);
    else merged.push(range);
  }
  return merged;
}
