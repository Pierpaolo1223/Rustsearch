// The ranking regression gate (ROADMAP §"Next cycle", item 3).
//
// The engine has numbers for bytes, build time and latency, but nothing stopped a
// ranking change from making the results worse: `COMPARISON.md` proves the project
// can measure, not that it notices a regression. This script is that check. It
//
//   1. writes a deterministic 17-page fixture with a known answer for every query,
//   2. builds an artifact with the real CLI (`--stemmer en --positions`),
//   3. runs the 21 judged queries in `tools/ranking-set.jsonl` through `search --json`,
//   4. scores them (nDCG@10, MRR@10, recall@10, per-query top) and compares against
//      `tools/ranking-baseline.json`,
//   5. fails on a drop, on a "must be in the top 3" miss, on a forbidden document,
//      on a missing typo correction, or on a latency p50 above the ceiling.
//
// The baseline is a file in the repository, so a proposal that changes ranking has to
// say so in the diff — `--accept` regenerates it, and the reviewer sees the numbers.
//
//   node tools/ranking-gate.mjs                 # check
//   node tools/ranking-gate.mjs --accept        # new baseline (review the diff!)
//   node tools/ranking-gate.mjs --title-boost 0 # must fail: it breaks title ranking
//
// The fixture is deliberately about this engine's vocabulary: a shard layout, a
// static site, a phrase that is adjacent in one page and apart in another, an
// inflection, a typo, a facet, a non-ASCII term and a long page. Each is a property
// the engine promises, not a word that merely exists.

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { performance } from "node:perf_hooks";

const here = dirname(fileURLToPath(import.meta.url));
const root = join(here, "..");
const setPath = join(here, "ranking-set.jsonl");
const baselinePath = join(here, "ranking-baseline.json");

const args = process.argv.slice(2);
const accept = args.includes("--accept");
const quiet = args.includes("--quiet");
let titleBoost = null;
let bin = process.env.RUSTSEARCH_BIN ?? join(root, "target", "release", "rustsearch");
for (let index = 0; index < args.length; index += 1) {
  if (args[index] === "--title-boost") titleBoost = Number(args[index + 1]);
  else if (args[index] === "--bin") bin = args[index + 1];
  else if (!["--accept", "--quiet"].includes(args[index]) && !["--title-boost", "--bin"].includes(args[index - 1])) {
    console.error(`unknown argument: ${args[index]}`);
    process.exit(2);
  }
}
if (!existsSync(bin)) {
  console.error(
    `ranking gate: no binary at ${bin}\nbuild it with: cargo build --release -p rustsearch-cli`,
  );
  process.exit(2);
}

const NDCG_DROP = 0.02;
const MRR_DROP = 0.02;
const LATENCY_CEILING_MS = 250;

function page({ title, body, section, lang = "en" }) {
  const meta = section ? `<meta name="section" content="${section}">` : "";
  return (
    `<!doctype html><html lang="${lang}"><head><meta charset="utf-8">${meta}` +
    `<title>${title}</title></head><body><main><h1>${title}</h1>` +
    body.map((paragraph) => `<p>${paragraph}</p>`).join("") +
    `</main></body></html>`
  );
}

const filler =
  "The documentation covers configuration, deployment and the operational details " +
  "that a reader needs when the first example is not enough to finish the task. ";

