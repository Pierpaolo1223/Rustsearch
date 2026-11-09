// Tests for the R2 shard fetcher the Worker uses (`node --test`).
//
// A fake bucket behaves like an R2 binding: `get(key, { range })` returns only the
// requested bytes and records every request, so the test can assert that the
// loader reads the metadata parts and never touches the text or string sections
// whole.
import assert from "node:assert/strict";
import test from "node:test";

import {
  MAX_CURATION_BYTES,
  chunkPartName,
  fnv1a64Hex,
  loadAllDocPages,
  loadCurations,
  loadDocPages,
  loadQueryChunks,
  loadSparseIndex,
  parseFilters,
  parseManifest,
  readTextBlock,
  searchWithSnippets,
  sha256Hex,
  titlePartName,
} from "./shard-source.js";

const DECODER = new TextDecoder();

class FakeBucket {
  constructor(objects) {
    this.objects = objects;
    this.requests = [];
  }

  async get(key, options = {}) {
    const range = options.range ?? null;
    this.requests.push({ key, range });
    const bytes = this.objects[key];
    if (!bytes) return null;
    const offset = range ? range.offset : 0;
    const length = range ? range.length : bytes.length - offset;
    const slice = bytes.slice(offset, offset + length);
    return {
      arrayBuffer: async () => slice.buffer,
      text: async () => DECODER.decode(slice),
    };
  }
}

function bytesOf(length, seed = 7) {
  const out = new Uint8Array(length);
  for (let index = 0; index < length; index += 1) {
    out[index] = (index * seed + 11) & 0xff;
  }
  return out;
}

const NAMES = [
  "head",
  "terms",
  "postings",
  "docs",
  "strings",
  "texts",
  "hashes",
  "strhashes",
  "facets",
];

function shardSet(
  sizes = {
    head: 104,
    terms: 40,
    postings: 24,
    docs: 12,
    strings: 32,
    texts: 50,
    hashes: 16,
    strhashes: 16,
    facets: 20,
  },
  names = NAMES,
) {
  const objects = {};
  const parts = [];
  let offset = 0;
  for (const name of names) {
    const bytes = bytesOf(sizes[name], name.length);
    objects[`index/${name}.bin`] = bytes;
    parts.push({
      name,
      offset,
      length: bytes.length,
      hash: fnv1a64Hex(bytes),
      key: `index/${name}.bin`,
    });
    offset += bytes.length;
  }
  const total = offset;
  const lines = ["format rsx-shard-1", `total ${total}`, "checksum 0123456789abcdef"];
  for (const part of parts) {
    lines.push(
      `part ${part.name} ${part.offset} ${part.length} ${part.hash} ${part.key}`,
    );
  }
  objects["index/manifest.txt"] = new TextEncoder().encode(`${lines.join("\n")}\n`);
  return { objects, parts, total };
}

const EMPTY = new Set();

test("fnv1a64 matches the Rust implementation", () => {
  const encoder = new TextEncoder();
  const vectors = {
    "": "cbf29ce484222325",
    a: "af63dc4c8601ec8c",
    foobar: "85944171f73967e8",
    rustsearch: "f46587acc770c687",
  };
  for (const [text, expected] of Object.entries(vectors)) {
    assert.equal(fnv1a64Hex(encoder.encode(text)), expected, text);
  }
});

test("the manifest is validated before anything is fetched", () => {
  const { objects } = shardSet();
  const text = DECODER.decode(objects["index/manifest.txt"]);
  const manifest = parseManifest(text);
  assert.equal(manifest.format, "rsx-shard-1");
  assert.equal(manifest.parts.length, NAMES.length);
  assert.equal(manifest.total, manifest.parts.at(-1).offset + manifest.parts.at(-1).length);

  assert.throws(() => parseManifest("format rsx-shard-2\ntotal 1\nchecksum 00\n"), /unsupported/);
  assert.throws(() => parseManifest("format rsx-shard-1\ntotal 8\n"), /checksum/);
  assert.throws(
    () =>
      parseManifest(
        "format rsx-shard-1\ntotal 8\nchecksum 00\npart head 2 6 deadbeef index/head.bin\n",
      ),
    /does not start at 0/,
  );
  assert.throws(
    () =>
      parseManifest(
        "format rsx-shard-1\ntotal 9\nchecksum 00\npart head 0 6 deadbeef index/head.bin\n",
      ),
    /parts cover/,
  );
});

