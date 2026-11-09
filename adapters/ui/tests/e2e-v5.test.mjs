// End to end: a real format-v5 artifact, published as a shard set and read over
// real HTTP `Range` requests by the browser client and the real WASM engine.
//
// The fixture is `tests/fixtures/v5/`: 30 generated pages built with
// `--term-chunk-bytes 1024`, so the dictionary is three chunks, the records are one
// page, and the text section is most of the bytes. The server below answers a
// `Range` with `206` and a `Content-Range`, the way R2 and serve.py do, and records
// every read so the test can assert what a cold query actually pays for.
//
// The comparison is against `index.rsx` loaded whole: same doc ids, scores, URLs,
// titles, snippets and highlights, from the same 26.8 KiB artifact.
import assert from "node:assert/strict";
import { createServer } from "node:http";
import { existsSync, readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  attachHitBlocks,
  attachQueryChunks,
  loadAllDocPages,
  loadDocPages,
  loadShardIndex,
} from "../core/shards.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURE = join(HERE, "fixtures", "v5");
const ENGINE = join(HERE, "..", "..", "..", "wasm", "pkg-node", "rustsearch_wasm.js");
const READY =
  existsSync(ENGINE) &&
  existsSync(join(FIXTURE, "index.rsx")) &&
  existsSync(join(FIXTURE, "shards", "manifest.txt"));

