import test from "node:test";
import assert from "node:assert/strict";

import {
  attachHitBlocks,
  attachQueryChunks,
  fnv1a64Hex,
  loadAllDocPages,
  loadDocPages,
  loadShardIndex,
  parseManifest,
  readPartRange,
} from "../core/shards.js";

/* --------------------------------------------------------------- fixtures */

const HEAD = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]);
const TERMS = new Uint8Array([11, 12, 13, 14]);
const STRINGS = new TextEncoder().encode("https://example.com/a\0A title\0");
const TEXTS = new TextEncoder().encode("the body of the document, long enough to slice");
const TOTAL = HEAD.length + TERMS.length + STRINGS.length + TEXTS.length;

const bytesOf = (...parts) => {
  const out = new Uint8Array(TOTAL);
  let offset = 0;
  for (const part of parts) {
    out.set(part, offset);
    offset += part.length;
  }
  return out;
};

const ARTIFACT = bytesOf(HEAD, TERMS, STRINGS, TEXTS);

function manifestText({ textHash = fnv1a64Hex(TEXTS), stringsHash = fnv1a64Hex(STRINGS) } = {}) {
  const lines = [
    "format rsx-shard-1",
    `total ${TOTAL}`,
    "checksum 0123456789abcdef",
    `part head 0 ${HEAD.length} ${fnv1a64Hex(HEAD)} index/head.bin`,
    `part terms ${HEAD.length} ${TERMS.length} ${fnv1a64Hex(TERMS)} index/terms.bin`,
    `part strings ${HEAD.length + TERMS.length} ${STRINGS.length} ${stringsHash} index/strings.bin`,
    `part texts ${HEAD.length + TERMS.length + STRINGS.length} ${TEXTS.length} ${textHash} index/texts.bin`,
  ];
  return `${lines.join("\n")}\n`;
}

/** A server that honours `Range`, plus the requests it saw. */
function rangeServer(objects, { ignoreRange = false } = {}) {
  const requests = [];
  const fetch = async (url, options = {}) => {
    const body = objects[url];
    requests.push({ url, range: options.headers?.Range ?? null });
    if (!body) return { ok: false, status: 404, statusText: "Not Found", arrayBuffer: async () => new ArrayBuffer(0) };
    const header = ignoreRange ? null : options.headers?.Range;
    if (!header) {
      return { ok: true, status: 200, text: async () => new TextDecoder().decode(body), arrayBuffer: async () => body.slice().buffer };
    }
    const [, start, end] = /^bytes=(\d+)-(\d+)$/.exec(header).map(Number);
    const slice = body.slice(start, end + 1);
    return { ok: true, status: 206, arrayBuffer: async () => slice.buffer };
  };
  return { fetch, requests };
}

const BASE = "http://127.0.0.1:8765/demo/shards/";

function objects() {
  return {
    [`${BASE}manifest.txt`]: new TextEncoder().encode(manifestText()),
    [`${BASE}head.bin`]: HEAD,
    [`${BASE}terms.bin`]: TERMS,
    [`${BASE}strings.bin`]: STRINGS,
    [`${BASE}texts.bin`]: TEXTS,
  };
}

function withFetch(fetcher, body) {
  const original = globalThis.fetch;
  globalThis.fetch = fetcher;
  return Promise.resolve()
    .then(body)
    .finally(() => {
      globalThis.fetch = original;
    });
}

/* ------------------------------------------------------------------ tests */

test("the manifest is validated before anything is fetched", () => {
  const manifest = parseManifest(manifestText());
  assert.equal(manifest.format, "rsx-shard-1");
  assert.equal(manifest.total, TOTAL);
  assert.deepEqual(manifest.parts.map((part) => part.name), ["head", "terms", "strings", "texts"]);

  assert.throws(() => parseManifest("format rsx-shard-2\ntotal 8\nchecksum 00\n"), /unsupported/);
  assert.throws(() => parseManifest("format rsx-shard-1\ntotal 8\n"), /checksum/);
  assert.throws(() => parseManifest(`format rsx-shard-1\ntotal ${TOTAL}\nchecksum 00\npart head 2 6 x index/head.bin\n`), /does not start at 0/);
  assert.throws(() => parseManifest(`format rsx-shard-1\ntotal 999\nchecksum 00\npart head 0 ${HEAD.length} x index/head.bin\n`), /of 999 bytes/);
  assert.throws(() => parseManifest("format rsx-shard-1\ntotal 4\nchecksum 00\n"), /no parts/);
});

test("FNV-1a/64 matches the published vectors", () => {
  assert.equal(fnv1a64Hex(new Uint8Array()), "cbf29ce484222325");
  assert.equal(fnv1a64Hex(new TextEncoder().encode("a")), "af63dc4c8601ec8c");
  assert.equal(fnv1a64Hex(new TextEncoder().encode("foobar")), "85944171f73967e8");
});

