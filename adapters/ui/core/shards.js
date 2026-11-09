// A rustsearch shard set, over plain HTTP.
//
// `rustsearch shard index.rsx -o demo/shards --prefix index/` writes a manifest and
// one object per section (DESIGN.md §7). The artifact is a routing table, not a
// second format: the parts at their manifest offsets concatenate back into the
// original `.rsx` byte for byte.
//
// The point of loading this way is what is *not* fetched. The metadata — header,
// dictionary, postings, record lengths, hashes, facets — is what a cold query needs;
// the text blocks of the documents (often most of the artifact) and the per-hit URL
// and title strings are read afterwards, one `Range` request each, for the hits that
// are actually rendered. A chunked artifact (v5) additionally keeps its dictionary
// in per-chunk objects, and only the chunks the query's terms live in are fetched.
//
// This is the browser port of `rustsearch/adapters/cloudflare/shard-source.js`: same
// manifest, same FNV-1a/64 part hashes, same order of operations. The only
// difference is the transport (fetch with a `Range` header instead of an R2 binding)
// and the fallback for a server that ignores `Range` and answers `200` with the
// whole object.

const MANIFEST_FORMAT = "rsx-shard-1";
const CHUNK_PART = /^chunk\d+$/;
const META_PART = /^meta\d+$/;
/** Parts a cold query does not need: they are read per hit, or not at all. */
const LAZY_PARTS = new Set(["texts", "strings", "docrecords"]);
/**
 * How many metadata slices a cold start fetches at once. Six is the per-origin
 * connection limit every browser still applies to HTTP/1.1, so a larger number
 * only queues requests client-side; `0` means no cap.
 */
export const DEFAULT_META_CONCURRENCY = 6;

/** The manifest name of one term chunk, matching `rustsearch_index::shard`. */
export function chunkPartName(chunk) {
  return `chunk${String(chunk).padStart(5, "0")}`;
}

/** `meta` or one slice of a metadata run cut by `--meta-part-bytes`. */
function isMetaPart(part) {
  return part.name === "meta" || META_PART.test(part.name);
}

/**
 * Run `worker` over `items` with at most `limit` in flight, preserving order in
 * the returned array. A limit of 0/Infinity means no cap; a non-positive or
 * invalid one becomes 1.
 */
async function mapLimit(items, limit, worker) {
  const total = items.length;
  if (total === 0) return [];
  const capped =
    limit === Infinity || limit === 0
      ? total
      : Math.max(1, Math.min(total, Math.floor(Number(limit)) || 1));
  const results = new Array(total);
  let next = 0;
  const runners = Array.from({ length: Math.min(capped, total) }, async () => {
    for (;;) {
      const index = next++;
      if (index >= total) return;
      results[index] = await worker(items[index]);
    }
  });
  await Promise.all(runners);
  return results;
}

/** Parse the line-oriented manifest written by `rustsearch shard`. */
export function parseManifest(text) {
  let format = null;
  let total = 0;
  let checksum = null;
  const parts = [];
  for (const raw of text.split("\n")) {
    const line = raw.trim();
    if (!line || line.startsWith("#")) continue;
    const fields = line.split(/\s+/);
    switch (fields[0]) {
      case "format":
        format = fields[1];
        break;
      case "total":
        total = Number(fields[1]);
        break;
      case "checksum":
        checksum = fields[1];
        break;
      case "part": {
        const [, name, offset, length, hash, key] = fields;
        parts.push({ name, offset: Number(offset), length: Number(length), hash, key });
        break;
      }
      default:
        throw new Error(`malformed manifest line: ${line}`);
    }
  }
  if (format !== MANIFEST_FORMAT) throw new Error(`unsupported manifest format: ${format}`);
  if (!Number.isSafeInteger(total) || total <= 0) throw new Error("manifest has no total");
  if (!checksum) throw new Error("manifest has no checksum");
  if (parts.length === 0) throw new Error("manifest lists no parts");
  let expected = 0;
  for (const part of parts) {
    if (part.offset !== expected) throw new Error(`part ${part.name} does not start at ${expected}`);
    expected += part.length;
  }
  if (expected !== total) throw new Error(`parts cover ${expected} of ${total} bytes`);
  return { format, total, checksum, parts };
}

