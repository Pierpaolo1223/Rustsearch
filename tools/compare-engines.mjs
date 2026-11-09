// Compare rustsearch, pagefind and ZBSearch on the same static site.
//
//   node tools/compare-engines.mjs \
//     --index /tmp/cmp/index.rsx \
//     --shards /tmp/cmp/shards \
//     --pagefind /tmp/cmp/site/pagefind \
//     --zbsearch /tmp/cmp/zb \
//     --iterations 30
//
// Pagefind is driven through the bundle it writes for browsers (`pagefind.js`) with
// `fetch` shimmed to the built directory; ZBSearch through `@zbsearch/static`, the
// sharded static client it ships, with the same in-memory transport. Both therefore
// execute the code and the files a browser would load, minus the network. rustsearch
// is measured twice: the single-artifact path (browser) and the shard path (Worker,
// `Range` reads).
//
// The engines are configured to be comparable in quality, not identical in features:
// pagefind and ZBSearch are told `english` (their default), so the rustsearch index
// in this comparison is built with `--stemmer en` too. All three rank BM25-style and
// return five hits.
//
// `--group` collapses rustsearch's documents into one hit per page
// (`SearchOptions::group_by_url`), which is what makes a comparison meaningful when
// the index was built from pages split with `--max-page-chars`: pagefind and
// ZBSearch return pages, so without it the top-5 lists measure chunk policy.
import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";
import { fileURLToPath } from "node:url";

import { Engine } from "../wasm/pkg-node/rustsearch_wasm.js";
import {
  loadSparseIndex,
  parseManifest,
  searchWithSnippets,
} from "../adapters/cloudflare/shard-source.js";


const args = process.argv.slice(2);
const option = (name, fallback) => {
  const at = args.indexOf(`--${name}`);
  return at >= 0 ? args[at + 1] : fallback;
};

const indexPath = option("index");
const siteDir = option("site");
const shardDir = option("shards");
const pagefindDir = option("pagefind");
const zbDir = option("zbsearch");
const iterations = Number(option("iterations", 30));
const limit = Number(option("limit", 5));
const group = args.includes("--group");

const QUERIES = [
  "index",
  "search",
  "query",
  "postgres",
  "embedding",
  "vector",
  "database",
  "cache",
  "latency",
  "schema",
  "migration",
  "token",
  "ranking",
  "snippet",
  "worker",
  "storage",
  "python",
  "rust",
  "docker",
  "kubernetes",
];

if (!indexPath) {
  console.error(
    "usage: node compare-engines.mjs --index FILE [--site DIR] [--shards DIR] [--pagefind DIR] [--zbsearch DIR]",
  );
  process.exit(2);
}

const now = () => Number(process.hrtime.bigint()) / 1e6;
const percentile = (sorted, p) =>
  sorted[Math.min(sorted.length - 1, Math.floor((p / 100) * sorted.length))];
const summarize = (times) => {
  const sorted = [...times].sort((a, b) => a - b);
  return {
    p50: +percentile(sorted, 50).toFixed(3),
    p95: +percentile(sorted, 95).toFixed(3),
    mean: +(times.reduce((sum, value) => sum + value, 0) / times.length).toFixed(3),
  };
};
const mb = (bytes) => `${(bytes / 1024 / 1024).toFixed(2)} MB`;
const kb = (bytes) => `${(bytes / 1024).toFixed(1)} KB`;

