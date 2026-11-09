// Cloudflare Worker adapter for rustsearch.
//
// The index is published as a shard set in R2 (`rustsearch shard`): a manifest
// plus the objects it names. A chunked (v5) set groups the metadata into one
// `meta` object (`meta00000`, … when published with `--meta-part-bytes`), so the
// first requests in an isolate are a manifest read and the metadata reads; a flat
// (v4) set still publishes one object per section. Either way
// every part is checked against the manifest hash and the result is a sparse
// `Engine` kept in module scope. What a query needs is fetched per query and per
// hit: the term chunks its terms route to, the record page of each hit, and then
// the hit's text block — each a `Range` request the engine verifies against its
// own hash before using it.
//
// Deploy shape:
//   rustsearch build public -o dist/index.rsx
//   rustsearch shard dist/index.rsx -o dist/index --prefix index/
//   wasm-bindgen --target web --out-dir adapters/cloudflare/pkg \
//     target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm
//   # upload dist/index/* under the `index/` prefix (see README.md)
//
// The query endpoint returns JSON, never HTML. A UI must escape snippets and
// wrap the returned byte ranges in its own <mark>, under a strict CSP.
//
// A query may also carry facet filters: `?q=walkdir&filter=section=guide&filter=
// lang=en`. Fields are AND-ed and repeated values inside one field OR-ed, exactly
// as the engine does. The response echoes the filters it applied. `?boost=3`
// weighs a title match against a body match (0 ignores titles, the default is 2),
// and `?group=1` returns one hit per page with its chunk count in `matches` (a
// build of long pages split with `--max-page-chars` is what makes that matter).
//
// H10: every response carries the policy in `headers.js` (nosniff, no-referrer,
// `default-src 'none'`, same-origin resource policy), CORS is the
// `ALLOWED_ORIGINS` allowlist instead of `*`, HEAD and OPTIONS are answered
// explicitly, and `MAX_ARTIFACT_BYTES` refuses an artifact before the engine
// copies it.

import init, { Engine } from "./pkg/rustsearch_wasm.js";
import wasm from "./pkg/rustsearch_wasm_bg.wasm";
import { loadSparseIndex, parseFilters, searchWithSnippets } from "./shard-source.js";
import {
  SECURITY_HEADERS,
  artifactLimit,
  corsHeaders,
  manifestDigest,
  preflightHeaders,
} from "./headers.js";

// Text blocks stay in the engine for the isolate's lifetime, so the fetcher caps
// how many a long-lived isolate keeps. Beyond the cap hits still rank; they just
// have no snippet.

let statePromise = null;
let stateKey = "";

// Curation is a small text file (`search --curate FILE`), fetched once per isolate.
// It is optional by design: a missing or malformed file leaves the ranking untouched
// rather than taking search down, because a rule file is an editorial convenience and
// not part of the index.
const MAX_CURATION_BYTES = 64 * 1024;

async function state(env) {
  const key = env.INDEX_KEY || "index/manifest.txt";
  if (statePromise && stateKey === key) return statePromise;
  stateKey = key;
  statePromise = (async () => {
    await init(wasm);
    const loaded = await loadSparseIndex(env.INDEX, key, {
      maxBytes: artifactLimit(env),
      // H1: the manifest is the routing table a cold client trusts. When the
      // deployment pins its digest, a substituted set is refused before a part is
      // parsed, even if every FNV in it was recomputed.
      expectManifestDigest: manifestDigest(env),
    });
    const engine = Engine.fromSparse(loaded.bytes);
    let curations = 0;
    if (env.CURATE_KEY) {
      try {
        const object = await env.INDEX.get(env.CURATE_KEY);
        if (object) {
          const text = (await object.text()).slice(0, MAX_CURATION_BYTES);
          curations = engine.setCurations(text);
        }
      } catch {
        // A broken rule file must not make the endpoint unusable.
        curations = 0;
      }
    }
    return { ...loaded, engine, curations };
  })().catch((error) => {
    // Do not poison the isolate after a transient R2 failure.
    statePromise = null;
    throw error;
  });
  return statePromise;
}

export default {
  async fetch(request, env) {
    const url = new URL(request.url);
    const cors = corsHeaders(env, request);
    const base = { ...SECURITY_HEADERS, ...cors };
    const json = (body, status = 200) =>
      new Response(request.method === "HEAD" ? null : JSON.stringify(body), {
        status,
        headers: {
          ...base,
          "content-type": "application/json; charset=utf-8",
          // Query results depend on q; let the edge cache the shards, not the answer.
          "cache-control": "no-store",
        },
      });

    if (request.method === "OPTIONS") {
      return new Response(null, {
        status: 204,
        headers: { ...base, ...preflightHeaders(env, request) },
      });
    }
    if (request.method !== "GET" && request.method !== "HEAD") {
      return new Response("method not allowed", {
        status: 405,
        headers: { ...base, allow: "GET, HEAD, OPTIONS" },
      });
    }
    if (url.pathname !== "/search") {
      return new Response("not found", { status: 404, headers: base });
    }
    const query = (url.searchParams.get("q") || "").slice(0, 512);
    const limit = Math.min(Math.max(Number(url.searchParams.get("limit")) || 10, 1), 50);
    const fuzzy = ["1", "true"].includes((url.searchParams.get("fuzzy") || "").toLowerCase());
    // At most eight `?filter=field=value`, malformed ones dropped; the engine
    // would reject the surplus anyway.
    const filters = parseFilters(url.searchParams.getAll("filter"));
    // The title boost is a query-time weight; the engine clamps it to 0..10.
    const boost = Number(url.searchParams.get("boost"));
    const titleBoost = url.searchParams.has("boost") && Number.isFinite(boost) ? boost : undefined;
    const group = ["1", "true"].includes((url.searchParams.get("group") || "").toLowerCase());
    if (!query.trim()) {
      return json(filters.length > 0 ? { results: [], filters } : { results: [] });
    }
    try {
      const loaded = await state(env);
      const results = await searchWithSnippets(env.INDEX, loaded, loaded.engine, query, limit, {
        fuzzy,
        filters,
        titleBoost,
        group,
      });
      // `curations` is how many queries the rules name, so an operator can tell an
      // empty file from a deploy that forgot CURATE_KEY.
      return json(loaded.curations > 0 ? { ...results, curations: loaded.curations } : results);
    } catch {
      // No message, no stack: the body is a fixed code the UI can translate.
      return json({ error: "search_unavailable" }, 503);
    }
  },
};
