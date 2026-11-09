// tools/make-corpus-100k.mjs
// Seeded 100k-document corpus with the same HTML shape as make-corpus.mjs.
// Vocabulary is sampled from the repository's own markdown so the term dictionary
// is realistic; the PRNG is seeded, so two runs over the same tree are byte-identical.
// The vocabulary is read from disk, though, so a run after an edit of the markdown
// produces a statistically identical corpus, not the same bytes (COMPARISON.md
// §"At 100,000 documents" says which corpus its numbers belong to).
//
//   node tools/make-corpus-100k.mjs /tmp/corpus-100k --target 100000 --chunk 850
import { mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const options = { out: null, target: 100000, chunk: 850, seed: 0xC0FFEE, roots: ["."] };
for (let i = 2; i < process.argv.length; i += 1) {
  const a = process.argv[i];
  if (a === "--target") options.target = Number(process.argv[++i]);
  else if (a === "--chunk") options.chunk = Number(process.argv[++i]);
  else if (a === "--seed") options.seed = Number(process.argv[++i]);
  else if (a === "--roots") {
    options.roots = [];
    while (process.argv[i + 1] && !process.argv[i + 1].startsWith("--")) options.roots.push(process.argv[++i]);
  } else if (a.startsWith("--")) throw new Error(`unknown flag: ${a}`);
  else if (options.out === null) options.out = a;
}
if (!options.out) { console.error("usage: node make-corpus-100k.mjs <out-dir> [--target N] [--chunk N] [--seed N] [--roots DIR...]"); process.exit(2); }

// mulberry32: small, fast, deterministic.
function rng(seed) {
  let s = seed >>> 0;
  return () => {
    s = (s + 0x6D2B79F5) >>> 0;
    let t = s;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}
const rand = rng(options.seed);
const pick = (arr) => arr[Math.floor(rand() * arr.length)];

function markdownFiles(dir, files = []) {
  for (const e of readdirSync(dir, { withFileTypes: true }).sort((a, b) => (a.name < b.name ? -1 : 1))) {
    if (e.name.startsWith(".") || e.name === "node_modules" || e.name === "target" || e.name === "rustsearch") continue;
    const p = join(dir, e.name);
    if (e.isDirectory()) markdownFiles(p, files);
    else if (e.name.endsWith(".md")) files.push(p);
  }
  return files;
}

// Vocabulary: every word of the real corpus, lowercased, deduped, sorted.
// Realistic Zipf shape comes from sampling with weights below.
const files = options.roots.flatMap((r) => markdownFiles(r));
if (files.length === 0) { console.error("no markdown found"); process.exit(1); }
const counts = new Map();
for (const f of files) {
  for (const w of readFileSync(f, "utf8").toLowerCase().match(/[a-z][a-z0-9'-]{2,}/g) ?? []) {
    counts.set(w, (counts.get(w) ?? 0) + 1);
  }
}
const vocab = [...counts.entries()].sort((a, b) => b[1] - a[1]).map(([w]) => w);
// Zipf-ish: weight ~ 1/rank, so common words dominate like real prose.
const weights = vocab.map((_, i) => 1 / (i + 1));
const totalW = weights.reduce((a, b) => a + b, 0);
function word() {
  let r = rand() * totalW;
  for (let i = 0; i < vocab.length; i += 1) { r -= weights[i]; if (r <= 0) return vocab[i]; }
  return vocab[vocab.length - 1];
}

// Section distribution from COMPARISON.md: 23 values, data ~66%, crewAI ~4.2%.
// We approximate with a weighted list; adjust if you have the exact table.
const sections = [
  ["data", 66], ["crewAI", 4.2], ["zbsearch", 4], ["pagefind", 3], ["docs", 3],
  ["eval", 3], ["design", 2.5], ["bench", 2.5], ["api", 2], ["cli", 2],
  ["wasm", 2], ["worker", 1.5], ["storage", 1.5], ["ranking", 1.5], ["schema", 1],
  ["migration", 1], ["cache", 1], ["token", 1], ["snippet", 1], ["vector", 1],
  ["embedding", 0.5], ["postgres", 0.5], ["root", 0.3],
];
const totalS = sections.reduce((a, [, w]) => a + w, 0);
function section() {
  let r = rand() * totalS;
  for (const [name, w] of sections) { r -= w; if (r <= 0) return name; }
  return "root";
}

function escapeHtml(t) { return t.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;"); }

// Build one document of ~`chunk` characters at paragraph boundaries.
function makeDoc() {
  const targetChars = options.chunk;
  const paras = [];
  let size = 0;
  while (size < targetChars) {
    const words = [];
    const n = 40 + Math.floor(rand() * 60); // 40–100 words per paragraph
    for (let i = 0; i < n; i += 1) words.push(word());
    const p = words.join(" ");
    paras.push(p);
    size += p.length + 2;
  }
  // Trim to a paragraph boundary close to the target.
  let body = "";
  for (const p of paras) {
    if (body.length + p.length > targetChars && body.length >= 300) break;
    body += (body ? "\n\n" : "") + p;
  }
  return body;
}

rmSync(options.out, { recursive: true, force: true });
mkdirSync(options.out, { recursive: true });

let written = 0, bytes = 0;
const sectionsPerPage = 3; // 3 docs per "page", like the real corpus
const pages = Math.ceil(options.target / sectionsPerPage);
for (let page = 0; page < pages && written < options.target; page += 1) {
  const sec = section();
  for (let s = 0; s < sectionsPerPage && written < options.target; s += 1) {
    const heading = `Section ${page}.${s}`;
    const body = makeDoc();
    if (body.length < 300) continue;
    const title = `doc-${String(page).padStart(5, "0")} — ${heading}`;
    const html =
      `<html lang="en"><head><meta name="section" content="${escapeHtml(sec)}">` +
      `<title>${escapeHtml(title)}</title></head>` +
      `<body><main><h1>${escapeHtml(title)}</h1>` +
      body.split(/\n{2,}/).map((p) => `<p>${escapeHtml(p)}</p>`).join("") +
      `</main></body></html>`;
    const name = join(options.out, `doc-${String(written).padStart(6, "0")}.html`);
    writeFileSync(name, html);
    written += 1; bytes += html.length;
  }
}
console.log(`${written} documents, ${(bytes / 1024 / 1024).toFixed(1)} MiB of HTML, seed ${options.seed}`);