// Synthetic corpus generator for 100k document comparison
// Generates HTML files with random terms to enable 10k/100k corpus comparison

import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { randomInt } from "node:crypto";

const TARGET = 100000; // number of documents to generate
const CHUNK_SIZE = 850; // chars per document
const VOCAB_SIZE = 10000; // unique terms
const OUTPUT_DIR = "/tmp/cmp/corpus-100k";

const vocab = [];
for (let i = 0; i < VOCAB_SIZE; i++) {
  vocab.push(`term${i.toString().padStart(5, "0")}`);
}

function randomTerm() {
  return vocab[randomInt(0, VOCAB_SIZE)];
}

function randomBody(words) {
  let body = "";
  for (let w = 0; w < words; w++) {
    if (w > 0) body += " ";
    body += randomTerm();
  }
  return body;
}

function makePage(docId, title, body) {
  return `<html><head><title>${title}</title></head>
<body><main><h1>${title}</h1><p>${body}</p></main></body></html>`;
}

rmSync(OUTPUT_DIR, { recursive: true, force: true });
mkdirSync(OUTPUT_DIR, { recursive: true });

console.log(`Generating ${TARGET} synthetic documents with ${VOCAB_SIZE} unique terms...`);

let written = 0;
for (let doc = 0; doc < TARGET; doc++) {
  const title = `Document ${doc}`;
  const bodyWords = randomInt(100, 500); // 100-500 words per doc
  const body = randomBody(bodyWords);
  const html = makePage(doc, title, body);
  const name = join(OUTPUT_DIR, `doc-${String(doc).padStart(5, "0")}.html`);
  writeFileSync(name, html);
  written++;
  
  if ((doc + 1) % 20000 === 0) {
    console.log(`  Written ${doc + 1}/${TARGET} documents...`);
  }
}

console.log(`\nGenerated ${written} documents in ${OUTPUT_DIR}`);
console.log(`Approximate HTML size: ~${Math.round(written * 2000 / 1024 / 1024)} MiB`);

// Also generate a facets version with lang/metadata
console.log("\nGenerating facets version...");
let facetsWritten = 0;
for (let doc = 0; doc < TARGET; doc++) {
  const section = `section-${String(doc % 23).padStart(2, "0")}`; // 23 values like in real corpus
  const title = `Document ${doc}`;
  const bodyWords = randomInt(100, 500);
  const body = randomBody(bodyWords);
  const html = `<html lang="en"><head><meta name="section" content="${section}"><title>${title}</title></head>
<body><main><h1>${title}</h1><p>${body}</p></main></body></html>`;
  const name = join(OUTPUT_DIR, `doc-${String(doc).padStart(5, "0")}.html`);
  writeFileSync(name, html);
  facetsWritten++;
}

console.log(`Generated ${facetsWritten} facet documents`);