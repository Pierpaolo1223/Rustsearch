// WASM query benchmark. Build the package first:
//   cargo build -p rustsearch-wasm --target wasm32-unknown-unknown --release
//   wasm-bindgen --target nodejs --out-dir wasm/pkg-node <wasm>
// Then:
//   node wasm/bench.mjs /tmp/index.rsx 2000
//   node wasm/bench.mjs /tmp/index.rsx 200 "index" --filter section=docs
//
// `--filter field=value` times the filtered path next to the unfiltered one on the
// same artifact: a selective filter should cost *less* than no filter at all, not
// more, because the score buffer covers the candidates instead of the corpus.
// `--boost N` sets the BM25F title weight (the default is the engine's 2), and
// `--group` adds the row that collapses a page's documents into one hit — which
// only differs from the unfiltered row on an artifact built with
// `--max-page-chars`.
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { performance } from "node:perf_hooks";

const require = createRequire(import.meta.url);
const { Engine } = require("./pkg-node/rustsearch_wasm.js");

const raw = process.argv.slice(2);
const positional = [];
const filters = [];
let titleBoost;
let grouped = false;
for (let index = 0; index < raw.length; index += 1) {
  const argument = raw[index];
  const value = raw[index + 1];
  if (argument === "--group") {
    grouped = true;
  } else if (argument === "--filter") {
    const at = (value ?? "").indexOf("=");
    if (at <= 0 || at >= (value ?? "").length - 1) throw new Error("--filter takes field=value");
    filters.push({ field: value.slice(0, at), value: value.slice(at + 1) });
    index += 1;
  } else if (argument === "--boost") {
    titleBoost = Number(value);
    if (!Number.isFinite(titleBoost)) throw new Error("--boost needs a number");
    index += 1;
  } else if (argument.startsWith("--")) {
    throw new Error(`unknown flag: ${argument}`);
  } else {
    positional.push(argument);
  }
}

const path = positional[0];
const iterations = Number(positional[1] ?? 1000);
const query = positional[2] ?? "immutable index";
if (!path) {
  console.error(
    "usage: node wasm/bench.mjs <index.rsx> [iterations] [query] [--boost N] [--filter field=value]...",
  );
  process.exit(2);
}

const engine = new Engine(new Uint8Array(readFileSync(path)));
const boost = titleBoost === undefined ? Number.NaN : titleBoost;
// The unfiltered query, the filter the caller asked for, and a value that matches
// nothing: the three costs a filtered query can have.
const configs = [{ label: "unfiltered", value: "", group: false }];
if (grouped) configs.push({ label: "grouped by page", value: "", group: true });
if (filters.length > 0) {
  configs.push({
    label: `filter ${filters.map((filter) => `${filter.field}=${filter.value}`).join(" ")}`,
    value: filters.map((filter) => `${filter.field}\u001f${filter.value}`).join("\u001e"),
    group: false,
  });
}
const firstField = filters[0]?.field ?? "section";
configs.push({
  label: "filter that matches nothing",
  value: `${firstField}\u001fno-such-value`,
  group: false,
});

const rows = [];
for (const config of configs) {
  for (let warmup = 0; warmup < 20; warmup += 1) {
    engine.searchJsonWithOptions(query, 10, true, false, 1, boost, config.value, config.group);
  }
  const samples = [];
  let hits = 0;
  let matches = 0;
  for (let run = 0; run < iterations; run += 1) {
    const start = performance.now();
    const result = JSON.parse(
      engine.searchJsonWithOptions(query, 10, true, false, 1, boost, config.value, config.group),
    );
    matches += result.results.reduce((sum, hit) => sum + (hit.matches ?? 1), 0);
    samples.push(performance.now() - start);
    hits += result.results.length;
  }
  samples.sort((a, b) => a - b);
  const at = (p) => samples[Math.min(samples.length - 1, Math.floor((samples.length - 1) * p))];
  rows.push({
    label: config.label,
    hits_per_query: +(hits / iterations).toFixed(2),
    matches_per_query: +(matches / iterations).toFixed(2),
    p50_ms: Number(at(0.5).toFixed(3)),
    p95_ms: Number(at(0.95).toFixed(3)),
    max_ms: Number(at(1).toFixed(3)),
  });
}

console.log(
  JSON.stringify(
    {
      docs: engine.docCount,
      terms: engine.termCount,
      bytes: engine.memoryBytes,
      title_tokens: engine.titleTokenCount,
      iterations,
      query,
      title_boost: titleBoost ?? "default",
      rows,
    },
    null,
    2,
  ),
);
