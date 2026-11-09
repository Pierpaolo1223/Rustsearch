import test from "node:test";
import assert from "node:assert/strict";

import { SearchSession } from "../core/search.js";

// The session talks to a worker through exactly two methods, so a fake one is
// enough to test the queue without a browser or a WASM build.
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

  searches() {
    return this.posted.filter((msg) => msg.kind === "search");
  }
}

const READY = {
  stats: {
    byteLength: 11_111,
    docCount: 10_000,
    termCount: 23_649,
    analyzer: "english",
    titleTokenCount: 1234,
    checksum: "ad0bf7a",
    formatVersion: 4,
  },
  facets: '{"section":{"guide":3}}',
};

function session() {
  const worker = new FakeWorker();
  return { worker, session: new SearchSession(worker, READY, "demo") };
}

const result = (hits = 1, elapsed = 0.5) => ({
  kind: "result",
  results: Array.from({ length: hits }, (_, index) => ({ docId: index })),
  appliedFilters: [],
  elapsed,
});

test("stats and facets come from the handshake, not from the worker", () => {
  const { session: current } = session();
  assert.equal(current.stats.docCount, 10_000);
  assert.equal(current.stats.formatVersion, 4);
  assert.equal(current.label, "demo");
  assert.equal(current.byteLength, 11_111);
  assert.deepEqual(current.facets(), { section: { guide: 3 } });
  // Parsed once: the same object comes back, and a second call does not re-parse.
  assert.equal(current.facets(), current.facets());
});

test("one query is posted with the engine arguments and resolved by its reply", async () => {
  const { worker, session: current } = session();
  const pending = current.search({
    query: "postgres",
    limit: 20,
    fuzzy: true,
    titleBoost: 3,
    group: true,
    filters: new Map([["section", new Set(["guide"])]]),
  });

  assert.equal(worker.posted.length, 1);
  const [posted] = worker.posted;
  assert.equal(posted.kind, "search");
  assert.equal(posted.query, "postgres");
  assert.equal(posted.limit, 20);
  assert.equal(posted.fuzzy, true);
  assert.equal(posted.titleBoost, 3);
  assert.equal(posted.group, true);
  assert.equal(posted.filters, "section\u001fguide");

  worker.reply({ ...result(2, 1.25), id: posted.id });
  assert.deepEqual(await pending, { ...result(2, 1.25), id: posted.id });
});

test("a second query waits for the first and is posted when it lands", async () => {
  const { worker, session: current } = session();
  const first = current.search({ query: "a" });
  const second = current.search({ query: "ab" });
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a"]);

  worker.reply(result(1));
  await first;
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a", "ab"]);

  worker.reply(result(2));
  assert.equal((await second).results.length, 2);
});

test("a third query drops the waiting one instead of queueing behind it", async () => {
  const { worker, session: current } = session();
  const first = current.search({ query: "a" });
  const second = current.search({ query: "ab" });
  const third = current.search({ query: "abc" });

  // The superseded call resolves to null at once, and is never sent.
  assert.equal(await second, null);
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a"]);

  worker.reply(result(1));
  await first;
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a", "abc"]);

  worker.reply(result(3));
  const payload = await third;
  assert.equal(payload.results.length, 3);
  assert.equal(worker.searches().some((msg) => msg.query === "ab"), false);
});

test("a fourth query while the first is still running keeps only the newest waiter", async () => {
  const { worker, session: current } = session();
  const first = current.search({ query: "a" });
  const dropped = [current.search({ query: "b" }), current.search({ query: "c" })];
  const last = current.search({ query: "d" });

  assert.deepEqual(await Promise.all(dropped), [null, null]);
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a"]);

  worker.reply(result(1));
  await first;
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a", "d"]);
  worker.reply(result(4));
  assert.equal((await last).results.length, 4);
});

test("an error reply rejects that query and the next waiting one still runs", async () => {
  const { worker, session: current } = session();
  const first = current.search({ query: "a" });
  const second = current.search({ query: "ab" });

  worker.reply({ kind: "error", id: 1, message: "the index is not loaded" });
  await assert.rejects(first, /the index is not loaded/);
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a", "ab"]);

  worker.reply(result(1));
  assert.equal((await second).results.length, 1);
});

test("dispose resolves whatever is pending and terminates the worker", async () => {
  const { worker, session: current } = session();
  const running = current.search({ query: "a" });
  const waiting = current.search({ query: "ab" });

  current.dispose();
  assert.equal(await running, null);
  assert.equal(await waiting, null);
  assert.equal(worker.terminated, true);
  // A reply that arrives after dispose cannot move the queue any more.
  worker.reply(result(1));
  assert.deepEqual(worker.searches().map((msg) => msg.query), ["a"]);
});
