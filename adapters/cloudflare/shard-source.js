// Fetch a rustsearch shard set from R2 with `Range` requests.
//
// The artifact is published as a manifest plus one object per section (see
// `rustsearch shard` and `DESIGN.md` §7). A Worker only needs the small parts —
// head, terms, postings, docs, hashes, strhashes — and then reads the URL+title
// and the text block of each rendered hit with a byte range. Every request is a
// `Range` request against the object that holds the bytes, so the transfer is
// proportional to the query, not to the corpus.
//
// A **chunked** artifact (`rustsearch build --term-chunk-bytes N`, format v5)
// publishes its dictionary as a `chunks` directory plus one `chunkNNNNN` object per
// term chunk, each far below the 1 MiB granularity a CDN caches in. The thread here
// is: fetch the metadata (the directory included), let the engine say which chunks
// the query's terms live in (`chunksForQuery`), fetch exactly those and attach them
// (`attachTermChunk`, verified against the directory hash), then rank. The chunks
// stay in the isolate, so a term is only paid for once.
//
// A metadata run too large to be one object (`rustsearch shard --meta-part-bytes N`)
// arrives as `meta00000`, `meta00001`, … — consecutive slices of the same artifact
// range. They are fetched together, because the index cannot be loaded until all of
// them are here.
//
// This module has no dependency on the WASM module: it is plain JS over an
// R2-shaped binding (`get(key, { range: { offset, length } })`), which also makes
// it testable in Node (`shard-source.test.mjs`).

const MANIFEST_FORMAT = "rsx-shard-1";
const FNV_PRIME_LOW = 0x1b3;
const FNV_PRIME_HIGH = 0x100;
/** A term-chunk object of a v5 artifact (`chunk00007`); `chunks` is the directory. */
const CHUNK_PART = /^chunk\d+$/;
/** The title list of one term chunk (`title00007`); only chunks with titles have one. */
const TITLE_PART = /^title\d+$/;
/** One slice of a metadata run split by `--meta-part-bytes` (`meta00007`). */
const META_PART = /^meta\d+$/;
/** Six matches the Workers' per-origin connection budget; `0` means no cap. */
export const DEFAULT_META_CONCURRENCY = 6;

/**
 * SHA-256 as 64 lowercase hex digits, through WebCrypto.
 *
 * The manifest is small and fetched on every cold start, so pinning its SHA-256 is
 * the cheap half of H1: the per-part FNV hashes are a corruption check, and a
 * writer who can replace the parts can recompute them. A deploy passes the digest
 * it built (`expectManifestDigest`), and a substituted manifest is refused before
 * anything is parsed.
 */
export async function sha256Hex(bytes) {
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return Array.from(new Uint8Array(digest), (byte) =>
    byte.toString(16).padStart(2, "0"),
  ).join("");
}

/** Run `worker` over `items` with at most `limit` in flight, keeping item order. */
async function mapLimit(items, limit, worker) {
  const total = items.length;
  if (total === 0) return [];
  const capped =
    limit === Infinity || limit === 0
      ? total
      : Math.max(1, Math.min(total, Math.floor(Number(limit)) || 1));
  let next = 0;
  const results = new Array(total);
  await Promise.all(
    Array.from({ length: Math.min(capped, total) }, async () => {
      for (;;) {
        const index = next++;
        if (index >= total) return;
        results[index] = await worker(items[index]);
      }
    }),
  );
  return results;
}

/** The manifest name of one term chunk, matching `rustsearch_index::shard`. */
export function chunkPartName(chunk) {
  return `chunk${String(chunk).padStart(5, "0")}`;
}

/** The manifest name of a chunk's title list, matching `rustsearch_index::shard`. */
export function titlePartName(chunk) {
  return `title${String(chunk).padStart(5, "0")}`;
}

