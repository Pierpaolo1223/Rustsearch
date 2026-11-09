import test from "node:test";
import assert from "node:assert/strict";

import { SearchSession } from "../core/search.js";

// The session talks to the engine worker through `postMessage`, so a fake worker is
// enough to prove the curation message ordering without a browser or a WASM build.
// What the engine does with the text is `core/src/rules.rs`, tested in Rust.
class FakeWorker {
  constructor() {
    this.posted = [];
    this.terminated = false;
    this.onmessage = null;
  }

  postMessage(msg) {
    this.posted.push(msg);
  }

  terminate() {
    this.terminated = true;
  }

  reply(msg) {
    this.onmessage?.({ data: msg });
  }
}

const READY = {
  stats: {
    byteLength: 1,
    docCount: 2,
    termCount: 3,
    analyzer: "english",
    titleTokenCount: 0,
    checksum: "0",
    formatVersion: 6,
  },
  facets: "{}",
};

function session() {
  const worker = new FakeWorker();
  return { worker, session: new SearchSession(worker, READY, "demo") };
}

test("setCurations posts the file text, in order with the query that follows", async () => {
  const { worker, session: current } = session();
  current.setCurations("install | pin /docs/install.html\n");
  const pending = current.search({ query: "install" });

  assert.deepEqual(
    worker.posted.map((msg) => msg.kind),
    ["curate", "search"],
  );
  assert.equal(worker.posted[0].text, "install | pin /docs/install.html\n");
  assert.equal(worker.posted[1].query, "install");

  worker.reply({ kind: "result", id: 1, results: [{ docId: 0 }], elapsed: 1 });
  assert.equal((await pending).results.length, 1);
});

test("an empty string drops the rules instead of sending a file", () => {
  const { worker, session: current } = session();
  current.setCurations("");
  assert.equal(worker.posted[0].kind, "curate");
  assert.equal(worker.posted[0].text, "");
});

test("the curations acknowledgement never resolves the query in flight", async () => {
  const { worker, session: current } = session();
  const pending = current.search({ query: "install" });
  current.setCurations("install | pin /docs/install.html\n");

  // The worker answers `curate` before the query; that message must not be mistaken
  // for the answer, or the caller would receive `{kind:"curations"}` as results.
  worker.reply({ kind: "curations", count: 1 });
  let settled = false;
  pending.then(() => {
    settled = true;
  });
  await Promise.resolve();
  assert.equal(settled, false, "the query is still running");

  worker.reply({ kind: "result", id: 1, results: [], elapsed: 1 });
  const payload = await pending;
  assert.equal(payload.kind, "result");
  assert.deepEqual(payload.results, []);
});

test("setCurations resolves with the number of curated queries in force", async () => {
  const { worker, session: current } = session();
  const curation = current.setCurations("install | pin /docs/install.html\n");
  const posted = worker.posted.at(-1);
  assert.equal(posted.kind, "curate");
  worker.reply({ kind: "curations", id: posted.id, count: 1 });
  assert.equal(await curation, 1);
});

test("a refused file rejects setCurations and leaves the query in flight alone", async () => {
  const { worker, session: current } = session();
  const pending = current.search({ query: "install" });
  const curation = current.setCurations("deploy | pin https://evil.example/x\n");
  const curate = worker.posted.at(-1);
  worker.reply({
    kind: "error",
    id: curate.id,
    message: "invalid curation: line 1: a pin takes one path on this site",
  });
  // The caller is told; the ranking it was waiting for is not a casualty of a bad
  // rule file, which is the difference between a note in the footer and an empty page.
  await assert.rejects(curation, /a pin takes one path/);

  const search = worker.posted.find((msg) => msg.kind === "search");
  worker.reply({ kind: "result", id: search.id, results: [{ docId: 0 }], elapsed: 1 });
  assert.equal((await pending).results.length, 1);
});