/** Serve one directory with byte ranges, and keep a log of what was read. */
async function rangeServer(root) {
  const requests = [];
  const server = createServer((request, response) => {
    const name = decodeURIComponent(new URL(request.url, "http://x").pathname).replace(/^\//, "");
    let body;
    try {
      body = readFileSync(join(root, name));
    } catch {
      response.writeHead(404);
      response.end();
      return;
    }
    const match = /^bytes=(\d+)-(\d+)$/.exec(request.headers.range ?? "");
    response.setHeader("Accept-Ranges", "bytes");
    if (match) {
      const start = Number(match[1]);
      const end = Math.min(Number(match[2]), body.length - 1);
      const slice = body.subarray(start, end + 1);
      requests.push({ name, status: 206, start, length: slice.length });
      response.writeHead(206, {
        "Content-Range": `bytes ${start}-${end}/${body.length}`,
        "Content-Length": slice.length,
      });
      response.end(slice);
      return;
    }
    requests.push({ name, status: 200, start: 0, length: body.length });
    response.writeHead(200, { "Content-Length": body.length });
    response.end(body);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    requests,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

const fields = (results) =>
  results.map((hit) => ({
    docId: hit.docId,
    score: hit.score,
    url: hit.url,
    title: hit.title,
    snippet: hit.snippet,
    highlights: hit.highlights,
    matches: hit.matches,
  }));

test(
  "a real v5 artifact over HTTP Range answers like the flat artifact",
  { skip: READY ? false : "fixture or wasm/pkg-node is missing (run ./setup.sh)" },
  async () => {
    const { Engine, fnv1a64 } = createRequire(import.meta.url)(ENGINE);
    const artifact = new Uint8Array(readFileSync(join(FIXTURE, "index.rsx")));
    const flat = new Engine(artifact);
    const server = await rangeServer(join(FIXTURE, "shards"));
    const reads = (name) => server.requests.filter((entry) => entry.name === name).length;
    try {
      // Cold start: the manifest and `meta`, and nothing else. The dictionary
      // chunks, the record pages, the strings and the texts are routed to later.
      const loaded = await loadShardIndex(`${server.url}/manifest.txt`, { hash: fnv1a64 });
      assert.equal(loaded.stats.requests, 2, "manifest plus one metadata object");
      assert.ok(loaded.stats.bytes < artifact.length / 2, "cold start must be smaller than the artifact");
      for (const name of ["docrecords.bin", "strings.bin", "texts.bin", "chunk00000.bin"]) {
        assert.equal(reads(name), 0, `${name} must not be read at cold start`);
      }
      assert.ok(loaded.skipped.docrecords, "the record table is paged, not loaded");

      const engine = Engine.fromSparse(loaded.bytes);
      assert.equal(engine.chunkCount, 3, "the fixture was built with a 1 KiB budget");
      assert.equal(engine.docPageCount, 1, "one page holds the thirty records");
      const plan = Array.from(engine.chunksForQuery("walkdir", true, false));
      assert.ok(plan.length >= 1 && plan.length < engine.chunkCount, `query plan: ${plan}`);
      assert.equal(engine.attachedChunks, 0);

      // First rank, then exactly the routed chunk, the hit records and the hit
      // blocks, then rank again. The result must be the flat artifact's result.
      await attachQueryChunks(loaded, engine, "walkdir", { prefix: true, fuzzy: false });
      assert.equal(engine.attachedChunks, plan.length, "only the plan's chunks");
      for (const chunk of plan) {
        const name = `chunk${String(chunk).padStart(5, "0")}.bin`;
        assert.equal(reads(name), 1, `each routed chunk is read once: ${name}`);
      }
      let payload = JSON.parse(engine.searchJson("walkdir", 5));
      assert.ok(payload.results.length > 0);
      const pages = await loadDocPages(loaded, engine, payload.results.map((hit) => hit.docId));
      const blocks = await attachHitBlocks(loaded, engine, payload.results);
      assert.equal(pages, 1, "the five hits share the single record page");
      assert.equal(blocks, 10, "five strings and five texts");
      payload = JSON.parse(engine.searchJson("walkdir", 5));

      const expected = JSON.parse(flat.searchJson("walkdir", 5));
      assert.deepEqual(fields(payload.results), fields(expected.results), "same ranking and rendering");
      assert.ok(loaded.stats.bytes < artifact.length, "a cold query still costs less than the artifact");
      assert.equal(reads("docrecords.bin"), 1);
      assert.equal(reads("strings.bin"), 5);
      assert.equal(reads("texts.bin"), 5);
      // Every `Range` was honoured: only the manifest is a whole-object read.
      const whole = server.requests.filter((entry) => entry.status === 200);
      assert.deepEqual(whole.map((entry) => entry.name), ["manifest.txt"]);
      const textReads = server.requests.filter((entry) => entry.name === "texts.bin");
      assert.ok(textReads.every((entry) => entry.length < 19_755), "texts are slices, not the object");
      engine.free();

      // A grouped query needs the whole record table before it can rank: page
      // identity is in the record and grouping runs before `limit`.
      const before = server.requests.length;
      const groupedLoaded = await loadShardIndex(`${server.url}/manifest.txt`, { hash: fnv1a64 });
      const grouped = Engine.fromSparse(groupedLoaded.bytes);
      await attachQueryChunks(groupedLoaded, grouped, "walkdir", { prefix: true, fuzzy: false });
      assert.equal(await loadAllDocPages(groupedLoaded, grouped), grouped.docPageCount);
      let groupedPayload = JSON.parse(
        grouped.searchJsonWithOptions("walkdir", 5, true, false, 1, 2, "", true),
      );
      await attachHitBlocks(groupedLoaded, grouped, groupedPayload.results);
      groupedPayload = JSON.parse(
        grouped.searchJsonWithOptions("walkdir", 5, true, false, 1, 2, "", true),
      );
      const flatGrouped = JSON.parse(flat.searchJsonWithOptions("walkdir", 5, true, false, 1, 2, "", true));
      assert.deepEqual(
        fields(groupedPayload.results),
        fields(flatGrouped.results),
        "a grouped query over a sparse chunked index must equal the flat one",
      );
      const groupReads = server.requests.slice(before).filter((entry) => entry.name === "docrecords.bin");
      assert.equal(groupReads.length, grouped.docPageCount, "all pages, once each");
      grouped.free();
    } finally {
      flat.free();
      await server.close();
    }
  },
);