/** Parse the line-oriented manifest written by `rustsearch shard`. */
/** A manifest is a few hundred lines. Four MiB is generous and bounds the parse. */
export const MAX_MANIFEST_BYTES = 4 * 1024 * 1024;

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
        parts.push({
          name,
          offset: Number(offset),
          length: Number(length),
          hash,
          key,
        });
        break;
      }
      default:
        throw new Error(`malformed manifest line: ${line}`);
    }
  }
  if (format !== MANIFEST_FORMAT) {
    throw new Error(`unsupported manifest format: ${format}`);
  }
  if (!Number.isSafeInteger(total) || total <= 0) {
    throw new Error("manifest has no total");
  }
  if (!checksum) throw new Error("manifest has no checksum");
  if (parts.length === 0) throw new Error("manifest lists no parts");
  let expected = 0;
  for (const part of parts) {
    if (part.offset !== expected) {
      throw new Error(`part ${part.name} does not start at ${expected}`);
    }
    expected += part.length;
  }
  if (expected !== total) {
    throw new Error(`parts cover ${expected} of ${total} bytes`);
  }
  return { format, total, checksum, parts };
}

/**
 * Point every part key at the directory the manifest was read from.
 *
 * `rustsearch shard` names a part `<prefix><name>.bin` and writes `manifest.txt` in
 * the same prefix, so the file name is the only part of the key that identifies an
 * object. Re-basing on the manifest's own directory lets one shard set be published
 * under a versioned prefix (`index/<digest>/`) without regenerating the manifest,
 * and keeps the manifest's digest stable across that move.
 */
export function rebaseParts(manifest, manifestKey) {
  const key = String(manifestKey);
  const directory = key.slice(0, key.lastIndexOf("/") + 1);
  for (const part of manifest.parts) {
    const objectKey = String(part.key);
    part.key = `${directory}${objectKey.slice(objectKey.lastIndexOf("/") + 1)}`;
  }
  return manifest;
}

/** A curation file is a few hundred rules; 64 KiB is generous and bounds the read. */
export const MAX_CURATION_BYTES = 64 * 1024;

/**
 * Read the editorial rules from R2, pinned to the digest the deploy built.
 *
 * Returns a status instead of throwing: a missing file is a deployment without
 * curation, and a file that does not match its pin must not reorder results, but
 * neither is a reason to take search down. The caller logs the status and keeps the
 * uncurated ranking. The bytes are hashed before they are decoded, so a substituted
 * file never reaches the parser.
 */
export async function loadCurations(bucket, key, options = {}) {
  const object = await bucket.get(key);
  if (!object) return { status: "missing" };
  const bytes = new Uint8Array(await object.arrayBuffer());
  const maxBytes = options.maxBytes ?? MAX_CURATION_BYTES;
  if (bytes.length > maxBytes) return { status: "too-large", bytes: bytes.length };
  if (options.expectDigest) {
    const pinned = String(options.expectDigest)
      .trim()
      .toLowerCase()
      .replace(/^sha256:/, "");
    const actual = await sha256Hex(bytes);
    if (actual !== pinned) return { status: "digest-mismatch", actual };
  }
  return { status: "loaded", text: new TextDecoder().decode(bytes) };
}

/** FNV-1a/64 as 16 hex digits, in 32-bit lanes (no BigInt, no float error). */
export function fnv1a64Hex(bytes) {
  let low = 0x84222325 >>> 0;
  let high = 0xcbf29ce4 >>> 0;
  for (let index = 0; index < bytes.length; index += 1) {
    low = (low ^ bytes[index]) >>> 0;
    // (high:low) *= (0x100:0x1b3) mod 2^64. `low * 0x1b3` stays below 2^41, so a
    // double holds it exactly; the rest is 32-bit lane arithmetic:
    //   low * 0x1b3        -> productLow at bit 0, productHigh at bit 32
    //   low * 0x100 * 2^32 -> (low << 8) at bit 32; the overflow is 2^64 and drops
    //   high * 0x1b3 * 2^32 -> (high * 0x1b3) at bit 32
    const lowProduct = low * FNV_PRIME_LOW;
    const productLow = lowProduct >>> 0;
    const productHigh = Math.floor(lowProduct / 4294967296) >>> 0;
    const shiftedHigh = (low << 8) >>> 0;
    const highProduct = (high * FNV_PRIME_LOW) >>> 0;
    low = productLow;
    high = (productHigh + shiftedHigh + highProduct) >>> 0;
  }
  return `${high.toString(16).padStart(8, "0")}${low.toString(16).padStart(8, "0")}`;
}

/**
 * Read `length` bytes of a part, as an R2 `Range` request.
 *
 * The range is relative to the part object; the manifest's offset is where the
 * part lives inside the assembled artifact, not inside the object.
 */
