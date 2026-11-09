// Build an HTML site out of the repository's own page corpora.
//
// The benchmark numbers in README.md are measured on real prose, not on a
// synthetic vocabulary: every document is a section of a markdown page from the
// domain corpora (`*/pages/*.md`) this repository already ships. Splitting at
// level-2 headings turns ~4.3k pages into the requested number of documents.
//
//   node tools/make-corpus.mjs /tmp/corpus --target 10000
//   rustsearch build /tmp/corpus -o /tmp/index.rsx
//   rustsearch shard /tmp/index.rsx -o /tmp/shards --prefix index/ --plan "index" -n 5
//
// `--facets` adds the metadata the v4 facet section records: `<html lang="en">`
// and a `section` meta named after the first path component of the source file.
// It does not change the page text, only the head, so a corpus can be measured
// with and without filters on the same documents.
import { mkdirSync, readdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const options = { out: null, target: 10000, chunk: 1000, roots: ["."], facets: false };
for (let index = 2; index < process.argv.length; index += 1) {
  const argument = process.argv[index];
  if (argument === "--target") options.target = Number(process.argv[++index]);
  else if (argument === "--chunk") options.chunk = Number(process.argv[++index]);
  else if (argument === "--facets") options.facets = true;
  else if (argument === "--roots") {
    options.roots = [];
    while (process.argv[index + 1] && !process.argv[index + 1].startsWith("--")) {
      options.roots.push(process.argv[++index]);
    }
  } else if (argument.startsWith("--")) {
    throw new Error(`unknown flag: ${argument}`);
  } else if (options.out === null) options.out = argument;
}
const out = options.out;
const target = options.target;
const chunkSize = options.chunk;
const roots = options.roots;
const facets = options.facets;

if (!out) {
  console.error("usage: node make-corpus.mjs <out-dir> [--target N] [--chunk N] [--facets] [--roots DIR...]");
  process.exit(2);
}

/** Every `.md` file under `dir`, sorted, skipping this module's own tree. */
function markdownFiles(dir, files = []) {
  for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) =>
    a.name < b.name ? -1 : 1,
  )) {
    if (
      entry.name.startsWith(".") ||
      entry.name === "node_modules" ||
      entry.name === "target" ||
      // The engine's and the UI's own docs change every time they are documented;
      // keep the corpus stable by indexing everything except those trees.
      entry.name === "rustsearch" ||
      entry.name === "searchui"
    ) {
      continue;
    }
    const path = join(dir, entry.name);
    if (entry.isDirectory()) {
      markdownFiles(path, files);
    } else if (entry.name.endsWith(".md")) {
      files.push(path);
    }
  }
  return files;
}

function escapeHtml(text) {
  return text.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");
}

