import assert from "node:assert/strict";
import test from "node:test";

import { createIndexRevision } from "./indexRevision.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => { resolve = done; reject = fail; });
  return { promise, resolve, reject };
}

test("a changed index invalidates old text, image and search replies before a new render/request", async () => {
  const clock = createIndexRevision();
  const old = clock.snapshot();
  const text = deferred<string>();
  const image = deferred<string>();
  const search = deferred<string>();
  const writes: string[] = [];
  const pending = Promise.all([text.promise, image.promise, search.promise].map((reply) =>
    reply.then((value) => { if (old.isCurrent()) writes.push(value); }),
  ));
  const latest = clock.advance();
  // No replacement requests or cleanup have run: invalidation is immediate.
  text.resolve("stale same-path text");
  image.resolve("stale same-page image");
  search.resolve("stale search hits");
  await pending;
  assert.deepEqual(writes, []);
  assert.equal(latest.value, old.value + 1);
  assert.equal(latest.isCurrent(), true);
  const fresh = deferred<string>();
  const loaded = fresh.promise.then((value) => { if (latest.isCurrent()) writes.push(value); });
  fresh.resolve("new index text");
  await loaded;
  assert.deepEqual(writes, ["new index text"]);
});

test("old failures stay discarded across multiple refreshes, while incidental snapshots remain current", async () => {
  const clock = createIndexRevision();
  const old = clock.snapshot();
  const failure = deferred<string>();
  const errors: string[] = [];
  const loaded = failure.promise.catch(() => { if (old.isCurrent()) errors.push("old error"); });
  clock.advance();
  const current = clock.advance();
  assert.equal(clock.snapshot().value, current.value);
  assert.equal(current.isCurrent(), true);
  failure.reject(new Error("pre-refresh image failed"));
  await loaded;
  assert.deepEqual(errors, []);
  assert.equal(old.isCurrent(), false);
  assert.equal(current.isCurrent(), true);
});