/** FNV-1a/64 as 16 hex digits, in 32-bit lanes (no BigInt). */
export function fnv1a64Hex(bytes) {
  let low = 0x84222325 >>> 0;
  let high = 0xcbf29ce4 >>> 0;
  for (let index = 0; index < bytes.length; index += 1) {
    low = (low ^ bytes[index]) >>> 0;
    const lowProduct = low * 0x1b3;
    const productLow = lowProduct >>> 0;
    const productHigh = Math.floor(lowProduct / 4294967296) >>> 0;
    const shiftedHigh = (low << 8) >>> 0;
    const highProduct = (high * 0x1b3) >>> 0;
    low = productLow;
    high = (productHigh + shiftedHigh + highProduct) >>> 0;
  }
  return `${high.toString(16).padStart(8, "0")}${low.toString(16).padStart(8, "0")}`;
}

/** `demo/shards/manifest.txt` + `index/texts.bin` -> `demo/shards/texts.bin`. */
function urlOf(manifestUrl, part) {
  const name = part.key.split("/").pop();
  return new URL(name, manifestUrl).href;
}

/**
 * Read `length` bytes at `offset` of one part.
 *
 * A `206` is the answer the client wants. A `200` means the server ignored the
 * `Range` header: the whole object arrived, so it is sliced here — correct, but the
 * transfer is the object's size, and the byte counter says so.
 */
export async function readPartRange(url, offset, length, stats) {
  const response = await fetch(url, {
    headers: { Range: `bytes=${offset}-${offset + length - 1}` },
    cache: "no-store",
  });
  if (response.status !== 206 && !response.ok) {
    throw new Error(`could not read ${url}: HTTP ${response.status} ${response.statusText}`.trim());
  }
  const body = new Uint8Array(await response.arrayBuffer());
  if (stats) {
    stats.requests += 1;
    stats.bytes += body.length;
  }
  if (response.status === 206) {
    if (body.length !== length) {
      throw new Error(`short range for ${url}: ${body.length} of ${length} bytes`);
    }
    return body;
  }
  if (body.length < offset + length) throw new Error(`short read for ${url}`);
  return body.subarray(offset, offset + length);
}

/**
 * Fetch the manifest and the metadata parts into one sparse artifact.
 *
 * Returns the buffer to hand to `Engine.fromSparse`, the parts by name, the ones
 * left for later, and the transfer statistics (`requests`, `bytes`) so the page can
 * show what the query actually cost.
 */
export async function loadShardIndex(
  manifestUrl,
  { metaConcurrency = DEFAULT_META_CONCURRENCY, hash = fnv1a64Hex } = {},
) {
  const stats = { requests: 0, bytes: 0 };
  const response = await fetch(manifestUrl, { cache: "no-store" });
  if (!response.ok) {
    throw new Error(`could not read ${manifestUrl}: HTTP ${response.status} ${response.statusText}`.trim());
  }
  const text = await response.text();
  stats.requests += 1;
  stats.bytes += new TextEncoder().encode(text).length;
  const manifest = parseManifest(text);

  const bytes = new Uint8Array(manifest.total);
  const parts = {};
  const skipped = {};
  for (const part of manifest.parts) {
    const url = urlOf(manifestUrl, part);
    parts[part.name] = { ...part, url };
  }

  // Every metadata part is needed before the index can load. A chunked artifact
  // ships them as one `meta` part — or, when `--meta-part-bytes` split the run, as
  // `meta00000`, `meta00001`, … — and this is one request per part. The slices are
  // independent, so they are fetched in parallel with a cap: six matches the
  // browser's per-origin connections, and `0` removes the cap. Flat artifacts keep
  // their handful of sections on the unbounded path.
  const wanted = manifest.parts.filter(
    (part) => !LAZY_PARTS.has(part.name) && !CHUNK_PART.test(part.name),
  );
  const fetchPart = async (part) => {
    const data = await readPartRange(parts[part.name].url, 0, part.length, stats);
    if (hash(data) !== part.hash) throw new Error(`part ${part.name} does not match its hash`);
    bytes.set(data, part.offset);
  };
  const metaParts = wanted.filter(isMetaPart);
  await Promise.all([
    ...wanted.filter((part) => !isMetaPart(part)).map(fetchPart),
    mapLimit(metaParts, metaConcurrency, fetchPart),
  ]);
  for (const part of manifest.parts) {
    if (!wanted.includes(part)) skipped[part.name] = parts[part.name];
  }

  return { bytes, manifest, parts, skipped, stats, attached: new Set(), attachedStrings: new Set(), attachedChunks: new Set() };
}