test("the metadata parts are loaded by range; texts and strings are skipped", async () => {
  const { objects, parts, total } = shardSet();
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");

  // One request per metadata part, each a range that covers the whole object:
  // the two big sections are never fetched.
  // `facets` is metadata: it is part of the same fetch as the dictionary, so a
  // filtered query costs nothing extra.
  const skippedNames = new Set(["texts", "strings", "hashes", "strhashes"]);
  const partRequests = bucket.requests.filter((request) => request.range !== null);
  assert.deepEqual(
    partRequests.map((request) => request.key),
    parts.filter((part) => !skippedNames.has(part.name)).map((part) => part.key),
  );
  for (const request of partRequests) {
    assert.deepEqual(request.range, { offset: 0, length: objects[request.key].length });
  }
  assert.equal(bucket.requests.length, NAMES.length - skippedNames.size + 1);

  // Every fetched byte lands at its offset and the two skipped regions stay zero.
  assert.equal(loaded.bytes.length, total);
  for (const part of parts) {
    const expected = skippedNames.has(part.name)
      ? new Uint8Array(part.length)
      : objects[part.key];
    assert.deepEqual(
      loaded.bytes.subarray(part.offset, part.offset + part.length),
      expected,
      part.name,
    );
  }
  assert.equal(loaded.parts.texts.length, 50);
  assert.equal(loaded.parts.strings.length, 32);
  assert.equal(loaded.skipped.texts.key, "index/texts.bin");
  assert.equal(loaded.skipped.strings.key, "index/strings.bin");
  assert.equal(loaded.stats.requests, NAMES.length - skippedNames.size + 1);
  assert.equal(
    loaded.stats.bytes,
    objects["index/manifest.txt"].length +
      parts
        .filter((part) => !skippedNames.has(part.name))
        .reduce((sum, part) => sum + part.length, 0),
  );
});

test("a part that does not match its manifest hash is refused", async () => {
  const { objects } = shardSet();
  const bucket = new FakeBucket({
    ...objects,
    "index/postings.bin": bytesOf(24, 3),
  });
  await assert.rejects(loadSparseIndex(bucket, "index/manifest.txt"), /postings/);
});

test("a missing object and a short range are errors, not silence", async () => {
  const { objects } = shardSet();
  const missingManifest = new FakeBucket({});
  await assert.rejects(
    loadSparseIndex(missingManifest, "index/manifest.txt"),
    /missing R2 object/,
  );

  const truncated = new FakeBucket({
    ...objects,
    "index/docs.bin": objects["index/docs.bin"].subarray(0, 10),
  });
  await assert.rejects(loadSparseIndex(truncated, "index/manifest.txt"), /short range/);
});

test("a text block is read by range and is exactly the slice asked for", async () => {
  const { objects, parts } = shardSet();
  const bucket = new FakeBucket(objects);
  const texts = parts.find((part) => part.name === "texts");
  const block = await readTextBlock(bucket, texts, 12, 16);

  assert.deepEqual(block, objects["index/texts.bin"].subarray(12, 28));
  assert.deepEqual(bucket.requests, [
    { key: "index/texts.bin", range: { offset: 12, length: 16 } },
  ]);
  // A block that was never fetched is not invented: the range must come back whole.
  const short = new FakeBucket({ "index/texts.bin": objects["index/texts.bin"].subarray(0, 8) });
  await assert.rejects(readTextBlock(short, texts, 0, 16), /short range/);
});

test("a chunked artifact fetches only the chunks a query routes to", async () => {
  const names = [
    "head",
    "chunks",
    "chunk00000",
    "chunk00001",
    "docs",
    "strings",
    "texts",
    "hashes",
    "strhashes",
    "facets",
  ];
  const { objects, parts } = shardSet(
    {
      head: 8,
      chunks: 12,
      chunk00000: 40,
      chunk00001: 24,
      docs: 12,
      strings: 32,
      texts: 50,
      hashes: 16,
      strhashes: 16,
      facets: 20,
    },
    names,
  );

  // Loading the metadata fetches the directory but not a single term chunk.
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");
  assert.ok(loaded.parts.chunks, "the directory travels with the metadata");
  assert.ok(loaded.parts.chunk00001, "the chunk parts are routed to, not loaded");
  assert.deepEqual(
    bucket.requests.map((request) => request.key),
    [
      "index/manifest.txt",
      "index/head.bin",
      "index/chunks.bin",
      "index/docs.bin",
      "index/facets.bin",
    ],
  );

  // The engine names the chunk, the client fetches exactly that object once.
  const attachedChunks = new Map();
  const engine = {
    chunkCount: 2,
    chunksForQuery(query, prefix, fuzzy) {
      assert.equal(query, "walkdir");
      assert.equal(prefix, true);
      assert.equal(fuzzy, false);
      return [1];
    },
    attachTermChunk(chunk, bytes) {
      attachedChunks.set(chunk, bytes);
    },
  };
  bucket.requests.length = 0;
  assert.deepEqual(await loadQueryChunks(bucket, loaded, engine, "walkdir"), [1]);
  assert.deepEqual(bucket.requests, [
    { key: `index/${chunkPartName(1)}.bin`, range: { offset: 0, length: 24 } },
  ]);
  assert.deepEqual(attachedChunks.get(1), objects["index/chunk00001.bin"]);

  bucket.requests.length = 0;
  assert.deepEqual(await loadQueryChunks(bucket, loaded, engine, "walkdir"), []);
  assert.equal(bucket.requests.length, 0, "a warm chunk is not fetched again");

  // A flat artifact (no chunk parts) makes the same call a no-op.
  const flat = new FakeBucket(objects);
  const flatLoaded = { parts: {}, attachedChunks: new Set(), stats: { requests: 0, bytes: 0 } };
  assert.deepEqual(await loadQueryChunks(flat, flatLoaded, { chunkCount: 0 }, "walkdir"), []);
  assert.equal(flat.requests.length, 0);

  // A chunk that does not match the manifest hash is refused before the engine sees it.
  const tampered = new FakeBucket({
    ...objects,
    "index/chunk00001.bin": objects["index/chunk00001.bin"].slice(),
  });
  tampered.objects["index/chunk00001.bin"][0] ^= 0xff;
  await assert.rejects(
    loadQueryChunks(tampered, { ...loaded, attachedChunks: new Set() }, engine, "walkdir"),
    /does not match its hash/,
  );
});

