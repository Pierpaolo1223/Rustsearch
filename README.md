# rustsearch

A static-site full-text search engine in Rust. It builds one immutable index at
deploy time and answers BM25 queries from that artifact in the browser, in a
Cloudflare Worker, or on the command line. No server, no database, no runtime
indexing.

The design targets three constraints at once:

* **fast** — a query is a dictionary lookup, a postings walk and a top-k sort; it
  does not parse HTML or touch the network;
* **small on the wire** — the runtime is `no_std` + `alloc` (plus the two vendored
  Snowball stemmers); the current WASM module is about 118 KiB gzipped (121,092 B in
  the 2026-11-09 measurement), and large indexes can be fetched as query-sized shards;
* **portable** — the same `rustsearch-core` runs native and as `wasm32`; the
  Cloudflare adapter is a thin R2 + `Engine` wrapper.

## Quick start

```bash
cargo build --release
cargo run --release -p rustsearch-cli -- build public -o dist/index.rsx \
  --base-url https://example.com --stemmer en

cargo run --release -p rustsearch-cli -- info dist/index.rsx
cargo run --release -p rustsearch-cli -- search dist/index.rsx "deploy preview"
cargo run --release -p rustsearch-cli -- search dist/index.rsx "deploy" --json
cargo run --release -p rustsearch-cli -- search dist/index.rsx "deploy" --filter section=guide
cargo run --release -p rustsearch-cli -- search dist/index.rsx "deploy" --title-boost 4
cargo run --release -p rustsearch-cli -- search dist/index.rsx "deploy" --no-typos
# Phrase queries require an index built with positions enabled.
cargo run --release -p rustsearch-cli -- build public -o dist/phrases.rsx --positions
cargo run --release -p rustsearch-cli -- search dist/phrases.rsx '"static site"'

# Chunk the dictionary and record table for selective Range reads (format v6).
cargo run --release -p rustsearch-cli -- build public -o dist/index.rsx \
  --stemmer en --term-chunk-bytes 64k
cargo run --release -p rustsearch-cli -- shard dist/index.rsx -o dist/index \
  --prefix index/ --plan "deploy"

# The identity a deploy pins: build and shard write a SHA-256 sidecar, and
# `verify` refuses anything else (add --require-signature/--public-key for a
# detached minisign signature).
cargo run --release -p rustsearch-cli -- verify dist/index.rsx
cargo run --release -p rustsearch-cli -- verify dist/index/manifest.txt \
  --expect-digest sha256:9f2c…
```

The fixture in `fixtures/site/` is a three-page site used by the tests:

```bash
cargo run -p rustsearch-cli -- build fixtures/site -o /tmp/site.rsx
cargo run -p rustsearch-cli -- search /tmp/site.rsx "immutable index"
```

Search output:

```
1. 1.2335  /  Rustsearch fixture
   Static search, without a server Rustsearch builds an immutable index ...
```

`--json` returns the same contract as the WASM module:

```json
{"results":[{"docId":0,"score":0.5992,"url":"/guide/install.html",
  "title":"Install rustsearch","snippet":"...","highlights":[[159,166]],"matches":1}]}
```

`highlights` are byte ranges **inside `snippet`**. They are data, not markup.
`matches` is how many documents the hit stands for: `1` unless the query grouped a
split page (see "Pages and chunks").

## How it works

```text
static site ──walkdir──> HTML pages ──scraper──> main text, title, URL
                                           │
                                           ▼
                     analyze (lowercase, stopwords, Snowball stem)
                                           │
                                           ▼
              body postings + title postings + doc lengths + BM25 stats
                                           │
                                           ▼
    immutable .rsx (header, terms, block postings, docs, strings, LZ4 text blocks,
                    per-document text hashes, per-document string hashes,
                    optional facet and title-postings sections)
                         │                                   │
                    fetch from R2                     read from disk
                         │                                   │
                         ▼                                   ▼
                    WASM Engine ───────────────────────> rustsearch-core
                         │
                         ▼
                 JSON hits + plain-text snippets
```

* **Build time** (`rustsearch-index`): `walkdir` visits the site in sorted order,
  `scraper` selects `main` / `article` / `[role=main]` / `#content` / `.content` /
  `body`, a DOM walk skips `nav`, `script`, `style`, `footer`, `aside`, `form`,
  `svg` and `template`, and the remaining text is tokenized, stemmed and posted. The
  reading, extraction and analysis of the pages run on one worker per core, each with
  its own term cache; the results are merged back in page order, so the artifact does
  not depend on the number of workers and is byte-identical to a single-threaded
  build's.
* **Query time** (`rustsearch-core`): analyze the query the same way, find exact
  terms, expand prefixes (2+ characters, capped), optionally run a bounded
  edit-distance pass, score with BM25F (`k1 = 1.2`, `b = 0.75`) over the body and
  the title as two fields, sort, and build a 320-byte snippet per hit.
* **Transport**: one artifact, or a manifest plus one object per section when the
  text dominates (`rustsearch shard`). A flat v4 artifact stores the dictionary in
  offset-addressed sections; a chunked v6 artifact replaces them with a term-chunk
  directory and independently fetchable chunks. The reader also accepts the earlier
  chunked v5 layout.