/** Fetch and attach the term chunks the query's terms live in (v5 artifacts). */
export async function attachQueryChunks(loaded, engine, query, { prefix = true, fuzzy = false } = {}) {
  const total = typeof engine.chunkCount === "number" ? engine.chunkCount : 0;
  if (total === 0 || typeof engine.chunksForQuery !== "function") return 0;
  let fetched = 0;
  for (const chunk of engine.chunksForQuery(query, prefix, fuzzy)) {
    if (loaded.attachedChunks.has(chunk)) continue;
    const part = loaded.parts[chunkPartName(chunk)];
    if (!part) continue;
    const data = await readPartRange(part.url, 0, part.length, loaded.stats);
    if (fnv1a64Hex(data) !== part.hash) throw new Error(`part ${part.name} does not match its hash`);
    engine.attachTermChunk(chunk, data);
    loaded.attachedChunks.add(chunk);
    fetched += 1;
  }
  return fetched;
}

/** Fetch and attach one record page the first time it is needed. */
async function attachDocPages(loaded, engine, candidates) {
  const part = loaded.parts.docrecords;
  if (!part) throw new Error("a chunked index without a docrecords part");
  const pages = loaded.attachedDocPages ?? (loaded.attachedDocPages = new Set());
  let fetched = 0;
  for (const page of candidates) {
    if (page === undefined || page === null || pages.has(page)) continue;
    const span = engine.docPageSpan(page);
    if (span.length !== 2) continue;
    const data = await readPartRange(part.url, span[0], span[1], loaded.stats);
    engine.attachDocPage(page, data);
    pages.add(page);
    fetched += 1;
  }
  return fetched;
}

/** Fetch the record page of every hit (v5 artifacts split them out). */
export async function loadDocPages(loaded, engine, docIds) {
  const total = typeof engine.docPageCount === "number" ? engine.docPageCount : 0;
  if (total === 0 || typeof engine.docPageOf !== "function") return 0;
  return attachDocPages(loaded, engine, docIds.map((docId) => engine.docPageOf(docId)));
}

/**
 * Fetch every record page. A grouped query needs all of them: grouping runs before
 * `limit` and the page identity it groups by lives in the record, so the engine
 * cannot rank a chunked sparse index until the whole record table is here. Flat
 * artifacts and failed calls are a no-op.
 */
export async function loadAllDocPages(loaded, engine) {
  const total = typeof engine.docPageCount === "number" ? engine.docPageCount : 0;
  if (total === 0) return 0;
  // `allDocPages` arrived with the v5 WASM getters; an older module already knows
  // how many pages there are, so the pages are just `0 .. pageCount - 1`.
  const pages =
    typeof engine.allDocPages === "function"
      ? engine.allDocPages()
      : Array.from({ length: total }, (_, page) => page);
  return attachDocPages(loaded, engine, pages);
}

/**
 * Read the URL+title and the text block of each hit that has none yet.
 *
 * A missing or corrupt slice is not fatal: the hit keeps whichever half verified, so
 * the list still ranks and the client can see what arrived.
 */
export async function attachHitBlocks(loaded, engine, hits, { maxBlocks = 256 } = {}) {
  const strings = loaded.parts.strings;
  const texts = loaded.parts.texts;
  if (!strings && !texts) return 0;
  let fetched = 0;
  for (const hit of hits) {
    if (strings && !loaded.attachedStrings.has(hit.docId) && loaded.attachedStrings.size < maxBlocks) {
      const span = engine.stringSpan(hit.docId);
      if (span.length === 4) {
        const [urlOffset, urlLength, titleOffset, titleLength] = span;
        try {
          const block =
            titleOffset === urlOffset + urlLength
              ? await readPartRange(strings.url, urlOffset, urlLength + titleLength, loaded.stats)
              : await concatenate(
                  await readPartRange(strings.url, urlOffset, urlLength, loaded.stats),
                  await readPartRange(strings.url, titleOffset, titleLength, loaded.stats),
                );
          engine.attachStringBlock(hit.docId, block);
          loaded.attachedStrings.add(hit.docId);
          fetched += 1;
        } catch {
          // Keep the hit without a URL; the next query tries again.
        }
      }
    }

    if (loaded.attached.size >= maxBlocks) break;
    if (!texts || loaded.attached.has(hit.docId)) continue;
    const span = engine.textSpan(hit.docId);
    if (span.length !== 2 || span[1] === 0) continue;
    try {
      const block = await readPartRange(texts.url, span[0], span[1], loaded.stats);
      engine.attachTextBlock(hit.docId, block);
      loaded.attached.add(hit.docId);
      fetched += 1;
    } catch {
      // Keep the hit without a snippet.
    }
  }
  return fetched;
}

function concatenate(left, right) {
  const joined = new Uint8Array(left.length + right.length);
  joined.set(left, 0);
  joined.set(right, left.length);
  return joined;
}