// Seventeen pages. The vocabulary is small on purpose: every judgment below is about
// a property of the engine, and a large corpus would make a regression harder to see,
// not easier. `sharding-primer` is the page whose only match is in its title, which is
// what makes `--title-boost 0` a failing mutation rather than a harmless one.
const fixture = {
  "shard-layout.html": page({
    title: "Shard layout",
    section: "guide",
    body: [
      "The shard layout splits the term dictionary into chunks. A shard holds a run of terms and the postings they point at.",
      "A query fetches the shard its terms live in, not the whole dictionary, so the shard layout is what makes a cold query cheap.",
      "The shard layout is the difference between reading a few kilobytes and reading the artifact.",
    ],
  }),
  "shard-routing.html": page({
    title: "Shard routing",
    section: "reference",
    body: [
      "Routing maps a query token to the shard that can answer it; the shard directory is read once at cold start.",
      "A shard whose terms never match is never fetched.",
    ],
  }),
  "static-site.html": page({
    title: "Static site search",
    section: "guide",
    body: [
      "A static site ships its index as files. A static site needs no query server, which is the whole point of the format.",
      "The static site is deployed to a bucket and served by a CDN.",
    ],
  }),
  "static-apart.html": page({
    title: "Beyond the static approach",
    section: "guide",
    body: [
      "The word static appears here, and the word site appears later in this paragraph after enough unrelated words that no phrase should match them together.",
      "This page exists to prove that a phrase query is adjacency, not co-occurrence.",
    ],
  }),
  "dictionary.html": page({
    title: "Term dictionary",
    section: "reference",
    body: [
      "The dictionary is sorted by term. A dictionary lookup is a binary search over the term table.",
      "The dictionary stores each term once. The dictionary is the part a query reads first.",
      "A dictionary with a bad order is rejected at load time, not at query time.",
    ],
  }),
  "index-format.html": page({
    title: "Index format",
    section: "reference",
    body: [
      "The index format is a header, a term table, the postings, the documents, the strings and the texts.",
      "The index records a checksum over the body; an index with a wrong checksum is refused.",
      "Positions are optional in the index and are read only by a phrase query.",
    ],
  }),
  "positions.html": page({
    title: "Token positions",
    section: "reference",
    body: [
      "Positions record the ordinal of every token. Positions are what a phrase needs to require adjacency.",
      "Without positions a phrase query is refused rather than answered with the words apart.",
    ],
  }),
  "facets.html": page({
    title: "Facets and filters",
    section: "reference",
    body: [
      "A facet is a field with a bounded set of values. A filter keeps the documents whose facet matches.",
      "The facet section is small and is fetched with the metadata. A filter on an artifact without facets is refused.",
    ],
  }),
  "long-page.html": page({
    title: "Warehouse operations",
    section: "reference",
    body: [
      filler.repeat(6) + "The warehouse inventory is reconciled every night.",
      filler.repeat(6) + "A shard of the inventory is audited each week.",
    ],
  }),
  "stemming.html": page({
    title: "Stemming and inflection",
    section: "reference",
    body: [
      "Indexing a page is the first step. Indexing twice is not needed.",
      "When indexing a site, the analyzer stems every token so that index and indexing meet.",
    ],
  }),
  "unicode.html": page({
    title: "Unicode text",
    section: "reference",
    body: [
      "A café in the text is a café in the index; naïve spacing is preserved as written.",
      "Non-ASCII terms are stored as UTF-8 and matched exactly.",
    ],
  }),
  "sharding-primer.html": page({
    title: "Sharding primer",
    section: "guide",
    body: [
      "This primer explains how a dictionary is divided into parts and why a query reads only one of them.",
      "Nothing in this paragraph mentions the title of the page.",
    ],
  }),
  "contact.html": page({
    title: "Contact",
    section: "reference",
    body: ["Write to the maintainer for a commercial licence."],
  }),
  "config.html": page({
    title: "Configuration",
    section: "guide",
    body: ["The configuration file names the analyzer, the base URL and the page bounds."],
  }),
  "deploy.html": page({
    title: "Deployment",
    section: "guide",
    body: ["Deployment uploads the artifact and the shards to a bucket, then invalidates the manifest."],
  }),
  "security.html": page({
    title: "Security model",
    section: "reference",
    body: ["The artifact is untrusted input: every section is checked before it is used."],
  }),
  "changelog.html": page({
    title: "Changelog",
    section: "guide",
    body: ["Version 4 added facets and title postings; version 5 added term chunks."],
  }),
};

function writeFixture(dir) {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  for (const [name, html] of Object.entries(fixture)) {
    writeFileSync(join(dir, name), html);
  }
}