The format is documented in [`DESIGN.md`](DESIGN.md) and
[`core/src/format.rs`](core/src/format.rs). A normal build writes v7 (v8 when
`--term-chunk-bytes` is set): the v4/v6 layout — flat or chunked dictionary, paged records
and compact BM25 lengths when they fit — with the checksum covering the header as well, so a
flipped flags word or a shifted span is refused instead of answered. `--no-header-checksum`
writes the v4/v6 bytes a client deployed before this build already reads. The reader accepts
v3, v4, v5, v6, v7 and v8; v5 is the earlier chunked layout, retained for compatibility. `--positions` adds token positions to posting lists without changing
the format version; it is opt-in and enables exact phrase queries. A v3 artifact has
no facets or title postings. Its per-document hashes still allow a client to
range-read and verify a hit's strings and text.

## Stemming and stopwords

`--stemmer none|en|it` selects the analyzer at build time and stores it in the
artifact, so a query is always normalized with the same tables as the pages:

```bash
rustsearch build public -o dist/index.rsx --stemmer en
rustsearch search dist/index.rsx "indexes"   # also finds "index" and "indexing"
```

* `none` (the default) is the v1 behaviour: Unicode lowercase + alphanumeric runs,
  no stopword list, no stemming. Right for a mixed-language site.
* `en` and `it` are Snowball stemmers vendored from `rust-stemmers` 1.2
  (MIT/BSD-3-Clause) and adapted to `no_std` + `alloc`, plus a stopword list for
  each language. Stopwords are dropped at build time, so they cost no postings and
  no dictionary entries, and a query made only of stopwords returns nothing.

On the 10,000-document corpus the English analyzer takes the dictionary from
29,667 to 22,602 terms and the token count from 1,547,496 to 1,089,612 (stopwords),
and the v4 artifact from 11,681,731 to 11,117,082 bytes — 564,649 of them the
postings and dictionary entries the stopwords and the stemmer remove.

Stemming changes snippets too: a hit for `index` marks the whole word in the page,
so `indexes` and `indexing` are highlighted, not just the stem that matched.

## Facets and filters

A facet is a taxonomy the **build** records — `lang`, `section`, `tags` — so a query
can filter on it without the client ranking first and hiding hits afterwards.The engine knows the values; a filter only picks among them. Facets are optional:
a build with no page metadata has no facet table, and filtering that artifact is
reported as unsupported rather than silently ignored.

```sh
rustsearch build public -o dist/index.rsx
rustsearch info dist/index.rsx                      # what the build recorded
rustsearch search dist/index.rsx "deploy" --filter section=guide --filter lang=en
```

The three fields come from the page itself:

| field | source |
|---|---|
| `lang` | `<html lang>` (lowercased) |
| `section` | `<meta name="section">`, else the first directory of the page path |
| `tags` | `<meta name="keywords">`, split on commas |

Values are opaque bytes: a quote, `<` or a newline survives a round trip unchanged,
and the renderer sets them with `textContent`. A page has no value for a field it
does not carry (a flat page has no `section`); a field with more than 512 values keeps
the most populous ones, and a value longer than 64 bytes is dropped whole rather than
truncated.

Filters are AND-ed across fields and OR-ed inside one (`--filter lang=en --filter
lang=it` matches either), resolved to a sorted candidate list **before** BM25 runs, so
`limit` applies to filtered results and pagination stays meaningful. An unknown field
or value returns `{"results":[]}` with status 200; a filter on an artifact with no
facet section is an error, not a silently unfiltered list. The caps — 8 fields, 8
values each, 512 bytes — are errors too. The response echoes what was applied:

```json
{"results":[…],"filters":[{"field":"lang","value":"en"}]}
```

Cost is opt-in. On the 10k corpus a metadata-free artifact grows by the section header
and nothing else; `lang` plus a `section` per tree costs **20,476 bytes (+0.2%)**,
and a site that also records `tags` pays for those values too. A filtered query is
**cheaper** than the same unfiltered one, not more expensive: the candidates are
picked before anything is scored, so the ranking pass is 17–22% shorter on a
selective filter (`--filter section=crewAI`: 0.096 ms against 0.121 ms before the
score buffer was indexed on the candidates), 1.155 ms against 1.298 ms end to end
(500 warm runs through WASM, p50), and a filter that matches nothing returns in 4 µs
instead of zeroing a corpus-sized score array. The bytes on the wire are the same:
the facet table is one of the metadata parts a query already fetches. This is format
v4; the reader still opens a v3 artifact, which simply has no facets.

## Title boosting

A page's title is scored as a second field with its own document frequency, its own
length normalization and a configurable weight, so a term in the title outranks the
same term buried in the prose:

```sh
rustsearch build public -o dist/index.rsx
rustsearch search dist/index.rsx "install"                  # boost 2 (the default)
rustsearch search dist/index.rsx "install" --title-boost 5  # trust the title more
rustsearch search dist/index.rsx "install" --title-boost 0  # body only, as before v4
```

