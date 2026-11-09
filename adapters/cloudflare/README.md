# Cloudflare Workers + R2

The Worker is a thin adapter: R2 holds the shard set (`rustsearch shard`), the WASM
module answers queries, and the isolate keeps one sparse engine plus the per-hit
strings and text blocks it has already fetched.

```bash
# 1. build the artifact and split it into shards
rustsearch build public -o dist/index.rsx --base-url https://example.com
rustsearch shard dist/index.rsx -o dist/index --prefix index/ --plan "index" -n 5

# 2. build the wasm package this Worker imports
cargo build -p rustsearch-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target web --out-dir adapters/cloudflare/pkg \
  target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm

# 3. upload the manifest and every part
for file in dist/index/*; do
  wrangler r2 object put "rustsearch-index/index/$(basename "$file")" --file "$file"
done

# 4. point INDEX_KEY at the manifest, then deploy
cp wrangler.toml.example wrangler.toml
# edit INDEX_KEY = "index/manifest.txt" if you used another prefix
wrangler deploy

# 5. optional: editorial rules (`rustsearch search --curate FILE`)
wrangler r2 object put "rustsearch-index/index/curate.txt" --file curate.txt
# and in wrangler.toml: CURATE_KEY = "index/curate.txt"
```

On every object, set `Cache-Control: public, max-age=31536000, immutable` and
`Content-Type: application/octet-stream`. A rebuild writes new keys; a deploy is a
manifest swap. Keep the manifest's key short-lived (`no-cache`) and the parts
immutable.

## What a request does

1. **Cold isolate** — fetch `manifest.txt`, then the metadata: `meta00000`,
   `meta00001`, … for a chunked (v5) artifact (`--meta-part-bytes N` changes the
   budget), skipping `hashes`, `strhashes` and the `titleNNNNN` lists, or `head`,
   `terms`, `postings`, `docs`, `hashes`,
   `strhashes`, `facets` and `titlepostings` (when the artifact has them) for a flat
   one. Each part is verified against the manifest's FNV hash and the result is a
   sparse `Engine`. The `texts`, `strings` and `docrecords` parts are never fetched
   whole, and the metadata slices are fetched concurrently. The manifest itself is
   pinned by the deploy: `worker.js` passes `MANIFEST_DIGEST` — the `sha256:` value
   `rustsearch shard` writes to `manifest.txt.sha256` — as
   `loadSparseIndex(bucket, key, { expectManifestDigest })`, which checks its SHA-256
   with WebCrypto before parsing it, so a substituted set is refused even when every
   FNV in it was recomputed. Unset keeps the per-part hashes only; a malformed value
   fails the query closed (503) instead of running unpinned.
2. **Query** — `engine.searchJson(q, limit)` ranks against the metadata; with
   `?fuzzy=1`, `?filter=field=value` (repeatable, at most eight), `?boost=N` or
   `?group=1` it calls `searchJsonWithOptions(q, limit, true, fuzzy, 1, boost,
   filters, group)` instead, so a typo is corrected by the same bounded walk the CLI
   uses, the ranking sees only the documents the facet selects, a title match is
   weighted, and a split page comes back as one hit. The facet table and the page
   identity are metadata, so filtering and grouping cost no extra request — and a
   filtered query is *cheaper* than an unfiltered one because the candidates are
   picked before anything is scored. Titles are not: the list of each chunk travels
   as its own `titleNNNNN` object, so a title-scored query reads it with the chunk.
   A fuzzy plan is capped at 64 chunks (H7) and the response carries
   `"planTruncated": true` when the cap applied, because that ranking is a bounded
   approximation rather than the whole dictionary. The answer echoes `filters`
   next to `results`, and every hit carries `matches` (1 unless grouped).
3. **Taxonomy** — `engine.facetsJson()` returns
   `{"lang":{"en":812},"section":{…},"tags":{…}}` from the same metadata, so a UI
   can build its filter list from the artifact instead of hard-coding the fields.
4. **Per hit** — on a chunked artifact the Worker reads the term chunks
   `engine.chunksForQuery` names, each with the `titleNNNNN` list
   `engine.attachTitleChunk` verifies against the directory, then the record page each hit's `docId` falls on
   (`engine.docPageOf` / `docPageSpan`, verified against the page table by
   `attachDocPage`, so a hit costs `1 / page_records` of the record table instead of
   all of it), then `strings.bin` at `url_off .. title_off + title_len` (one range:
   the builder writes URL then title back to back) and `texts.bin` at
   `text_off .. text_off + text_len`. `engine.attachStringBlock` and
   `engine.attachTextBlock` verify each slice against its 8-byte hash from the header
   before it is used. One more search renders the URLs and snippets.
5. **Warm isolate** — the engine and the attached blocks stay in module scope; a
   repeated hit fetches nothing. Each set is capped (256) so a long-lived isolate
   cannot grow without bound.
6. **Curation (optional)** — when `CURATE_KEY` is set, the isolate reads that object
   once and calls `engine.setCurations(text)`: the same parser as the CLI, so
   `query | pin /path` and `query | boost /path 1.5` behave identically here. The
   object is capped at 64 KiB and a missing or malformed file is ignored rather than
   failing the request — a rule file is editorial convenience, not part of the index.
   The response carries `"curations": N` when rules are loaded, so a deploy that
   forgot the file is distinguishable from one with an empty file.

