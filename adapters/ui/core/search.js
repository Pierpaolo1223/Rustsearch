// Headless search API: everything the UI needs from the engine, with no DOM.
//
// The engine runs in ./engine.worker.js, so `search()` answers with a promise and
// the page never blocks on a query. That is the only difference a consumer sees:
// the protocol (`searchJsonWithOptions`, `facetsJson`, the statistics getters) is
// the engine's public boundary, forwarded by the worker, not redefined here.
//
// Two sources are supported from day one: a URL (the artifact served next to the
// page, or a remote one) and bytes (a file the user dropped in). Bytes are
// transferred to the worker, so the caller must not reuse the buffer afterwards.

import { encodeFilters } from "./format.js";

/** Start a worker, send one open message and wait for the handshake. */
async function openSession(workerMessage, label, transfer = []) {
  const worker = new Worker(new URL("./engine.worker.js", import.meta.url), { type: "module" });
  let ready;
  try {
    ready = await new Promise((resolve, reject) => {
      worker.onmessage = (event) => {
        const msg = event.data;
        if (msg.kind === "ready") resolve(msg);
        else if (msg.kind === "error") reject(new Error(msg.message));
      };
      worker.onerror = (event) => reject(new Error(event.message || "the engine worker could not start"));
      worker.postMessage(workerMessage, transfer);
    });
  } catch (error) {
    worker.terminate();
    throw error;
  }
  return new SearchSession(worker, ready, label);
}

/** Load and validate an artifact: a URL, or bytes to transfer. */
export async function openIndex(source, label = typeof source === "string" ? source : "local file") {
  if (source instanceof Uint8Array || source instanceof ArrayBuffer) {
    const bytes = source instanceof Uint8Array ? source : new Uint8Array(source);
    return openSession({ kind: "open", bytes, label }, label, [bytes.buffer]);
  }
  // A relative source has to be resolved here: inside the worker it would be
  // relative to ./engine.worker.js (core/), not to the page.
  const url = new URL(String(source), document.baseURI).href;
  return openSession({ kind: "open", url, label }, label);
}

/**
 * Load a shard set instead of the whole artifact.
 *
 * The metadata comes first, then the per-hit text blocks and the dictionary chunks
 * a query needs — so a huge artifact is never downloaded whole. `manifestUrl` is
 * the `manifest.txt` written by `rustsearch shard`.
 */
export async function openSharded(manifestUrl, label = "shards") {
  const url = new URL(String(manifestUrl), document.baseURI).href;
  return openSession({ kind: "openShards", url, label }, label);
}

/** One loaded index plus the query policy and the engine's own statistics. */
export class SearchSession {
  #worker;
  #facetsJson;
  #cachedFacets = null;
  #nextId = 1;
  #current = null;  // the query the worker is running
  #waiting = null;  // the only query that may run next
  #curation = null; // the curation message in flight

  constructor(worker, ready, label) {
    this.#worker = worker;
    this.#facetsJson = ready.facets;
    this.stats = { label, ...ready.stats };
    this.byteLength = ready.stats.byteLength;
    this.label = label;
    this.#worker.onmessage = (event) => this.#onMessage(event.data);
  }

  /**
   * Apply the editor's curation file (`search --curate FILE`): `query | pin /path`
   * and `query | boost /path 1.5`, parsed by the engine itself. Pass an empty string
   * to drop the rules. Messages are ordered with the queries, so a call made before
   * `search()` is in force for that query.
   *
   * Resolves to the number of curated queries in force, or to **null** when a newer
   * call superseded this one or the session was disposed, and rejects when the engine
   * refuses the file. The rejection is the caller's only way to learn that a ranking
   * is plain rather than curated, so it is never swallowed here.
   */
  setCurations(text) {
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      // One file at a time: a newer call supersedes the wait of the previous one.
      this.#curation?.resolve(null);
      this.#curation = { id, resolve, reject };
      this.#worker.postMessage({ kind: "curate", id, text: String(text ?? "") });
    });
  }

  /** `{ section: { value: count }, ... }`, `{}` when the build recorded none. */
  facets() {
    if (this.#cachedFacets) return this.#cachedFacets;
    try {
      const parsed = JSON.parse(this.#facetsJson);
      this.#cachedFacets = parsed && typeof parsed === "object" ? parsed : {};
    } catch {
      this.#cachedFacets = {};
    }
    return this.#cachedFacets;
  }

  /**
   * Run one query. `filters` is a `Map<field, Set<value>>`; the HTML contract is
   * AND across fields, OR inside one.
   *
   * Only the latest query matters: while one is running, a second call replaces
   * the one waiting instead of queueing behind it, and the superseded call
   * resolves to **null** so its caller knows it was dropped rather than answered.
   */
  search({
    query,
    limit = 10,
    fuzzy = false,
    maxEdits = 1,
    titleBoost = 2,
    group = false,
    filters = new Map(),
  }) {
    const request = {
      id: this.#nextId++,
      params: {
        query,
        limit,
        fuzzy,
        maxEdits,
        titleBoost,
        group,
        filters: encodeFilters(filters),
      },
    };
    return new Promise((resolve, reject) => {
      request.resolve = resolve;
      request.reject = reject;
      if (this.#current) {
        if (this.#waiting) this.#waiting.resolve(null);
        this.#waiting = request;
        return;
      }
      this.#current = request;
      this.#post(request);
    });
  }

  #post(request) {
    this.#worker.postMessage({ kind: "search", id: request.id, ...request.params });
  }

  #onMessage(msg) {
    // The `curations` acknowledgement and a curation error belong to `setCurations`,
    // never to the query in flight: a rejected rule file must not reject a search.
    if (msg.kind === "curations") {
      const pending = this.#curation;
      this.#curation = null;
      pending?.resolve(msg.count);
      return;
    }
    if (msg.kind === "error" && this.#curation && this.#curation.id === msg.id) {
      const pending = this.#curation;
      this.#curation = null;
      pending.reject(new Error(msg.message));
      return;
    }
    // Only a query reply resolves a query.
    if (msg.kind !== "result" && msg.kind !== "error") return;
    const request = this.#current;
    this.#current = null;
    if (msg.kind === "error") request?.reject(new Error(msg.message));
    else request?.resolve(msg);

    if (this.#waiting) {
      this.#current = this.#waiting;
      this.#waiting = null;
      this.#post(this.#current);
    }
  }

  /** Release the WASM memory: the worker is terminated, so it is not reusable. */
  dispose() {
    if (this.#curation) this.#curation.resolve(null);
    this.#curation = null;
    if (this.#waiting) this.#waiting.resolve(null);
    if (this.#current) this.#current.resolve(null);
    this.#current = null;
    this.#waiting = null;
    this.#worker.terminate();
  }
}