test("a chunk's title list is fetched with the chunk, never at cold start", async () => {
  const names = [
    "head",
    "chunks",
    "chunk00000",
    "title00000",
    "chunk00001",
    "title00001",
    "docs",
    "strings",
    "texts",
    "hashes",
    "strhashes",
    "facets",
  ];
  const sizes = {
    head: 8,
    chunks: 12,
    chunk00000: 40,
    title00000: 18,
    chunk00001: 24,
    title00001: 10,
    docs: 12,
    strings: 32,
    texts: 50,
    hashes: 16,
    strhashes: 16,
    facets: 20,
  };
  const { objects } = shardSet(sizes, names);
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");

  // The title lists are routed to, not loaded: a cold start pays nothing for them.
  assert.equal(titlePartName(1), "title00001");
  assert.ok(loaded.parts.title00001, "the title part is addressable");
  assert.equal(loaded.skipped.title00001.key, "index/title00001.bin");
  assert.ok(
    !bucket.requests.some((request) => request.key.includes("title")),
    "a cold start must not fetch a title list",
  );
  assert.equal(
    loaded.bytes.subarray(
      loaded.parts.title00001.offset,
      loaded.parts.title00001.offset + loaded.parts.title00001.length,
    ).every((byte) => byte === 0),
    true,
  );

  const fetchedTitles = new Map();
  const engine = {
    chunkCount: 2,
    chunksForQuery: () => [1],
    attachTermChunk() {},
    attachTitleChunk(chunk, bytes) {
      fetchedTitles.set(chunk, bytes);
    },
  };
  bucket.requests.length = 0;
  assert.deepEqual(await loadQueryChunks(bucket, loaded, engine, "walkdir"), [1]);
  assert.deepEqual(
    bucket.requests.map((request) => request.key),
    ["index/chunk00001.bin", "index/title00001.bin"],
    "the title list rides along with the chunk that owns it",
  );
  assert.deepEqual(fetchedTitles.get(1), objects["index/title00001.bin"]);

  // A warm chunk fetches neither half again.
  bucket.requests.length = 0;
  assert.deepEqual(await loadQueryChunks(bucket, loaded, engine, "walkdir"), []);
  assert.equal(bucket.requests.length, 0);

  // A chunk with no title list has no object: no request, no error.
  const noTitle = new FakeBucket({
    ...objects,
    "index/manifest.txt": objects["index/manifest.txt"],
  });
  const titleOnlyMissing = {
    ...loaded,
    parts: { ...loaded.parts, title00001: undefined },
    attachedChunks: new Set(),
    stats: { requests: 0, bytes: 0 },
  };
  assert.deepEqual(await loadQueryChunks(noTitle, titleOnlyMissing, engine, "walkdir"), [1]);
  assert.deepEqual(
    noTitle.requests.map((request) => request.key),
    ["index/chunk00001.bin"],
    "a chunk without a title list costs one request, not two",
  );

  // A corrupt title list is refused before the engine can score it.
  const tampered = new FakeBucket({
    ...objects,
    "index/title00001.bin": objects["index/title00001.bin"].slice(),
  });
  tampered.objects["index/title00001.bin"][0] ^= 0xff;
  await assert.rejects(
    loadQueryChunks(tampered, { ...loaded, attachedChunks: new Set() }, engine, "walkdir"),
    /title00001 does not match its hash/,
  );

  // An engine built before the title parts existed cannot score them: that is a
  // deployment error, not a silent drop of the title signal.
  const oldEngine = {
    chunkCount: 2,
    chunksForQuery: () => [1],
    attachTermChunk() {},
  };
  await assert.rejects(
    loadQueryChunks(
      new FakeBucket(objects),
      { ...loaded, attachedChunks: new Set() },
      oldEngine,
      "walkdir",
    ),
    /cannot score fetched title chunks/,
  );
});