/** Inline Markdown for one paragraph: code spans, bold and emphasis. */
function inline(text) {
  const code = [];
  const replaced = escapeHtml(text).replace(/(`{1,3})([^`\n]+)\1/g, (_, _fence, body) => {
    code.push(`<code>${body}</code>`);
    return `\u0000${code.length - 1}\u0000`;
  });
  return replaced
    .replace(/\*\*([^*]+)\*\*/g, "<strong>$1</strong>")
    .replace(/__([^_]+)__/g, "<strong>$1</strong>")
    .replace(/(^|[\s(])\*([^*\s][^*]*?)\*/g, "$1<em>$2</em>")
    .replace(/(^|[\s(])_([^_\s][^_]*?)_/g, "$1<em>$2</em>")
    .replace(/\u0000(\d+)\u0000/g, (_, index) => code[Number(index)]);
}

/** The same inline text without the generated tags, for `<title>`. */
function plainInline(text) {
  return inline(text).replace(/<[^>]+>/g, "");
}

/** Markdown sections, one per level-2 heading, as `{ heading, body, codes }`. */
function sections(markdown) {
  const out = [];
  let heading = "";
  let body = [];
  let codes = [];
  let open = null;

  const flush = () => {
    if (body.join("").trim()) out.push({ heading, body: body.join("\n"), codes });
    heading = "";
    body = [];
    codes = [];
  };

  for (const line of markdown.split("\n")) {
    const fence = /^\s*(?:[-*+]|\d+[.)])?\s*(`{3,}|~{3,})(.*)$/.exec(line);
    if (open) {
      if (
        fence &&
        fence[1][0] === open.marker &&
        fence[1].length >= open.length &&
        fence[2].trim() === ""
      ) {
        const text = open.lines.join("\n").replace(/^\n+|\n+$/g, "");
        const token = `\u0001${codes.length}\u0001`;
        codes.push(`<pre><code>${escapeHtml(text)}</code></pre>`);
        body.push("", token, "");
        open = null;
      } else {
        open.lines.push(line);
      }
      continue;
    }
    if (fence) {
      open = { marker: fence[1][0], length: fence[1].length, lines: [] };
      continue;
    }
    const match = /^#{1,3} (.+)$/.exec(line);
    if (match) {
      flush();
      heading = match[1].trim();
      continue;
    }
    body.push(line);
  }

  if (open) {
    const text = open.lines.join("\n").replace(/^\n+|\n+$/g, "");
    const token = `\u0001${codes.length}\u0001`;
    codes.push(`<pre><code>${escapeHtml(text)}</code></pre>`);
    body.push("", token, "");
  }
  flush();
  return out;
}

/** Split a section into documents of at most `max` characters. */
function chunk(body, max) {
  const blocks = body
    .split(/\n{2,}/)
    .map((block) => block.replace(/\s+/g, " ").trim())
    .filter((block) => block.length > 0);
  const chunks = [];
  let current = [];
  let size = 0;
  for (const block of blocks) {
    current.push(block);
    size += block.length;
    if (size >= max) {
      chunks.push(current);
      current = [];
      size = 0;
    }
  }
  if (current.length > 0) chunks.push(current);
  return chunks.filter((group) => group.join(" ").length >= 300);
}

function paragraphs(markdown, codes) {
  return markdown
    .split(/\n{2,}/)
    .map((block) => block.replace(/\s+/g, " ").trim())
    .filter((block) => block.length > 0)
    .map((block) => {
      const match = /^\u0001(\d+)\u0001$/.exec(block);
      return match ? codes[Number(match[1])] ?? "" : `<p>${inline(block)}</p>`;
    })
    .join("");
}

const files = roots.flatMap((root) => markdownFiles(root));
if (files.length === 0) {
  console.error("no markdown found");
  process.exit(1);
}

rmSync(out, { recursive: true, force: true });
mkdirSync(out, { recursive: true });

let written = 0;
let bytes = 0;
for (const file of files) {
  if (written >= target) break;
  const markdown = readFileSync(file, "utf8");
  if (markdown.length < 400) continue;
  for (const section of sections(markdown)) {
    if (written >= target) break;
    const slug = file.replace(/^\.\//, "").replace(/[^\w/-]+/g, "-");
    const base = `${slug}${section.heading ? ` — ${section.heading}` : ""}`;
    const groups = chunk(section.body, chunkSize);
    for (const [index, group] of groups.entries()) {
      if (written >= target) break;
      const title = groups.length > 1 ? `${base} (${index + 1})` : base;
      // The section is the tree the page came from, which is the taxonomy a
      // directory of documentation actually has: `docs/pages/x.md` is one section,
      // `data/y.md` another, a page at the root its own. The file name is never a
      // section: that would be one value per document, not a taxonomy.
      const parts = slug.split("/").filter((part) => part && part !== ".");
      const sectionName = parts.length > 1 ? parts[0] : "root";
      const head = facets
        ? `<html lang="en"><head><meta charset="utf-8">` +
          `<meta name="section" content="${escapeHtml(sectionName)}">` +
          `<title>${plainInline(title)}</title></head>`
        : `<html lang="en"><head><meta charset="utf-8">` +
          `<title>${plainInline(title)}</title></head>`;
      const html =
        `${head}` +
        `<body><main><h1>${inline(title)}</h1>${paragraphs(group.join("\n\n"), section.codes)}</main></body></html>`;
      const name = join(out, `doc-${String(written).padStart(5, "0")}.html`);
      writeFileSync(name, html);
      written += 1;
      bytes += html.length;
    }
  }
}

console.log(
  `${written} documents from ${files.length} markdown files, ${(bytes / 1024 / 1024).toFixed(1)} MiB of HTML${facets ? ", with lang/section metadata" : ""}`,
);
if (written < target) {
  console.log(`only ${written} sections were long enough for the ${target} target`);
}