`--title-boost` takes 0–10 and defaults to 2. The same weight is a query knob in the
WASM module (`searchJsonWithOptions(query, limit, prefix, fuzzy, maxEdits, titleBoost,
filters)`), in the Worker (`GET /search?q=…&boost=5`) and in
`rustsearch bench --title-boost N`, so one artifact serves both readings. `0`
ignores the title field entirely, which is what a build that never indexed titles
gives — the scores are then the v3 ones.

What it costs: 168,742 bytes of title postings and 4 bytes per document record on
the 10k corpus (+2.1% with the facets, +1.9% without), and no measurable query time
— 500-query p50 is 1.278–1.298 ms with the boost at 2 and 1.32 ms with it at 0 over
four runs, a spread that overlaps, because no query term appears in a title in most
queries. What it buys is the ranking: top-5 agreement with pagefind went from
21% to 37% and with ZBSearch from 25% to 44% — see [`COMPARISON.md`](COMPARISON.md).

## Term chunks

A query needs the postings of a handful of terms and nothing else in the dictionary,
but the dictionary and the postings are one object each: a cold query on the 10k
corpus fetches 1,547,052 bytes of them to read five terms. `--term-chunk-bytes N`
groups the dictionary into chunks of whole terms around the `N`-byte budget (a chunk
may exceed it by one posting list) and writes format v6: a `chunks` directory plus the chunks, which `rustsearch shard` publishes as
one object per chunk. A client then asks the engine which chunks its query's terms
live in (`chunksForQuery`), fetches those, attaches them against their FNV hash, and
ranks.

```sh
rustsearch build public -o dist/index.rsx --stemmer en --term-chunk-bytes 64k
rustsearch shard dist/index.rsx -o dist/index --prefix index/ --plan "deploy" -n 5
```

The budget is a byte budget, not a term count: 64 terms of `the` and 64 terms of
`zygote` are very different sizes. `64k` is the budget to start with.

v6 chunks everything that would otherwise be one big object per query:

* the **dictionary and its postings**, one chunk per byte budget;
* the **title lists**, one `titleNNNNN` object per chunk that has one, read together
  with the chunk a query fetched, so a cold start carries none of them and a chunk
  with no title costs nothing;
* the **document records**, split into `doctokens` (the BM25 lengths, enough to
  rank: two `u16`s per document when all lengths fit, otherwise two `u32`s) and
  32-byte records paged at the same budget, so a hit costs one page read instead of
  the whole table at 100k;
* the **metadata**, written as one contiguous run so `rustsearch shard` can publish it
  as `meta00000.bin`, `meta00001.bin`, … — consecutive slices of at most `N` with
  `--meta-part-bytes N`, `0` for one slice per range — still addressed by the same
  manifest (see §"Sharding"). Every slice is fetched at cold start, so the budget
  bounds each object and leaves the bytes unchanged: it is a cache-object limit, not
  a way to skip metadata. The two hash tables and the title lists are published
  separately and skipped at cold start, so the run is the header, the directory, the
  lengths, the facets and the page table: three requests at 10k and at 100k on the
  seeded corpora, about twenty at a million documents.

Measured on a 5-hit query, every number a cold one: 10k corpus **2,429,200 →
873,361 bytes** (19 → 17.4 requests); at 100k the current shard set reads **1,013,335
bytes in 29 requests**, against 16,020,017 bytes in 19 requests for the flat v4 layout
(the 2026-10-30 re-run, `COMPARISON.md`); the cold start, before any query can rank,
drops from nine metadata requests to **three** at 10k and at 100k, and neither the
hash tables nor the title lists are part of it: a hit reads two 8-byte entries instead
and a title-scored query reads the title list of the
chunks it fetched (at 100k the cold start falls from 1,701,706 to 623,984 bytes).
The flat dictionary (9,133,481 bytes in two objects at 100k) is the eight-slice
problem Cloudflare's 1 MiB cache granularity makes.

What it costs: 0.01–0.02% of artifact at 64 KiB, one `part` line per chunk in the
manifest (fetched on every cold start), 8–12 µs of extra WASM ranking p50 (+1.5–4%)
from one directory search and one indirection per selected term, and a record page per
hit that the flat client read with the metadata — the page is bounded by the same
budget and kept in the isolate, which is why the request count barely moves. An
explicit **fuzzy** query can walk the whole sorted dictionary; the Worker caps it at
64 chunks and reports `planTruncated` when incomplete. Automatic typo correction is
available on flat native/CLI indexes, not on the sparse chunked Worker path. A
**grouped** query (`group=1`) needs every record page, because grouping runs before
`limit` and page identity lives in the record. The full comparison, including the 8 KiB, 256 KiB and 1 MiB budgets, and the same
artifact at 100,000 documents
against pagefind and ZBSearch — build memory, output size, cold start and warm
latency — is [`COMPARISON.md`](COMPARISON.md) §"At 100,000 documents".

## Pages and chunks

Not to be confused with the term chunks above: a long page is indexed at paragraph
granularity and presented at page granularity:

```sh
rustsearch build public -o dist/index.rsx --max-page-chars 850   # long pages become several documents
rustsearch search dist/index.rsx "deploy" --group                # ... and come back as one hit each
```