test("a pinned manifest digest is checked before the manifest is parsed", async () => {
  const { objects, total } = shardSet();
  const digest = await sha256Hex(objects["index/manifest.txt"]);
  assert.equal(digest.length, 64);
  const loaded = await loadSparseIndex(
    new FakeBucket(objects),
    "index/manifest.txt",
    { expectManifestDigest: `sha256:${digest}` },
  );
  assert.equal(loaded.bytes.length, total);

  // A substituted manifest is refused before a single part is parsed.
  await assert.rejects(
    loadSparseIndex(new FakeBucket(objects), "index/manifest.txt", {
      expectManifestDigest: "0".repeat(64),
    }),
    /does not match the pinned digest/,
  );
  assert.equal(
    await sha256Hex(new TextEncoder().encode("abc")),
    "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
    "the WebCrypto digest must be plain SHA-256",
  );
});

test("a shard set published under a versioned prefix is read from that prefix", async () => {
  const { objects, total } = shardSet();
  const digest = await sha256Hex(objects["index/manifest.txt"]);
  // The same set, uploaded under `index/<digest>/` without regenerating the manifest:
  // its part keys still say `index/...`, and the loader re-bases them.
  const prefix = `index/${digest}/`;
  const versioned = {};
  for (const [key, bytes] of Object.entries(objects)) {
    versioned[prefix + key.slice("index/".length)] = bytes;
  }
  const bucket = new FakeBucket(versioned);
  const loaded = await loadSparseIndex(bucket, `${prefix}manifest.txt`, {
    expectManifestDigest: digest,
  });
  assert.equal(loaded.bytes.length, total);
  assert.ok(
    loaded.manifest.parts.every((part) => part.key.startsWith(prefix)),
    "every part is re-based on the manifest's directory",
  );
  assert.ok(bucket.requests.every((request) => request.key.startsWith(prefix)));
});

test("curation is read as bytes and pinned to its digest", async () => {
  const text = "install | pin /docs/install.html\n";
  const bytes = new TextEncoder().encode(text);
  const digest = await sha256Hex(bytes);
  const loaded = await loadCurations(
    new FakeBucket({ "index/curate.txt": bytes }),
    "index/curate.txt",
    { expectDigest: `sha256:${digest}` },
  );
  assert.deepEqual(loaded, { status: "loaded", text });

  // A substituted rule file never reaches the parser.
  const tampered = new FakeBucket({
    "index/curate.txt": new TextEncoder().encode(`${text}install | pin /evil.html\n`),
  });
  const mismatch = await loadCurations(tampered, "index/curate.txt", {
    expectDigest: digest,
  });
  assert.equal(mismatch.status, "digest-mismatch");
  assert.equal(mismatch.text, undefined);

  // Missing and oversized files are statuses, not exceptions: curation is optional.
  assert.deepEqual(await loadCurations(new FakeBucket({}), "index/curate.txt"), {
    status: "missing",
  });
  const oversized = new FakeBucket({ "index/curate.txt": bytesOf(MAX_CURATION_BYTES + 1) });
  const large = await loadCurations(oversized, "index/curate.txt");
  assert.equal(large.status, "too-large");
  assert.equal(large.bytes, MAX_CURATION_BYTES + 1);
});

test("a truncated fuzzy plan is reported on the response", async () => {
  const names = [
    "head",
    "chunks",
    "chunk00000",
    "docs",
    "strings",
    "texts",
    "hashes",
    "strhashes",
    "facets",
  ];
  const sizes = {
    head: 8,
    chunks: 12,
    chunk00000: 40,
    docs: 12,
    strings: 32,
    texts: 50,
    hashes: 16,
    strhashes: 16,
    facets: 20,
  };
  const { objects } = shardSet(sizes, names);
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");
  let planArgs = null;
  let truncated = true;
  const engine = {
    chunkCount: 1,
    chunksForQuery: () => [0],
    planTruncated: (...args) => {
      planArgs = args;
      return truncated;
    },
    attachTermChunk() {},
    searchJson: () =>
      JSON.stringify({ results: [{ docId: 0, url: "", title: "", snippet: "" }] }),
    searchJsonWithOptions: () =>
      JSON.stringify({ results: [{ docId: 0, url: "", title: "", snippet: "" }] }),
    stringSpan: () => [],
    textSpan: () => [0, 0],
  };
  const response = await searchWithSnippets(bucket, loaded, engine, "wlakdir", 5, {
    fuzzy: true,
  });
  assert.deepEqual(planArgs, ["wlakdir", true, true]);
  assert.equal(response.planTruncated, true);
  assert.equal(response.results.length, 1);

  // A plan that covers the dictionary is not reported at all: the shape of every
  // other response is unchanged.
  truncated = false;
  const whole = await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, {});
  assert.equal("planTruncated" in whole, false);
});

