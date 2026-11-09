// End to end against the artifacts `setup.sh` actually produces: the demo shard set
// (`demo/shards/`, format v8 — header checksum plus a chunked dictionary) served over
// HTTP `Range`, read by the browser client (`core/shards.js`) and the real WASM engine.
//
// The comparison is the flat `demo/index.rsx` loaded whole: same doc ids, scores, URLs,
// titles, snippets and highlights. That is what makes this a check of the client and not
// of the builder: two different load paths, one ranking.
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
  loadDocPages,
  loadShardIndex,
} from "../core/shards.js";

const HERE = dirname(fileURLToPath(import.meta.url));
const DEMO = join(HERE, "..", "demo");
const ENGINE = join(HERE, "..", "..", "..", "wasm", "pkg-node", "rustsearch_wasm.js");
const READY =
  existsSync(ENGINE) &&
  existsSync(join(DEMO, "index.rsx")) &&
  existsSync(join(DEMO, "shards", "manifest.txt"));

const QUERY = "search";

/** Serve one directory with byte ranges. */
async function rangeServer(root) {
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
      response.writeHead(206, {
        "Content-Range": `bytes ${start}-${end}/${body.length}`,
        "Content-Length": slice.length,
      });
      response.end(slice);
      return;
    }
    response.writeHead(200, { "Content-Length": body.length });
    response.end(body);
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
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
  "the demo v8 shard set answers like the flat artifact through the WASM client",
  { skip: READY ? false : "artifacts are missing (run ./setup.sh)" },
  async () => {
    const { Engine, fnv1a64 } = createRequire(import.meta.url)(ENGINE);
    const artifact = new Uint8Array(readFileSync(join(DEMO, "index.rsx")));
    const flat = new Engine(artifact);
    const server = await rangeServer(join(DEMO, "shards"));
    try {
      assert.ok(flat.formatVersion >= 7, `expected the checksummed header, got v${flat.formatVersion}`);
      assert.ok(flat.chunkCount > 0, "the demo is published with a chunked dictionary");

      const loaded = await loadShardIndex(`${server.url}/manifest.txt`, { hash: fnv1a64 });
      const engine = Engine.fromSparse(loaded.bytes);
      try {
        assert.equal(engine.formatVersion, flat.formatVersion, "same format on both paths");
        assert.equal(engine.checksum, flat.checksum, "same checksum on both paths");
        assert.equal(engine.docCount, flat.docCount, "same document count");
        assert.equal(engine.chunkCount, flat.chunkCount, "same dictionary split");
        assert.equal(engine.attachedChunks, 0, "a cold open attaches no chunk");

        // Rank, fetch exactly what the query routed, then rank again, as the worker does.
        await attachQueryChunks(loaded, engine, QUERY, { prefix: true, fuzzy: false });
        let payload = JSON.parse(engine.searchJson(QUERY, 5));
        assert.ok(payload.results.length > 0, `no hits for ${QUERY}`);
        const pages = await loadDocPages(loaded, engine, payload.results.map((hit) => hit.docId));
        const blocks = await attachHitBlocks(loaded, engine, payload.results);
        assert.ok(pages + blocks > 0, "the hits needed records or blocks from the shard set");
        payload = JSON.parse(engine.searchJson(QUERY, 5));

        const expected = JSON.parse(flat.searchJson(QUERY, 5));
        assert.deepEqual(fields(payload.results), fields(expected.results));
        assert.ok(
          loaded.stats.bytes < artifact.length,
          `a cold query (${loaded.stats.bytes} B) must cost less than the artifact (${artifact.length} B)`,
        );
      } finally {
        engine.free();
      }
    } finally {
      flat.free();
      await server.close();
    }
  },
);
