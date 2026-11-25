import assert from "node:assert/strict";
import { test } from "node:test";

import { nameRanges } from "./nameRanges.ts";

test("file-name highlights ignore case and Turkish diacritics", () => {
  assert.deepEqual(nameRanges("Şirket Notları.txt", "sirket"), [[0, 6]]);
  assert.deepEqual(nameRanges("KİRA SÖZLEŞMESİ.pdf", "sozlesme"), [[5, 13]]);
  assert.deepEqual(nameRanges("IMG_20231117.jpg", "img 2023"), [[0, 3], [4, 8]]);
  assert.deepEqual(nameRanges("rapor.pdf", "fatura"), []);
});


test("apostrophes fold symmetrically while original filename offsets stay intact", () => {
  assert.deepEqual(nameRanges("Ankara'da.txt", "ankarada"), [[0, 9]]);
  assert.deepEqual(nameRanges("Ankara’da.txt", "Ankara'da"), [[0, 9]]);
  assert.deepEqual(nameRanges("Ankarada.txt", "Ankara’da"), [[0, 8]]);
  assert.deepEqual(nameRanges("𝄞 Ankara’da ŞİRKET.txt", "ankarada sirket"), [[3, 12], [13, 19]]);
  assert.deepEqual(nameRanges("Ankara’da.txt", "da"), [[7, 9]]);
});

test("NFC and NFD filename/query pairs preserve original UTF-16 ranges", () => {
  const nfd = "𝄞 U\u0308CRET I\u0307STANBUL.txt";
  assert.deepEqual(nameRanges(nfd, "ücret istanbul"), [[3, 9], [10, 19]]);
  assert.deepEqual(nameRanges("𝄞 ÜCRET İSTANBUL.txt", "U\u0308CRET I\u0307STANBUL"), [[3, 8], [9, 17]]);
  assert.equal(nfd.slice(...nameRanges(nfd, "ücret")[0]!), "U\u0308CRET");
});

test("one-edit content queries also highlight their filename word", () => {
  for (const query of ["zavxeklm", "zavrxeklm", "zaveklm", "zaverklm"]) {
    assert.deepEqual(nameRanges("𝄞 zavreklm.txt", query), [[3, 11]], query);
  }
  assert.deepEqual(nameRanges("zavreklm.txt", "zaxbeklm"), []);
  assert.deepEqual(nameRanges("rapor.txt", "rpa"), [], "short queries have no fuzzy expansion");
});