test("loading shards fetches the metadata and leaves the texts alone", async () => {
  const { fetch, requests } = rangeServer(objects());
  await withFetch(fetch, async () => {
    const loaded = await loadShardIndex(`${BASE}manifest.txt`);
    assert.deepEqual(loaded.bytes.slice(0, HEAD.length + TERMS.length), ARTIFACT.slice(0, HEAD.length + TERMS.length));
    // The lazy sections stay zero until a hit asks for them.
    assert.ok(loaded.bytes.slice(HEAD.length + TERMS.length).every((value) => value === 0));
    assert.deepEqual(Object.keys(loaded.parts).sort(), ["head", "strings", "terms", "texts"]);
    assert.deepEqual(Object.keys(loaded.skipped).sort(), ["strings", "texts"]);
    assert.equal(loaded.parts.texts.url, `${BASE}texts.bin`);
    // The manifest plus the two metadata parts; the two text sections are not read.
    assert.equal(requests.filter((entry) => entry.url.endsWith("texts.bin")).length, 0);
    assert.equal(requests.filter((entry) => entry.url.endsWith("strings.bin")).length, 0);
    assert.equal(loaded.stats.requests, 3);
  });
});

test("metadata slices are verified with the provided hash function", async () => {
  const { fetch } = rangeServer(objects());
  await withFetch(fetch, async () => {
    let calls = 0;
    const loaded = await loadShardIndex(`${BASE}manifest.txt`, {
      hash: (bytes) => {
        calls += 1;
        return fnv1a64Hex(bytes);
      },
    });
    assert.equal(calls, 2, "head and terms only; the lazy sections are not hashed");
    assert.equal(loaded.stats.requests, 3);
    await assert.rejects(
      loadShardIndex(`${BASE}manifest.txt`, { hash: () => "0".repeat(16) }),
      /does not match its hash/,
    );
  });
});

test("a part that does not match its hash is refused", async () => {
  const corrupt = objects();
  corrupt[`${BASE}head.bin`] = new Uint8Array([9, 9, 9, 9, 9, 9, 9, 9]);
  const { fetch } = rangeServer(corrupt);
  await withFetch(fetch, async () => {
    await assert.rejects(loadShardIndex(`${BASE}manifest.txt`), /head does not match its hash/);
  });
});

test("a server that ignores Range still yields the right bytes, and says what it cost", async () => {
  const { fetch, requests } = rangeServer(objects(), { ignoreRange: true });
  await withFetch(fetch, async () => {
    const stats = { requests: 0, bytes: 0 };
    const slice = await readPartRange(`${BASE}texts.bin`, 4, 10, stats);
    assert.deepEqual(slice, TEXTS.slice(4, 14));
    assert.equal(requests[0].range, "bytes=4-13");
    // The whole object arrived: the counter reports the transfer, not the slice.
    assert.equal(stats.bytes, TEXTS.length);
  });
});

test("a short read is an error, not a silent gap", async () => {
  const { fetch } = rangeServer(objects());
  await withFetch(fetch, async () => {
    await assert.rejects(readPartRange(`${BASE}texts.bin`, 0, 9_999, { requests: 0, bytes: 0 }), /short range/);
  });
});

/* ------------------------------------------------------------- attachments */

function fakeEngine() {
  return {
    chunkCount: 2,
    chunksForQuery: (query) => (query.startsWith("pos") ? [1] : [0]),
    attachTermChunk: (chunk, data) => {
      fakeEngine.attached.set(chunk, data.length);
    },
    stringSpan: () => [0, 22, 22, 8],
    textSpan: () => [0, 12],
    attachStringBlock: (docId, block) => {
      fakeEngine.strings.set(docId, block.length);
    },
    attachTextBlock: (docId, block) => {
      fakeEngine.texts.set(docId, block.length);
    },
    docPageCount: 0,
  };
}
fakeEngine.attached = new Map();
fakeEngine.strings = new Map();
fakeEngine.texts = new Map();

test("only the term chunks the query needs are fetched, once", async () => {
  fakeEngine.attached = new Map();
  const CHUNK = new Uint8Array([7, 7, 7]);
  const data = objects();
  data[`${BASE}chunk00001.bin`] = CHUNK;
  const manifest = [
    "format rsx-shard-1",
    `total ${HEAD.length + CHUNK.length}`,
    "checksum 0123456789abcdef",
    `part head 0 ${HEAD.length} ${fnv1a64Hex(HEAD)} index/head.bin`,
    `part chunk00001 ${HEAD.length} ${CHUNK.length} ${fnv1a64Hex(CHUNK)} index/chunk00001.bin`,
  ].join("\n");
  data[`${BASE}manifest.txt`] = new TextEncoder().encode(manifest);
  const { fetch, requests } = rangeServer(data);
  await withFetch(fetch, async () => {
    const loaded = await loadShardIndex(`${BASE}manifest.txt`);
    const engine = fakeEngine();
    assert.equal(await attachQueryChunks(loaded, engine, "postgres"), 1);
    assert.equal(fakeEngine.attached.get(1), 3);
    const before = requests.length;
    // The chunk is already attached: a second query pays nothing.
    assert.equal(await attachQueryChunks(loaded, engine, "postgres"), 0);
    assert.equal(requests.length, before);
  });
});