export async function readPartRange(bucket, part, offset, length, stats) {
  const object = await bucket.get(part.key, { range: { offset, length } });
  if (!object) throw new Error(`missing R2 object: ${part.key}`);
  const buffer = await object.arrayBuffer();
  const bytes = new Uint8Array(buffer);
  if (bytes.length !== length) {
    throw new Error(`short range for ${part.key}: ${bytes.length} of ${length} bytes`);
  }
  if (stats) {
    stats.requests += 1;
    stats.bytes += bytes.length;
  }
  return bytes;
}

/** Read one document's text block from the `texts` part. */
export async function readTextBlock(bucket, textsPart, offset, length, stats) {
  return readPartRange(bucket, textsPart, offset, length, stats);
}

/**
 * Read one document's URL followed by its title from the `strings` part.
 *
 * The builder writes the two back to back, so `offset .. offset + length` is the
 * exact slice the engine hashes; `length` is `urlLen + titleLen`. A caller that
 * finds a non-adjacent record (never produced by `rustsearch build`) reads the two
 * halves separately and concatenates them before attaching.
 */
export async function readStringBlock(bucket, stringsPart, offset, length, stats) {
  return readPartRange(bucket, stringsPart, offset, length, stats);
}

/**
 * Load the metadata shards into one sparse artifact.
 *
 * A chunked (v5) set publishes everything a cold query needs — header, chunk
 * directory, BM25 lengths, facets and the record page table — as `metaNNNNN` slices,
 * and the per-hit tables as their own objects: `hashes`/`strhashes` (one 8-byte entry
 * per document) and `titleNNNNN` (the title list of one term chunk). This reads the
 * manifest and every metadata slice and skips those: a rendered hit reads one 8-byte
 * entry from each hash table, and a query reads the title lists of the chunks it
 * fetched. A flat (v4) set still publishes one part per section; the loop below reads
 * them the same way.
 *
 * Every part except `skip` and the ones a query fetches per hit (the text blocks,
 * the strings, the record pages) is fetched with a `Range` request, checked against
 * the manifest's FNV hash and copied to its offset. The result loads with
 * `Engine.fromSparse`; per-hit blocks, pages and per-term chunks arrive later, one
 * verified `Range` read each.
 */
export async function loadSparseIndex(bucket, manifestKey, options = {}) {
  const skip = new Set(
    options.skip ?? ["texts", "strings", "docrecords", "hashes", "strhashes"],
  );
  const metaConcurrency = options.metaConcurrency ?? DEFAULT_META_CONCURRENCY;
  const stats = { requests: 0, bytes: 0 };
  const object = await bucket.get(manifestKey);
  if (!object) throw new Error(`missing R2 object: ${manifestKey}`);
  const manifestText = await object.text();
  const manifestBytes = new TextEncoder().encode(manifestText);
  if (options.expectManifestDigest) {
    const pinned = String(options.expectManifestDigest)
      .trim()
      .toLowerCase()
      .replace(/^sha256:/, "");
    const actual = await sha256Hex(manifestBytes);
    if (actual !== pinned) {
      throw new Error(`manifest does not match the pinned digest: ${actual}`);
    }
  }
  // H10: bound what the worker will accept before it allocates the artifact. Both
  // limits are the deployment's, since only it knows the corpus it published.
  if (manifestBytes.length > (options.maxManifestBytes ?? MAX_MANIFEST_BYTES)) {
    throw new Error(
      `manifest is ${manifestBytes.length} bytes, over the ${options.maxManifestBytes ?? MAX_MANIFEST_BYTES} byte limit`,
    );
  }
  const manifest = rebaseParts(parseManifest(manifestText), manifestKey);
  if (options.maxBytes && manifest.total > options.maxBytes) {
    throw new Error(`artifact is ${manifest.total} bytes, over the ${options.maxBytes} byte limit`);
  }
  stats.requests += 1;
  stats.bytes += manifestBytes.length;

  const bytes = new Uint8Array(manifest.total);
  const parts = {};
  const skipped = {};
  for (const part of manifest.parts) {
    parts[part.name] = part;
  }
  const fetchPart = async (part) => {
    const data = await readPartRange(bucket, part, 0, part.length, stats);
    const hash = fnv1a64Hex(data);
    if (hash !== part.hash) {
      throw new Error(`part ${part.name} does not match its hash`);
    }
    bytes.set(data, part.offset);
  };
  // The metadata slices are independent objects and every one of them is needed, so
  // a large metadata run is fetched concurrently with a cap instead of one round
  // trip at a time. A flat set has no such parts and this is empty.
  await mapLimit(
    manifest.parts.filter((part) => META_PART.test(part.name)),
    metaConcurrency,
    fetchPart,
  );
  for (const part of manifest.parts) {
    // A metadata slice is already here: it was part of the concurrent fetch above.
    if (META_PART.test(part.name)) continue;
    // Term chunks are fetched per query and record pages per hit, not with the
    // metadata: that is the whole point of the chunked layout. The title lists of
    // those chunks are read with them, so a query that never scores a title never
    // fetches one.
    if (
      skip.has(part.name) ||
      CHUNK_PART.test(part.name) ||
      TITLE_PART.test(part.name)
    ) {
      skipped[part.name] = part;
      continue;
    }
    await fetchPart(part);
  }
  // `attached` tracks the text blocks the engine already holds and
  // `attachedStrings` the URL+title blocks, so a warm query fetches neither again.
  return {
    bytes,
    manifest,
    parts,
    skipped,
    stats,
    attached: new Set(),
    attachedStrings: new Set(),
    attachedChunks: new Set(),
  };
}