test("a split metadata run is fetched as bounded, independent objects", async () => {
  const names = [
    "meta00000",
    "meta00001",
    "meta00002",
    "docrecords",
    "chunk00000",
    "strings",
    "texts",
  ];
  const { objects, parts, total } = shardSet(
    {
      meta00000: 64,
      meta00001: 64,
      meta00002: 32,
      docrecords: 64,
      chunk00000: 24,
      strings: 32,
      texts: 50,
    },
    names,
  );
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");

  // Every metadata slice is part of the cold start, and they are independent
  // objects: the loader asks for all of them instead of looking for one `meta`.
  assert.deepEqual(
    bucket.requests.map((request) => request.key),
    [
      "index/manifest.txt",
      "index/meta00000.bin",
      "index/meta00001.bin",
      "index/meta00002.bin",
    ],
  );
  assert.equal(loaded.bytes.length, total);
  for (const part of parts.filter((candidate) => candidate.name.startsWith("meta"))) {
    assert.deepEqual(
      loaded.bytes.subarray(part.offset, part.offset + part.length),
      objects[part.key],
      part.name,
    );
  }
  // Only the metadata was loaded; the record pages and the term chunks are routed
  // to later, exactly as with a single `meta` object.
  assert.equal(loaded.parts.meta, undefined, "there is no aggregate meta part");
  assert.ok(loaded.skipped.docrecords);
  assert.ok(loaded.skipped.chunk00000);
  assert.equal(loaded.stats.requests, 4);

  // One corrupt slice rejects the whole cold start rather than loading a partial
  // index: the engine cannot route from half a metadata run.
  const tampered = new FakeBucket({
    ...objects,
    "index/meta00001.bin": bytesOf(64, 3),
  });
  await assert.rejects(
    loadSparseIndex(tampered, "index/manifest.txt"),
    /meta00001 does not match its hash/,
  );
});

test("a chunked set loads one meta object and pages the records per hit", async () => {
  const names = ["meta", "docrecords", "chunk00000", "strings", "texts"];
  const { objects } = shardSet(
    { meta: 64, docrecords: 64, chunk00000: 24, strings: 32, texts: 50 },
    names,
  );
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");
  assert.deepEqual(
    bucket.requests.map((request) => request.key),
    ["index/manifest.txt", "index/meta.bin"],
    "one metadata object instead of one per section",
  );
  assert.ok(loaded.parts.docrecords, "the record table is paged, not loaded");
  assert.ok(loaded.skipped.chunk00000, "the term chunks are routed to, not loaded");

  const pages = new Map();
  const engine = {
    chunkCount: 0,
    docPageCount: 2,
    docPageRecords: 8,
    docPageOf(docId) {
      return docId;
    },
    docPageSpan(page) {
      return [page * 32, 32];
    },
    attachDocPage(page, bytes) {
      pages.set(page, bytes);
    },
    searchJson(query, limit) {
      return JSON.stringify({ results: [{ docId: 0 }, { docId: 1 }].slice(0, limit) });
    },
    stringSpan() {
      return [0, 4, 4, 2];
    },
    attachStringBlock() {},
    textSpan() {
      return [0, 10];
    },
    attachTextBlock() {},
  };
  await searchWithSnippets(bucket, loaded, engine, "walkdir", 2);
  assert.deepEqual([...pages.keys()], [0, 1], "each hit's page is fetched");
  const recordReads = bucket.requests.filter((r) => r.key === "index/docrecords.bin");
  assert.deepEqual(
    recordReads,
    [
      { key: "index/docrecords.bin", range: { offset: 0, length: 32 } },
      { key: "index/docrecords.bin", range: { offset: 32, length: 32 } },
    ],
    "one verified range per page, not the whole table",
  );
  // The strings are read after the page that names their offsets.
  const order = bucket.requests.map((request) => request.key);
  assert.ok(
    order.indexOf("index/docrecords.bin") < order.indexOf("index/strings.bin"),
    `records before strings: ${order.join(" ")}`,
  );

  // A warm isolate keeps the pages and the strings: the next query reads neither.
  bucket.requests.length = 0;
  await searchWithSnippets(bucket, loaded, engine, "walkdir", 2);
  assert.equal(
    bucket.requests.filter((request) => request.key === "index/docrecords.bin").length,
    0,
  );
  assert.equal(await loadDocPages(bucket, loaded, engine, [0, 1]).then((p) => p.length), 0);
});

test("a grouped query reads every record page before it ranks", async () => {
  const { objects } = shardSet(
    { meta: 64, docrecords: 64, strings: 32, texts: 50 },
    ["meta", "docrecords", "strings", "texts"],
  );
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");
  const pages = [];
  const engine = {
    chunkCount: 0,
    docPageCount: 2,
    docPageRecords: 8,
    docPageOf: (docId) => docId,
    allDocPages: () => [0, 1],
    docPageSpan: (page) => [page * 32, 32],
    attachDocPage(page) {
      pages.push(page);
    },
    searchJson() {
      return JSON.stringify({ results: [] });
    },
    searchJsonWithOptions() {
      return JSON.stringify({ results: [] });
    },
  };
  await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, { group: true });
  assert.deepEqual(pages, [0, 1], "grouping needs the whole record table");
  const recordReads = bucket.requests.filter((request) => request.key === "index/docrecords.bin");
  assert.deepEqual(
    recordReads.map((request) => request.range),
    [
      { offset: 0, length: 32 },
      { offset: 32, length: 32 },
    ],
    "one bounded range per page, not the table",
  );
  assert.equal(await loadAllDocPages(bucket, loaded, engine).then((fetched) => fetched.length), 0);
});