`--max-page-chars` splits a page longer than that many bytes on a word boundary; the
documents share its URL, its title and its facets, and the builder writes that
string pair once, so the chunks also share one `url_off`. `--group` (default off)
collapses them after the ranking and before `limit`: the best chunk names the page,
the others only raise its `matches`, so `limit` counts pages and no window of
pagination is eaten by four paragraphs of the same page. The JSON gains `matches` on
every hit (`1` when grouping is off), nothing else changes, and the page identity is
in the metadata, so a shard client groups without fetching a string or a text block.

On a 990-page corpus split into 2,960 documents, grouping is what the top-5
comparison with pagefind and ZBSearch needs: five chunks of one page are not five
pages. See [`COMPARISON.md`](COMPARISON.md) §"Page-level agreement" for the numbers
and for what grouping costs (one doc-record read per matching document).

## Performance

Measured on this machine (Linux x86_64, release build), with the reproducible
benchmark (2,000 synthetic pages × 120 words, 200 queries):

| metric | v4 (this build) | v3 | v2 (two formats back) |
|---|---:|---:|---:|
| index (LZ4 text blocks + both hash tables) | 1,804,012 B | **1,755,544 B** | 2,010,609 B |
| build | 119 ms | 150 ms | 154 ms |
| build peak RSS | 21,208 KiB | 28,460 KiB | 29,368 KiB |
| query p50 / p95 / max | 0.053 / 0.076 / 0.124 ms | **0.048 / 0.061 / 0.095 ms** | 0.055 / 0.082 / 0.152 ms |
| WASM (wasm-bindgen output; historical v4 measurement) | 212,828 B (208 KiB) | 164,058 B (160 KiB) | 127,063 B (124 KiB) |
| WASM (gzip -9; historical v4 measurement) | 77,834 B (76 KiB) | 60,997 B (60 KiB) | 50,286 B (49 KiB) |
| WASM query p50 / p95 | 0.119 / 0.283 ms | 0.11 / 0.23 ms | 0.096 / 0.187 ms |

The v3 and v2 rows are measurements recorded for those formats on this machine; the
v4 row is the historical measurement after title postings, fuzzy matching and facets
landed. The current `wasm-bindgen` output measured on 2026-11-09 is **292,987 B raw /
121,092 B gzip**; the table is retained for format-era comparison, not as today's
module-size claim. Its 48,468 extra bytes over v3 are 8,000 for the title length in each of the
2,000 40-byte records, 38,029 of title postings, 2,415 of `section` facets for the
20 directories the benchmark writes and 24 for the larger header; the 0.009 ms of
extra query time is within this machine's run-to-run spread (the same loop with
`--title-boost 0` reports 0.061 ms p50, i.e. no difference). The WASM module grew by
16 KiB gzipped over v3 — and 5 KiB more since this table was first written, the page
grouping entry point and the borrowed-token analyzer between them — because it also
carries the English and Italian Snowball tables, the bounded fuzzy walk, the
facet/title readers and the candidate-indexed filter path; a site that uses none of
them still pays for them, which is the honest cost of shipping the runtime as one
module.

The numbers above are a gate, not only a note: `node tools/perf-gate.mjs` runs the
same 2,000 × 120 benchmark against a release binary, gzips the `wasm-bindgen`
module and compares all of it with `tools/perf-budget.json` (measured 2026-11-09:
1,804,012 B artifact, p50 0.058 ms, p95 0.105 ms, 292,987 B raw / 121,092 B gzip
with `gzip -9`).
A change over budget fails CI; `--accept` rewrites the budget so the diff shows the
number that moved, and CI also runs the gate with an impossible budget
(`--max-p95-ms 0.0001`, `--max-gzip 1`) and requires it to fail. Latency budgets
carry 2.5× headroom because a shared runner is noisy; size budgets are 2–3%
because a pinned toolchain makes them deterministic.

The 2,000-page artifact is **10.3% smaller** than v2 even though it carries 16,000
bytes of new string hashes and a title postings list: the postings are block-packed
instead of plain LEB128, and text frequencies are 2-bit codes with a varint escape
rather than one byte per posting. In WASM the bit unpacking costs ~15% (0.119 vs
0.096 ms p50 against v2) for 10% fewer bytes, which is the trade a network-bound
Worker wants.

On the repository's own page corpus (10,000 documents, 12.0 MB of HTML files) a
single-term query answers in **1.30 ms** (p50 of 500 WASM runs, which includes
decompressing ten text blocks and building ten snippets) and the artifact is
11,117,082 bytes with `--stemmer en`:

```bash
node tools/make-corpus.mjs /tmp/cmp/facets --target 10000 --chunk 850 --facets
cargo run --release -p rustsearch-cli -- build /tmp/cmp/facets -o /tmp/cmp/index.rsx \
  --stemmer en
cargo run --release -p rustsearch-cli -- shard /tmp/cmp/index.rsx \
  -o /tmp/cmp/shards --prefix index/ --plan "index" -n 5
```

| section | bytes |
|---|---:|
| terms | 581,530 |
| postings | **965,522** |
| title postings | 168,742 |
| documents | 400,004 (40 bytes each) |
| strings | 1,039,102 |
| texts | 7,781,586 |
| text hashes | 80,000 |
| string hashes | 80,000 |
| facets | 20,476 (`lang` + `section`) |
| header | 120 |