test("a metadata run split into meta slices is fetched whole", async () => {
  const META_A = new Uint8Array([1, 1, 1, 1]);
  const META_B = new Uint8Array([2, 2, 2]);
  const DOCRECORDS = new Uint8Array(64);
  const total = META_A.length + META_B.length + DOCRECORDS.length;
  const manifest = [
    "format rsx-shard-1",
    `total ${total}`,
    "checksum 0123456789abcdef",
    `part meta00000 0 ${META_A.length} ${fnv1a64Hex(META_A)} index/meta00000.bin`,
    `part meta00001 ${META_A.length} ${META_B.length} ${fnv1a64Hex(META_B)} index/meta00001.bin`,
    `part docrecords ${META_A.length + META_B.length} ${DOCRECORDS.length} ${fnv1a64Hex(DOCRECORDS)} index/docrecords.bin`,
  ].join("\n");
  const data = {
    [`${BASE}manifest.txt`]: new TextEncoder().encode(manifest),
    [`${BASE}meta00000.bin`]: META_A,
    [`${BASE}meta00001.bin`]: META_B,
    [`${BASE}docrecords.bin`]: DOCRECORDS,
  };
  const { fetch, requests } = rangeServer(data);
  await withFetch(fetch, async () => {
    const loaded = await loadShardIndex(`${BASE}manifest.txt`);
    // Both slices are metadata: the index cannot load without either one.
    assert.deepEqual(
      requests.map((entry) => entry.url.split("/").pop()).sort(),
      ["manifest.txt", "meta00000.bin", "meta00001.bin"],
    );
    assert.deepEqual(loaded.bytes.slice(0, META_A.length + META_B.length), new Uint8Array([...META_A, ...META_B]));
    assert.equal(loaded.bytes.slice(META_A.length + META_B.length).every((value) => value === 0), true);
    assert.deepEqual(Object.keys(loaded.skipped), ["docrecords"]);
  });
});

test("meta slices are fetched in parallel, bounded by metaConcurrency", async () => {
  const COUNT = 6;
  const SIZE = 8;
  const bodies = Array.from({ length: COUNT }, (_, part) => new Uint8Array(SIZE).fill(part + 1));
  const total = COUNT * SIZE;
  const lines = ["format rsx-shard-1", `total ${total}`, "checksum 0123456789abcdef"];
  for (const [part, body] of bodies.entries()) {
    const name = String(part).padStart(5, "0");
    lines.push(`part meta${name} ${part * SIZE} ${SIZE} ${fnv1a64Hex(body)} index/meta${name}.bin`);
  }
  const manifest = lines.join("\n");
  const data = { [`${BASE}manifest.txt`]: new TextEncoder().encode(manifest) };
  for (const [part, body] of bodies.entries()) {
    data[`${BASE}meta${String(part).padStart(5, "0")}.bin`] = body;
  }

  let inFlight = 0;
  let max = 0;
  const fetcher = async (url, init = {}) => {
    const name = url.split("/").pop();
    if (name === "manifest.txt") {
      return { ok: true, status: 200, text: async () => manifest, arrayBuffer: async () => new ArrayBuffer(0) };
    }
    const body = data[url];
    assert.ok(body, url);
    inFlight += 1;
    max = Math.max(max, inFlight);
    await new Promise((resolve) => setTimeout(resolve, 5));
    const [, start, end] = /^bytes=(\d+)-(\d+)$/.exec(init.headers.Range).map(Number);
    const slice = body.slice(start, end + 1);
    inFlight -= 1;
    return { ok: true, status: 206, arrayBuffer: async () => slice.buffer };
  };

  await withFetch(fetcher, async () => {
    const loaded = await loadShardIndex(`${BASE}manifest.txt`, { metaConcurrency: 2 });
    assert.equal(max, 2, "two slices in flight, never more");
    assert.equal(loaded.stats.requests, COUNT + 1);
    const joined = new Uint8Array(total);
    for (const [part, body] of bodies.entries()) joined.set(body, part * SIZE);
    assert.deepEqual(loaded.bytes, joined, "all slices land at their offsets");
  });
  max = 0;
  await withFetch(fetcher, async () => {
    await loadShardIndex(`${BASE}manifest.txt`, { metaConcurrency: 1 });
    assert.equal(max, 1, "a limit of one is sequential");
  });
});

