// Pure helpers shared by the UI and its tests. No DOM, no WASM: this module must
// stay importable from Node so the safety rules are unit-tested directly.

/** Separators the WASM boundary expects: field/value and filter/filter. */
export const FILTER_FIELD_VALUE = "\u001f";
export const FILTER_RECORD = "\u001e";

const DANGEROUS_CONTROL = /[\u0000-\u001f\u007f]/;

/**
 * A URL that is safe to put on an anchor.
 *
 * The artifact is untrusted, so `href` must not be able to run script:
 * `javascript:`, `data:` and `vbscript:` are dropped, control characters (which
 * browsers strip, turning "java\nscript:" into a working scheme) reject the whole
 * URL, and everything else is kept as a relative or http(s) link.
 */
export function safeHref(url) {
  if (typeof url !== "string") return "";
  const value = url.trim();
  if (value === "" || DANGEROUS_CONTROL.test(value)) return "";
  const scheme = /^([a-zA-Z][a-zA-Z0-9+.-]*):/.exec(value);
  if (!scheme) return value;
  const name = scheme[1].toLowerCase();
  return name === "http" || name === "https" ? value : "";
}

/** `Map<field, Set<value>>` -> the string `searchJsonWithOptions` takes. */
export function encodeFilters(filters) {
  if (!(filters instanceof Map)) return "";
  const parts = [];
  for (const [field, values] of filters) {
    if (!field) continue;
    for (const value of values) parts.push(`${field}${FILTER_FIELD_VALUE}${value}`);
  }
  return parts.join(FILTER_RECORD);
}

/** `"section=guide"` (one `?f=` value) -> `["section", "guide"]`, or null. */
export function parseFilterParam(raw) {
  if (typeof raw !== "string") return null;
  const at = raw.indexOf("=");
  if (at <= 0 || at === raw.length - 1) return null;
  return [raw.slice(0, at), raw.slice(at + 1)];
}

/** 1024 -> "1.0 KiB"; 1751414 -> "1.7 MiB"; 120 -> "120 B". */
export function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  if (bytes < 1024) return `${Math.round(bytes)} B`;
  const units = ["KiB", "MiB", "GiB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value >= 100 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
}

/** 12345 -> "12,345", independent of the runtime locale. */
export function formatCount(value) {
  if (!Number.isFinite(value)) return "—";
  return String(Math.trunc(value)).replace(/\B(?=(\d{3})+(?!\d))/g, ",");
}

/** `pluralize(1, "result")` -> "1 result"; `pluralize(2, "result")` -> "2 results". */
export function pluralize(count, singular, plural = `${singular}s`) {
  return `${formatCount(count)} ${count === 1 ? singular : plural}`;
}

/** True when a byte index falls between two UTF-8 code points. */
function isUtf8Boundary(bytes, index) {
  if (index <= 0 || index >= bytes.length) return true;
  return (bytes[index] & 0xc0) !== 0x80;
}

/**
 * Split a snippet into text and `<mark>` segments from byte ranges.
 *
 * The engine returns **byte** offsets into a UTF-8 string, while JavaScript
 * indexes UTF-16 code units, so the ranges are applied to the encoded bytes and
 * each slice is decoded back. A range that is malformed, out of bounds, overlapping
 * an earlier one or cuts a code point in half is dropped instead of producing a
 * replacement character: highlighting is best effort, the text is not.
 */
export function highlightSegments(text, ranges) {
  if (typeof text !== "string" || text === "") return [];
  const bytes = new TextEncoder().encode(text);
  const decoder = new TextDecoder("utf-8");

  const clean = [];
  for (const range of Array.isArray(ranges) ? ranges : []) {
    if (!Array.isArray(range) || range.length < 2) continue;
    const start = Number(range[0]);
    const end = Number(range[1]);
    if (!Number.isFinite(start) || !Number.isFinite(end)) continue;
    const from = Math.max(0, Math.trunc(start));
    const to = Math.min(bytes.length, Math.trunc(end));
    if (from >= to) continue;
    if (!isUtf8Boundary(bytes, from) || !isUtf8Boundary(bytes, to)) continue;
    clean.push([from, to]);
  }
  clean.sort((a, b) => a[0] - b[0]);

  const segments = [];
  let cursor = 0;
  for (const [from, to] of clean) {
    if (from < cursor) continue;
    if (from > cursor) {
      segments.push({ text: decoder.decode(bytes.subarray(cursor, from)), mark: false });
    }
    segments.push({ text: decoder.decode(bytes.subarray(from, to)), mark: true });
    cursor = to;
  }
  if (cursor < bytes.length) {
    segments.push({ text: decoder.decode(bytes.subarray(cursor)), mark: false });
  }
  return segments;
}