That table sums to the artifact. Against the v3 build of the same documents it is
+229,236 bytes (**+2.1%**): +168,742 title postings, +40,000 for the title length in
every record, +24 header, +20,476 facets; a site with no metadata pays only the first
three (+208,765, +1.9%). Re-encoded as v2 LEB128, the same 918,622 body postings
take 1,951,788 bytes, so the v3 encoding is **33.8% smaller**: the doc-delta stream
saves 2.9% (LEB128 is already good at a long-tailed gap distribution), the tf stream
saves 68.4% (71% of postings have tf 1, and 95% are ≤ 4, which two bits encode).

Head-to-head with pagefind 1.5.2 and `@zbsearch/static` 4.0.1 on this corpus is in
[`COMPARISON.md`](COMPARISON.md): build wall time and peak RSS, bundle size and its
breakdown, cold start in bytes and requests, warm p50/p95, and how much the three
rankings agree.

The WASM row is the same 2,000-document artifact measured in Node 24 (`wasm/bench.mjs`,
500 queries); it includes the JSON parse of every result. Reproduce with:

```bash
cargo run --release -p rustsearch-cli -- bench --docs 2000 --words 120 --queries 200 \
  --out /tmp/rs-bench.rsx
wasm-bindgen --target nodejs --out-dir wasm/pkg-node \
  target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm
node wasm/bench.mjs /tmp/rs-bench.rsx 500 term0007
```

## WASM

```bash
rustup target add wasm32-unknown-unknown
cargo build -p rustsearch-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir wasm/pkg-web \
  target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm
```

```js
import init, { Engine } from "./pkg-web/rustsearch_wasm.js";
import wasm from "./pkg-web/rustsearch_wasm_bg.wasm";

await init(wasm);
const bytes = new Uint8Array(await (await fetch("/index.rsx")).arrayBuffer());
const engine = new Engine(bytes);

const { results } = JSON.parse(engine.searchJson("deploy preview", 10));
for (const hit of results) {
  // hit.snippet is plain text; hit.highlights are byte ranges into it.
  render(hit);
}
```

`Engine` validates magic, version, analyzer id, section bounds and the FNV-1a
checksum before it answers a query. A truncated or corrupted artifact throws instead
of reading out of bounds.

For the shard path the boundary grows by five calls, and nothing else:

```js
const engine = Engine.fromSparse(bytes);      // metadata only, no texts, no strings
const [textOff, textLen] = engine.textSpan(docId);
engine.attachTextBlock(docId, blockBytes);   // verifies the text hash

const [urlOff, urlLen, titleOff, titleLen] = engine.stringSpan(docId);
// The builder writes url then title back to back: one range covers both.
engine.attachStringBlock(docId, stringBytes); // verifies the strhashes entry

// A chunked (v5/v6) artifact: the engine names the chunks a query needs.
if (engine.chunked) {
  for (const chunk of engine.chunksForQuery(query, true, false)) {
    engine.attachTermChunk(chunk, await fetchChunk(chunk)); // verifies the directory hash
  }
}

// ... and the record page of each hit, which names its strings and text block.
const page = engine.docPageOf(docId);              // undefined on a flat artifact
if (page !== undefined) {
  const [pageOff, pageLen] = engine.docPageSpan(page);
  engine.attachDocPage(page, await fetchPage(pageOff, pageLen)); // verifies the page hash
}
```

`engine.analyzer` reports `none`, `english` or `italian`.

`engine.facetsJson()` returns the taxonomies as
`{"lang":{"en":812,"it":20},"section":{…}}` (`{}` when the site had none), so a UI
can build its filter list from the artifact instead of hard-coding the site's fields.
`engine.searchJsonWithOptions(query, limit, prefix, fuzzy, maxEdits, titleBoost,
filters, group)` runs a query with an explicit policy. In the Rust engine and CLI,
flat artifacts correct unmatched tokens by default (one edit, minimum four bytes, at
most three corrections); `--no-typos` opts out. Sparse/chunked indexes do not run
automatic typo correction, because locating a correction could require the whole
dictionary. Explicit fuzzy expansion is separately bounded to 64 chunks on the
Worker and reports `planTruncated` when that plan is incomplete. The filter is
`"lang\u{1f}en\u{1e}section\u{1f}guide"`, i.e.
`filters.map(f => `${f.field}\u{1f}${f.value}`).join("\u{1e}")` (an empty string
means no filter), `titleBoost` is the BM25F title weight, where `NaN` — an unset
option in JS — means the engine default (2.0), `0` ignores titles and anything above
10 clamps to 10, and `group` collapses a page's documents into one hit (see "Pages
and chunks"). The JSON answer carries the applied filters next to `results`, and
every hit carries `matches`.
`engine.titleTokenCount` reports the title tokens the artifact carries (0 when the
build indexed no titles), so a client can tell whether the boost has anything to
weigh. A typo costs about 0.4 ms natively on the 10k corpus; the walk is bounded by
`MAX_FUZZY_WORK`, not by the dictionary size.

## Browser UI