test("an older engine without allDocPages enumerates pages from the count", async () => {
  const { objects } = shardSet(
    { meta: 64, docrecords: 96 },
    ["meta", "docrecords"],
  );
  const bucket = new FakeBucket(objects);
  const loaded = await loadSparseIndex(bucket, "index/manifest.txt");
  const pages = [];
  const engine = {
    docPageCount: 3,
    docPageSpan: (page) => [page * 32, 32],
    attachDocPage(page) {
      pages.push(page);
    },
  };
  const fetched = await loadAllDocPages(bucket, loaded, engine);
  assert.deepEqual(fetched, [0, 1, 2], "the count is enough without the getter");
  assert.deepEqual(pages, [0, 1, 2]);
});

function fakeEngine(attached, attachedStrings) {
  let searches = 0;
  return {
    get searches() {
      return searches;
    },
    searchJson(query, limit) {
      searches += 1;
      const hit = (docId, url) => ({
        docId,
        url: attachedStrings.has(docId) ? url : "",
        title: attachedStrings.has(docId) ? url.slice(1).toUpperCase() : "",
        snippet: attached.has(docId) ? `snippet-${url.slice(1)}` : "",
      });
      return JSON.stringify({ results: [hit(0, "/a"), hit(1, "/b")].slice(0, limit) });
    },
    textSpan(docId) {
      return [docId * 10, 10];
    },
    stringSpan(docId) {
      // The URL is 4 bytes and the title 2, written back to back.
      return [docId * 20, 4, docId * 20 + 4, 2];
    },
    attachTextBlock(docId, block) {
      assert.equal(block.length, 10);
      attached.set(docId, block);
    },
    attachStringBlock(docId, block) {
      assert.ok(block.length > 0);
      attachedStrings.add(docId);
    },
    attachStringHash(docId, bytes) {
      assert.equal(bytes.length, 8);
    },
    attachTextHash(docId, bytes) {
      assert.equal(bytes.length, 8);
    },
  };
}

function loadedSet(parts) {
  return {
    parts: {
      texts: parts.find((part) => part.name === "texts"),
      strings: parts.find((part) => part.name === "strings"),
    },
    attached: new Set(),
    attachedStrings: new Set(),
  };
}

test("a query reads the strings and block of each hit, once, and reranks", async () => {
  const { objects, parts } = shardSet();
  const bucket = new FakeBucket(objects);
  const attached = new Map();
  const attachedStrings = new Set();
  const engine = fakeEngine(attached, attachedStrings);
  const loaded = loadedSet(parts);

  const cold = await searchWithSnippets(bucket, loaded, engine, "walkdir", 10);
  assert.equal(cold.results[0].url, "/a");
  assert.equal(cold.results[0].snippet, "snippet-a");
  assert.equal(cold.results[1].url, "/b");
  assert.equal(cold.results[1].snippet, "snippet-b");
  assert.equal(engine.searches, 2, "the second pass renders urls and snippets");
  assert.deepEqual(bucket.requests, [
    { key: "index/strings.bin", range: { offset: 0, length: 6 } },
    { key: "index/texts.bin", range: { offset: 0, length: 10 } },
    { key: "index/strings.bin", range: { offset: 20, length: 6 } },
    { key: "index/texts.bin", range: { offset: 10, length: 10 } },
  ]);
  assert.deepEqual([...loaded.attached], [0, 1]);
  assert.deepEqual([...loaded.attachedStrings], [0, 1]);

  bucket.requests.length = 0;
  const warm = await searchWithSnippets(bucket, loaded, engine, "walkdir", 10);
  assert.equal(warm.results[0].snippet, "snippet-a");
  assert.equal(bucket.requests.length, 0, "a warm query fetches nothing");
});

test("a hit reads its two hash entries, not the tables", async () => {
  const { objects, parts } = shardSet();
  const bucket = new FakeBucket(objects);
  const attached = new Map();
  const attachedStrings = new Set();
  const engine = fakeEngine(attached, attachedStrings);
  const loaded = {
    parts: {
      texts: parts.find((part) => part.name === "texts"),
      strings: parts.find((part) => part.name === "strings"),
      hashes: parts.find((part) => part.name === "hashes"),
      strhashes: parts.find((part) => part.name === "strhashes"),
    },
    attached: new Set(),
    attachedStrings: new Set(),
    stats: { requests: 0, bytes: 0 },
  };

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 1);
  assert.deepEqual(bucket.requests, [
    { key: "index/strings.bin", range: { offset: 0, length: 6 } },
    { key: "index/strhashes.bin", range: { offset: 0, length: 8 } },
    { key: "index/texts.bin", range: { offset: 0, length: 10 } },
    { key: "index/hashes.bin", range: { offset: 0, length: 8 } },
  ]);
});

