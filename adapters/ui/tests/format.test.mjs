import test from "node:test";
import assert from "node:assert/strict";

import {
  encodeFilters,
  formatBytes,
  formatCount,
  highlightSegments,
  parseFilterParam,
  pluralize,
  safeHref,
} from "../core/format.js";

test("safeHref keeps relative and http(s) links only", () => {
  assert.equal(safeHref("site/doc-00001.html"), "site/doc-00001.html");
  assert.equal(safeHref("/guide/install.html"), "/guide/install.html");
  assert.equal(safeHref("#anchor"), "#anchor");
  assert.equal(safeHref("https://example.com/a?b=1"), "https://example.com/a?b=1");
  assert.equal(safeHref("HTTP://example.com"), "HTTP://example.com");
  assert.equal(safeHref("  https://example.com  "), "https://example.com");
  assert.equal(safeHref("javascript:alert(1)"), "");
  assert.equal(safeHref("JaVaScRiPt:alert(1)"), "");
  assert.equal(safeHref("data:text/html,<script>"), "");
  assert.equal(safeHref("vbscript:msgbox(1)"), "");
  assert.equal(safeHref("java\nscript:alert(1)"), "");
  assert.equal(safeHref("https://example.com/\u0000"), "");
  assert.equal(safeHref(""), "");
  assert.equal(safeHref(null), "");
  assert.equal(safeHref(42), "");
});

test("encodeFilters uses the WASM separators and ORs inside a field", () => {
  const filters = new Map([
    ["section", new Set(["guide", "reference"])],
    ["lang", new Set(["en"])],
  ]);
  assert.equal(
    encodeFilters(filters),
    "section\u001fguide\u001esection\u001freference\u001elang\u001fen",
  );
  assert.equal(encodeFilters(new Map()), "");
  assert.equal(encodeFilters(null), "");
});

test("parseFilterParam splits on the first equals sign", () => {
  assert.deepEqual(parseFilterParam("section=guide"), ["section", "guide"]);
  assert.deepEqual(parseFilterParam("tags=a=b"), ["tags", "a=b"]);
  assert.equal(parseFilterParam("section"), null);
  assert.equal(parseFilterParam("=guide"), null);
  assert.equal(parseFilterParam("section="), null);
  assert.equal(parseFilterParam(null), null);
});

test("highlightSegments marks ASCII ranges and keeps the rest as text", () => {
  const segments = highlightSegments("the immutable index", [[4, 13]]);
  assert.deepEqual(segments, [
    { text: "the ", mark: false },
    { text: "immutable", mark: true },
    { text: " index", mark: false },
  ]);
  assert.deepEqual(highlightSegments("plain", []), [{ text: "plain", mark: false }]);
  assert.deepEqual(highlightSegments("", [[0, 1]]), []);
});

test("highlightSegments handles UTF-8 byte offsets, not code-unit offsets", () => {
  const text = "caffè e index";
  const bytes = new TextEncoder().encode(text);
  const start = text.indexOf("è");
  // "è" is two bytes and one UTF-16 code unit: the byte range is not (7, 8).
  const segments = highlightSegments(text, [[bytes.indexOf(0xc3), bytes.indexOf(0xc3) + 2]]);
  assert.deepEqual(segments, [
    { text: "caff", mark: false },
    { text: "è", mark: true },
    { text: " e index", mark: false },
  ]);
  assert.equal(start, 4);
});

test("highlightSegments drops bad, overlapping and out-of-bounds ranges", () => {
  assert.deepEqual(highlightSegments("abcdef", [[2, 4], [3, 5]]), [
    { text: "ab", mark: false },
    { text: "cd", mark: true },
    { text: "ef", mark: false },
  ]);
  assert.deepEqual(highlightSegments("abcdef", [[-4, 2], [4, 99]]), [
    { text: "ab", mark: true },
    { text: "cd", mark: false },
    { text: "ef", mark: true },
  ]);
  assert.deepEqual(highlightSegments("abcdef", [[4, 2], [3, 3], ["x", 2], []]), [
    { text: "abcdef", mark: false },
  ]);
  // A range that starts inside a two-byte character is dropped whole.
  const bytes = new TextEncoder().encode("aèb");
  assert.deepEqual(highlightSegments("aèb", [[2, 3]]), [{ text: "aèb", mark: false }]);
  assert.deepEqual(highlightSegments("aèb", [[1, 3]]), [
    { text: "a", mark: false },
    { text: "è", mark: true },
    { text: "b", mark: false },
  ]);
  assert.ok(bytes.length === 4);
});

test("formatBytes, formatCount and pluralize", () => {
  assert.equal(formatBytes(120), "120 B");
  assert.equal(formatBytes(1024), "1.0 KiB");
  assert.equal(formatBytes(1751414), "1.7 MiB");
  assert.equal(formatBytes(-1), "—");
  assert.equal(formatCount(12345), "12,345");
  assert.equal(formatCount(0), "0");
  assert.equal(pluralize(1, "result"), "1 result");
  assert.equal(pluralize(2, "result"), "2 results");
  assert.equal(pluralize(1000, "document"), "1,000 documents");
});