`adapters/ui/` contains a static, dependency-free search playground. It runs the same
WASM engine in a Web Worker, can load a whole `.rsx` file or use the sharded client,
and exposes query, filters, typo tolerance, title boost, grouping, pagination and
optional editorial curation. It renders snippets and highlights as text nodes (never
HTML), validates result URLs, and ships with a restrictive CSP and Node tests for the
headless query/session and shard logic. It is a reference consumer of the engine, not
a server-side search service or a framework package.

See [`adapters/ui/README.md`](adapters/ui/README.md) for setup, corpus generation,
shard traffic and the curation-file format.

## Cloudflare Workers + R2

Two environment variables are the deployment's policy: `ALLOWED_ORIGINS` (a
comma-separated CORS allowlist; unset means same-origin only) and
`MAX_ARTIFACT_BYTES` (the artifact ceiling, 256 MiB by default). Every response
carries `nosniff`, `no-referrer`, `default-src 'none'` and a same-origin resource
policy, and `HEAD`/`OPTIONS` are answered explicitly (H10).

See [`adapters/cloudflare/README.md`](adapters/cloudflare/README.md). The worker
loads the shard set's metadata from R2 once per isolate with `Range` requests — one
`meta` object for a chunked artifact — keeps the sparse `Engine`, and reads each
query's term chunks, each hit's record page and each hit's URL+title and text block on
demand;
the objects are cached immutably, so `INDEX_KEY = "index/{digest}/manifest.txt"`
publishes each build under its manifest digest and a rebuild writes new keys instead
of overwriting the ones a cached manifest names. The query endpoint
is `GET /search?q=...&limit=...&fuzzy=1&filter=section%3Dguide&boost=5&group=1` and
returns JSON only. Explicit fuzzy expansion is opt-in and bounded to a 64-chunk plan;
`planTruncated` reports when the plan is incomplete. Automatic one-edit typo
correction is available for flat indexes, not the chunked Worker index. `filter` is
repeatable, `boost` sets the title weight (default 2, `0` for body only), and
`group=1` returns one hit per page. An optional `CURATE_KEY` applies the CLI's
editorial curation rules; `CURATE_DIGEST` pins their content, and a rule file that
does not match is ignored (with a logged reason) so the ranking stays uncurated.

## Sharding for R2

One artifact is right for a site up to a few thousand pages. For a corpus where the
page text dwarfs the index, split it into a manifest plus one object per section and
let the client read only what a query needs:

```sh
rustsearch shard index.rsx -o public/index --prefix index/ --plan "index" -n 5
```

That writes `manifest.txt` and `head.bin`, `terms.bin`, `postings.bin`, `docs.bin`,
`strings.bin`, `texts.bin`, `hashes.bin`, `strhashes.bin` (plus `facets.bin` and
`titlepostings.bin` when the artifact carries them). An artifact built with
`--term-chunk-bytes` publishes its own plan instead: `meta00000.bin`, … (header, chunk
directory, BM25 lengths, facets, the record page table and the title chunks — sliced at
1 MiB, with `hashes.bin` and `strhashes.bin` as separate objects), `docrecords.bin`,
one
`chunk00000.bin` per term chunk, `strings.bin` and `texts.bin`. The manifest is
line-oriented text (no JSON parser needed) and carries each part's offset, length and
FNV-1a hash, plus the artifact checksum; the parts concatenate back into the original
`.rsx` byte for byte in both layouts.

A chunked artifact's metadata run is cut at 1 MiB by default, so no metadata object
grows past a cache entry without the shard command being told. `--meta-part-bytes N` changes the budget
and `0` restores the single object:

```sh
rustsearch shard index.rsx -o public/index --prefix index/          # 1 MiB budget
rustsearch shard index.rsx -o public/index --prefix index/ --meta-part-bytes 0
```

It writes `meta00000.bin`, `meta00001.bin`, …, each at most `N` bytes and still an
artifact slice at its offset; below the budget a range is one slice. The two hash
tables are always separate objects and are not part of the cold start.
Every metadata slice is part of the cold start, so the split does not reduce bytes —
it bounds each object. On a 100k corpus built for this measurement the 3,689,862-byte
metadata run becomes 4 objects at 1 MiB (and 57 at 64 KiB, which costs 56 extra
requests for no bytes saved). A **1,000,000-document** build (the seeded
`tools/make-corpus-100k.mjs` run at 320 chars/document: 630,701,591-byte artifact,
344,723 terms) has a 35,394,525-byte metadata run, published as 34 objects at 1 MiB
(136 at 256 KiB, 541 at 64 KiB); cold start is 35 requests (2 with
`--meta-part-bytes 0`) and the bytes are unchanged. Measured on a 1M build, 64 KiB is 542 cold-start requests and
2,234 ms p50 (20 ms RTT, concurrency 6) against 35 requests and 319 ms at 1 MiB, for the
same metadata bytes: use 1 MiB here, 64 KiB only when an object-size cap is mandatory. An explicit non-zero `--meta-part-bytes` is rejected for a flat v4 artifact, whose
metadata is published one section per object and cannot be grouped after the fact.
`node tools/bench-meta-shards.mjs` reproduces the 1M case on a deterministic corpus
and verifies the budget, the part hashes and lossless reassembly.

