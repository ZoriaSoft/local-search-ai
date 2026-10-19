import assert from "node:assert/strict";
import test from "node:test";

import { createRequestGate } from "./requestGate.ts";

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason: unknown) => void;
  const promise = new Promise<T>((done, fail) => {
    resolve = done;
    reject = fail;
  });
  return { promise, resolve, reject };
}

test("older metadata replies cannot restore removed folders or an old document count", async () => {
  const gate = createRequestGate();
  const state = { info: { documentCount: 12 }, folders: ["removed"] };
  const refresh = (info: Promise<{ documentCount: number }>, folders: Promise<string[]>) => {
    const current = gate.next();
    return Promise.all([
      info.then((value) => { if (current()) state.info = value; }),
      folders.then((value) => { if (current()) state.folders = value; }),
    ]);
  };
  const oldInfo = deferred<{ documentCount: number }>();
  const oldFolders = deferred<string[]>();
  const newInfo = deferred<{ documentCount: number }>();
  const newFolders = deferred<string[]>();
  const oldRefresh = refresh(oldInfo.promise, oldFolders.promise);
  const newRefresh = refresh(newInfo.promise, newFolders.promise);
  newFolders.resolve(["kept"]);
  newInfo.resolve({ documentCount: 3 });
  await newRefresh;
  assert.deepEqual(state, { info: { documentCount: 3 }, folders: ["kept"] });
  oldInfo.resolve({ documentCount: 12 });
  oldFolders.resolve(["removed", "kept"]);
  await oldRefresh;
  assert.deepEqual(state, { info: { documentCount: 3 }, folders: ["kept"] });
});

test("a late failure list cannot restore errors from a removed folder", async () => {
  const gate = createRequestGate();
  let failures = ["removed/broken.pdf"];
  const load = (response: Promise<string[]>) => {
    const current = gate.next();
    return response.then((value) => { if (current()) failures = value; });
  };
  const old = deferred<string[]>();
  const latest = deferred<string[]>();
  const oldLoad = load(old.promise);
  const latestLoad = load(latest.promise);
  latest.resolve([]);
  await latestLoad;
  old.resolve(["removed/broken.pdf"]);
  await oldLoad;
  assert.deepEqual(failures, []);
});

test("an older failure-list rejection cannot erase newer failures", async () => {
  const gate = createRequestGate();
  let failures: string[] = [];
  const load = (response: Promise<string[]>) => {
    const current = gate.next();
    return response.then(
      (value) => { if (current()) failures = value; },
      () => { if (current()) failures = []; },
    );
  };
  const old = deferred<string[]>();
  const latest = deferred<string[]>();
  const oldLoad = load(old.promise);
  const latestLoad = load(latest.promise);
  latest.resolve(["kept/broken.pdf"]);
  await latestLoad;
  old.reject(new Error("old request failed"));
  await oldLoad;
  assert.deepEqual(failures, ["kept/broken.pdf"]);
});

test("cleanup invalidates pending successes and errors; the next mount can request again", async () => {
  const gate = createRequestGate();
  const writes: string[] = [];
  const success = deferred<string>();
  const failure = deferred<string>();
  const current = gate.next();
  const replies = Promise.all([
    success.promise.then((value) => { if (current()) writes.push(value); }),
    failure.promise.then(undefined, () => { if (current()) writes.push("error"); }),
  ]);
  gate.invalidate();
  success.resolve("unmounted success");
  failure.reject(new Error("unmounted error"));
  await replies;
  assert.deepEqual(writes, []);
  const mounted = gate.next();
  await Promise.resolve("new mount").then((value) => { if (mounted()) writes.push(value); });
  assert.deepEqual(writes, ["new mount"]);
});
