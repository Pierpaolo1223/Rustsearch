# rustsearch — design

Status: formats v3–v6, 2026-10-30. Default builds write flat v4; builds with
`--term-chunk-bytes` write chunked v6. The reader also accepts v3 and the earlier
chunked v5 layout. The implementation is `core/`, `indexer/`, `cli/`, `wasm/`, with
a browser UI and a Cloudflare Worker adapter under `adapters/`.

## 1. The problem

A static site has no search service. The usual options are:

1. a hosted search API — a network round trip, a key, a per-query cost;
2. a server-side index — a process to run and a cache to invalidate;
3. a build-time index shipped with the site — one artifact, searched locally.

`rustsearch` takes option 3 and makes the artifact small enough to load and cheap
enough to query on a phone. The build step is allowed to be native and slower;
the runtime is not.

**Goals.** Sub-millisecond queries on a 10k-page site; a WASM module under 100 KiB
gzipped; no I/O, threads or `unsafe` in the runtime; an artifact that is immutable,
checksummed and safe to cache forever; a format that can be sharded later without
a rewrite.

**Non-goals.** Distributed indexing, vector/hybrid search, a hosted search service,
and silently expanding the analyzer beyond the shipped `none`, English and Italian
modes. The builder has an unchanged-build cache, not partial posting-list merging.
Fuzzy expansion and automatic typo correction are bounded and deliberately distinct;
the sparse Worker path does not automatically scan the full dictionary for typos.

## 2. Shape

```
build (native)                         runtime (wasm or native)
──────────────                         ───────────────────────
walkdir → HTML → scraper                bytes ──validate──> Index
   │        │                            │                  │
   │        └─ drop nav/script/...       │                  ├─ analyze(query)
   ▼                                     │                  ├─ dictionary lookup
main text + title + URL                  │                  ├─ body + title walk
   │                                     │                  ├─ BM25F + top-k
   ▼                                     │                  └─ snippet
analyze → postings → .rsx ───────────────┘
```

`rustsearch-core` is `#![no_std]` + `alloc` and forbids `unsafe`. It has no
filesystem, clock or network dependency, so the native CLI and the WASM module
run exactly the same ranking code. The two Snowball stemmers are vendored into it
so the analyzer is available in WASM without pulling `std` or `serde`.
`rustsearch-index` is the only crate that knows about files and HTML; it is not
compiled into WASM.

## 3. Artifact format v4 (and the chunked v5/v6)

Little-endian, 120-byte header, seven sections plus the optional facet and
title-postings sections. All offsets are `u32` from the start of the file; the limit
is 4 GiB.