// The three engines are fed the same URLs, but pagefind prefixes them with the
// directory its bundle lives in and the others do not, so strip the prefix and any
// trailing `index.html` before comparing which documents a query returned.
const normalizeUrl = (url) =>
  String(url ?? "")
    .replace(/^\/pagefind(?=\/)/, "")
    .replace(/[?#].*$/, "")
    .replace(/^\/+|\/+$/g, "")
    .replace(/index\.html?$/, "");
const overlap = (a, b) => {
  const setB = new Set(b);
  const shared = a.filter((url) => setB.has(url)).length;
  return { shared, of: a.length };
};
const tops = {
  rustsearch: new Map(),
  pagefind: new Map(),
  zbsearch: new Map(),
  pagefind50: new Map(),
  zbsearch50: new Map(),
};
const zeroHits = { rustsearch: 0, pagefind: 0, zbsearch: 0 };

function dirStats(dir) {
  let bytes = 0;
  let files = 0;
  for (const entry of readdirSync(dir, { recursive: true })) {
    const full = join(dir, String(entry));
    if (!statSync(full).isFile()) continue;
    bytes += statSync(full).size;
    files += 1;
  }
  return { bytes, files };
}

/** Documents and bytes of the HTML corpus a site directory ships. */
function corpusStats(dir) {
  let documents = 0;
  let htmlBytes = 0;
  for (const entry of readdirSync(dir, { recursive: true })) {
    const full = join(dir, String(entry));
    if (!String(entry).endsWith(".html") || !statSync(full).isFile()) continue;
    documents += 1;
    htmlBytes += statSync(full).size;
  }
  return { documents, htmlBytes };
}

/** Every file under `dir`, keyed by its relative path with `/` separators. */
function loadTree(dir) {
  const files = new Map();
  for (const entry of readdirSync(dir, { recursive: true })) {
    const name = String(entry).split(sep).join("/");
    const full = join(dir, name);
    if (!statSync(full).isFile()) continue;
    files.set(name, new Uint8Array(readFileSync(full)));
  }
  return files;
}

// ---------------------------------------------------------------- pagefind
let pagefind = null;
if (pagefindDir) {
  let pagefindBytes = 0;
  let pagefindRequests = 0;

  globalThis.fetch = async (input) => {
    const raw = typeof input === "string" ? input : input.url;
    const url = new URL(raw, `file://${pagefindDir}/`);
    const candidates =
      url.protocol === "file:"
        ? [fileURLToPath(url)]
        : [join(pagefindDir, url.pathname.replace(/^\/pagefind\//, ""))];
    candidates.push(join(pagefindDir, url.pathname.split("/").pop()));
    let path = null;
    for (const candidate of candidates) {
      try {
        readFileSync(candidate);
        path = candidate;
        break;
      } catch {
        // try the next shape
      }
    }
    if (!path) return new Response(null, { status: 404 });
    const buffer = readFileSync(path);
    pagefindBytes += buffer.length;
    pagefindRequests += 1;
    return new Response(buffer, {
      headers: {
        "content-type": path.endsWith(".wasm")
          ? "application/wasm"
          : "application/octet-stream",
      },
    });
  };

  const module = await import(`file://${join(pagefindDir, "pagefind.js")}`);
  await module.options({ baseUrl: "/pagefind/" });

  const coldStart = now();
  await module.init();
  const first = await module.search(QUERIES[0]);
  const firstHits = await Promise.all(first.results.slice(0, limit).map((r) => r.data()));
  const coldMs = now() - coldStart;
  const coldBytes = pagefindBytes;
  const coldRequests = pagefindRequests;

  const times = [];
  let hits = 0;
  let matches = 0;
  const warmStart = pagefindBytes;
  const warmRequestsStart = pagefindRequests;
  for (let round = 0; round < iterations; round += 1) {
    for (const query of QUERIES) {
      const started = now();
      const result = await module.search(query);
      const data = await Promise.all(
        result.results.slice(0, limit).map((entry) => entry.data()),
      );
      times.push(now() - started);
      if (round === 0) {
        hits += data.length;
        matches += result.results.length;
        tops.pagefind.set(
          query,
          data.map((entry) => normalizeUrl(entry.url)),
        );
        if (data.length === 0) zeroHits.pagefind += 1;
      }
    }
  }
  const warmBytes = pagefindBytes - warmStart;
  const warmRequests = pagefindRequests - warmRequestsStart;

  // Ranking pass: the 50 best documents each engine returns for a query, after
  // the timed loops so the extra fragment reads do not pollute the latency
  // samples. It answers "are these the same documents, ordered differently".
  const deep = new Map();
  for (const query of QUERIES) {
    const result = await module.search(query);
    const data = await Promise.all(
      result.results.slice(0, 50).map((entry) => entry.data()),
    );
    deep.set(query, data.map((entry) => normalizeUrl(entry.url)));
  }
  tops.pagefind50 = deep;

  const bundle = dirStats(pagefindDir);
  pagefind = {
    bundleBytes: bundle.bytes,
    files: bundle.files,
    coldMs: +coldMs.toFixed(1),
    coldBytes,
    coldRequests,
    warm: summarize(times),
    warmBytes,
    warmRequests,
    resultsReturned: hits,
    matchesCounted: matches,
  };
  await module.close?.();
}

// --------------------------------------------------------------- zbsearch
let zbsearch = null;
if (zbDir) {
  const files = loadTree(zbDir);
  const totalBytes = [...files.values()].reduce((sum, bytes) => sum + bytes.length, 0);
  // Dynamic, so a pagefind-only run works without the sibling `zbsearch/`
  // checkout: `@zbsearch/static` is read from the monorepo's built dist.
  const module = await import(
    new URL("../../zbsearch/packages/static/dist/index.js", import.meta.url)
  );
  let served = 0;
  let requests = 0;
  const fetchBytes = async (path) => {
    requests += 1;
    const bytes = files.get(path);
    if (!bytes) throw new Error(`missing zbsearch artifact: ${path}`);
    served += bytes.length;
    return bytes;
  };
  const client = module.createStaticSearchClient({ fetchBytes });
  // Engine defaults: prefix expansion on, typo tolerance 0, no field boosts.
  // Turning on `tolerance: 1` would make ZBSearch the only fuzzy engine, which
  // is a feature difference, not a ranking one.
  const params = (term) => ({ term, limit });

  const coldStart = now();
  await client.preload();
  const first = await client.search(params(QUERIES[0]));
  const coldMs = now() - coldStart;
  const coldBytes = served;
  const coldRequests = requests;

  const times = [];
  let hits = 0;
  const warmStart = served;
  const warmRequestsStart = requests;
  for (let round = 0; round < iterations; round += 1) {
    for (const query of QUERIES) {
      const started = now();
      const result = await client.search(params(query));
      times.push(now() - started);
      if (round === 0) {
        hits += result.hits.length;
        tops.zbsearch.set(
          query,
          result.hits.slice(0, limit).map((hit) => normalizeUrl(hit.document.url)),
        );
        if (result.hits.length === 0) zeroHits.zbsearch += 1;
      }
    }
  }
  const warmBytes = served - warmStart;
  const warmRequests = requests - warmRequestsStart;

  const deep = new Map();
  for (const query of QUERIES) {
    const result = await client.search({ ...params(query), limit: 50 });
    deep.set(
      query,
      result.hits.slice(0, 50).map((hit) => normalizeUrl(hit.document.url)),
    );
  }
  tops.zbsearch50 = deep;

  const stats = client.stats();
  zbsearch = {
    bundleBytes: totalBytes,
    files: files.size,
    coldMs: +coldMs.toFixed(1),
    coldBytes,
    coldRequests,
    warm: summarize(times),
    warmBytes,
    warmRequests,
    resultsReturned: hits,
    stats,
  };
}

// --------------------------------------------------------------- rustsearch
const artifact = new Uint8Array(readFileSync(indexPath));
const loadStart = now();
const engine = new Engine(artifact);
const loadMs = now() - loadStart;
const firstStart = now();
engine.searchJson(QUERIES[0], limit);
const firstMs = now() - firstStart;

// One call shape for both paths: the plain query, or the full policy when the run
// asks for grouping (which needs the options entry point).
const searchArtifact = (q, n) =>
  group
    ? engine.searchJsonWithOptions(q, n, true, false, 1, Number.NaN, "", true)
    : engine.searchJson(q, n);

const fullTimes = [];
let fullHits = 0;
for (let round = 0; round < iterations; round += 1) {
  for (const query of QUERIES) {
    const started = now();
    const json = searchArtifact(query, limit);
    fullTimes.push(now() - started);
    if (round === 0) {
      const hits = JSON.parse(json).results;
      fullHits += hits.length;
      tops.rustsearch.set(
        query,
        hits.map((hit) => normalizeUrl(hit.url)),
      );
      if (hits.length === 0) zeroHits.rustsearch += 1;
    }
  }
}

let sharded = null;
if (shardDir) {
  const manifestText = readFileSync(join(shardDir, "manifest.txt"), "utf8");
  const manifest = parseManifest(manifestText);
  const files = new Map();
  files.set("index/manifest.txt", new Uint8Array(Buffer.from(manifestText)));
  for (const part of manifest.parts) {
    files.set(part.key, new Uint8Array(readFileSync(join(shardDir, `${part.name}.bin`))));
  }
  const bucket = {
    requests: 0,
    bytes: 0,
    async get(key, options = {}) {
      const bytes = files.get(key);
      const range = options.range ?? null;
      this.requests += 1;
      if (!bytes) return null;
      const offset = range ? range.offset : 0;
      const length = range ? range.length : bytes.length - offset;
      const slice = bytes.slice(offset, offset + length);
      this.bytes += slice.length;
      return {
        arrayBuffer: async () => slice.buffer,
        text: async () => new TextDecoder().decode(slice),
      };
    },
  };

  const coldStart = now();
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");
  loaded.engine = Engine.fromSparse(loaded.bytes);
  const coldHits = await searchWithSnippets(bucket, loaded, loaded.engine, QUERIES[0], limit, {
    group,
  });
  const coldMs = now() - coldStart;
  const coldBytes = bucket.bytes;
  const coldRequests = bucket.requests;

  const times = [];
  const warmStart = bucket.bytes;
  const warmRequests = bucket.requests;
  for (let round = 0; round < iterations; round += 1) {
    for (const query of QUERIES) {
      const started = now();
      await searchWithSnippets(bucket, loaded, loaded.engine, query, limit, { group });
      times.push(now() - started);
    }
  }
  sharded = {
    diskBytes: [...files.values()].reduce((sum, bytes) => sum + bytes.length, 0),
    coldMs: +coldMs.toFixed(1),
    coldBytes,
    coldRequests,
    warm: summarize(times),
    warmBytes: bucket.bytes - warmStart,
    warmRequests: bucket.requests - warmRequests,
    resultsReturned: coldHits.results.length,
    parts: manifest.parts.length + 1,
  };
}

const quality = { zeroHitQueries: zeroHits, top5Overlap: {}, perQuery: {} };
for (const query of QUERIES) {
  const a = tops.rustsearch.get(query) ?? [];
  quality.perQuery[query] = {
    rustsearch: a.length,
    pagefind: overlap(a, tops.pagefind.get(query) ?? []).shared,
    zbsearch: overlap(a, tops.zbsearch.get(query) ?? []).shared,
  };
}
for (const name of ["pagefind", "zbsearch"]) {
  if (tops[name].size === 0) continue;
  let shared = 0;
  let total = 0;
  let deep = 0;
  for (const query of QUERIES) {
    const a = tops.rustsearch.get(query) ?? [];
    const result = overlap(a, tops[name].get(query) ?? []);
    shared += result.shared;
    total += result.of;
    deep += overlap(a, tops[`${name}50`].get(query) ?? []).shared;
  }
  quality.top5Overlap[name] = {
    shared,
    of: total,
    percent: +((shared / Math.max(1, total)) * 100).toFixed(1),
    inTheirTop50: { shared: deep, of: total },
  };
}

const report = {
  corpus: siteDir ? corpusStats(siteDir) : null,
  iterations,
  queries: QUERIES.length,
  limit,
  group,
  rustsearch: {
    artifactBytes: artifact.length,
    artifactDisplay: mb(artifact.length),
    analyzer: engine.analyzer,
    loadMs: +loadMs.toFixed(1),
    coldMs: +(loadMs + firstMs).toFixed(1),
    coldBytes: artifact.length,
    warm: summarize(fullTimes),
    resultsReturned: fullHits,
  },
  rustsearchSharded: sharded,
  pagefind,
  zbsearch,
  quality,
};

console.log(JSON.stringify(report, null, 2));

const perQuery = (engine) =>
  engine ? kb(engine.warmBytes / (iterations * QUERIES.length)) : "—";
const cell = (value, digits = 3) =>
  value === null || value === undefined ? "—" : Number(value).toFixed(digits);

console.log(`\ngrouped by page: ${group}`);
console.log("\n| metric | rustsearch (file) | rustsearch (shards) | pagefind | zbsearch |");
console.log("|---|---:|---:|---:|---:|");
console.log(
  `| files on disk | ${mb(artifact.length)} (1) | ${mb(sharded?.diskBytes ?? 0)} (${sharded?.parts ?? 0}) | ${mb(pagefind?.bundleBytes ?? 0)} (${pagefind?.files ?? 0}) | ${mb(zbsearch?.bundleBytes ?? 0)} (${zbsearch?.files ?? 0}) |`,
);
console.log(
  `| cold: bytes / requests | ${kb(report.rustsearch.coldBytes)} / 1 | ${kb(sharded?.coldBytes ?? 0)} / ${sharded?.coldRequests ?? 0} | ${kb(pagefind?.coldBytes ?? 0)} / ${pagefind?.coldRequests ?? 0} | ${kb(zbsearch?.coldBytes ?? 0)} / ${zbsearch?.coldRequests ?? 0} |`,
);
console.log(
  `| cold: ms | ${cell(report.rustsearch.coldMs, 1)} | ${cell(sharded?.coldMs, 1)} | ${cell(pagefind?.coldMs, 1)} | ${cell(zbsearch?.coldMs, 1)} |`,
);
console.log(
  `| warm p50 / p95 (ms) | ${cell(report.rustsearch.warm.p50)} / ${cell(report.rustsearch.warm.p95)} | ${cell(sharded?.warm.p50)} / ${cell(sharded?.warm.p95)} | ${cell(pagefind?.warm.p50)} / ${cell(pagefind?.warm.p95)} | ${cell(zbsearch?.warm.p50)} / ${cell(zbsearch?.warm.p95)} |`,
);
console.log(
  `| warm bytes per query | 0 (in memory) | ${perQuery(sharded)} | ${perQuery(pagefind)} | ${perQuery(zbsearch)} |`,
);
console.log(
  `| warm requests per query | 0 | ${sharded ? (sharded.warmRequests / (iterations * QUERIES.length)).toFixed(1) : "—"} | ${pagefind ? (pagefind.warmRequests / (iterations * QUERIES.length)).toFixed(1) : "—"} | ${zbsearch ? (zbsearch.warmRequests / (iterations * QUERIES.length)).toFixed(1) : "—"} |`,
);
console.log(
  `| hits in round 0 | ${report.rustsearch.resultsReturned} | ${sharded?.resultsReturned ?? "—"} | ${pagefind?.resultsReturned ?? "—"} | ${zbsearch?.resultsReturned ?? "—"} |`,
);
console.log(
  `| queries with 0 hits | ${zeroHits.rustsearch} | ${zeroHits.rustsearch} | ${zeroHits.pagefind} | ${zeroHits.zbsearch} |`,
);
const shared = (name) => {
  const entry = quality.top5Overlap[name];
  return entry
    ? `${entry.shared}/${entry.of} (${entry.percent}%)`
    : "—";
};
const deep = (name) => {
  const entry = quality.top5Overlap[name]?.inTheirTop50;
  return entry ? `${entry.shared}/${entry.of}` : "—";
};
console.log(
  `| top-5 shared with rustsearch | — | — | ${shared("pagefind")} | ${shared("zbsearch")} |`,
);
console.log(
  `| of those, in their top-50 | — | — | ${deep("pagefind")} | ${deep("zbsearch")} |`,
);