test("record pages are read per hit, and every page for a grouped query", async () => {
  const META = new Uint8Array([1, 2, 3]);
  const DOCRECORDS = new Uint8Array(64);
  const manifest = [
    "format rsx-shard-1",
    `total ${META.length + DOCRECORDS.length}`,
    "checksum 0123456789abcdef",
    `part meta 0 ${META.length} ${fnv1a64Hex(META)} index/meta.bin`,
    `part docrecords ${META.length} ${DOCRECORDS.length} ${fnv1a64Hex(DOCRECORDS)} index/docrecords.bin`,
  ].join("\n");
  const data = {
    [`${BASE}manifest.txt`]: new TextEncoder().encode(manifest),
    [`${BASE}meta.bin`]: META,
    [`${BASE}docrecords.bin`]: DOCRECORDS,
  };
  const pages = new Map();
  const engine = {
    chunkCount: 0,
    docPageCount: 2,
    docPageOf: (docId) => docId % 2,
    allDocPages: () => [0, 1],
    docPageSpan: (page) => [page * 32, 32],
    attachDocPage: (page, bytes) => pages.set(page, bytes.length),
  };
  const { fetch, requests } = rangeServer(data);
  await withFetch(fetch, async () => {
    const loaded = await loadShardIndex(`${BASE}manifest.txt`);
    assert.equal(await loadDocPages(loaded, engine, [2, 3]), 2);
    assert.deepEqual([...pages.entries()], [[0, 32], [1, 32]]);
    const reads = requests.filter((entry) => entry.url.endsWith("docrecords.bin"));
    assert.deepEqual(
      reads.map((entry) => entry.range),
      ["bytes=0-31", "bytes=32-63"],
    );

    // A grouped query asks for all of them before ranking; the two already here
    // cost nothing, so a second grouped query fetches nothing.
    pages.clear();
    const before = requests.length;
    assert.equal(await loadAllDocPages(loaded, engine), 0);
    assert.equal(requests.length, before);

    const fresh = await loadShardIndex(`${BASE}manifest.txt`);
    assert.equal(await loadAllDocPages(fresh, engine), 2);
    assert.deepEqual([...pages.keys()], [0, 1]);
  });
});

test("an older engine without allDocPages still fetches every record page", async () => {
  const META = new Uint8Array([1, 2, 3]);
  const DOCRECORDS = new Uint8Array(96);
  const manifest = [
    "format rsx-shard-1",
    `total ${META.length + DOCRECORDS.length}`,
    "checksum 0123456789abcdef",
    `part meta 0 ${META.length} ${fnv1a64Hex(META)} index/meta.bin`,
    `part docrecords ${META.length} ${DOCRECORDS.length} ${fnv1a64Hex(DOCRECORDS)} index/docrecords.bin`,
  ].join("\n");
  const data = {
    [`${BASE}manifest.txt`]: new TextEncoder().encode(manifest),
    [`${BASE}meta.bin`]: META,
    [`${BASE}docrecords.bin`]: DOCRECORDS,
  };
  const pages = [];
  // Deliberately no `allDocPages`: the v5 getters arrived after the first module
  // that could read a chunked artifact, and the count is enough to find the pages.
  const engine = {
    docPageCount: 3,
    docPageSpan: (page) => [page * 32, 32],
    attachDocPage: (page) => pages.push(page),
  };
  const { fetch } = rangeServer(data);
  await withFetch(fetch, async () => {
    const loaded = await loadShardIndex(`${BASE}manifest.txt`);
    assert.equal(await loadAllDocPages(loaded, engine), 3);
    assert.deepEqual(pages, [0, 1, 2]);
  });
});

test("per-hit strings and texts are range reads, cached per document", async () => {
  fakeEngine.strings = new Map();
  fakeEngine.texts = new Map();
  const { fetch, requests } = rangeServer(objects());
  await withFetch(fetch, async () => {
    const loaded = await loadShardIndex(`${BASE}manifest.txt`);
    const engine = fakeEngine();
    const hits = [{ docId: 3 }, { docId: 4 }];
    assert.equal(await attachHitBlocks(loaded, engine, hits), 4);
    assert.deepEqual([...fakeEngine.strings.keys()], [3, 4]);
    assert.deepEqual([...fakeEngine.texts.keys()], [3, 4]);
    assert.equal(fakeEngine.strings.get(3), 22 + 8);
    const stringReads = requests.filter((entry) => entry.url.endsWith("strings.bin"));
    assert.equal(stringReads.length, 2);
    assert.equal(stringReads[0].range, "bytes=0-29");

    const before = requests.length;
    assert.equal(await attachHitBlocks(loaded, engine, hits), 0);
    assert.equal(requests.length, before);
  });
});
