// Reproducible benchmark for the metadata-run split (`rustsearch shard
// --meta-part-bytes`).
//
//   cargo build --release -p rustsearch-cli
//   node tools/bench-meta-shards.mjs
//   node tools/bench-meta-shards.mjs --docs 100000 --chunk 320 --budget 256k
//
// The corpus is generated here from a seeded PRNG over a fixed vocabulary, so the
// same command produces the same HTML — and therefore the same artifact — on every
// checkout. A million documents is the default because that is the size the option
// exists for: it writes ~1M files (~4 GB of 4 KiB blocks), builds for a couple of
// minutes and peaks around 6 GB of RSS. `--docs` is the knob for a quick run.
//
// The check is the client's own: parse the manifest with `parseManifest`, require
// every `metaNNNNN` part to be at most the budget, hash each part against its
// manifest FNV, require the parts to tile the run from byte 0, and compare their
// concatenation with the first bytes of the artifact. A mismatch exits non-zero.
import { spawnSync } from "node:child_process";
import {
  closeSync,
  mkdirSync,
  openSync,
  readFileSync,
  readSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, join } from "node:path";
import { fileURLToPath } from "node:url";

import { fnv1a64Hex, parseManifest } from "../adapters/cloudflare/shard-source.js";

const ROOT = fileURLToPath(new URL("..", import.meta.url));
const META_PART = /^meta\d+$/;
const MIN_BUDGET = 64 * 1024;

function usage() {
  console.error(
    "usage: node bench-meta-shards.mjs [--docs N] [--chunk N] [--budget 1M]\n" +
      "                                  [--rustsearch PATH] [--work PARENT] [--keep]",
  );
}

/** A byte budget: `65536`, `256k` or `1M`, exactly as the CLI parses it. */
function byteBudget(raw) {
  const match = /^(\d+)([kKmM])?$/.exec(raw);
  if (!match) throw new Error(`invalid byte budget: ${raw}`);
  const scale = match[2] ? (match[2].toLowerCase() === "m" ? 1024 * 1024 : 1024) : 1;
  return Number(match[1]) * scale;
}

function parseOptions(argv) {
  const options = {
    docs: 1_000_000,
    chunk: 320,
    budget: 1024 * 1024,
    rustsearch: join(ROOT, "target", "release", "rustsearch"),
    work: tmpdir(),
    keep: false,
  };
  for (let index = 2; index < argv.length; index += 1) {
    const flag = argv[index];
    if (flag === "--help" || flag === "-h") {
      usage();
      process.exit(0);
    }
    if (flag === "--keep") {
      options.keep = true;
      continue;
    }
    const value = argv[index + 1];
    if (value === undefined) {
      usage();
      throw new Error(`${flag} needs a value`);
    }
    switch (flag) {
      case "--docs":
        options.docs = Number(value);
        break;
      case "--chunk":
        options.chunk = Number(value);
        break;
      case "--budget":
        options.budget = byteBudget(value);
        break;
      case "--rustsearch":
        options.rustsearch = value;
        break;
      case "--work":
        options.work = value;
        break;
      default:
        usage();
        throw new Error(`unknown flag: ${flag}`);
    }
    index += 1;
  }
  return options;
}

/** mulberry32: the same deterministic PRNG as `tools/make-corpus-100k.mjs`. */
function rng(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let value = state;
    value = Math.imul(value ^ (value >>> 15), value | 1);
    value ^= value + Math.imul(value ^ (value >>> 7), value | 61);
    return ((value ^ (value >>> 14)) >>> 0) / 4294967296;
  };
}

const SECTIONS = [
  "data",
  "crewAI",
  "zbsearch",
  "pagefind",
  "docs",
  "eval",
  "design",
  "bench",
  "api",
  "cli",
  "wasm",
  "worker",
  "storage",
  "ranking",
  "schema",
  "migration",
  "cache",
  "token",
  "snippet",
  "vector",
  "embedding",
  "postgres",
  "root",
];
const VOCABULARY = 20_000;

