// End-to-end check of the Worker's data path, without Cloudflare.
//
// Reads a shard set from a directory (`rustsearch shard index.rsx -o dir`), serves
// it from an in-memory bucket that honours `Range` like R2 does, loads the sparse
// engine from `wasm/pkg-node`, runs a query the way the Worker does, and prints the
// bytes and requests it cost.
//
//   cargo build -p rustsearch-wasm --target wasm32-unknown-unknown --release
//   wasm-bindgen --target nodejs --out-dir wasm/pkg-node \
//     target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm
//   rustsearch build public -o dist/index.rsx
//   rustsearch shard dist/index.rsx -o dist/index --prefix index/
//   node adapters/cloudflare/verify.mjs dist/index 3 "walkdir"
//   node adapters/cloudflare/verify.mjs dist/index 3 "walkdir" --filter section=guide
//   node adapters/cloudflare/verify.mjs dist/index 3 "walkdir" --boost 5 --group
import { readFileSync, readdirSync } from "node:fs";
import { join } from "node:path";

import { Engine } from "../../wasm/pkg-node/rustsearch_wasm.js";
import { loadSparseIndex, parseFilters, searchWithSnippets } from "./shard-source.js";

const dir = process.argv[2];
const limit = Number(process.argv[3] || 3);
const query = process.argv[4] || "index";
const rawFilters = [];
let titleBoost;
let group = false;
let curatePath = null;
for (let index = 5; index < process.argv.length; index += 1) {
  if (process.argv[index] === "--filter") rawFilters.push(process.argv[++index] ?? "");
  else if (process.argv[index] === "--group") group = true;
  else if (process.argv[index] === "--curate") curatePath = process.argv[++index] ?? null;
  else if (process.argv[index] === "--boost") {
    const value = Number(process.argv[++index]);
    if (!Number.isFinite(value) || value < 0 || value > 10) {
      throw new Error("--boost takes a number in 0..10");
    }
    titleBoost = value;
  } else throw new Error(`unknown flag: ${process.argv[index]}`);
}
const filters = parseFilters(rawFilters);
if (!dir) {
  console.error(
    "usage: node verify.mjs <shard-dir> [limit] [query] [--filter field=value]... [--boost weight] [--group] [--curate FILE]",
  );
  process.exit(2);
}

const objects = {};
for (const name of readdirSync(dir)) {
  objects[`index/${name}`] = new Uint8Array(readFileSync(join(dir, name)));
}

const bucket = {
  async get(key, options = {}) {
    const bytes = objects[key];
    this.requests.push({ key, range: options.range ?? null });
    if (!bytes) return null;
    const { offset = 0, length = bytes.length - offset } = options.range ?? {};
    const slice = bytes.slice(offset, offset + length);
    return {
      arrayBuffer: async () => slice.buffer,
      text: async () => new TextDecoder().decode(slice),
    };
  },
  requests: [],
};

const loaded = await loadSparseIndex(bucket, "index/manifest.txt");
const artifact = new Uint8Array(loaded.manifest.total);
for (const part of loaded.manifest.parts) {
  const bytes = objects[part.key];
  artifact.set(bytes, part.offset);
}

const metadataRequests = bucket.requests.length;
const metadata = { ...loaded.stats };
const engine = Engine.fromSparse(loaded.bytes);
if (curatePath) {
  // The same parser and the same apply step the Worker and the UI run.
  const count = engine.setCurations(readFileSync(curatePath, "utf8"));
  console.log(`curation            ${count} quer${count === 1 ? "y" : "ies"} from ${curatePath}`);
}
// The engine reports the title statistics it carries, so a shard set built without
// F3 (`--boost 0` behaves the same) is visible here instead of silently ignored.
if (engine.titleTokenCount > 0) {
  console.log(`title tokens        ${engine.titleTokenCount}`);
}
if (titleBoost !== undefined) console.log(`title boost         ${titleBoost}`);
const before = JSON.parse(engine.searchJson(query, limit));
const after = await searchWithSnippets(bucket, loaded, engine, query, limit, {
  filters,
  titleBoost,
  group,
});
if (group) {
  console.log(`group by page       ${after.results.map((hit) => hit.matches ?? 1).join("+")}`);
}
if (filters.length > 0) {
  console.log(`filters             ${JSON.stringify(after.filters ?? filters)}`);
  const taxonomy = JSON.parse(engine.facetsJson());
  console.log(
    `taxonomy            ${Object.entries(taxonomy)
      .map(([field, values]) => `${field}:${Object.keys(values).length}`)
      .join(" ")}`,
  );
}

const blockRequests = bucket.requests.slice(metadataRequests);
const withSnippets =
  metadata.bytes +
  blockRequests.reduce((sum, request) => sum + (request.range ? request.range.length : 0), 0);
const requests = metadata.requests + blockRequests.length;

console.log(`artifact            ${artifact.length} bytes / 1 request`);
console.log(
  `sparse metadata     ${metadata.bytes} bytes / ${metadata.requests} range requests`,
);
console.log(`query "${query}"`);
console.log(
  `  with snippets     ${withSnippets} bytes / ${requests} range requests (${(
    (100 * withSnippets) / artifact.length
  ).toFixed(1)}% of the artifact)`,
);
console.log(
  `  hits              ${after.results.length} (${before.results.length} unfiltered/snippetless)`,
);
for (const hit of after.results) {
  console.log(`  ${hit.url}  ${hit.snippet.slice(0, 80).replace(/\s+/g, " ")}`);
}

const texts = loaded.parts.texts;
const strings = loaded.parts.strings;
if (after.results.length === 0) {
  // A filter that matches nothing, or a query with no hits: nothing to render.
  console.log("no hits: nothing to attach (the filter may exclude every match)");
} else if (
  texts &&
  strings &&
  after.results.some((hit) => hit.snippet) &&
  after.results.every((hit) => hit.url)
) {
  console.log("urls and snippets render from attached, hash-verified blocks");
} else {
  console.error("a hit is missing its url or snippet: the shard set or the query is wrong");
  process.exit(1);
}
if (withSnippets >= artifact.length) {
  // Expected on a tiny index: the metadata alone is most of the artifact.
  console.warn("note: this corpus is too small for the shards to save bytes");
}