/**
 * Fetch and attach the term chunks one query needs.
 *
 * `engine.chunksForQuery(query, prefix, fuzzy)` is the same routing the Rust engine
 * documents: the chunk holding each analyzed query term, plus the following chunks
 * while their first term still shares the token's prefix. A fuzzy query asks for
 * every chunk, because its walk crosses the whole sorted dictionary.
 *
 * Each object is read whole (a chunk is small by construction), checked against the
 * manifest hash and handed to the engine, which checks it again against the
 * directory hash before reading a term out of it. A flat artifact has no chunks and
 * this is a no-op.
 */
export async function loadQueryChunks(bucket, loaded, engine, query, options = {}) {
  const total = typeof engine.chunkCount === "number" ? engine.chunkCount : 0;
  if (total === 0 || typeof engine.chunksForQuery !== "function") return [];
  const attached = loaded.attachedChunks ?? (loaded.attachedChunks = new Set());
  const needed = engine.chunksForQuery(
    query,
    options.prefix ?? true,
    Boolean(options.fuzzy),
  );
  // A fuzzy plan is capped at 64 chunks; when the cap applied, the ranking is a
  // bounded approximation and the response says so instead of presenting it as the
  // whole dictionary's answer.
  if (typeof engine.planTruncated === "function") {
    loaded.planTruncated = Boolean(
      engine.planTruncated(query, options.prefix ?? true, Boolean(options.fuzzy)),
    );
  }
  const fetched = [];
  for (const chunk of needed) {
    if (attached.has(chunk)) continue;
    const part = loaded.parts[chunkPartName(chunk)];
    // Published flat (an artifact built without `--term-chunk-bytes`): nothing to do.
    if (!part) continue;
    const bytes = await readPartRange(bucket, part, 0, part.length, loaded.stats);
    if (fnv1a64Hex(bytes) !== part.hash) {
      throw new Error(`part ${part.name} does not match its hash`);
    }
    engine.attachTermChunk(chunk, bytes);
    // The title list of the same terms is its own object: attach it now, so the
    // ranking that follows sees exactly what a whole artifact would. A chunk whose
    // terms never occur in a title has no title part, and one that was fetched before
    // is already attached.
    const titlePart = loaded.parts[titlePartName(chunk)];
    if (titlePart) {
      if (typeof engine.attachTitleChunk !== "function") {
        throw new Error("this engine build cannot score fetched title chunks");
      }
      const titleBytes = await readPartRange(
        bucket,
        titlePart,
        0,
        titlePart.length,
        loaded.stats,
      );
      if (fnv1a64Hex(titleBytes) !== titlePart.hash) {
        throw new Error(`part ${titlePart.name} does not match its hash`);
      }
      engine.attachTitleChunk(chunk, titleBytes);
    }
    attached.add(chunk);
    fetched.push(chunk);
  }
  return fetched;
}

/** The per-document block memory cap a long-lived isolate keeps. */
export const DEFAULT_MAX_BLOCKS = 256;