/** Write a deterministic corpus: one file per document, `chunk` chars of body. */
function makeCorpus(dir, options) {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  for (const section of SECTIONS) {
    mkdirSync(join(dir, section), { recursive: true });
  }
  const rand = rng(0xc0ffee);
  const word = () => `term${String(Math.floor(rand() * VOCABULARY)).padStart(5, "0")}`;
  // `term00000` is 9 bytes plus a separator, so this lands near `chunk` characters.
  const terms = Math.max(8, Math.round(options.chunk / 10));
  let htmlBytes = 0;
  for (let doc = 0; doc < options.docs; doc += 1) {
    const title = [word(), word(), word(), word()].join(" ");
    const body = Array.from({ length: terms }, word).join(" ");
    const section = SECTIONS[Math.floor(rand() * SECTIONS.length)];
    const html =
      `<html lang="en"><head><meta name="section" content="${section}">` +
      `<title>${title}</title></head><body><main><h1>${title}</h1>` +
      `<p>${body}</p></main></body></html>`;
    writeFileSync(join(dir, section, `page${String(doc).padStart(6, "0")}.html`), html);
    htmlBytes += Buffer.byteLength(html);
  }
  return { files: options.docs, htmlBytes };
}

function run(program, args) {
  const result = spawnSync(program, args, { encoding: "utf8" });
  if (result.error) throw result.error;
  if (result.status !== 0) {
    process.stderr.write(result.stdout ?? "");
    process.stderr.write(result.stderr ?? "");
    throw new Error(`${program} ${args[0]} exited with status ${result.status}`);
  }
  return result.stdout ?? "";
}

