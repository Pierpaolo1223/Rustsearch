// Regression tests for the HTML corpus generator (`node --test tools/make-corpus.test.mjs`).
//
// The generated pages are opened directly in a browser as search results. They
// must declare UTF-8 and must not expose the inline Markdown the source pages use.
import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const TOOL = join(dirname(fileURLToPath(import.meta.url)), "make-corpus.mjs");

const PARAGRAPH =
  "A paragraph with **bold**, *emphasis*, __strong__, _soft_, `inline <code> & stuff`, " +
  "and ``double`` code. It also has a literal <tag> and an ampersand. ";

const CODE_BLOCK = "tree │ **/*.md\n  ├── <tag> & stuff\n  └── child";

function generate({ facets = false } = {}) {
  const root = mkdtempSync(join(tmpdir(), "rustsearch-make-corpus-"));
  const docs = join(root, "docs");
  const out = join(root, "site");
  mkdirSync(docs);
  writeFileSync(
    join(docs, "guide.md"),
    `# A **bold** title\n\n${PARAGRAPH.repeat(2)}\n\n\`\`\`text\n${CODE_BLOCK}\n\`\`\`\n\n` +
      `${PARAGRAPH.repeat(2)}\n\n## More \`code\`\n\n${PARAGRAPH.repeat(2)}\n`,
  );

  const args = [TOOL, out, "--roots", docs, "--target", "10", "--chunk", "850"];
  if (facets) args.push("--facets");
  const result = spawnSync(process.execPath, args, { encoding: "utf8" });
  assert.equal(result.status, 0, result.stderr || result.stdout);

  const pages = readdirSync(out)
    .filter((name) => name.endsWith(".html"))
    .map((name) => join(out, name));
  assert.ok(pages.length > 0, "the generator wrote at least one page");

  return { root, pages };
}

function readPages(pages) {
  return pages.map((page) => readFileSync(page, "utf8"));
}

test("generated pages declare UTF-8 before any text", () => {
  for (const facets of [false, true]) {
    const { root, pages } = generate({ facets });
    try {
      for (const [index, html] of readPages(pages).entries()) {
        assert.match(html, /^<html lang="en"><head><meta charset="utf-8">/, `${pages[index]}`);
      }
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  }
});

test("fenced code blocks stay verbatim inside <pre><code>", () => {
  const { root, pages } = generate({ facets: true });
  try {
    const html = readPages(pages).join("\n");
    assert.match(
      html,
      /<pre><code>tree │ \*\*\/\*\.md\n  ├── &lt;tag&gt; &amp; stuff\n  └── child<\/code><\/pre>/,
    );
    assert.doesNotMatch(html, /<p>tree/);
    assert.doesNotMatch(html, /```/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("inline Markdown is rendered as HTML and source markup is escaped", () => {
  const { root, pages } = generate({ facets: true });
  try {
    const html = readPages(pages).join("\n");
    assert.match(html, /<strong>bold<\/strong>/);
    assert.match(html, /<em>emphasis<\/em>/);
    assert.match(html, /<strong>strong<\/strong>/);
    assert.match(html, /<em>soft<\/em>/);
    assert.match(html, /<code>inline &lt;code&gt; &amp; stuff<\/code>/);
    assert.match(html, /<code>double<\/code>/);
    assert.doesNotMatch(html, /\*\*bold\*\*/);
    assert.doesNotMatch(html, /`code`/);
    assert.doesNotMatch(html, /``double``/);
    assert.doesNotMatch(html, /<tag>/);
    assert.match(html, /<title>[^<]*A bold title<\/title>/);
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