test("a block that cannot be read leaves the hit without a snippet", async () => {
  const { objects, parts } = shardSet();
  const bucket = new FakeBucket({ ...objects, "index/texts.bin": undefined });
  const attached = new Map();
  const attachedStrings = new Set();
  const engine = fakeEngine(attached, attachedStrings);
  const loaded = loadedSet(parts);

  const result = await searchWithSnippets(bucket, loaded, engine, "walkdir", 10);
  assert.equal(result.results.length, 2, "the hits are still returned");
  assert.equal(result.results[0].url, "/a", "the strings were attached");
  assert.equal(result.results[0].snippet, "");
  assert.equal(engine.searches, 2, "the strings were attached, so one rerank");
  assert.equal(loaded.attached.size, 0);
});

test("strings that cannot be read leave the hit without a URL", async () => {
  const { objects, parts } = shardSet();
  const bucket = new FakeBucket({ ...objects, "index/strings.bin": undefined });
  const attached = new Map();
  const attachedStrings = new Set();
  const engine = fakeEngine(attached, attachedStrings);
  const loaded = loadedSet(parts);

  const result = await searchWithSnippets(bucket, loaded, engine, "walkdir", 10);
  assert.equal(result.results[0].url, "");
  assert.equal(result.results[0].snippet, "snippet-a");
  assert.equal(loaded.attachedStrings.size, 0);
  assert.equal(loaded.attached.size, 2);
});

test("a non-adjacent record is fetched as two ranges and concatenated", async () => {
  const { objects, parts } = shardSet();
  const bucket = new FakeBucket(objects);
  const attached = new Map();
  const attachedStrings = new Set();
  const engine = fakeEngine(attached, attachedStrings);
  engine.stringSpan = () => [4, 3, 20, 5];
  const loaded = loadedSet(parts);

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 1);
  assert.deepEqual(bucket.requests.slice(0, 2), [
    { key: "index/strings.bin", range: { offset: 4, length: 3 } },
    { key: "index/strings.bin", range: { offset: 20, length: 5 } },
  ]);
  assert.equal(attachedStrings.size, 1);
});

test("the block cap bounds what a long-lived isolate keeps", async () => {
  const { objects, parts } = shardSet();
  const bucket = new FakeBucket(objects);
  const attached = new Map();
  const attachedStrings = new Set();
  const engine = fakeEngine(attached, attachedStrings);
  const loaded = loadedSet(parts);

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 10, { maxBlocks: 1 });
  assert.equal(loaded.attachedStrings.size, 1);
  assert.equal(loaded.attached.size, 1);
  assert.deepEqual(bucket.requests, [
    { key: "index/strings.bin", range: { offset: 0, length: 6 } },
    { key: "index/texts.bin", range: { offset: 0, length: 10 } },
  ]);
});

test("the query string's filters are parsed, capped and never guessed at", () => {
  assert.deepEqual(parseFilters(["section=guide", "lang=en"]), [
    { field: "section", value: "guide" },
    { field: "lang", value: "en" },
  ]);
  // Only the first `=` splits: a value may contain one.
  assert.deepEqual(parseFilters(["level=a=b"]), [{ field: "level", value: "a=b" }]);
  // Nothing to split on, an empty field or an empty value: dropped silently.
  assert.deepEqual(parseFilters(["guide", "=en", "lang="]), []);
  assert.equal(parseFilters(Array.from({ length: 20 }, () => "lang=en")).length, 8);
  assert.equal(parseFilters(Array.from({ length: 20 }, () => "lang=en"), { max: 3 }).length, 3);
});

