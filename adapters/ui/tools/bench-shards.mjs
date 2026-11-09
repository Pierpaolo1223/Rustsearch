// Compare a sharded format-v5 artifact with the same artifact loaded whole.
//
//   node tools/bench-shards.mjs --flat /tmp/v5-10k.rsx \
//       --shards /tmp/shards-10k --label 10k
//
// `--flat` is what `?index=` does: one request for the whole `.rsx`, then maybe a
// query. `--shards` is what `?shards=` does: the manifest and `meta` at cold start,
// then the term chunks, record pages and per-hit blocks of each query, all through
// the real client (`core/shards.js`) and the real WASM engine (`wasm/pkg-node`).
// If that module is missing, build it with:
//   wasm-bindgen --target nodejs --out-dir wasm/pkg-node \
//     target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm
//
// Cold numbers use a fresh index per query, so every byte is paid again; warm ones
// keep one isolate for `--runs` queries and show what the caches buy. Every query
// also compares the two result sets field by field: the sharded layout must answer
// the flat artifact's ranking and rendering exactly.
//
// `--meta-concurrency N` limits the parallel metadata-slice fetches (0 = no cap),
// and `--http` serves the shard directory over real loopback HTTP instead of the
// in-process file fetch, so the cold-start line includes the round trips.
// `--http-latency MS` delays each HTTP response to model a real RTT; without it the
// loopback has no round-trip term, so the concurrency limit cannot change the time.
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import { basename, dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import {
  attachHitBlocks,
  attachQueryChunks,
  DEFAULT_META_CONCURRENCY,
  loadAllDocPages,
  loadDocPages,
  loadShardIndex,
} from "../core/shards.js";

const require = createRequire(import.meta.url);
const WASM = join(
  dirname(fileURLToPath(import.meta.url)),
  "..",
  "..",
  "rustsearch",
  "wasm",
  "pkg-node",
  "rustsearch_wasm.js",
);
const { Engine, fnv1a64 } = require(WASM);

// Twenty distinct queries, so the warm session measures caching between different
// terms rather than the same five over and over. The first five are the cold set.
const QUERIES = [
  "postgres",
  "deploy preview",
  "security",
  "immutable index",
  "docker compose",
  "search",
  "database",
  "cache",
  "worker",
  "artifact",
  "ranking",
  "benchmark",
  "test",
  "error",
  "config",
  "server",
  "client",
  "token",
  "query",
  "index",
];
const COLD_QUERIES = 5;

function parseArgs(argv) {
  const options = {
    flat: null,
    shards: null,
    label: "corpus",
    limit: 5,
    runs: 20,
    queries: QUERIES,
    metaConcurrency: DEFAULT_META_CONCURRENCY,
    http: false,
    httpLatency: 0,
    jsHash: false,
  };
  for (let index = 2; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    switch (flag) {
      case "--flat":
        options.flat = value;
        index += 1;
        break;
      case "--shards":
        options.shards = value;
        index += 1;
        break;
      case "--label":
        options.label = value;
        index += 1;
        break;
      case "--limit":
        options.limit = Number(value);
        index += 1;
        break;
      case "--runs":
        options.runs = Number(value);
        index += 1;
        break;
      case "--queries":
        options.queries = value.split(",").filter(Boolean);
        index += 1;
        break;
      case "--meta-concurrency":
        options.metaConcurrency = ["0", "inf", "infinity"].includes(value?.toLowerCase())
          ? 0
          : Number(value);
        index += 1;
        break;
      case "--http":
        options.http = true;
        break;
      case "--http-latency":
        options.httpLatency = Number(value);
        options.http = true;
        index += 1;
        break;
      case "--js-hash":
        options.jsHash = true;
        break;
      default:
        throw new Error(`unknown flag: ${flag}`);
    }
  }
  if (!options.flat || !options.shards) {
    throw new Error("usage: bench-shards.mjs --flat ARTIFACT --shards DIR --label NAME");
  }
  return options;
}

/* ------------------------------------------------------------- file fetch */

/**
 * Serve the shard parts from a directory as the browser would get them: a `Range`
 * header slices the object, no header returns it whole, and the byte counter sees
 * exactly what a network client would.
 */
function serveDirectory(root) {
  return async (url, init = {}) => {
    const name = basename(new URL(url).pathname);
    let body;
    try {
      body = readFileSync(join(root, name));
    } catch {
      return {
        ok: false,
        status: 404,
        statusText: "Not Found",
        arrayBuffer: async () => new ArrayBuffer(0),
      };
    }
    const header = init.headers?.Range;
    if (!header) {
      return {
        ok: true,
        status: 200,
        text: async () => new TextDecoder().decode(body),
        arrayBuffer: async () =>
          body.buffer.slice(body.byteOffset, body.byteOffset + body.length),
      };
    }
    const [, start, end] = /^bytes=(\d+)-(\d+)$/.exec(header).map(Number);
    const slice = body.subarray(start, end + 1);
    return {
      ok: true,
      status: 206,
      arrayBuffer: async () =>
        slice.buffer.slice(slice.byteOffset, slice.byteOffset + slice.length),
    };
  };
}

/** The same directory over loopback HTTP with `206`/`Content-Range`, for timing. */
async function serveOverHttp(root, latency = 0) {
  const server = createServer((request, response) => {
    const respond = () => {
      const name = basename(new URL(request.url, "http://x").pathname);
      let body;
      try {
        body = readFileSync(join(root, name));
      } catch {
        response.writeHead(404);
        response.end();
        return;
      }
      const header = request.headers.range;
      const match = header ? /^bytes=(\d+)-(\d+)$/.exec(header) : null;
      response.setHeader("Accept-Ranges", "bytes");
      if (!match) {
        response.writeHead(200, { "Content-Length": body.length });
        response.end(body);
        return;
      }
      const start = Number(match[1]);
      const end = Math.min(Number(match[2]), body.length - 1);
      const slice = body.subarray(start, end + 1);
      response.writeHead(206, {
        "Content-Range": `bytes ${start}-${end}/${body.length}`,
        "Content-Length": slice.length,
      });
      response.end(slice);
    };
    if (latency > 0) setTimeout(respond, latency);
    else respond();
  });
  await new Promise((resolve) => server.listen(0, "127.0.0.1", resolve));
  return {
    url: `http://127.0.0.1:${server.address().port}`,
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}

/* ---------------------------------------------------------------- search */

function rank(engine, query, limit, group = false) {
  const started = performance.now();
  const raw = engine.searchJsonWithOptions(query, limit, true, false, 1, 2, "", group);
  const payload = JSON.parse(raw);
  return { payload, elapsed: performance.now() - started };
}

function fields(results) {
  return results.map((hit) => ({
    docId: hit.docId,
    score: hit.score,
    url: hit.url,
    title: hit.title,
    snippet: hit.snippet,
    highlights: hit.highlights,
    matches: hit.matches,
  }));
}

async function shardedQuery(loaded, engine, query, limit) {
  const chunks = await attachQueryChunks(loaded, engine, query, { prefix: true, fuzzy: false });
  let { payload, elapsed } = rank(engine, query, limit);
  const pages = await loadDocPages(loaded, engine, payload.results.map((hit) => hit.docId));
  const blocks = await attachHitBlocks(loaded, engine, payload.results);
  if (chunks > 0 || pages > 0 || blocks > 0) ({ payload, elapsed } = rank(engine, query, limit));
  return { payload, elapsed };
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  return sorted[Math.floor(sorted.length / 2)];
}

const percent = (part, whole) => `${((100 * part) / whole).toFixed(1)}%`;

/* ------------------------------------------------------------------ main */

const options = parseArgs(process.argv);
const artifact = new Uint8Array(readFileSync(options.flat));
const http = options.http ? await serveOverHttp(options.shards, options.httpLatency) : null;
if (!http) globalThis.fetch = serveDirectory(options.shards);
const manifestUrl = http ? `${http.url}/manifest.txt` : "http://bench/manifest.txt";
const load = () =>
  loadShardIndex(manifestUrl, {
    metaConcurrency: options.metaConcurrency,
    // The worker verifies with the WASM hash; `--js-hash` keeps the old JS one so
    // the two can be compared on the same machine.
    ...(options.jsHash ? {} : { hash: fnv1a64 }),
  });

const flatEngine = new Engine(artifact);
const cold = [];
let mismatches = 0;
let totalHits = 0;
const coldQueries = options.queries.slice(0, COLD_QUERIES);
for (const query of coldQueries) {
  const flat = rank(flatEngine, query, options.limit);

  const loadStarted = performance.now();
  const loaded = await load();
  const loadMs = performance.now() - loadStarted;
  const engine = Engine.fromSparse(loaded.bytes);
  const loadBytes = loaded.stats.bytes;
  const loadRequests = loaded.stats.requests;
  const sharded = await shardedQuery(loaded, engine, query, options.limit);
  const queryBytes = loaded.stats.bytes - loadBytes;
  const queryRequests = loaded.stats.requests - loadRequests;

  if (JSON.stringify(fields(sharded.payload.results)) !== JSON.stringify(fields(flat.payload.results))) {
    mismatches += 1;
    console.error(`MISMATCH ${JSON.stringify(query)}`);
  }
  totalHits += flat.payload.results.length;
  cold.push({
    query,
    hits: flat.payload.results.length,
    flatBytes: artifact.length,
    flatRequests: 1,
    loadBytes,
    loadRequests,
    loadMs,
    queryBytes,
    queryRequests,
    shardBytes: loaded.stats.bytes,
    shardRequests: loaded.stats.requests,
    flatMs: flat.elapsed,
    shardMs: sharded.elapsed,
  });
  engine.free();
}

// Warm session: one index, the query list repeated to `--runs` queries, every
// block and chunk paid once.
// One grouped query, the shape that needs the whole record table before ranking.
const groupQuery = coldQueries[0];
const flatGroup = rank(flatEngine, groupQuery, options.limit, true);
const groupLoaded = await load();
const groupEngine = Engine.fromSparse(groupLoaded.bytes);
await attachQueryChunks(groupLoaded, groupEngine, groupQuery, { prefix: true, fuzzy: false });
const groupPages = await loadAllDocPages(groupLoaded, groupEngine);
let groupPayload = rank(groupEngine, groupQuery, options.limit, true);
await attachHitBlocks(groupLoaded, groupEngine, groupPayload.payload.results);
groupPayload = rank(groupEngine, groupQuery, options.limit, true);
const groupSame =
  JSON.stringify(fields(groupPayload.payload.results)) ===
  JSON.stringify(fields(flatGroup.payload.results));
if (!groupSame) mismatches += 1;

const warmLoaded = await load();
const warmEngine = Engine.fromSparse(warmLoaded.bytes);
const warmTimes = [];
for (let run = 0; run < options.runs; run += 1) {
  const query = options.queries[run % options.queries.length];
  const { payload, elapsed } = await shardedQuery(warmLoaded, warmEngine, query, options.limit);
  if (payload.results.length === 0) console.error(`no hits for ${JSON.stringify(query)}`);
  warmTimes.push(elapsed);
}

const coldBytes = cold.reduce((sum, row) => sum + row.shardBytes, 0);
const coldFlat = cold.reduce((sum, row) => sum + row.flatBytes, 0);
const coldRequests = cold.reduce((sum, row) => sum + row.shardRequests, 0);
const queryBytes = cold.reduce((sum, row) => sum + row.queryBytes, 0);
const queryRequests = cold.reduce((sum, row) => sum + row.queryRequests, 0);
const flatColdMs = median(cold.map((row) => row.flatMs));
const shardColdMs = median(cold.map((row) => row.shardMs));

console.log(
  `\n${options.label}: ${artifact.length} B artifact, ${cold.length} cold queries, ` +
    `${options.runs} warm queries, limit ${options.limit}`,
);
console.log(
  `  cold start      flat ${artifact.length} B / 1 request   ` +
    `sharded ${cold[0].loadBytes} B / ${cold[0].loadRequests} requests   ` +
    `p50 ${median(cold.map((row) => row.loadMs)).toFixed(1)} ms ` +
    `(meta concurrency ${options.metaConcurrency === 0 ? "unlimited" : options.metaConcurrency})`,
);
console.log(
  `  cold query avg  incremental ${Math.round(queryBytes / cold.length)} B / ` +
    `${(queryRequests / cold.length).toFixed(1)} requests (${percent(queryBytes, coldFlat)} of flat)   ` +
    `session ${Math.round(coldBytes / cold.length)} B / ${(coldRequests / cold.length).toFixed(1)} requests ` +
    `(${percent(coldBytes, coldFlat)} of flat)`,
);
for (const row of cold) {
  console.log(
    `    ${JSON.stringify(row.query).padEnd(18)} ${String(row.hits).padStart(2)} hits  ` +
      `sharded ${String(row.shardBytes).padStart(8)} B / ${String(row.shardRequests).padStart(2)} req   ` +
      `flat ${String(row.flatBytes).padStart(8)} B   ` +
      `engine ${row.shardMs.toFixed(2)} vs ${row.flatMs.toFixed(2)} ms`,
  );
}
console.log(
  `  warm ${options.runs} queries ${warmLoaded.stats.bytes} B / ${warmLoaded.stats.requests} requests  ` +
    `query p50 ${median(warmTimes).toFixed(2)} ms`,
);
console.log(
  `  grouped query   ${JSON.stringify(groupQuery)} ${flatGroup.payload.results.length} hits  ` +
    `sharded ${groupLoaded.stats.bytes} B / ${groupLoaded.stats.requests} requests ` +
    `(${groupPages} of ${groupEngine.docPageCount} record pages) versus flat ${artifact.length} B   ` +
    `same ${groupSame}`,
);
console.log(
  `  engine p50      flat ${flatColdMs.toFixed(2)} ms   sharded ${shardColdMs.toFixed(2)} ms   ` +
    `hits ${totalHits}   mismatches ${mismatches}`,
);

if (mismatches > 0) process.exitCode = 1;

flatEngine.free();
warmEngine.free();
groupEngine.free();
if (http) await http.close();