Every document has an **8-byte FNV-1a hash of its text block** in the `hashes`
section and an **8-byte hash of its URL+title bytes** in `strhashes`. Each is
fetched with the metadata, so one `Range` read of a text block or of a hit's strings
can be verified on its own, without the rest of the artifact.

Measured on the repository's own page corpus (10,000 documents, 11,117,082-byte
artifact, 7,781,586 of them text, built with `--stemmer en` and `--facets`):

```
artifact    11117082 bytes in one request before sharding
plan "index": 5 hit(s) -> 2301477 bytes in 19 range requests (20.7% of the artifact)
  manifest 673 · metadata 2296394 · strings 489 · text blocks 3921
```

A flat query reads the manifest and the eight metadata parts (`head`, `terms`,
`postings`, `docs`, `hashes`, `strhashes`, `facets`, `titlepostings` = 2,297,067
bytes in 9 requests), then one `Range` per rendered hit for its URL+title and one for
its text block. The chunked plan reads the manifest and `meta` (2 requests), the
chunks the query routes to, the record page of each hit, and then the same per-hit
strings and text blocks. The 7.78 MB text section and the 1.04 MB string section are
never downloaded whole; the five hits of the plan cost 489 bytes of strings and 3,921
bytes of text. Before per-hit string ranges a query had to fetch the whole
1,039,102-byte `strings` section; the five hits now cost 489 bytes.

Most text blocks are never downloaded, so a hit whose block was not fetched ranks
with an empty snippet instead of failing the query; a hit whose strings were not
fetched keeps an empty URL and title. Four independent checks guard the bytes:

* `cargo test -p rustsearch-index --test shard_range` publishes the set to a loopback
  HTTP server, fetches it with real `Range` headers, asserts every `206` body is the
  exact artifact slice, that the full set reassembles losslessly, that each per-hit
  string and text block matches its own hash before the engine attaches it, and that
  the sparse index filters on the facet part it fetched with the metadata;
* `node --test adapters/cloudflare/shard-source.test.mjs` covers the R2 client the
  Worker uses (manifest parsing, part hashes, short ranges, per-hit string and text
  reads, non-adjacent records, warm queries);
* `node adapters/cloudflare/verify.mjs <shard-dir> 5 "index"` runs the Worker's real
  path end to end: sparse `Engine`, `Range` reads, hash checks, URLs and snippets
  (`--filter section=guide` and `--boost 5` exercise the two query knobs);
* `node --test adapters/ui/tests/*.test.mjs` covers the search UI's core (folding,
  highlight ranges, payload validation, URL safety), the shard client, the session
  queue and the curation message order — see
  [`adapters/ui/README.md`](adapters/ui/README.md);
* `node --test adapters/cloudflare/headers.test.mjs` covers the Worker's response
  policy: the security headers, the CORS allowlist, preflight and the artifact
  ceiling (H10);
* `.github/workflows/hardening.yml` is the CI gate: `cargo fmt --check`, clippy with
  `-D warnings`, the tests, `cargo audit`, `cargo deny check`, a CycloneDX SBOM, a
  two-build reproducibility diff and a fuzz smoke (H5, H6);
* `Index::from_bytes` still verifies the whole-body checksum for an artifact loaded
  in one piece.

## Security model

* **The artifact is untrusted.** The reader validates the checksum and every
  section/record offset before a query; it uses checked arithmetic and no
  `unsafe`. A v7/v8 artifact — what a build writes unless told otherwise — extends the
  checksum over the header, so a flipped flags word or a shifted span is refused; v4–v6
  hash the body only, which is why `--no-header-checksum` exists. A shard client that loads a partial buffer (`from_sparse`) verifies each
  whole part against the manifest and each per-hit string/text block against its
  header hash; `Index::from_bytes` still checks the whole-body checksum. An analyzer
  id this build does not implement is rejected, never silently ignored.
* **Postings are decoded defensively.** Every block header, bit width, tf mode and
  varint is bounds- and range-checked, a list must be consumed exactly, and a
  deterministic byte sweep keeps the decoder panic-free on arbitrary input.
* **The indexer treats HTML as hostile.** It never executes scripts, drops
  `script`/`style`/`iframe` subtrees, builds URLs from relative path components
  only (rejecting `..`, roots and prefixes), and percent-encodes each segment.
* **Snippets are text.** The engine returns a plain string and byte ranges, never
  HTML. The renderer must use `textContent` and wrap ranges in `<mark>` under a
  CSP; the Worker sets `nosniff`. The JSON boundary also escapes `<`, `>`, `&` and
  the two line separators as `\uXXXX`, so a page title cannot close an inline
  `<script type="application/json">` block; parsing the escape back yields the same
  bytes, so `highlights` stay valid.
* **Queries are bounded.** 512 bytes, 16 terms, 32 prefix expansions, 16 fuzzy
  candidates, 1,000 hits, 8 filters of 8 values each within 512 bytes of filter
  text, and a 400,000-term-visit budget on the fuzzy walk, which prunes by edit
  distance instead of stopping at a dictionary size. The per-hit block cap (256 in
  the Worker) bounds what a long-lived isolate keeps. A filter is matched against the
  build's own facet values and is never rendered as markup.