test("filters are forwarded to the engine and echoed back", async () => {
  const { objects } = shardSet();
  const bucket = new FakeBucket(objects);
  const loaded = {
    parts: {},
    attached: new Set(),
    attachedStrings: new Set(),
    stats: { requests: 0, bytes: 0 },
  };
  const calls = [];
  const engine = {
    searchJson(query, limit) {
      calls.push(["exact", query, limit]);
      return JSON.stringify({ results: [] });
    },
    searchJsonWithOptions(query, limit, prefix, fuzzy, maxEdits, boost, filters) {
      calls.push(["options", query, limit, prefix, fuzzy, maxEdits, boost, filters]);
      return JSON.stringify({
        results: [],
        filters:
          filters === ""
            ? []
            : filters.split("\u001e").map((entry) => {
                const [field, value] = entry.split("\u001f");
                return { field, value };
              }),
      });
    },
  };

  const plain = await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, {
    filters: [],
  });
  assert.deepEqual(calls.at(-1), ["exact", "walkdir", 5]);
  assert.equal(plain.filters, undefined, "no filters, no echo");

  const filtered = await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, {
    filters: [{ field: "section", value: "guide" }, { field: "lang", value: "en" }],
  });
  assert.deepEqual(calls.at(-1), [
    "options",
    "walkdir",
    5,
    true,
    false,
    1,
    Number.NaN,
    "section\u001fguide\u001elang\u001fen",
  ]);
  assert.deepEqual(filtered.filters, [
    { field: "section", value: "guide" },
    { field: "lang", value: "en" },
  ]);
  // The facet section is metadata: filtering costs no request.
  assert.equal(bucket.requests.length, 0);

  const bare = {
    searchJson(query, limit) {
      calls.push(["exact", query, limit]);
      return JSON.stringify({ results: [] });
    },
  };
  await assert.rejects(
    searchWithSnippets(bucket, loaded, bare, "walkdir", 5, {
      filters: [{ field: "lang", value: "en" }],
    }),
    /cannot apply search options/,
    "a filter must never be dropped silently",
  );
});

test("a title boost is forwarded only when the caller sets one", async () => {
  const { objects } = shardSet();
  const bucket = new FakeBucket(objects);
  const loaded = {
    parts: {},
    attached: new Set(),
    attachedStrings: new Set(),
    stats: { requests: 0, bytes: 0 },
  };
  const calls = [];
  const engine = {
    searchJson(query, limit) {
      calls.push(["exact", query, limit]);
      return JSON.stringify({ results: [] });
    },
    searchJsonWithOptions(query, limit, prefix, fuzzy, maxEdits, boost, filters) {
      calls.push(["options", query, limit, prefix, fuzzy, maxEdits, boost, filters]);
      return JSON.stringify({ results: [] });
    },
  };

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 5);
  assert.deepEqual(calls.at(-1), ["exact", "walkdir", 5]);

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, { titleBoost: 0 });
  assert.deepEqual(calls.at(-1), ["options", "walkdir", 5, true, false, 1, 0, ""]);

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, { titleBoost: 7.5 });
  assert.deepEqual(calls.at(-1), ["options", "walkdir", 5, true, false, 1, 7.5, ""]);
});

test("fuzzy is forwarded to the engine only when asked for", async () => {
  const { objects } = shardSet();
  const bucket = new FakeBucket(objects);
  const loaded = {
    parts: {},
    attached: new Set(),
    attachedStrings: new Set(),
    stats: { requests: 0, bytes: 0 },
  };
  const calls = [];
  const engine = {
    searchJson(query, limit) {
      calls.push(["exact", query, limit]);
      return JSON.stringify({ results: [] });
    },
    searchJsonWithOptions(query, limit, prefix, fuzzy, maxEdits, boost, filters) {
      calls.push(["options", query, limit, prefix, fuzzy, maxEdits, boost, filters]);
      return JSON.stringify({ results: [] });
    },
  };

  await searchWithSnippets(bucket, loaded, engine, "walkdxr", 5);
  assert.deepEqual(calls.at(-1), ["exact", "walkdxr", 5]);

  await searchWithSnippets(bucket, loaded, engine, "walkdxr", 5, { fuzzy: true });
  assert.deepEqual(calls.at(-1), [
    "options",
    "walkdxr",
    5,
    true,
    true,
    1,
    Number.NaN,
    "",
  ]);
  await searchWithSnippets(bucket, loaded, engine, "walkdxr", 5, { fuzzy: true, maxEdits: 2 });
  assert.deepEqual(calls.at(-1), [
    "options",
    "walkdxr",
    5,
    true,
    true,
    2,
    Number.NaN,
    "",
  ]);
});

test("grouping is forwarded to the engine, and only when asked for", async () => {
  const { objects } = shardSet();
  const bucket = new FakeBucket(objects);
  const loaded = {
    parts: {},
    attached: new Set(),
    attachedStrings: new Set(),
    stats: { requests: 0, bytes: 0 },
  };
  const calls = [];
  const engine = {
    searchJson(query, limit) {
      calls.push(["exact", query, limit]);
      return JSON.stringify({ results: [] });
    },
    searchJsonWithOptions(query, limit, prefix, fuzzy, maxEdits, boost, filters, group) {
      calls.push(["options", group]);
      return JSON.stringify({ results: [] });
    },
  };

  // No group, no options call: a plain query keeps using `searchJson`.
  await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, {});
  assert.deepEqual(calls.at(-1), ["exact", "walkdir", 5]);

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, { group: true });
  assert.deepEqual(calls.at(-1), ["options", true]);

  await searchWithSnippets(bucket, loaded, engine, "walkdir", 5, { group: false });
  assert.deepEqual(calls.at(-1), ["exact", "walkdir", 5]);
});
