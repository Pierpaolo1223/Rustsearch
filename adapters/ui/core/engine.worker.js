// The engine, off the main thread.
//
// Every query on the ten-thousand-document demo costs the same as it always did —
// what changes is who pays it. Here the WASM call, the JSON parse and the snippet
// decompression happen in this worker, so a keystroke returns to the page in
// microseconds even while a hundred-hit query is still running.
//
// The protocol is small and one-directional: the page sends `open`, `search`,
// `curate` and `dispose`; the worker answers `ready`, `result`, `curations` and
// `error`. Replies carry the request id, and they are posted in request order, which
// is what lets the page keep at most one query in flight and drop the ones a fast
// typist has already moved past.
//
// Curation is the one policy that does not live in the artifact: the page fetches the
// editor's text file and hands it here, where the engine parses it with the same
// implementation the CLI uses (`setCurations`). A malformed file is an error message,
// not a silent ranking change.

import init, { Engine, fnv1a64 } from "../vendor/pkg-web/rustsearch_wasm.js";
import {
  attachHitBlocks,
  attachQueryChunks,
  loadAllDocPages,
  loadDocPages,
  loadShardIndex,
} from "./shards.js";

let engine = null;
// Set when the artifact was loaded from a shard set: the parts that were not
// fetched yet, and the sets of the blocks already attached.
let shard = null;

async function bytesFor(msg) {
  if (msg.bytes) return msg.bytes;
  let response;
  try {
    response = await fetch(msg.url, { cache: "no-cache" });
  } catch (error) {
    throw new Error(`Could not load ${msg.url}: ${error.message ?? "network error"}`);
  }
  if (!response.ok) {
    throw new Error(`Could not load ${msg.url}: HTTP ${response.status} ${response.statusText}`.trim());
  }
  return new Uint8Array(await response.arrayBuffer());
}

async function openShards(msg) {
  await init();
  // Verify each metadata slice in WASM: the 35 MiB metadata run of a 1M-document
  // artifact costs milliseconds there instead of ~260 ms of per-byte JS.
  const loaded = await loadShardIndex(msg.url, { hash: fnv1a64 });
  let built;
  try {
    built = Engine.fromSparse(loaded.bytes);
  } catch (error) {
    throw new Error(`Not a valid rustsearch shard set: ${error.message ?? error}`);
  }
  if (engine) engine.free();
  engine = built;
  shard = loaded;
  return {
    stats: {
      byteLength: loaded.manifest.total,
      docCount: engine.docCount,
      termCount: engine.termCount,
      analyzer: engine.analyzer,
      titleTokenCount: engine.titleTokenCount,
      checksum: engine.checksum,
      formatVersion: engine.formatVersion,
      sharded: true,
      transfer: { ...loaded.stats },
    },
    facets: engine.facetsJson(),
  };
}

async function open(msg) {
  await init();
  const bytes = await bytesFor(msg);
  let built;
  try {
    built = new Engine(bytes);
  } catch (error) {
    throw new Error(`Not a valid rustsearch artifact: ${error.message ?? error}`);
  }
  if (engine) engine.free();
  engine = built;
  shard = null;
  return {
    stats: {
      byteLength: bytes.byteLength,
      docCount: engine.docCount,
      termCount: engine.termCount,
      analyzer: engine.analyzer,
      titleTokenCount: engine.titleTokenCount,
      checksum: engine.checksum,
      formatVersion: engine.formatVersion,
    },
    facets: engine.facetsJson(),
  };
}

function rank(msg) {
  const started = performance.now();
  const raw = engine.searchJsonWithOptions(
    msg.query,
    msg.limit,
    true,
    msg.fuzzy,
    msg.maxEdits,
    msg.titleBoost,
    msg.filters,
    msg.group,
  );
  const elapsed = performance.now() - started;
  const payload = JSON.parse(raw);
  return {
    results: Array.isArray(payload.results) ? payload.results : [],
    appliedFilters: Array.isArray(payload.filters) ? payload.filters : [],
    elapsed,
  };
}

async function search(msg) {
  if (!engine) throw new Error("the index is not loaded");
  if (!shard) return rank(msg);

  // A sharded index ranks only once the dictionary chunks the query needs are
  // here; the records, URLs and text blocks of the hits come after the ranking,
  // and the hit shapes (url, snippet) only exist once they are attached, so a
  // sharded query ranks twice when it had to fetch something.
  // Grouping reads the record of every candidate before it can collapse pages, so a
  // chunked sparse index has to have the whole record table before the first rank.
  const allPages = msg.group ? await loadAllDocPages(shard, engine) : 0;
  const attachedChunks = await attachQueryChunks(shard, engine, msg.query, {
    prefix: true,
    fuzzy: msg.fuzzy,
  });
  let payload = rank(msg);
  const pages = await loadDocPages(shard, engine, payload.results.map((hit) => hit.docId));
  const blocks = await attachHitBlocks(shard, engine, payload.results);
  if (allPages > 0 || pages > 0 || blocks > 0 || attachedChunks > 0) payload = rank(msg);
  return {
    ...payload,
    transfer: { ...shard.stats },
  };
}

self.onmessage = async (event) => {
  const msg = event.data;
  try {
    if (msg.kind === "open") {
      self.postMessage({ kind: "ready", ...(await open(msg)) });
    } else if (msg.kind === "openShards") {
      self.postMessage({ kind: "ready", ...(await openShards(msg)) });
    } else if (msg.kind === "search") {
      self.postMessage({ kind: "result", id: msg.id, ...(await search(msg)) });
    } else if (msg.kind === "curate") {
      if (!engine) throw new Error("the index is not loaded");
      // A worker message is ordered with the searches around it, so the next query
      // already sees these rules.
      let count = 0;
      if (msg.text) {
        count = engine.setCurations(msg.text);
      } else {
        engine.clearCurations();
      }
      self.postMessage({ kind: "curations", id: msg.id, count });
    } else if (msg.kind === "dispose") {
      if (engine) engine.free();
      engine = null;
    }
  } catch (error) {
    self.postMessage({ kind: "error", id: msg.id, message: error.message ?? String(error) });
  }
};