* **Integrity is not authentication.** FNV-1a detects truncation and bit rot; a
  deliberate rewrite needs a deploy-time signature or a trusted object key. Use
  TLS, a private bucket or a signed Worker route.

## Limits and operational notes (formats v3–v6)

* Stemming and stopwords are available for English and Italian; `none` is the default
  for mixed-language sites. Other languages and scripts without whitespace
  segmentation (for example Chinese, Japanese and Thai) have no language-specific
  analyzer today. Input is HTML; render Markdown before indexing.
* `build --cache DIR` is a no-change fast path, not partial incremental merging. It
  hashes every admitted page and reuses the saved artifact only when page bytes
  **and build options/rules** match, avoiding parsing and serialization on an unchanged
  build. Changed or deleted pages trigger a full rebuild because document IDs are
  positional. Cache failure is an optimization
  miss, not a build failure.
* Build-time synonyms (`build --synonyms FILE`) add target postings to pages that
  contain a source term, so query-time synonym rules are unnecessary. They add no
  ordinal and therefore cannot be combined with `--positions`.
* Phrase positions are opt-in (`build --positions`): quoted phrases require adjacent
  analyzed token ordinals. Positions include stopwords, preserving gaps between
  indexed words; phrase queries against an index without positions return
  `Error::NoPositions`. Snippet highlights are still found by scanning the attached
  text rather than directly using positions.
* Flat Rust/CLI searches automatically correct up to three unmatched tokens of at
  least four bytes, by one edit, at a lower score; `--no-typos` disables this.
  Sparse/chunked clients do not perform automatic correction because finding a
  correction can require the full dictionary. Explicit fuzzy expansion is separate:
  a chunked Worker limits the plan to 64 chunks and reports `planTruncated` if it
  cannot cover the dictionary.
* `rustsearch shard` publishes an artifact as a manifest and independently
  range-readable parts. Per-part FNV and per-block hashes detect corruption; they are
  not authentication. Build and shard write SHA-256 sidecars, `rustsearch verify`
  checks a pinned digest, and can verify detached minisign signatures. The Worker
  pins the manifest with `MANIFEST_DIGEST` (the `sha256:` value `rustsearch shard`
  writes to `manifest.txt.sha256`): a substituted set is refused before it is
  parsed, and a malformed pin fails the query closed instead of running unpinned.
  The same pin versions the prefix (`{digest}` in `INDEX_KEY`), and `CURATE_DIGEST`
  pins the optional rule file, which is ignored when it does not match.
* Editorial curation (`search --curate FILE`, or the browser/Worker curation input)
  can pin or boost only hits already returned for a query. It cannot introduce a
  document, and paths must be same-site. Rules are separate from the immutable
  artifact and can be changed without rebuilding.
* A long page can be split with `--max-page-chars`; `--group` combines its matching
  documents after ranking and before `limit`, so result limits count pages.
  Chunked fuzzy searches and grouped sparse searches have higher fetch costs: fuzzy
  may need many term chunks, while grouping needs the record pages to identify every
  candidate's page.

Implemented behavior is documented in this file and [`DESIGN.md`](DESIGN.md).
The hardening workflow runs Rust and adapter tests, a judged ranking regression gate,
supply-chain checks, a reproducible-build check and a fuzz smoke test.

## Layout

```
rustsearch/
├── core/        format reader, analyzer + stemmers, block postings, BM25, snippets, JSON
├── indexer/     walkdir + scraper + deterministic writer + shard split
├── tools/       make-corpus.mjs (the 10k corpus), bench-meta-shards.mjs (the 1M
│                metadata-split check), compare-engines.mjs and zbsearch-build.mjs
│                (the pagefind/ZBSearch harness behind COMPARISON.md)
├── cli/         build / search / info / shard / diff / bench
├── wasm/        wasm-bindgen boundary (one Engine, one JSON string)
├── adapters/    Cloudflare Worker + R2 client and static WASM browser UI (+ Node tests)
├── fuzz/        cargo-fuzz targets for binary parsing, attachment, filters and extraction
├── fixtures/    a small site used by the tests
└── .githooks/   pre-commit linter (enable with `git config core.hooksPath .githooks`)
```

Tests: `cargo test --workspace`; focused adapter tests are run with
`node --test adapters/cloudflare/shard-source.test.mjs` and
`node --test adapters/ui/tests/*.test.mjs`. `node tools/bench-meta-shards.mjs`
reproduces the 1M metadata-split check. CI gates formatting, clippy, Rust and adapter
tests, the ranking fixture, dependency advisories/licenses, an SBOM, reproducibility
and a fuzz smoke. Format and trust-boundary details: [`DESIGN.md`](DESIGN.md).

Commits are linted locally: `.githooks/pre-commit` runs `cargo fmt --check` and
`cargo clippy -- -D warnings` when Rust files are staged, and `node --check` on every
staged JavaScript module. Enable it once per clone with
`git config core.hooksPath .githooks`; CI still runs the full suite, because a hook can
be skipped with `--no-verify`.

## License

MIT — see [`LICENSE`](LICENSE).
