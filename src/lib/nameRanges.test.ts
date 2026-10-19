import assert from "node:assert/strict";
import { test } from "node:test";

import { nameRanges } from "./nameRanges.ts";

test("file-name highlights ignore case and Turkish diacritics", () => {
  assert.deepEqual(nameRanges("Şirket Notları.txt", "sirket"), [[0, 6]]);
  assert.deepEqual(nameRanges("KİRA SÖZLEŞMESİ.pdf", "sozlesme"), [[5, 13]]);
  assert.deepEqual(nameRanges("IMG_20231117.jpg", "img 2023"), [[0, 3], [4, 8]]);
  assert.deepEqual(nameRanges("rapor.pdf", "fatura"), []);
});