| offset | field |
|---:|---|
| 0 | magic `RSRCH01\0` |
| 8 | version (`4`; `3` is still read) |
| 12 | flags (bits 0–1: analyzer; bit 2: per-document LZ4 text blocks; bit 3: facets; bit 4: title postings; bit 5: compact v6 doc lengths; bit 6: body token positions) |
| 16 | document count |
| 20 | term count |
| 24 | token count (`u64`, the BM25 length total) |
| 32 | title token count (`u64`, the title field's length total) |
| 40 | terms offset, length |
| 48 | postings offset, length |
| 56 | documents offset, length |
| 64 | strings offset, length |
| 72 | texts offset, length |
| 80 | text block hashes offset, length (`8 × doc_count`) |
| 88 | string hashes offset, length (`8 × doc_count`) |
| 96 | facets offset, length (v4; empty in a v3 artifact) |
| 104 | title postings offset, length (v4; empty without titles) |
| 112 | FNV-1a/64 of everything after the header |

v4 adds two sections and 24 bytes of header (a 96-byte header in v3), and grows the
document record by four bytes. Every other offset and encoding is unchanged, so a v3
artifact loads as-is (both new spans are empty and it has no title lengths) and a v3
reader refuses v4 rather than misreading it. The safe direction of the upgrade is
therefore "ship the reader first".

The analyzer id is stored, not inferred: a stemmed index must be queried with the
same stemmer, so `0` (none), `1` (English) and `2` (Italian) are the only values
this build reads. Any other id, or any unknown flag bit, is a format error rather
than a silently different ranking. Bit 5 marks v6 compact document lengths; bit 6
marks positional body postings. Positions are an optional feature flag, not a new
format version, and a build without them retains its existing posting encoding.

**Terms.** `u32 count`, then `count` `u32` entry offsets, then entries:

```
u32 df              document frequency
u32 postings_off    relative to the postings section
u32 postings_len
u16 term_len
u8  term[term_len]  UTF-8, strictly sorted by bytes
```

The offset table makes `term(i)` O(1) and binary search O(log n). The terms are
sorted by UTF-8 bytes, which is `str`'s `Ord`. No front coding: the table is
a few hundred KiB on a 30k-term corpus and random access is worth more than the
bytes.

**Postings.** For each term, ascending doc ids, in blocks of at most 128:

```
u8  header          bits 0–5: bit width of the packed doc deltas (0..=32)
                    bits 6–7: tf mode
                      0  every posting has tf == 1, no tf stream
                      1  2-bit codes: 0..2 => tf 1..3, 3 => varint(tf-1) follows
                      2  varint(tf-1) for every posting
                      3  reserved (corrupt)
u8  deltas[]        count × width bits, little-endian within the stream
u8  tf_codes[]      mode 1 only: count × 2 bits
u8  tf_escapes[]    varints, in posting order (mode 1 escapes, or all of mode 2)
```

The first posting of a list is an absolute doc id; every following value is
`doc - previous`. The builder picks mode 0/1/2 per block by costing the block
under each encoding, so a block of long repeats pays one varint per posting while a
block of prose pays two bits for 95% of its frequencies.

**Term chunks (v5/v6).** A build given a term-chunk budget (`--term-chunk-bytes N`)
writes version 6: a 136-byte header with eleven spans. The v5 reader layout is also
supported. The chunked format keeps the v4 payload sections (`strings`, `texts`, both
hash tables, `facets`), replaces the dictionary's two spans with a directory and
chunks, adds title chunks, and splits the document records in two. The eleven spans,
in header order, are `chunks`, `chunkdata`, `titledata`, `doctokens`, `docpageindex`,
`docrecords`, `strings`, `texts`, `hashes`, `strhashes`, `facets` — the writer emits
the body in a different order so that the first seven are one contiguous run (§7).

```
chunks       u32 chunk_count, then 48 bytes per chunk, then the first terms
to the term  u32 first_term      global index of the chunk's first term
chunk:       u32 term_count      terms in this chunk (>= 1)
             u32 data_off        from the chunkdata section start
             u32 data_len
             u32 title_off       from the titledata section start, 0 when none
             u32 title_len       0 when no term of this chunk is in a title
             u32 key_off         from the directory start
             u16 key_len, u16 reserved
             u64 hash, u64 title_hash             FNV-1a of each
chunkdata    chunk[0] || chunk[1] || …   contiguous, in term order
chunk        u32 count, u32 offsets[count], entries, postings
titledata    title[0] || title[1] || …   one title section per chunk, in chunk order
title        u32 count, title_entry[count], postings   (the v4 titlepostings layout)
```

The chunk body is byte for byte the v4 `terms` table followed by the slice of the v4
`postings` section its entries point at; only the base of `postings_off` changes, from
the section start to the chunk start. That is what keeps one decoder for both: the
same `Table` reads a whole dictionary and a single chunk. The directory makes the
dictionary self-describing at chunk granularity — `first_term` and `term_count`
partition the index space, the keys are the first terms (strictly increasing, so a
binary search finds a term's chunk without any chunk being present), and the hash
lets a client verify one chunk on its own.

The budget bounds a chunk's bytes, not its term count, because a chunk is the unit of
a `Range` read: 64 terms of `the` and 64 terms of `zygote` are two very different
sizes, and Cloudflare aligns its origin range fetches to 1 MiB, so the number that
matters is the object's size. A chunk is at most `budget + one posting list`, since
the builder only starts a new chunk when the current one is already non-empty. At the
1 MiB a CDN caches in, that is the whole target; at 64 KiB it is two orders of
magnitude below it, which is why the cold query reads 8–64 KB of dictionary instead
of 1.5 MB. A chunk may exceed the requested budget by one posting list: the writer
never splits a term's postings across chunks.

The invariants the validator enforces are the ones the reader relies on: chunks are
contiguous from `chunkdata` offset 0, their ranges do not overlap, `first_term`
increases by exactly `term_count`, the keys sort strictly, the title chunks are
contiguous among themselves (and `0/0/no hash` when a chunk has none), and the last
chunk ends on the dictionary count and on both section lengths. A chunk the client
never fetched is zeroed; its count word cannot match the directory, so every lookup in
it reads as "no such term" rather than as a term with an empty list.

The title list of a chunk is the v4 `titlepostings` section restricted to that chunk's
terms — same layout, same 16-byte entries, offsets relative to the chunk's own title
section — so a client that fetches a chunk's terms has its title lists in the same
object and `title_boost` needs no second read. It also means the title bytes are
fetched for the chunks a query actually touches instead of for the whole dictionary:
at 100k the flat section is 1,077,630 bytes.

**Document records (v5/v6).** Ranking needs a document's BM25 length; rendering
needs its offsets. Flat v4 stores both in one 40-byte record, which makes the record
table a section every query has to hold. The chunked layouts split it:

```
doctokens      u32 doc_count, then two u32 lengths per document in v5; v6 may
               store the same body/title pair as two u16s when all values fit
docpageindex   u32 doc_count, u32 page_records, u64 page_hash[page_count]
docrecords     record[doc_count], 32 bytes each
record         u32 url_off, url_len, title_off, title_len, text_off, text_len, u64 hash
```

The page of a document is `doc / page_records`, so no directory is needed;
`page_records` is the chunk budget divided by 32 (at least 1), which is how a page
stays under the same cache granularity as a term chunk. Each page carries its own
FNV-1a, so one page read is verifiable on its own, and the page table is small enough
to travel with the rest of the metadata: at 100k documents it is 8 bytes per 2,048
records plus 8. A query ranks from `doctokens` alone, so a sparse index with no page
fetched still returns the right scores and `matches`, and a document whose page is
absent reports no `url` instead of document 0's offsets.

Two independent wins, measured on the 10,000-document corpus (918,622 postings):
the delta stream shrinks **2.9%** (LEB128 is already near-optimal for the
long-tailed gap distribution; a fixed width per block only helps when the gaps are
uniform) and the tf stream shrinks **68.4%** (71% of postings have tf 1 and 95% are
≤ 4, which two bits express). Together the section is **33.8% smaller** than the
same postings re-encoded as v2 LEB128.

**Documents.** `u32 count`, then 40-byte records:

```
u32 url_off, url_len        into strings
u32 title_off, title_len    into strings
u32 text_off, text_len      into texts
u32 token_len               body BM25 length normalisation
u32 title_token_len         title BM25 length normalisation (v4)
u64 hash                    FNV-1a of the encoded text block (per-hit integrity)
```

**Strings/texts.** URLs and titles are concatenated UTF-8, and the builder writes
the URL of a document directly followed by its title, so one range covers both.
Each `url ++ title` pair is stored **once**: a page split into several documents
(`--max-page-chars`) shares one pair, so its chunks carry the same `url_off` and the
same `title_off`. That offset is the page identity a query groups on, and it is in
the doc record, so a sparse index groups without fetching a string. Offsets are
validated against the section length before a query. Texts are independent blocks:

```
u32 raw_len                 high bit clear: LZ4 block follows
u32 raw_len | 0x8000_0000  high bit set:   raw UTF-8 follows (small/incompressible page)
```

The builder compresses only when the LZ4 result is smaller; a tiny page stays raw.
The reader caps `raw_len` at 8 MiB before allocating, decompresses one hit at a time,
and drops the buffer after the snippet is built. A corrupt block returns a format
error instead of a decompression bomb.

**Hash tables.** `hashes` and `strhashes` are one `u64` FNV-1a per document, in
document order: the first over the **encoded** text block (length word included),
the second over the URL followed by the title. v2 added `hashes` so a shard client
can verify a single `Range` read of a text block; v3 adds `strhashes` for the other
per-hit read. `Index::attach_text_block` and `Index::attach_string_block` compare
the length and the hash before storing anything, and a mismatch is `BadDocument`.
Each table costs 8 bytes per document (80 KB each on 10,000 documents).

**Facets (optional, v4).** A taxonomy the build records so a query can filter on it:
fields (`lang`, `section`, `tags`) sorted by name, values sorted inside a field, and
the sorted doc-id list of each value, delta-encoded as varints.

```
u16 field_count
u32 field_offsets[field_count]      from the section start, name order
field:
  u16 name_len, name bytes          ≤ 32 bytes
u16 value_count                     ≤ 512
  u32 value_offsets[value_count]    from the section start, value order
  value:
    u16 value_len, value bytes      ≤ 64 bytes, opaque (quotes, `<`, newlines)
    u32 doc_count
    doc_count varints               first id absolute, then positive deltas
```

Offset tables again, for the same reason as the dictionary: a field and a value are
found by binary search, and a doc list is decoded without touching its neighbours.
Because every entry starts at an offset, the validator can require that each list
ends **exactly** on the next entry's boundary — a short list would hide bytes, a long
one would eat into the next value.

The section is absent from a site with no metadata (`flags` bit 3 clear, an empty
span), which is why the cost is opt-in: 20,476 bytes for `lang` + `section` over the
10,000 documents of the comparison corpus (0.2% of the artifact), and a site that
also records `tags` pays for those values on top. Values come from the site's own
build — `<html lang>`, `<meta name="section">` or the first directory of the path,
and `<meta name="keywords">` — so a filter cannot name anything the build did not
write. A field with more than 512 values keeps the most populous ones (ties by name),
so a site with hundreds of tags still gets a usable taxonomy.

**Title postings (optional, v4).** The title is scored as a second field, so it needs
a second postings list per term — but not a second dictionary. The dictionary stays
the single place a term is named (an entry is 14 bytes and its postings are the
**body** list); this section says where the title list of a term lives:

```
u32 count                          terms with title postings, ≤ 2M
entry[count], sorted by term_index, 16 bytes each:
  u32 term_index                   into the term dictionary
  u32 title_df                     postings in this list (read_list's count)
  u32 postings_off                 from the section start, after the table
  u32 postings_len                 exact: read_list consumes it whole
postings blobs, in entry order
```

`title_df` is stored rather than prefixed to the blob because a title list is decoded
with the same `read_list` the body uses, and that decoder takes the count up front.
Four bytes per title term buys a reader that never has to look at the blob to know its
length, and the count is checked against `doc_count` before it is used. Only terms with
a non-empty title list have an entry, so the common case — a title word that also
occurs in the body — costs 16 bytes here and no dictionary growth at all. Binary
search on the sorted `term_index` is what keeps the section usable without a second
dictionary; a term that is in no title is a miss of ~12 steps, which is why a query
whose words are not in titles pays nothing measurable for having the field.

The section is absent from a build that indexed no titles (`flags` bit 4 clear). On
the 10k corpus it is 168,742 bytes — 3.5% of the body postings, since a title is a
tenth of a page's text — and it is the reason a title match outranks a body match.

### Why this shape

* **One file, one fetch.** A static site's index is usually smaller than its
  JavaScript; one round trip beats a manifest plus N shard requests. When the text
  dominates, §7 splits the same bytes into a manifest plus per-section objects
  without changing the format.
* **Offset-addressed, not serialised.** There is no deserialisation step and no
  allocation per term. The reader just slices the `Vec<u8>`.
* **Deterministic.** Sorted walk, sorted dictionary, no hash-map iteration in the
  output. Building the same site twice is byte-identical, so the key can be
  `index/<sha256>.rsx` — and so is building it on a different number of threads: the
  page phase spreads over the cores and its results are merged back in page order
  before a doc id is assigned. A determinism test builds the
  same site twice and compares the bytes.
* **Checksum first.** `from_bytes` rejects a truncated or bit-rotted artifact (a v7/v8 artifact's checksum also covers the header, so a flipped flag or span is refused; v4–v6 hash the body only)
  before it parses a count or an offset.
* **One analyzer, stored.** Tokenization, stopwords and stemming change what a
  term is; putting the analyzer in the header means a query can never use a
  different one than the page did.

## 4. Ranking

BM25F over two fields. For a query term with document frequency `df` and `N`
documents:

```
idf_field = ln(1 + (N - df_field + 0.5) / (df_field + 0.5))
score += idf_body  * (tf_b * (k1 + 1)) / (tf_b + k1 * (1 - b + b * dl / avgdl))          * weight
score += boost * idf_title * (tf_t * (k1 + 1)) / (tf_t + k1 * (1 - b + b * tl / avgtl)) * weight
```

`k1 = 1.2`, `b = 0.75`. Exact terms weigh 1.0, prefix expansions 0.6, and explicit
fuzzy expansions and automatic typo corrections weigh 0.5. A term repeated in the
query is deduplicated. Each field keeps its own
document frequency and its own length normalization; a document with the word in both
the title and the body accumulates both counts, which is the point of the boost. The
title weight `boost` is a query knob (`0` ignores the title list entirely, so the
scores are the pre-v4 ones; the default is 2, the cap 10). `idf` and `avgdl` are
global to the artifact — never to the filter — because a filtered query has to return
the unfiltered ranking restricted to the candidates.

The query path:

1. analyze (lowercase, stopwords, stem with the artifact's analyzer; split quoted
   phrase segments from ordinary terms);
2. for each of at most 16 ordinary terms: exact lookup and prefix range (2+ chars,
   up to 32 terms); optionally apply explicit fuzzy expansion. On flat native/CLI
   searches, unmatched terms of at least four bytes may also receive a bounded
   one-edit correction (at most three per query), unless `--no-typos` is set;
3. evaluate each quoted phrase against positional postings, if the artifact was built
   with `--positions`; otherwise return `NoPositions` rather than approximate it;
4. if the query carries filters, resolve them to a sorted candidate list first;
5. accumulate body/title and phrase scores into a `Vec<f32>`: one float per document
   without a filter, one float per **candidate** with one, plus the bitset that maps
   a document to its slot;
6. sort by score descending, doc id ascending;
7. if requested, collapse split documents by page, then truncate to `limit` so it
   counts pages; and
8. for each hit, decompress its text block, build a 320-byte snippet around the
   earliest match, and drop the text.

A posting list is decoded with a running bit buffer: one byte refill per two or
three values, no random access, no allocation. The decoder refuses a reserved tf
mode, a width above 32, a zero delta after the first posting, a tf outside
`1..=100_000`, and any trailing byte — so a corrupt list is `OutOfBounds`, not a
guess.

The snippet is plain text. `highlights` are ranges into it; the renderer escapes
and marks them. The engine never emits HTML.

### Filters

A filter is a `(field, value)` pair. Values inside one field are OR-ed, fields are
AND-ed, and the intersection happens **before** BM25: the candidate list is built from
the facet doc lists (union per field, intersection across fields) and sorted. So
`limit` applies to filtered results and pagination stays meaningful, which is the whole
point of filtering at query time rather than in the client after ranking. `idf` and
`avgdl` stay global, so a filtered query returns exactly the unfiltered ranking
restricted to the candidates.

The candidate set is then *indexed*, not searched. A posting is tested with a word
load, a shift and a popcount against a bitset of the candidates plus a per-word rank
base, so a document the filter keeps reaches its own slot in an array that has one
float per candidate; a document it rejects costs that test and nothing else. The
earlier shape — a `Vec<f32>` of the whole corpus plus a binary search per posting —
made a selective filter do *more* work than no filter, and a broad filter worse still:
on the 10k corpus `--filter section=data` (6,618 candidates) cost 0.223 ms of ranking
against 0.194 ms unfiltered, where the indexed version costs 0.194 ms. Measured with
`limit 1`, 300 runs, release build, query `index`: 419 candidates 0.121 → 0.096 ms,
392 → 0.103, 304 → 0.104, 593 → 0.130, and a filter that matches no document 0.037 →
0.025 ms. End to end through WASM the same filter is 1.155 ms against 1.298 ms
unfiltered (500 runs, p50), and a query with no candidates at all returns in 4 µs
because the walk is skipped entirely.

* an unknown field or value is an empty candidate set with status 200, because a stale
  UI link is not a server error;
* a filter on an artifact with no facet section is `Error::UnsupportedFacet`, not a
  silently unfiltered list;
* the caps are 8 fields, 8 values per field and 512 bytes of filter text, all of them
  errors (`QueryTooLong`) rather than silent truncation;
* the response echoes the applied filters next to `results`, so a UI can render them
  without keeping its own copy of the query string.

### Grouping a page's documents

A long page is worth indexing at paragraph granularity — BM25 on a 4,000-byte page
is otherwise all-or-nothing for a term that occurs in one sentence — but presenting
it as five hits is wrong. `--max-page-chars N` splits such a page into documents that
share its URL, title and facets; `SearchOptions::group_by_url` (CLI `--group`, WASM
`searchJsonWithOptions(..., group)`, Worker `?group=1`) collapses them back:

* grouping happens **after** the ranking and **before** the limit: the best-scoring
  chunk names the page, the others only raise `matches`, and `limit` counts pages;
* the page identity is the URL's offset in `strings`, which the builder writes once
  per page, so grouping needs no string fetch and works on a sparse shard index;
* without grouping the result is unchanged (and `matches` is 1), so a site that keeps
  one document per page pays nothing.

Cost: one doc-record read per *matching* document, i.e. the cost of deciding which
page each match belongs to. On a 990-page corpus split into 2,960 documents, three
runs of 30 rounds each, p50 goes from 0.090–0.097 to 0.102–0.116 ms on the single
artifact and from 0.079–0.094 to 0.108–0.134 ms through the shard path: about
0.02 ms, the run-to-run spread of the ungrouped number itself. The alternative —
grouping in the client after ranking — is what the engines' clients do, and it
cannot produce `matches` or a correct `limit`.

### Snippets under stemming

A stemmed index stores `index`, but the page says `indexes`. Snippets therefore
look for the stored term **at the start of a word** and highlight the whole word, so
the reader sees `indexes` marked. Highlight generation scans the attached text; it
does not currently consume the positional postings used for phrase matching. The
boundary check keeps `run` from marking `brunch`. When a term never appears at a
boundary (a script without spaces, for example), the plain substring match is used,
which is the v2 behaviour.

### Explicit fuzzy expansion

Fuzzy matching is a query option (`--fuzzy` in the CLI, `fuzzy` in the WASM/Worker
options), not the same thing as default typo correction. It treats the sorted term
table as a trie whose nodes are prefix ranges and carries a Levenshtein DP row for
the prefix against the query token:

* a node is dropped when every prefix of the token is already more than `max_edits`
  away, because appending bytes cannot lower that distance;
* a node is dropped past `token_len + max_edits`, the longest term that can still
  match;
* a prefix that is itself a term is reported when its row distance is within budget;
* the whole query shares `MAX_FUZZY_WORK` (400,000 term visits), bounding query work.

On the 10k corpus (22,602 terms) a one-edit query costs about 0.4 ms natively (p95
~0.5 ms), and a 16-term query made entirely of typos stops at the budget after
~3.7 ms. On chunked artifacts a fuzzy fetch plan is additionally capped at 64 chunks
around the query terms; `truncated`/`planTruncated` marks when it cannot cover the
whole dictionary.

### Automatic typo correction

Flat Rust/CLI searches automatically attempt a one-edit correction for unmatched
query tokens of at least four bytes, up to three corrections per query. The selected
term is scored at weight 0.5, below prefix matches (0.6) and exact matches (1.0), and
the response reports each `{from, to}` correction for a “did you mean” UI. Exact
matches are resolved first; `--no-typos` disables this behavior. The correction pass
uses the bounded trie walk, but is intentionally unavailable on sparse/chunked
indexes and in the Worker: locating a nearest term there may require fetching the
whole dictionary. Explicit fuzzy expansion remains a separate caller-controlled
option.

### Phrase matching with token positions

`build --positions` stores each body token's ordinal in the posting list. Ordinals
count the analyzer stream including stopwords, so dropping a stopword from the
dictionary does not collapse the gap between indexed terms. Quoted phrases such as
`"static site"` require the terms at their analyzer ordinals, and phrase matches
contribute a BM25-style score based on the phrase's document frequency and occurrence
count. Queries against an artifact without positions fail with `NoPositions` rather
than silently matching the words apart. Positions are opt-in and do not bump the
format version; the postings carry a flag and non-phrase queries do not decode them.
Synonym expansion cannot be combined with positions because a generated synonym has
no source token ordinal. Current snippet highlighting still searches the attached
text; it does not derive ranges from posting positions.

## 5. Security

The threat model has three boundaries:

| boundary | risk | mitigation |
|---|---|---|
| site HTML → indexer | scripts, hidden text, path escape | no JS execution; skip `script`/`style`/`iframe`; relative components only; reject `..`, root and prefix; percent-encode segments |
| artifact → runtime | corrupt counts/offsets, truncation, tampering, text bombs, malformed packed postings | magic, version, analyzer id, section bounds, term order, doc ranges, checksum, 8 MiB text cap, exact per-list consumption, block header validation; checked arithmetic; no `unsafe`; `Result`, never `unwrap` on input |
| index → renderer | XSS via snippet/title/URL | plain text only; byte ranges; worker sets `nosniff`; UI uses `textContent` + `<mark>` and a CSP |

The postings decoder is the newest attack surface, so it is tested three ways: unit
vectors for each tf mode, a corruption sweep over the whole artifact (truncate every
97th byte, flip every 53rd), and a deterministic 4,000-case byte sweep that feeds
arbitrary headers and widths to `read_list` and asserts no panic. Every length is
checked before it is used to slice; nothing is allocated from an untrusted count.

What the checksum does **not** do: it is not a signature. A deliberate rewrite of
the artifact can recompute FNV. Treat the object key as trusted (private bucket,
signed route, or a deploy-time signature); use TLS for transport.

Query DoS is bounded by: 512 query bytes, 16 terms, 32 prefix terms, 16 fuzzy
terms, 1,000 hits, 8 filters of 8 values each within 512 bytes of filter text, and a
400k term-visit budget shared by the fuzzy walk of the whole query. A Worker also caps
`limit` at 50, accepts `fuzzy=1` and eight `filter=field=value` parameters, drops
malformed ones, and keeps at most 256 attached blocks per isolate.

## 6. Edge and R2

The artifact is immutable and content-addressed, published either as one object or
as the shard set of §7:

* build: `rustsearch build public -o index.rsx --base-url https://example.com`
* shard: `rustsearch shard index.rsx -o public/index --prefix index/`
* upload: `index/*.bin` plus `index/manifest.txt`, all with
  `Cache-Control: public, max-age=31536000, immutable`
* worker: for a flat v4 set, nine `Range` reads for the metadata at cold start
  (manifest plus `head`, `terms`, `postings`, `docs`, `hashes`, `strhashes`,
  `facets`, `titlepostings`); for the chunked set the run is one `meta` object (or
  its `metaNNNNN` slices), so the cold start is three requests at 10k and at 100k,
  and the hash tables and title lists are per-hit objects. `Engine.fromSparse`
  either way, reuse in module scope; one `Range` read per rendered hit for its
  URL+title and one for its text block, each verified against the header
* query: `GET /search?q=...&limit=...&fuzzy=1&filter=lang%3Den&boost=5` → JSON,
  `cache-control: no-store`

Cold start is 2,426,354 bytes in nine requests for the flat 10k set and 63,809
bytes in three for the chunked one, plus WASM
instantiation. The
~71 KiB gzipped module is the cost; after that a query is microseconds of CPU and
the bytes travelling are the answer's strings and blocks. The worker does not fetch
a document, a database or an embedding.

**Browser path.** `adapters/ui/` is a static, dependency-free reference playground.
It runs the same WASM engine in a Web Worker, loads either a whole `.rsx` artifact or
the range-based shard set, and exposes filters, fuzzy search, typo policy, title
boost, page grouping, pagination and curation. Snippets and highlights are rendered
as text, result URLs are validated, and the page uses a restrictive CSP. It is a
reference client, not a server or framework component. A generic browser client can
also fetch the artifact from a public bucket or Worker route and search locally; the
index is immutable/cacheable while the HTML and JS change with a deploy.

## 7. Sharding for R2

One artifact is right for a site up to a few thousand pages. Past that, the text
section is the problem: on the 10,000-document corpus it is 7,781,586 of 11,117,082
bytes (70%), and a query that renders five hits needs five text blocks. The strings
section is the second problem: 1,039,102 bytes, of which five hits need 489.

`rustsearch shard` (in `rustsearch-index::shard`) splits the artifact into a
manifest and one object per section; the sections are the format's own, so the split
is a routing table, not a second format:

```
manifest.txt       format rsx-shard-1, total, checksum, one `part` line per section
head.bin           0..120         the header, unchanged
terms.bin          the dictionary
postings.bin       the block posting lists
docs.bin           the 40-byte records
strings.bin        URLs and titles
texts.bin          the LZ4 text blocks
hashes.bin         the text block hash table (8 bytes per document)
strhashes.bin      the URL+title hash table (8 bytes per document)
facets.bin         the taxonomy (absent when the build recorded none)
titlepostings.bin  the per-term title lists (absent when the build indexed no titles)
```

Each `part` line is `part <name> <offset> <length> <fnv1a-64> <key>` and the parts
concatenate back into the original `.rsx` byte for byte, so sharding is a routing
table, not a second format: `rustsearch-index::shard::assemble` verifies every part
hash and the body checksum before the engine sees the bytes.

**Chunked dictionaries.** A build with `--term-chunk-bytes N` writes the chunked v6
layout (v5 is the previous, still-readable chunked version). Sharding publishes the
term chunks and record pages independently rather than downloading flat
`head`/`terms`/`postings`/`docs` sections:

```text
metaNNNNN          header, chunk directory, doc tokens, facets, record-page table;
                   consecutive artifact slices at the configured metadata budget
hashes.bin         text-block hashes, fetched per hit
strhashes.bin      URL+title hashes, fetched per hit
titleNNNNN.bin     title list for a chunk, when it has one
docrecords.bin     paged 32-byte document records
chunkNNNNN.bin     one independently fetchable dictionary/postings chunk
strings.bin        URLs and titles
texts.bin          LZ4 text blocks
```

`meta` is possible because the writer puts those sections first, in this order:
`chunks`, `doctokens`, `facets`, `hashes`, `strhashes`, `titledata`,
`docpageindex` — so they are one contiguous run from byte 0 and one artifact slice,
and the manifest needs no new line type. The three sections a cold query does not
need are carved out of that run: `hashes` and `strhashes` (one entry per rendered
hit) and `titledata` (one object per chunk with a title list, read with the chunk). A
cold client reads the manifest and the metadata run — one `meta` object, two requests,
while it fits the 1 MiB default budget, and one request per slice above it — and
starts ranking; a flat v4 artifact keeps one object per section, because re-ordering
its body would cost the byte-for-byte compatibility v4 is kept for.

At a million documents that run is 35,394,525 bytes, so the default budget cuts it
into 34 consecutive slices
published as `meta00000.bin`, `meta00001.bin`, …, each at most 1 MiB.
`--meta-part-bytes N` changes the budget and `0` restores the single object. Nothing inside the run points at a later part of it — every offset the
header and the directory carry is relative to a section start that the header also
carries — so a cut can fall anywhere, the parts remain artifact slices at their
offsets, and the manifest format is unchanged. A
cold client reads every slice before it loads the index; the split bounds each
object, it does not reduce the bytes of a cold start. 1 MiB is the budget that
matches the cache slice Cloudflare uses; 64 KiB simply costs round trips.

The BM25 length table distinguishes the chunked versions: v5 stores two `u32`s per
document; v6 stores two `u16`s when every length fits and falls back to two `u32`s
for the whole artifact otherwise. The ranker reads exact values in both cases.
The reader accepts both v5 and v6.

The chunks are contiguous slices of the `chunkdata` section and the record pages of
`docrecords`, so the parts still cover the artifact exactly; the directory carries
each chunk's first term (its key), its 48-byte table span with the title span and
both hashes, and its global term range. A client binary-searches the keys to find the
chunk a query term lives in, and the engine (`Index::chunks_for_query`) names the rest
of the prefix run, so what is fetched is decided in one place for the CLI, the WASM
module and the Worker. The chunk's own table is the v4 one, offsets relative to the
chunk, so the decoder is shared and a half-fetched chunk reads as "no such term",
never as a posting list made of zeroes.

**The query plan.** `meta` is what every query needs, so it is read once — which is
also why a filtered or grouped query costs no extra request. A title-scored query also
reads the `titleNNNNN` object of every chunk it fetched: the lists are per chunk, so
that is one request per title-bearing chunk instead of the whole section at cold
start. A fuzzy plan is capped at 64 chunks around the query's tokens
(`Index::chunk_plan`), and a plan that does not cover the dictionary reports
`truncated`, which the Worker turns into `planTruncated: true` next to the results. Per query the client reads the chunks `chunksForQuery` names, and per rendered hit the record page
its `docId` falls on (`docrecords` at `page * page_records * 32`), `strings.bin` at
`url_off .. title_off + title_len` — one range, because the builder writes the two
back to back — and `texts.bin` at `text_off .. text_off + text_len`. Each block is
verified against its 8-byte hash, which arrived with the metadata, and each page
against its entry in the page table. `rustsearch shard --plan "index" -n 5` prints the
same accounting on any artifact. Measured through the Worker's real client
(`shard-source.js` + WASM), five hits, a fresh isolate per query:

| what | 10k corpus | 100k corpus |
|---|---:|---:|
| whole artifact (chunked) | 8,663,636 | 78,373,521 |
| cold start (manifest + metadata) | 523,207 B / 2 req | 623,984 B / 3 req |
| cold query with URLs and snippets | **873,361 B / 17.4 req** | **1,013,335 B / 29 req** |
| same, flat v4 | 2,429,200 B / 19 req (28.0%) | 16,020,017 B / 19 req (20.4%) |
| of which strings | 5 ranges of 446,667 | 5 ranges of 4,466,670 |
| of which text | 5 blocks of 5,789,753 | 5 blocks of 57,881,248 |

For comparison, v2 fetched `strings` whole: the same 5-hit plan cost 4,097,152
bytes in 12 requests on the older corpus, against 11,454,305 bytes for the artifact
(35.8%). The string ranges and their hash table are what take it below a quarter of
the artifact. The two cold-start rows are the 2026-10-30 re-run on the seeded 100k
corpus (`COMPARISON.md`), whose artifact is 75,016,979 bytes; the artifact size, the
flat-v4 row and the two section sizes above still describe the earlier
78,373,521-byte build. The 10k column is that Worker measurement too: its current
figures are 63,809 bytes in three requests for the metadata run and 328,502 bytes in
27 requests for the 5-hit query (the re-run's 10k row). The chunked rework moved the
100k side: the metadata cold start went from
3,701,811 bytes in two requests to 623,984 in three, because the hash tables became
per-hit objects and the title lists left the run — now 613 KB, one 1 MiB slice plus a
400-byte tail — and the 5-hit query from 4,103,772 bytes in 18.2 requests to 1,013,335
in 29: 4× fewer bytes for 11 more requests.

`rustsearch-index/tests/shard_range.rs` verifies this over real HTTP: a loopback
server answers `206 Partial Content` the way R2 does, the test asserts that every
body is the exact artifact slice at that offset, that the set reassembles
losslessly and searches, that a partial buffer (`Index::from_sparse`) ranks the same
documents with the same URLs and snippets once the per-hit blocks are attached, and
that an unsatisfiable range is a `416`.

**Identity.** FNV answers "did a byte change", not "is this the artifact the build
produced": whoever rewrites the bytes recomputes every FNV. So `build` writes
`index.rsx.sha256` and `shard` writes `manifest.txt.sha256`, `rustsearch verify`
recomputes and compares them (or a digest passed with `--expect-digest`), and
`--require-signature`/`--public-key` verify a detached minisign signature over the
same bytes. The Worker takes the pinned manifest digest
(`expectManifestDigest`), so a deploy pins one 64-hex string and the per-part FNV
stays the cheap corruption check it was.

**Integrity.** Four independent checks, at four granularities:

* a whole part is checked against the manifest's FNV hash before it is copied into
  the buffer (`assemble`, `loadSparseIndex`);
* a single text block is checked against its `u64` in `hashes` by
  `Index::attach_text_block`;
* a single URL+title slice is checked against its `u64` in `strhashes` by
  `Index::attach_string_block` — this is what the v3 section exists for;
* a full artifact is checked against the whole-body checksum in `from_bytes`.

`Index::from_sparse` skips only the last one, by design: the buffer is incomplete, so
the body checksum cannot match. It still validates every section offset and record,
and a document whose block was never fetched renders an empty snippet (or empty URL
and title) instead of failing the query. Anything loaded from an untrusted source
uses `from_bytes`.

**Failure modes.** A missing part is an error from `assemble`, never a silent
partial result. A manifest that points at a stale part is caught by the per-part
hash. A `416` from R2 means the manifest and the objects disagree, which is the
signal to re-read the manifest. A per-hit block that fails its hash is dropped: the
hit keeps its place, loses only that field.

## 8. Alternatives considered

| alternative | why not |
|---|---|
| Tantivy / Lucene-style index | excellent for a server, too large and too I/O-shaped for WASM; brings a directory abstraction and threads |
| SQLite FTS5 | a great embedded option, but ~1 MiB+ WASM and a database file; the query API is SQL, not a small JSON boundary |
| Pagefind / JS engines | the inspiration for the deployment shape; a Rust core gives a smaller runtime and native build speed, and can share code with a Worker |
| JSON index | simple, but 2–4× the bytes and a parse on load; the binary format is the point |
| rkyv / bincode / postcard | a serialization framework for a format that is mostly offset arithmetic; the hand-written reader is smaller and validates precisely |
| one posting list per term, separate objects | too many requests; the section layout keeps one fetch for the common case |
| `rust-stemmers` as a dependency | requires `std` and `serde`, and its `Algorithm` enum links all 18 languages; the two vendored Snowball modules keep the WASM small and `no_std` |
| front-coded dictionary | ~300 KB on this corpus, but binary search would have to reconstruct every probe; worth doing only if the dictionary becomes the largest section (it is 783 KB of 11.4 MB) |

## 9. Measurements and what they mean

Benchmark (release, Linux x86_64, 2,000 pages × 120 words), v4 against the v3 and
v2 numbers measured before each format change:

| metric | v4 | v3 | v2 |
|---|---:|---:|---:|
| index (LZ4 text blocks + both hash tables) | 1,804,012 B (1.72 MiB) | 1,755,544 B (1.67 MiB) | 2,010,609 B (1.92 MiB) |
| build | 119 ms | 150 ms | 154 ms |
| build peak RSS | 21,208 KiB | 28,460 KiB | 29,368 KiB |
| query p50 / p95 / max | 0.053 / 0.076 / 0.124 ms | 0.048 / 0.061 / 0.095 ms | 0.055 / 0.082 / 0.152 ms |
| WASM query p50 / p95 (Node) | 0.119 / 0.283 ms | 0.11 / 0.23 ms | 0.096 / 0.187 ms |
| WASM (wasm-bindgen output) / gzip | 212,828 B / 77,834 B | 164,058 B / 60,997 B | 127,063 B / 50,286 B |

The 2,000-page artifact is 10.3% smaller than v2 with 16,000 bytes of *new* hash
table in it: the posting encoding more than pays for the string hashes. The module is
17 KiB gzipped larger than v3 because it now carries the English and Italian Snowball
tables, the bounded fuzzy walk, the facet and title readers, the candidate-indexed
filter path, page grouping and the borrowed-token analyzer. The native query path got slightly faster with v3 (0.048 vs 0.055 ms
p50); in WASM the bit unpacking costs about 15% (0.118 vs 0.096 ms p50 against v2),
which is the price of the smaller artifact on a network-bound client. The v4 artifact
is 48,468 B larger than v3 — 8,000 of title lengths in the 2,000 records, 38,029 of
title postings, 2,415 of `section` facets for the 20 directories the benchmark writes
and 24 of header — and its query time is the same within the run-to-run spread (0.057
ms p50 against 0.048 ms recorded for v3 on an earlier, quieter machine; the same loop
with `--title-boost 0` reports 0.061 ms today).

Real corpus (`tools/make-corpus.mjs`, 10,000 documents, 12,608,975 bytes of HTML from
990 markdown files, chunk 850, `--facets`):

| metric | value |
|---|---:|
| artifact | 11,117,082 bytes (10.6 MiB), 7,781,586 of it text, `--stemmer en` |
| documents / terms / tokens / title tokens | 10,000 / 22,602 / 1,089,612 / 120,344 |
| sections | terms 581,530 · postings 965,522 · title postings 168,742 · docs 400,004 · strings 1,039,102 · facets 20,476 · texts 7,781,586 |
| same corpus without the metadata | 11,096,611 bytes, i.e. the facets cost +20,476 (+0.2%) |
| artifact against the v3 build of the same documents | +229,236 bytes (+2.1%): +168,742 title postings, +40,000 title lengths, +24 header, +20,476 facets |
| build | 0.53 s on 8 workers (1.33 s of CPU; 0.97 s single-threaded, same session), 117 MiB peak RSS (before the analyzer work: 1.94 s, 144 MiB; the first v4 run: 1.88 s, 147 MiB; v3: 1.67 s, 129 MiB) |
| query p50 / p95 through WASM | 1.265 / 1.821 ms (500 runs; p50 1.303 with `--title-boost 0`, one run of noise away, so the title field costs nothing measurable) |
| filtered query, `--filter section=crewAI` (419 candidates of 10,000) | 1.134 ms against 1.265 unfiltered (p50); 0.094 against 0.172 ms of ranking with `limit 1` |
| 5-hit sharded query | 2,301,477 bytes, 19 range requests (20.7%) |
| same corpus, `--stemmer none` | 29,667 terms, 1,547,496 tokens, 11,681,731 bytes |
| postings encoding, same corpus either way | 33.8% smaller than the same 918,622 postings as v2 LEB128 (1,292,582 vs 1,951,788 bytes, plain analyzer) |

Per-document LZ4 cut the 2,000-page artifact from a 2,916,713-byte baseline to
2,010,609 bytes (−31.1%) in v2, and the block postings cut another 255 KB in v3, of
which 16,000 went back into the new string hash table. The text is still the largest
section; HTTP compression on R2 (gzip/Brotli) stacks on top. The cost of the text
blocks is a decompressed top-hit block on each query; the cost of stemming is ~10
KiB gzipped of tables in the module.

## 10. Consultation notes (`askd.sh`, session `rust-search`)

The repository's daemon was consulted on architecture, security and Rust, as
requested:

* **Architecture** (`architect` corpus, v2 round): confirmed the build-time
  immutable artifact + object-store/CDN shape, versioned keys and cache-control for
  invalidation, and the monolithic-artifact single point of failure. It did not
  have material on cold-start budgets or sharded failure modes; those are covered
  in §6–§7 from first principles and the measurements above.
* **Security** (`security` + `rust`, v2 round): flagged stored/client-side XSS,
  client-side resource manipulation, template injection, and the `Path::join`
  absolute-path escape. It did not cover index tampering, query DoS or the placement
  of mitigations; §5 assigns each one to the indexer, the engine, the Worker or the
  renderer.
* **Rust** (`rust` corpus, v2 round): recommended `walkdir` with `?` instead of
  `unwrap`, `scraper` with a `main` container and a `:not(nav):not(script)` filter,
  `anyhow` at the application edge and `thiserror` for library errors. The
  implementation uses the same shape.
* **Rust + security** (v3 round): asked about decoding an untrusted binary format
  in `no_std` (bounds, shifts, overflow, allocation) it recommended checked
  arithmetic, length validation before allocation, and fuzz/static testing. §5 and
  the postings decoder apply that: no allocation from an untrusted count, a
  bounds-checked running bit buffer, an exact-consumption check per list, and the
  deterministic byte sweep in `postings.rs`. The daemon had nothing specific on
  bit-packed postings or on `no_std` Snowball tables, so those decisions are argued
  from the measurements in §3 and §9.

The daemon's answers were partial; where a topic was missing, the design says so
rather than pretending the corpus covered it.

## 11. Implemented capabilities and remaining limits

The current system includes more than the flat v4 reader described above:

* **Build cache.** `build --cache DIR` hashes admitted page bytes and fingerprints
  the build options and synonym rules. If every page and the fingerprint match, it
  returns the cached artifact and stats without parsing or serializing again. Any
  changed, added or removed page causes a complete rebuild; positional document IDs
  mean this cache does not merge only changed pages. Cache corruption or a write
  failure is a cache miss, not a failed index build.
* **Build-time synonyms.** `build --synonyms FILE` parses bounded
  `source => target, target` rules (up to eight targets per source) and expands
  matching body/title terms into the postings. Rules are analyzed with the selected
  stemmer; generated postings have tf 1 and do not inflate repeated term frequency.
  Queries need no runtime rules. Synonyms and `--positions` are rejected together
  because synthetic postings cannot satisfy phrase ordinals.
* **Editorial curation.** The CLI, browser UI and optional Worker `CURATE_KEY` load
  shared rules such as `query | pin /path` and `query | boost /path 1.5`. Rules only
  reorder or rescore matching hits already returned by the engine; they cannot create
  a result, and a pin is constrained to a same-site path. Keeping them outside the
  artifact lets editorial changes ship independently.
* **Artifact identity.** FNV checksums protect format sections and fetched blocks
  against accidental corruption, not intentional substitution. `build` and `shard`
  emit SHA-256 sidecars; `verify --expect-digest` checks a pinned SHA-256, and
  `--require-signature --public-key` verifies a detached minisign signature. The
  Worker can pin the manifest SHA-256 before parsing its routing table.
* **Engineering gates.** The hardening workflow runs format/lint/tests, adapter
  tests, the judged ranking regression gate, dependency advisory/license/source
  checks, CycloneDX SBOM generation, a reproducible build comparison and fuzz smoke
  tests. The workspace pins Rust 1.95.0 while declaring Rust 1.81 as its minimum.

Remaining product limits include English/Italian stemming only, no whitespace
segmenter for CJK/Thai, and no partial incremental posting merge. Phrase matches do
not yet source snippet highlights directly from their stored ordinals. A sparse
fuzzy plan may be truncated at 64 chunks; the Worker reports this instead of implying
a complete dictionary scan.
