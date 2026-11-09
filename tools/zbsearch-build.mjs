// Build a ZBSearch static index (`@zbsearch/static`) from the HTML corpus.
//
//   node tools/zbsearch-build.mjs /tmp/corpus10k750 /tmp/zb-index
//
// The result is the deployed file set — manifest, dictionary, postings shards and
// fragment groups — the same shape `rustsearch shard` and pagefind write, so the
// three engines can be compared on the bytes they actually ship. Run it under
// `/usr/bin/time -v` to get the build time and peak RSS in COMPARISON.md.
import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";

import { buildStaticIndex } from "../../zbsearch/packages/static/dist/index.js";
import { readCorpus } from "./corpus-records.mjs";

const [corpusDir, outDir] = process.argv.slice(2);
if (!corpusDir || !outDir) {
  console.error("usage: node tools/zbsearch-build.mjs <corpus-dir> <out-dir>");
  process.exit(2);
}

// The package resolves `zbsearch` from its own node_modules; from an ESM file the
// monorepo checkout is what loads, not an npm copy.
const records = readCorpus(corpusDir);

const started = performance.now();
const { files, stats } = await buildStaticIndex({
  records,
  schema: { title: "string", content: "string" },
  language: "english",
});

rmSync(outDir, { recursive: true, force: true });
mkdirSync(outDir, { recursive: true });
for (const [name, bytes] of files) {
  const target = join(outDir, name);
  mkdirSync(dirname(target), { recursive: true });
  writeFileSync(target, bytes);
}
const elapsed = performance.now() - started;

console.log(
  JSON.stringify(
    {
      engine: "zbsearch-static",
      documents: records.length,
      buildMs: Math.round(elapsed),
      files: files.size,
      bytes: stats.totalBytes,
      terms: stats.termCount,
      shards: stats.shardCount,
      fragments: stats.fragmentCount,
      dictionaryBytes: stats.dictionaryBytes,
      postingsBytes: stats.postingsBytes,
      fragmentsBytes: stats.fragmentsBytes,
      out: outDir,
    },
    null,
    2,
  ),
);