function readSet() {
  return readFileSync(setPath, "utf8")
    .split("\n")
    .filter((line) => line.trim() !== "")
    .map((line, number) => {
      try {
        return JSON.parse(line);
      } catch (error) {
        console.error(`ranking-set.jsonl line ${number + 1}: ${error.message}`);
        process.exit(2);
      }
    });
}

function slugOf(url) {
  return url.replace(/^\//, "").replace(/\.html?$/, "");
}

function search(artifact, entry) {
  const argv = [artifact, entry.query, "-n", "10", "--json"];
  for (const filter of entry.filters ?? []) argv.push("--filter", filter);
  if (titleBoost !== null) argv.push("--title-boost", String(titleBoost));
  const started = performance.now();
  const output = execFileSync(bin, ["search", ...argv], { encoding: "utf8" });
  const elapsed = performance.now() - started;
  return { report: JSON.parse(output), elapsed };
}

function dcg(grades) {
  return grades.reduce((sum, grade, index) => sum + (2 ** grade - 1) / Math.log2(index + 2), 0);
}

function score(entry, hits) {
  const ranked = hits.map((hit) => slugOf(hit.url));
  const grades = ranked.map((slug) => entry.grades?.[slug] ?? 0);
  const ideal = Object.values(entry.grades ?? {}).sort((a, b) => b - a);
  const idcg = dcg(ideal);
  const ndcg = idcg === 0 ? (ranked.length === 0 ? 1 : 0) : dcg(grades) / idcg;
  const first = grades.findIndex((grade) => grade >= 1);
  const mrr = first === -1 ? 0 : 1 / (first + 1);
  const found = new Set(ranked);
  const relevant = Object.keys(entry.grades ?? {});
  const recall = relevant.length === 0 ? 1 : relevant.filter((slug) => found.has(slug)).length / relevant.length;
  return { ndcg: +ndcg.toFixed(4), mrr: +mrr.toFixed(4), recall: +recall.toFixed(4), top: ranked[0] ?? null, ranked };
}

function at(samples, p) {
  const sorted = [...samples].sort((a, b) => a - b);
  return sorted[Math.min(sorted.length - 1, Math.max(0, Math.floor((sorted.length - 1) * p)))];
}

const set = readSet();
const work = join(tmpdir(), `rustsearch-ranking-${process.pid}`);
const corpus = join(work, "site");
const artifact = join(work, "index.rsx");
writeFixture(corpus);

const buildStarted = performance.now();
execFileSync(bin, ["build", corpus, "-o", artifact, "--stemmer", "en", "--positions"], {
  encoding: "utf8",
});
const buildMs = performance.now() - buildStarted;

const results = [];
for (const entry of set) {
  const { report, elapsed } = search(artifact, entry);
  const hits = report.results ?? [];
  const scored = score(entry, hits);
  const problems = [];
  if (entry.expect_hits === 0 && hits.length !== 0) {
    problems.push(`${hits.length} hit(s) for a query that matches nothing`);
  }
  if (entry.expect_hits !== 0 && hits.length === 0) {
    problems.push("no hits");
  }
  for (const slug of entry.must_top3 ?? []) {
    const rank = scored.ranked.indexOf(slug);
    if (rank === -1 || rank > 2) {
      problems.push(`${slug} must be in the top 3 (rank ${rank === -1 ? "missing" : rank + 1})`);
    }
  }
  for (const slug of entry.excluded ?? []) {
    if (scored.ranked.includes(slug)) problems.push(`${slug} must not match`);
  }
  if (entry.correction) {
    const corrected = (report.corrected ?? []).map((correction) => correction.to);
    if (!corrected.includes(entry.correction)) {
      problems.push(`expected a correction to ${entry.correction}, got ${JSON.stringify(corrected)}`);
    }
  }
  results.push({ ...entry, ...scored, elapsed, problems });
}

const mean = (values) => values.reduce((sum, value) => sum + value, 0) / values.length;
// A query with no relevant document is a shape check (`expect_hits: 0`), not a ranking
// measurement: including it would pin the mean MRR below 1 forever and hide a real
// drop behind a known zero.
const graded = results.filter((result) => Object.keys(result.grades ?? {}).length > 0);
const metrics = {
  ndcg: +mean(graded.map((result) => result.ndcg)).toFixed(4),
  mrr: +mean(graded.map((result) => result.mrr)).toFixed(4),
  recall: +mean(graded.map((result) => result.recall)).toFixed(4),
  p50_ms: +at(results.map((result) => result.elapsed), 0.5).toFixed(1),
  build_ms: +buildMs.toFixed(1),
  bytes: Number(execFileSync("wc", ["-c", artifact], { encoding: "utf8" }).trim().split(/\s+/)[0]),
  queries: results.length,
};

const baseline = existsSync(baselinePath)
  ? JSON.parse(readFileSync(baselinePath, "utf8"))
  : null;

const failures = [];
for (const result of results) {
  if (result.problems.length > 0) failures.push(`${result.id}: ${result.problems.join("; ")}`);
  const before = baseline?.queries?.[result.id];
  if (before && !accept) {
    if (before.ndcg - result.ndcg > NDCG_DROP) {
      failures.push(`${result.id}: nDCG ${result.ndcg} is below the baseline ${before.ndcg}`);
    }
    if (before.mrr - result.mrr > MRR_DROP) {
      failures.push(`${result.id}: MRR ${result.mrr} is below the baseline ${before.mrr}`);
    }
  }
}
if (metrics.p50_ms > LATENCY_CEILING_MS) {
  failures.push(`p50 ${metrics.p50_ms} ms is above the ${LATENCY_CEILING_MS} ms ceiling`);
}
if (!accept && baseline) {
  if (baseline.metrics.ndcg - metrics.ndcg > NDCG_DROP) {
    failures.push(`mean nDCG ${metrics.ndcg} is below the baseline ${baseline.metrics.ndcg}`);
  }
  if (baseline.metrics.mrr - metrics.mrr > MRR_DROP) {
    failures.push(`mean MRR ${metrics.mrr} is below the baseline ${baseline.metrics.mrr}`);
  }
  if (baseline.metrics.queries !== metrics.queries) {
    failures.push(
      `the set has ${metrics.queries} queries, the baseline was taken over ${baseline.metrics.queries}: review the diff`,
    );
  }
}

const next = {
  note: "Generated by tools/ranking-gate.mjs --accept. Review the per-query diff before committing.",
  fixture: "tools/ranking-gate.mjs (17 pages, deterministic)",
  build: { stemmer: "en", positions: true, title_boost: 2 },
  metrics,
  queries: Object.fromEntries(
    results.map((result) => [
      result.id,
      { ndcg: result.ndcg, mrr: result.mrr, recall: result.recall, top: result.top },
    ]),
  ),
};

if (!quiet) {
  const rows = results.map((result) => ({
    query: result.id,
    nDCG: result.ndcg,
    MRR: result.mrr,
    top: result.top,
    ms: +result.elapsed.toFixed(1),
  }));
  console.table(rows);
  console.log(
    `ranking gate: ${metrics.queries} queries, nDCG@10 ${metrics.ndcg}, MRR ${metrics.mrr}, ` +
      `recall ${metrics.recall}, p50 ${metrics.p50_ms} ms, build ${metrics.build_ms} ms, ` +
      `artifact ${(metrics.bytes / 1024).toFixed(1)} KiB`,
  );
}

if (accept) {
  writeFileSync(baselinePath, `${JSON.stringify(next, null, 2)}\n`);
  console.log(`baseline written to ${baselinePath}`);
} else if (!baseline) {
  console.log("no baseline yet: run with --accept once and review the diff");
}

rmSync(work, { recursive: true, force: true });

if (failures.length > 0) {
  console.error(`\nranking gate FAILED (${failures.length}):`);
  for (const failure of failures) console.error(`  - ${failure}`);
  process.exit(1);
}
console.log("ranking gate OK");