/**
 * Fetch and attach the record page of each hit.
 *
 * A chunked artifact ranks without the records — the BM25 lengths live in their own
 * table — so the record pages are read only once the hits are known: one `Range`
 * read per distinct page, shared by every hit that lands on it, kept for the life of
 * the isolate. A hit whose page is missing renders no URL and no snippet; it still
 * ranks.
 *
 * A flat artifact has `docPageCount === 0` and this is a no-op: its records came
 * with the metadata.
 */
export async function loadDocPages(bucket, loaded, engine, docIds) {
  const total = typeof engine.docPageCount === "number" ? engine.docPageCount : 0;
  if (total === 0 || typeof engine.docPageOf !== "function") return [];
  return readDocPages(bucket, loaded, engine, docIds.map((docId) => engine.docPageOf(docId)));
}

/**
 * Fetch every record page, for a grouped query.
 *
 * Grouping collapses documents before `limit`, and the page identity it groups by
 * is in the record, so a chunked sparse index cannot group until the whole record
 * table is attached. It is the one query shape that pays for `docrecords` up front;
 * a flat artifact has nothing to fetch and this is a no-op.
 */
export async function loadAllDocPages(bucket, loaded, engine) {
  const total = typeof engine.docPageCount === "number" ? engine.docPageCount : 0;
  if (total === 0) return [];
  // An older WASM module has the count but not the `allDocPages` helper.
  const pages =
    typeof engine.allDocPages === "function"
      ? engine.allDocPages()
      : Array.from({ length: total }, (_, page) => page);
  return readDocPages(bucket, loaded, engine, pages);
}

async function readDocPages(bucket, loaded, engine, pages) {
  const attached = loaded.attachedDocPages ?? (loaded.attachedDocPages = new Set());
  const part = loaded.parts.docrecords;
  if (!part) throw new Error("a chunked index without a docrecords part");
  const fetched = [];
  for (const page of pages) {
    if (page === undefined || page === null || attached.has(page)) continue;
    const span = engine.docPageSpan(page);
    if (span.length !== 2) continue;
    const bytes = await readPartRange(bucket, part, span[0], span[1], loaded.stats);
    // The engine checks the page against its hash in the page table.
    engine.attachDocPage(page, bytes);
    attached.add(page);
    fetched.push(page);
  }
  return fetched;
}

/**
 * Search, then read and verify the URL+title and text block of each hit that has
 * none yet.
 *
 * `options.filters` (an array of `{ field, value }`) restricts the ranking to the
 * documents a facet selects, before `limit` applies; the filters come back in the
 * JSON next to the results. `options.titleBoost` weighs a title match against a
 * body match, `options.group` collapses a page's documents into one hit, and
 * `options.fuzzy`/`maxEdits` do what they do in the CLI. Grouping needs no extra
 * request: the page identity is the URL offset in the metadata.
 *
 * `loaded` is the result of {@link loadSparseIndex} plus its two `Set`s of attached
 * doc ids; `engine` is the WASM `Engine` (any object with `searchJson`,
 * `textSpan`, `stringSpan`, `attachTextBlock` and `attachStringBlock` works). A
 * missing or corrupt slice is not fatal: the hit keeps whichever half was verified,
 * the other stays empty, and the client can see it.
 */
/**
 * Turn repeated `?filter=field=value` parameters into the filter list the engine
 * takes.
 *
 * Only the first `=` splits the pair, so a value may contain one. An entry without
 * a field or without a value is dropped rather than guessed at, and at most `max`
 * survive: a query string is caller-controlled and the engine rejects the surplus
 * anyway.
 */
export function parseFilters(values, { max = 8 } = {}) {
  const filters = [];
  for (const entry of values) {
    const at = entry.indexOf("=");
    if (at <= 0 || at >= entry.length - 1) continue;
    filters.push({ field: entry.slice(0, at), value: entry.slice(at + 1) });
    if (filters.length >= max) break;
  }
  return filters;
}

/**
 * The engine call, from the plain query to the full policy.
 *
 * The facet section is part of the metadata parts, so a filtered query needs no
 * extra request. `options.filters` is `[{ field, value }]`; the engine takes the
 * same pairs in one string, a unit separator between field and value and a record
 * separator between filters, which no build-time value contains. `titleBoost` of 0
 * ignores the titles; an unset one gets the engine default (see the WASM docs).
 */
