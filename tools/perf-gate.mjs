// The size and latency budget gate.
//
// `COMPARISON.md` proves the project can measure; this
// script is what notices when a change spends the budget. It gates what a user
// pays for on every deploy and every query:
//
//   1. the module a browser downloads (the `wasm-bindgen` output, raw and gzip -9);
//   2. the reproducible benchmark (`bench --docs 2000 --words 120 --queries 200`):
//      artifact bytes and query p50/p95.
//
// The budgets live in `tools/perf-budget.json` next to the measurement that set
// them, so raising one is a reviewed diff instead of a silent drift.
//
//   node tools/perf-gate.mjs                       # check
//   node tools/perf-gate.mjs --accept              # new budget (review the diff!)
//   node tools/perf-gate.mjs --max-p95-ms 0.0001   # must fail
//   node tools/perf-gate.mjs --max-gzip 1          # must fail
//
// Latency budgets carry 2.5x headroom over the measured value on purpose: a shared
// CI runner is noisy, so this gate catches an order-of-magnitude regression, not a
// 5% one. Size and artifact budgets are tight (2-3%), because those numbers are
// deterministic for a pinned toolchain. CI runs the two failing forms above as a
// negative control: a gate that cannot be made to fail proves nothing.

import { execFileSync } from "node:child_process";
import { existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const budgetPath = join(here, "perf-budget.json");

let bin = process.env.RUSTSEARCH_BIN ?? join(root, "target", "release", "rustsearch");
let wasmPath = process.env.RUSTSEARCH_WASM ?? join(root, "wasm", "pkg-node", "rustsearch_wasm_bg.wasm");
let accept = false;
let quiet = false;
const overrides = {};

const raw = process.argv.slice(2);
for (let index = 0; index < raw.length; index += 1) {
  const flag = raw[index];
  const value = raw[index + 1];
  const numeric = () => {
    const parsed = Number(value);
    if (!Number.isFinite(parsed)) {
      console.error(`${flag} needs a number`);
      process.exit(2);
    }
    index += 1;
    return parsed;
  };
  switch (flag) {
    case "--accept":
      accept = true;
      break;
    case "--quiet":
      quiet = true;
      break;
    case "--bin":
      bin = value;
      index += 1;
      break;
    case "--wasm":
      wasmPath = value;
      index += 1;
      break;
    case "--max-p50-ms":
      overrides.p50 = numeric();
      break;
    case "--max-p95-ms":
      overrides.p95 = numeric();
      break;
    case "--max-raw":
      overrides.raw = numeric();
      break;
    case "--max-gzip":
      overrides.gzip = numeric();
      break;
    case "--max-index-bytes":
      overrides.indexBytes = numeric();
      break;
    default:
      console.error(`unknown argument: ${flag}`);
      process.exit(2);
  }
}

if (!existsSync(bin)) {
  console.error(`perf gate: no binary at ${bin}\nbuild it with: cargo build --release -p rustsearch-cli`);
  process.exit(2);
}
if (!existsSync(wasmPath)) {
  console.error(
    `perf gate: no wasm module at ${wasmPath}\nbuild it with: wasm-bindgen --target nodejs --out-dir wasm/pkg-node <wasm>`,
  );
  process.exit(2);
}

const budgets = JSON.parse(readFileSync(budgetPath, "utf8"));
const corpus = budgets.corpus ?? { docs: 2000, words: 120, queries: 200 };
const command = [
  "bench",
  "--docs",
  String(corpus.docs),
  "--words",
  String(corpus.words),
  "--queries",
  String(corpus.queries),
];

let output;
try {
  output = execFileSync(bin, command, { encoding: "utf8", stdio: ["ignore", "pipe", "pipe"] });
} catch (error) {
  console.error(`perf gate: the benchmark failed\n${error.stderr ?? error.message}`);
  process.exit(2);
}

const field = (pattern, label) => {
  const match = output.match(pattern);
  if (!match) {
    console.error(`perf gate: the benchmark printed no ${label}\n${output}`);
    process.exit(2);
  }
  return Number(match[1]);
};

const measured = {
  artifact_bytes: field(/index bytes:\s+(\d+)/, "index bytes"),
  build_ms: field(/build:\s+(\d+)\s*ms/, "build time"),
  build_rss_kib: field(/build RSS:\s+(\d+)\s*KiB/, "build RSS"),
  p50_ms: field(/query p50:\s+([0-9.]+)\s*ms/, "p50"),
  p95_ms: field(/query p95:\s+([0-9.]+)\s*ms/, "p95"),
};
const module = readFileSync(wasmPath);
const wasm = { raw_bytes: module.length, gzip_bytes: gzipSync(module, { level: 9 }).length };

const round = (value, digits = 3) => Number(value.toFixed(digits));
const ceil = (value, step = 1000) => Math.ceil(value / step) * step;

if (accept) {
  const next = {
    measured: new Date().toISOString().slice(0, 10),
    command: `rustsearch ${command.join(" ")}`,
    corpus,
    wasm: {
      path: relative(root, wasmPath),
      raw_bytes: wasm.raw_bytes,
      gzip_bytes: wasm.gzip_bytes,
      max_raw_bytes: ceil(wasm.raw_bytes * 1.02),
      max_gzip_bytes: ceil(wasm.gzip_bytes * 1.03, 100),
    },
    native: {
      p50_ms: measured.p50_ms,
      p95_ms: measured.p95_ms,
      max_p50_ms: round(measured.p50_ms * 2.5),
      max_p95_ms: round(measured.p95_ms * 2.5),
    },
    artifact: {
      index_bytes: measured.artifact_bytes,
      max_index_bytes: ceil(measured.artifact_bytes * 1.03),
    },
  };
  writeFileSync(budgetPath, `${JSON.stringify(next, null, 2)}\n`);
  console.log(`perf gate: budget written to ${relative(root, budgetPath)}`);
  console.log(`  wasm ${wasm.raw_bytes} raw / ${wasm.gzip_bytes} gzip`);
  console.log(`  artifact ${measured.artifact_bytes} bytes, p50 ${measured.p50_ms} ms, p95 ${measured.p95_ms} ms`);
  process.exit(0);
}

const checks = [
  ["wasm raw", wasm.raw_bytes, overrides.raw ?? budgets.wasm.max_raw_bytes, "bytes"],
  ["wasm gzip", wasm.gzip_bytes, overrides.gzip ?? budgets.wasm.max_gzip_bytes, "bytes"],
  ["artifact", measured.artifact_bytes, overrides.indexBytes ?? budgets.artifact.max_index_bytes, "bytes"],
  ["query p50", measured.p50_ms, overrides.p50 ?? budgets.native.max_p50_ms, "ms"],
  ["query p95", measured.p95_ms, overrides.p95 ?? budgets.native.max_p95_ms, "ms"],
];

if (!quiet) {
  console.log(`perf gate (${process.platform} ${process.arch}, budget measured ${budgets.measured})`);
  console.log(`  corpus ${corpus.docs} pages x ${corpus.words} words, ${corpus.queries} queries`);
  console.log(`  wasm   ${relative(root, wasmPath)}`);
  console.log(`  build  ${measured.build_ms} ms, ${measured.build_rss_kib} KiB RSS (informational)`);
}

let failed = 0;
for (const [label, actual, limit, unit] of checks) {
  const ok = actual <= limit;
  if (!ok) failed += 1;
  if (!quiet || !ok) {
    console.log(`  ${ok ? "ok  " : "FAIL"} ${label.padEnd(10)} ${String(actual).padStart(9)} ${unit} (limit ${limit})`);
  }
}

if (failed > 0) {
  console.error(
    `perf gate: ${failed} budget(s) exceeded. If the change is intentional, run --accept and review the diff.`,
  );
  process.exit(1);
}
if (!quiet) console.log("perf gate: all budgets hold");
