// Read the HTML corpus `make-corpus.mjs` writes as `{ url, title, content }`
// records, the shape `@zbsearch/static` indexes.
//
// The corpus pages are deliberately simple (`<html><head><title>…</title></head>
// <body><main>…</main></body></html>`), so a regex extraction is enough. The text
// is the same prose rustsearch indexes; only the HTML furniture differs, and
// stripping it keeps the three engines on the same words.
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";

const ENTITIES = {
  amp: "&",
  lt: "<",
  gt: ">",
  quot: '"',
  apos: "'",
  "#39": "'",
  nbsp: " ",
};

function decode(text) {
  return text.replace(/&(#x?[0-9a-f]+|[a-z]+);/gi, (match, name) => {
    if (name[0] === "#") {
      const code = name[1].toLowerCase() === "x" ? parseInt(name.slice(2), 16) : Number(name.slice(1));
      return Number.isFinite(code) ? String.fromCodePoint(code) : match;
    }
    return ENTITIES[name.toLowerCase()] ?? match;
  });
}

function strip(html) {
  return html
    .replace(/<(script|style)\b[^>]*>[\s\S]*?<\/\1>/gi, " ")
    .replace(/<[^>]*>/g, " ");
}

function normalize(text) {
  return text.replace(/\s+/g, " ").trim();
}

function filesUnder(dir, files = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) =>
    a.name < b.name ? -1 : 1,
  )) {
    if (entry.name.startsWith(".")) continue;
    const path = join(dir, entry.name);
    if (entry.isDirectory()) filesUnder(path, files);
    else if (entry.name.endsWith(".html")) files.push(path);
  }
  return files;
}

/** Every page under `dir`, in sorted path order. */
export function readCorpus(dir) {
  if (!statSync(dir).isDirectory()) throw new Error(`not a directory: ${dir}`);
  return filesUnder(dir).map((path) => {
    const html = readFileSync(path, "utf8");
    const title = normalize(decode((/<title[^>]*>([\s\S]*?)<\/title>/i.exec(html) || [, ""])[1]));
    const main = /<main[^>]*>([\s\S]*?)<\/main>/i.exec(html);
    const content = normalize(decode(strip(main ? main[1] : html)));
    const url = `/${relative(dir, path).split(sep).join("/").replace(/index\.html?$/i, "")}`;
    return { url, title: title || url, content };
  });
}