function group(number) {
  return String(number).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

function mib(bytes) {
  return `${(bytes / (1024 * 1024)).toFixed(1)} MiB`;
}

/** Read exactly `length` bytes at `offset` from an open file. */
function readAt(fd, length, offset) {
  const out = Buffer.alloc(length);
  let read = 0;
  while (read < length) {
    const count = readSync(fd, out, read, length - read, offset + read);
    if (count <= 0) throw new Error(`short read at ${offset + read} of ${length} bytes`);
    read += count;
  }
  return out;
}

function bench(options, work) {
  if (!Number.isInteger(options.docs) || options.docs <= 0) {
    throw new Error("--docs must be a positive integer");
  }
  if (!Number.isInteger(options.chunk) || options.chunk < 64) {
    throw new Error("--chunk must be at least 64 characters");
  }
  if (options.budget < MIN_BUDGET) {
    throw new Error(`--budget must be at least ${MIN_BUDGET} bytes`);
  }
  const site = join(work, "site");
  const artifact = join(work, "index.rsx");
  const shards = join(work, "index");

  console.log(`corpus:      ${group(options.docs)} documents, ~${options.chunk} chars each`);
  const corpus = makeCorpus(site, options);
  console.log(
    `generated:   ${group(corpus.files)} files, ${mib(corpus.htmlBytes)} of HTML in ${site}`,
  );

  const buildStarted = process.hrtime.bigint();
  const buildOutput = run(options.rustsearch, [
    "build",
    site,
    "-o",
    artifact,
    "--term-chunk-bytes",
    "64k",
  ]);
  const buildSeconds = Number(process.hrtime.bigint() - buildStarted) / 1e9;
  process.stdout.write(buildOutput);
  const artifactBytes = statSync(artifact).size;

  const shardStarted = process.hrtime.bigint();
  run(options.rustsearch, [
    "shard",
    artifact,
    "-o",
    shards,
    "--prefix",
    "index/",
    "--meta-part-bytes",
    String(options.budget),
  ]);
  const shardSeconds = Number(process.hrtime.bigint() - shardStarted) / 1e9;

  const manifestBytes = statSync(join(shards, "manifest.txt")).size;
  const manifest = parseManifest(readFileSync(join(shards, "manifest.txt"), "utf8"));
  if (manifest.total !== artifactBytes) {
    throw new Error(`manifest covers ${manifest.total} of ${artifactBytes} artifact bytes`);
  }

  const whole = manifest.parts.find((part) => part.name === "meta");
  const slices = manifest.parts.filter((part) => META_PART.test(part.name));
  const metaParts = whole ? [whole] : slices;
  if (metaParts.length === 0) throw new Error("the manifest has no metadata part");
  const metaBytes = metaParts.reduce((sum, part) => sum + part.length, 0);
  if (metaBytes > options.budget && !slices.length) {
    throw new Error(
      `the ${group(metaBytes)}-byte metadata run is one object over a ` +
        `${group(options.budget)}-byte budget`,
    );
  }
  const expected = metaBytes > options.budget ? Math.ceil(metaBytes / options.budget) : 1;
  if (metaParts.length !== expected) {
    throw new Error(
      `${metaParts.length} metadata parts for ${group(metaBytes)} bytes at a ` +
        `${group(options.budget)}-byte budget, expected ${expected}`,
    );
  }

  // Each part is the artifact slice at its offset, verifies against its manifest
  // hash, and the parts tile the run from zero.
  const contents = [];
  let expectedOffset = 0;
  let largest = 0;
  for (const part of metaParts) {
    if (part.offset !== expectedOffset) {
      throw new Error(`${part.name} starts at ${part.offset}, expected ${expectedOffset}`);
    }
    if (part.length > options.budget) {
      throw new Error(`${part.name} is ${group(part.length)} bytes over the budget`);
    }
    const bytes = readFileSync(join(shards, basename(part.key)));
    if (bytes.length !== part.length) {
      throw new Error(`${part.name} is ${bytes.length} bytes, the manifest says ${part.length}`);
    }
    if (fnv1a64Hex(bytes) !== part.hash) {
      throw new Error(`${part.name} does not match its manifest hash`);
    }
    contents.push(bytes);
    largest = Math.max(largest, part.length);
    expectedOffset += part.length;
  }
  const metaEnd = expectedOffset;

  const fd = openSync(artifact, "r");
  let prefix;
  try {
    prefix = readAt(fd, metaEnd, 0);
  } finally {
    closeSync(fd);
  }
  if (Buffer.compare(prefix, Buffer.concat(contents)) !== 0) {
    throw new Error("the metadata slices do not equal the artifact prefix");
  }
  const after = manifest.parts[metaParts.length];
  if (after && after.offset !== metaEnd) {
    throw new Error(`${after.name} starts at ${after.offset}, not at the run end ${metaEnd}`);
  }

  const coldDefault = 2;
  const coldSplit = 1 + metaParts.length;
  console.log(`artifact:    ${group(artifactBytes)} bytes (${mib(artifactBytes)})`);
  console.log(`metadata:    ${group(metaBytes)} bytes (${mib(metaBytes)}), ${metaParts.length} part(s)`);
  console.log(
    `budget:      ${group(options.budget)} bytes (largest part ${group(largest)}, ` +
      `${largest <= options.budget ? "within" : "over"} the budget)`,
  );
  console.log(`manifest:    ${group(manifestBytes)} bytes`);
  console.log(
    `cold start:  ${coldDefault} requests -> ${coldSplit} requests, ` +
      `${mib(manifestBytes + metaBytes)} either way`,
  );
  console.log(
    `check:       ok — ${metaParts.length} hash(es), slices tile 0..${group(metaEnd)}, ` +
      `prefix exact`,
  );
  console.log(`build:       ${buildSeconds.toFixed(1)} s`);
  console.log(`shard:       ${shardSeconds.toFixed(1)} s`);
  if (options.keep) console.log(`work:        ${work} (kept)`);
}

try {
  const options = parseOptions(process.argv);
  // A per-run directory under the caller's parent: the benchmark removes what it
  // created, never a path the caller named.
  const work = join(options.work, `rustsearch-meta-bench-${process.pid}`);
  try {
    bench(options, work);
  } finally {
    if (!options.keep) rmSync(work, { recursive: true, force: true });
  }
} catch (error) {
  console.error(`bench-meta-shards: ${error.message}`);
  process.exitCode = 1;
}