function searchJson(engine, query, limit, options) {
  const filters = options.filters ?? [];
  const boost = options.titleBoost;
  const group = Boolean(options.group);
  const advanced = filters.length > 0 || options.fuzzy || boost !== undefined || group;
  if (advanced) {
    if (typeof engine.searchJsonWithOptions !== "function") {
      throw new Error("this engine build cannot apply search options");
    }
    return engine.searchJsonWithOptions(
      query,
      limit,
      true,
      Boolean(options.fuzzy),
      options.maxEdits ?? 1,
      boost === undefined ? Number.NaN : boost,
      filters.map((filter) => `${filter.field}\u001f${filter.value}`).join("\u001e"),
      group,
    );
  }
  return engine.searchJson(query, limit);
}

export async function searchWithSnippets(
  bucket,
  loaded,
  engine,
  query,
  limit,
  options = {},
) {
  const maxBlocks = options.maxBlocks ?? DEFAULT_MAX_BLOCKS;
  // Carries the fetch-plan truncation next to the results, and only when it applies,
  // so every other response keeps its exact shape.
  const answer = (payload) =>
    loaded.planTruncated ? { ...payload, planTruncated: true } : payload;
  // Grouping reads a record for every candidate before `limit`, so on a chunked
  // sparse index it needs the record table before the first rank; every other query
  // reads only the pages its hits land on, after the ranking.
  if (options.group) await loadAllDocPages(bucket, loaded, engine);
  // A chunked artifact ranks nothing until the chunks its terms live in are here.
  await loadQueryChunks(bucket, loaded, engine, query, options);
  const first = JSON.parse(searchJson(engine, query, limit, options));
  // The records a hit needs to render come after the ranking, so the page of each
  // hit is one extra read — shared by every hit on it — instead of the whole table
  // arriving with the metadata.
  if (typeof engine.docPageCount === "number" && engine.docPageCount > 0) {
    await loadDocPages(
      bucket,
      loaded,
      engine,
      first.results.map((hit) => hit.docId),
    );
  }
  const texts = loaded.parts.texts;
  const strings = loaded.parts.strings;
  if (!texts && !strings) return answer(first);
  const attachedStrings = loaded.attachedStrings ?? (loaded.attachedStrings = new Set());

  let fetched = 0;
  for (const hit of first.results) {
    // Strings first: they make the hit readable even when its text block is not
    // available, and the engine verifies them against `strhashes`.
    if (strings && !attachedStrings.has(hit.docId) && attachedStrings.size < maxBlocks) {
      const span = engine.stringSpan(hit.docId);
      if (span.length === 4) {
        const [urlOffset, urlLength, titleOffset, titleLength] = span;
        try {
          let block;
          if (titleOffset === urlOffset + urlLength) {
            block = await readStringBlock(
              bucket,
              strings,
              urlOffset,
              urlLength + titleLength,
              loaded.stats,
            );
          } else {
            const url = await readPartRange(bucket, strings, urlOffset, urlLength, loaded.stats);
            const title = await readPartRange(
              bucket,
              strings,
              titleOffset,
              titleLength,
              loaded.stats,
            );
            block = new Uint8Array(url.length + title.length);
            block.set(url, 0);
            block.set(title, url.length);
          }
          // The entry, not the whole table: 8 bytes against a hit that is already
          // paying for its URL and title. A set that still carries the table in a
          // fetched part simply has no `strhashes` object to read.
          if (loaded.parts.strhashes && typeof engine.attachStringHash === "function") {
            const entry = await readPartRange(
              bucket,
              loaded.parts.strhashes,
              hit.docId * 8,
              8,
              loaded.stats,
            );
            engine.attachStringHash(hit.docId, entry);
          }
          engine.attachStringBlock(hit.docId, block);
          attachedStrings.add(hit.docId);
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
      const block = await readTextBlock(bucket, texts, span[0], span[1], loaded.stats);
      if (loaded.parts.hashes && typeof engine.attachTextHash === "function") {
        const entry = await readPartRange(
          bucket,
          loaded.parts.hashes,
          hit.docId * 8,
          8,
          loaded.stats,
        );
        engine.attachTextHash(hit.docId, entry);
      }
      engine.attachTextBlock(hit.docId, block);
      loaded.attached.add(hit.docId);
      fetched += 1;
    } catch {
      // Keep the hit without a snippet.
    }
  }
  if (fetched === 0) return answer(first);
  // The hit shapes come from the attached blocks, so rank once more with them
  // present.
  return answer(JSON.parse(searchJson(engine, query, limit, options)));
}
