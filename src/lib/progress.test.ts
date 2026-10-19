// node --experimental-strip-types --test src/lib/progress.test.ts
import assert from "node:assert/strict";
import test from "node:test";

import { holdRatio, progressRatio } from "./progress.ts";

const base = {
  phase: "indexing", total: 0, done: 0, failed: 0, current: null, startedAt: 1, finishedAt: null,
  watching: false, cancelling: false, cancelled: false, textDone: 0, embedDone: 0, embedTotal: 0,
};
const P = (x) => ({ ...base, ...x });

test("no model: text stage only", () => {
  assert.equal(progressRatio(P({ total: 30, textDone: 15 }), false), 0.5);
  assert.equal(progressRatio(P({ total: 30, textDone: 30, done: 30 }), false), 1);
});

test("model: mean of text read and semantic index", () => {
  // the reported bug: text 30/30, embed 0/30 used to show an empty bar
  assert.equal(progressRatio(P({ total: 30, textDone: 30, embedTotal: 30, embedDone: 0 }), true), 0.5);
  assert.equal(progressRatio(P({ total: 30, textDone: 30, embedTotal: 30, embedDone: 15 }), true), 0.75);
  assert.equal(progressRatio(P({ total: 30, textDone: 30, done: 30, embedTotal: 30, embedDone: 30 }), true), 1);
});

test("model: unread files count toward the semantic stage, so reading never pulls it back", () => {
  const seq = [
    P({ total: 4, textDone: 0, embedTotal: 0 }),
    P({ total: 4, textDone: 1, embedTotal: 1 }),
    P({ total: 4, textDone: 1, embedTotal: 1, embedDone: 1 }),
    P({ total: 4, textDone: 2, embedTotal: 2, embedDone: 1 }),
    P({ total: 4, textDone: 3, embedTotal: 2, embedDone: 1 }), // a file without text
    P({ total: 4, textDone: 4, embedTotal: 3, embedDone: 2 }),
    P({ total: 4, textDone: 4, embedTotal: 3, embedDone: 3, done: 4 }),
  ];
  const r = seq.map((p) => progressRatio(p, true));
  for (let i = 1; i < r.length; i++) assert.ok(r[i] >= r[i - 1], `step ${i}: ${r}`);
  assert.equal(r[0], 0);
  assert.equal(r.at(-1), 1);
});

test("scan with a model: the embedding file's chunks move the semantic stage", () => {
  // one big file: text read, its 48 chunks embedding — the bar used to sit at 50 %
  const s = (x) => P({ total: 1, textDone: 1, embedTotal: 1, ...x });
  assert.equal(progressRatio(s({}), true), 0.5);
  assert.equal(progressRatio(s({ chunksDone: 24, chunksTotal: 48 }), true), 0.75);
  assert.equal(progressRatio(s({ chunksDone: 48, chunksTotal: 48 }), true), 1);
  assert.equal(progressRatio(s({ done: 1, embedDone: 1 }), true), 1);
  // two files: the second file's chunks after the first one finished
  const seq = [
    P({ total: 2, textDone: 2, embedTotal: 2, chunksDone: 0, chunksTotal: 4 }),
    P({ total: 2, textDone: 2, embedTotal: 2, chunksDone: 2, chunksTotal: 4 }),
    P({ total: 2, textDone: 2, embedTotal: 2, chunksDone: 4, chunksTotal: 4 }),
    P({ total: 2, textDone: 2, embedTotal: 2, embedDone: 1, done: 1 }),
    P({ total: 2, textDone: 2, embedTotal: 2, embedDone: 1, done: 1, chunksDone: 3, chunksTotal: 6 }),
    P({ total: 2, textDone: 2, embedTotal: 2, embedDone: 2, done: 2 }),
  ];
  const r = seq.map((p) => progressRatio(p, true));
  for (let i = 1; i < r.length; i++) assert.ok(r[i] >= r[i - 1], `step ${i}: ${r}`);
  assert.equal(r.at(-1), 1);
  // no share once every file is done (stale chunk counts are ignored)
  assert.equal(progressRatio(s({ done: 1, embedDone: 1, chunksDone: 1, chunksTotal: 48 }), true), 1);
});

test("model expected but none loaded, or no file has text: the semantic stage is not waited on", () => {
  assert.equal(progressRatio(P({ total: 5, textDone: 5, embedTotal: 0 }), true), 1);
});

test("backfill: embedDone / embedTotal plus the running file's chunks", () => {
  const b = (x) => P({ phase: "embedding", total: 30, embedTotal: 30, ...x });
  assert.equal(progressRatio(b({}), true), 0);
  assert.equal(progressRatio(b({ chunksDone: 8, chunksTotal: 16 }), true), 0.5 / 30);
  assert.equal(progressRatio(b({ done: 15, embedDone: 15 }), true), 0.5);
  assert.equal(progressRatio(b({ done: 16, embedDone: 15 }), true), 16 / 30); // one failed embed
  assert.equal(progressRatio(b({ done: 30, embedDone: 30, chunksDone: 3, chunksTotal: 3 }), true), 1);
  // without the semantic flag (app info not loaded yet) it is still the embed stage
  assert.equal(progressRatio(b({ done: 15, embedDone: 15 }), false), 0.5);
});

test("phases without counts", () => {
  assert.equal(progressRatio(P({ phase: "scanning" }), true), 0);
  assert.equal(progressRatio(P({ phase: "committing", total: 3, done: 3 }), true), 1);
  assert.equal(progressRatio(P({ phase: "embedding", embedTotal: 0 }), true), 0);
  assert.equal(progressRatio(P({ total: 0 }), true), 0);
});

test("holdRatio never moves back within a run, restarts for a new run", () => {
  let s = holdRatio(null, 1, 0.4);
  s = holdRatio(s, 1, 0.3);
  assert.equal(s.ratio, 0.4);
  s = holdRatio(s, 1, 0.9);
  assert.equal(s.ratio, 0.9);
  s = holdRatio(s, 2, 0.1);
  assert.deepEqual(s, { run: 2, ratio: 0.1 });
  assert.equal(holdRatio(null, 3, Number.NaN).ratio, 0);
  assert.equal(holdRatio(null, 3, 7).ratio, 1);
});