On the 100,000-document corpus (`--stemmer en`, `--term-chunk-bytes 64k`, 2026-10-30
re-run in `COMPARISON.md`) a 5-hit query costs **1,013,335 bytes in 29 `Range`
requests**, against 16,020,017 bytes and 19 requests for the flat v4 layout of the
earlier build and 78,373,521 bytes for its whole artifact: the 57.9 MB text section,
the 4.5 MB string section and the 3.2 MB record table are never downloaded whole. A
cold isolate is **three** requests (manifest + the metadata run, 623,984 bytes); a
query with no hits costs no more than that, because filtering on a
facet value that matches nothing still needs the metadata to know nothing matched. Per
query, a warm isolate fetches only the hits it has not seen.

## Why this shape

* **Bytes are proportional to the answer, not the corpus.** Text and strings are
  the two large sections; the Worker reads five ranges of each instead of 7.78 MB and
  1.04 MB.
* **Every per-hit read is authenticated on its own.** The header stores a `u64`
  FNV-1a per document over the text block and another over the URL+title bytes;
  `attachTextBlock` and `attachStringBlock` refuse a slice whose length or hash does
  not match. A corrupt object degrades to a missing snippet or URL, never to a bad
  page or a panic.
* **No query work on the origin.** Ranking happens inside the isolate on bytes it
  already has. The Worker never fetches a document or a database.
* **No HTML from the engine.** `searchJson` returns plain snippets and byte ranges.
  Render them with `textContent` and `<mark>` under a CSP; do not feed the snippet to
  `innerHTML`.
* **Bounded request.** `q` is capped at 512 bytes, `limit` at 50, `fuzzy` is a
  flag (no user-supplied edit distance), `boost` is clamped to 0–10 by the engine, at
  most eight `filter` parameters survive,
  and the engine caps query terms, filters, prefix expansions, fuzzy candidates and
  the term visits the fuzzy walk may spend. A facet value is matched as bytes and is
  never rendered as markup: like a snippet, it goes through `textContent`.

## Verifying it without Cloudflare

`shard-source.js` is the R2 client and has no WASM dependency, so it is testable:

```bash
node --test adapters/cloudflare/shard-source.test.mjs      # manifest, hashes, ranges
node adapters/cloudflare/verify.mjs dist/index 5 "index"   # real WASM + real shards
node adapters/cloudflare/verify.mjs dist/index 5 "index" --filter section=guide
node adapters/cloudflare/verify.mjs dist/index 5 "index" --boost 5
node adapters/cloudflare/verify.mjs dist/index 5 "index" --group
```

The end-to-end run prints the same shape as above, and the title statistics, the
applied filters and the taxonomy when they apply:

```
title tokens        50000
filters             [{"field":"section","value":"crewAI"}]
taxonomy            lang:1 section:23
artifact            8662107 bytes / 1 request          # flat v4
sparse metadata     2426354 bytes / 9 range requests
query "index"
  with snippets     2429231 bytes / 19 range requests (28.0% of the artifact)

# the same 10k corpus built with --term-chunk-bytes 64k
artifact            8663636 bytes / 1 request
sparse metadata     523207 bytes / 2 range requests
query "index"
  with snippets     845748 bytes / 17 range requests (9.8% of the artifact)
```

`verify.mjs` serves the shard directory from an in-memory bucket that honours
`Range`, loads `wasm/pkg-node`, searches through the same code path as the Worker and
prints the bytes and requests. `cargo test -p rustsearch-index --test shard_range`
does the same over a loopback HTTP server in Rust.

## Limits and alternatives

* `hashes` and `strhashes` are separate objects and a client reads one 8-byte entry
  per rendered hit (two extra requests) instead of fetching the tables: 1.6 MB saved
  at 100,000 documents, 16 MB at a million. The entry is the index: it is fixed width,
  so a hit's offset is `docId * 8`.
* `meta` is bounded by the 1 MiB default budget: 623,984 bytes at 100k documents
  (2 objects, 3 cold-start requests), read once per isolate. `--meta-part-bytes 0`
  keeps it in one object and two requests, trading cache granularity for round trips.
  At a million documents the run is
  35,394,525 bytes, so the default publishes it as 34 objects; the cold start costs 35
  requests and the same bytes, but no single object is larger than a cache entry.
* Grouping (`?group=1`) needs the artifact to have been built with
  `--max-page-chars`; one document per page means one hit per page and `matches: 1`.
  It reads no string: the page identity is the URL offset in the doc record.
* Workers have a WASM size limit that depends on the plan; the reference build
  measured on 2026-11-09 is 292,987 bytes raw (121,092 gzipped), larger than v2
  because it carries the English
  and Italian Snowball tables, the fuzzy walk, the facet and title readers, page
  grouping and the borrowed-token analyzer.
* For a Rust Worker instead of JS glue, use `workers-rs` and depend on
  `rustsearch-core` directly: `Index::from_sparse`, `attach_text_block`,
  `attach_string_block` and the two span accessors are the whole API the adapter
  needs.
* A public R2 bucket can serve the shards directly to browsers; a private bucket
  needs a Worker route that streams them with the same immutable headers.
