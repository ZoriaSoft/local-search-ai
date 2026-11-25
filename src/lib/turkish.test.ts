import assert from "node:assert/strict";
import test from "node:test";

import { matchRanges, oneEditApart, queryTerms, tokenSpans } from "./turkish.ts";

test("tokenization keeps combining marks with original UTF-16 spans", () => {
  const text = "𝄞 U\u0308CRET I\u0307STANBUL Ankara’da";
  assert.deepEqual(tokenSpans(text), [[3, 9], [10, 19], [20, 29]]);
  assert.deepEqual(queryTerms(text), ["ücret", "istanbul", "ankara’da"]);
  assert.deepEqual(matchRanges(text, queryTerms("ücret istanbul")), [[3, 9], [10, 19]]);
  assert.deepEqual(matchRanges("𝄞 ÜCRET İSTANBUL", queryTerms("U\u0308CRET I\u0307STANBUL")), [[3, 8], [9, 17]]);
});

test("one-edit matching is bounded to one insertion, deletion, substitution or swap", () => {
  for (const word of ["zavreklm", "zavxeklm", "zavrxeklm", "zaveklm", "zaverklm"]) {
    assert.equal(oneEditApart("zavreklm", word), true, word);
    assert.equal(oneEditApart(word, "zavreklm"), true, word);
    assert.deepEqual(matchRanges(`𝄞 ${word}`, queryTerms("zavreklm")), [[3, 3 + word.length]]);
  }
  assert.equal(oneEditApart("zavreklm", "zaxbeklm"), false);
  assert.equal(oneEditApart("𐐨bcde", "𐐨bced"), true);
  assert.equal(oneEditApart("𐐨bcde", "𐐨xced"), false);
  assert.deepEqual(matchRanges("zaxbeklm", queryTerms("zavreklm")), []);
});
