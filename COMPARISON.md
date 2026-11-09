# rustsearch vs pagefind vs ZBSearch on 10,000 and 100,000 documents

Same corpus, same machine, same 20 queries, same five-result limit. pagefind 1.5.2
(the reference static-site engine), `@zbsearch/static` 4.0.1 (ZBSearch's sharded
browser client, from the `zbsearch/` checkout next to this one) and rustsearch 0.1.0
in its format-v4 build at 10k and its format-v5 chunked build at 100k, with page
grouping available but off in these tables — the page-level section turns it on,
which is the only place the three engines index the same pages. Everything below is reproducible with the commands in "Method".

The whole measurement was re-run on **2026-10-29** against the corpus as it stands
after the `frontend`, `ir` and the extended `compliance` corpora landed. That is 1,136
markdown files and 14.8 MiB of HTML, against 990 files and 12.0 MiB in the previous
revision, so the absolute numbers below are not comparable with the ones in earlier
copies of this file. The ratios and the ordering are.

The first tables are the 10,000-document corpus; §"At 100,000 documents" repeats the
whole measurement at ten times the size, where the three engines' shapes and their
memory ceilings stop being details.

The corpus is this repository's own documentation, not synthetic text: every
markdown page under the domain corpora (`*/pages/`, the design documents, the eval
notes) — 1,136 files — split into 10,000 documents of at most ~850 characters at
paragraph boundaries. The engine's own tree and `adapters/ui/` are excluded, so
writing this file does not change the corpus. Each page also carries
`<html lang="en">` and a `<meta name="section">` naming the tree it came from
(`data`, `crewAI`, `zbsearch`, …), which is what the filter section below measures.

| | |
|---|---:|
| documents | 10,000 |
| HTML indexed | 15,474,379 bytes (14.8 MiB) |
| unique terms, rustsearch `--stemmer en` | 24,829 |
| unique terms, ZBSearch (`language: 'english'`) | 48,457 |
| words, pagefind (`--force-language en`) | 63,932 |

The three are told English so the comparison is about the engines, not about who
stems. Every engine prefix-expands the query term (verified: `postgres` and
`postgre` return the same matches, while `postgresql` returns fewer), and no engine
returns zero hits for any of the 20 queries.

## Build

Median of three warm-cache runs (`/usr/bin/time`), each writing a deployable bundle,
re-run on 2026-10-29 against the v4 + grouping build:

| metric | rustsearch | pagefind | ZBSearch |
|---|---:|---:|---:|
| wall time | **0.58 s** (0.56–0.58) | 7.06 s | 4.95 s |
| build peak RSS | **126 MiB** | 872 MiB | 436 MiB |
| output | 11,941,626 B in 1 file | 12,968,736 B in 10,193 files | 19,035,087 B in 752 files |

All three rows are three warm-cache runs each, median. rustsearch reads, extracts and
analyzes its pages on one worker per core; the artifact is
byte-for-byte the same single-threaded. pagefind and ZBSearch are single-threaded.
The build now takes 0.58 s against pagefind's 7.06 s and ZBSearch's 4.95 s, on 126 MiB
instead of 872 and 436 — the same shape as the previous revision, moved by the larger
corpus and by pagefind's own scaling. At 14.8 MiB of HTML, pagefind writes its 10,193
files in 7.06 s, 12× the time rustsearch spends writing one object (0.58 s).

The build got here through four passes: removing the duplicate analysis of every page,
memoizing the stemmer and
borrowing tokens instead of allocating them, interning one stem handle per spelling
and parsing selectors once, then moving the page phase to one worker per core. This
revision changed no builder code; it re-measured the same binary on a corpus 23%
larger, which is why the numbers moved without the algorithm changing.

## Output size

| | rustsearch | pagefind | ZBSearch |
|---|---:|---:|---:|
| total | **11,941,626 B** | 12,968,736 B | 19,035,087 B |
| page text | 8,439,986 B (LZ4 blocks) | 6,784,929 B (10,000 fragments) | 13,269,748 B (625 JSON fragment files) |
| index | 1,665,990 B (terms 642,481 + postings 1,023,509) | 5,468,271 B (180 chunks) | 5,750,351 B (dictionary 972,101 + 125 postings shards 4,778,250) |
| title postings | 169,273 B | — (folded into the chunks) | — (properties, not a second list) |
| per-document records | 1,645,813 B (docs 400,004 + strings 1,085,809 + 2 hash tables 160,000) | 70,209 B (meta) | 14,988 B (manifest) |
| taxonomy | 20,444 B (`lang`, `section`) | — (no filter field on this corpus) | — (not indexed) |
| runtime / UI | — | 645,327 B (WASM 140,233 + JS 441,271 + CSS 63,648) | — |
| all files, `gzip -9` | **6,288,330 B (52.7%)** | 13,050,014 B (100.6%) | 7,306,310 B (38.4%) |

rustsearch is the smallest bundle on disk and the only one that ships its text
compressed. pagefind's pieces are already compressed (brotli), so gzip leaves them at
100.6% of their size; ZBSearch's JSON fragments compress by 61.6%. On the wire
rustsearch is 6.29 MB, ZBSearch 7.31 MB (16% more) and pagefind 13.05 MB, and
rustsearch is the only one that can be shipped as a single object.

Format v4 is what the last three rows measure. The title postings are a second
postings list of 169,273 B and the facet table is 20,444 B; both are included above.
On a corpus with no metadata the same build would save the facet table, but not the
title field, which is part of the ranking rather than of the taxonomy.

The shapes differ more than the totals: rustsearch is one offset-addressed file,
pagefind is a directory of 10,000 independently fetchable fragments, ZBSearch is a
manifest + dictionary + postings shards + fragment groups.

## Cold start (one query, nothing cached)

| | rustsearch (single artifact) | rustsearch (shards, `Range`) | pagefind | ZBSearch |
|---|---:|---:|---:|---:|
| bytes to first 5 results | 11,941,626 | 2,421,318 | **169,489** | 1,096,662 |
| requests | 1 | 19 | 9 | 7 |
| time | 42.6 ms | 46.9 ms | 158.7 ms | 65.9 ms |

The single artifact is the wrong object for a cold query: the browser has to fetch
11.9 MB before it can rank anything. That is what `rustsearch shard` exists for: the
manifest, the metadata parts (the facet table and the title postings ride along with
the head, terms, postings, docs, hashes and strhashes) and one `Range` per hit
(5 string blocks + 5 text blocks) bring the first answer down to 2.42 MB in 19
reads, 20.3% of the artifact, and the 8.44 MB text section is never downloaded whole.

pagefind wins on bytes because its index is chunked by term and it fetches one chunk
plus five fragments; 140 KB of its 169 KB is the WASM module, which it caches. At
that scale rustsearch's 2.42 MB looks expensive, and it is: the term dictionary and
the postings have to come whole because they are not chunked by term.

ZBSearch sits between the two on bytes (1.10 MB, 7 requests; its 972 KB dictionary is
most of it) and is second on time: 65.9 ms against rustsearch's 42.6 ms (46.9 ms
through the shard path) and pagefind's 158.7 ms, whose WASM instantiation and index
decode dominate. These cold times are CPU and local reads, and they move with the
machine more than the byte counts do. pagefind's 169 KB already contains its WASM;
rustsearch's 234 KB module (85 KB gzipped) and ZBSearch's client bundle are not in
their counts.

## Warm query latency

20 queries × 30 rounds each (`index`, `search`, `query`, `postgres`, `embedding`,
`vector`, `database`, `cache`, `latency`, `schema`, `migration`, `token`, `ranking`,
`snippet`, `worker`, `storage`, `python`, `rust`, `docker`, `kubernetes`), top 5
results with snippets. Median of three harness runs:

| | p50 | p95 | mean |
|---|---:|---:|---:|
| rustsearch, single artifact | **0.248 ms** | 1.158 ms | 0.433 ms |
| rustsearch, shards (local bucket) | 0.270 ms | 1.129 ms | 0.400 ms |
| ZBSearch | 0.789 ms | 2.552 ms | 1.255 ms |
| pagefind | 5.061 ms | 16.445 ms | 6.111 ms |

Each row is the median of three harness runs, 20 queries × 30 rounds each; the byte
counts and ranking overlaps were identical in all three, only the latency rows moved.
Cold time is the noisier row (pagefind 157.0–184.0 ms, rustsearch's shard path
45.9–49.2 ms — a handful of file reads on a machine with other work on it). The
ranking pass that follows runs outside the timed loops.

The title field remains part of this cost but is not separable in this run: the title
postings are 169,273 B and every query that has a term in a title pays a second short
postings walk. A query term that appears in no title (most of them) pays one failed
binary search in the title table instead.

Both rustsearch paths rank the same 24,829-term dictionary and serialize snippets
inline. ZBSearch runs its own `search()` against the merged shards, which is where
its ~0.8 ms goes; pagefind runs its JS layer, decodes the matching index chunk and
resolves one fragment per hit, which is where its ~5 ms go.

Steady-state cost per query, after each engine's caches are warm (600 queries):

| | rustsearch (shards) | pagefind | ZBSearch |
|---|---:|---:|---:|
| bytes fetched per query | **135** | 1,048 | 3,051 |
| requests per query | 0.3 | 0.2 | 0.1 |

rustsearch only reads a block the first time a document is rendered, so 600 queries
cost 81 KB in total; pagefind re-reads a fragment whenever a hit it has not rendered
before appears; ZBSearch pulls each fragment group once, which is why its per-query
bytes are highest but its request count is lowest.

## Filters

rustsearch indexes the taxonomies a page carries (`lang`, `section`, `tags`) and
resolves a filter to a sorted candidate list *before* BM25 scores anything, so both
the ranking and the rendering see only the documents the filter kept. pagefind and
ZBSearch can filter too, but they need a different corpus for it — pagefind reads
only `data-pagefind-filter` attributes, ZBSearch only properties declared in its
schema — and this corpus was deliberately left with neither, so the cross-engine
numbers above stay comparable. The rest of this section is one engine on one
artifact.

`section` has 22 values; `--filter section=crewAI` keeps 328 of 10,000 documents
(3.3%), `--filter section=data` keeps 7,157 (71.6%).

| | bytes / requests on the wire | p50 (WASM, 300 runs, limit 1) |
|---|---:|---:|
| no filter | 2,421,318 B / 19 | 0.246 ms |
| `--filter section=crewAI` | 2,421,580 B / 19 | **0.133 ms** |
| a filter that matches nothing | 2,416,505 B / 9 (nothing to render) | 0.003 ms |

Filtering is free on the wire and cheaper in CPU. It costs no request because the
facet table (20,444 B) is one of the metadata parts the cold query already fetches,
and it saves 46% of the query because the candidate set picks the documents before
they are scored: the score buffer is one float per candidate instead of one per
document, a posting the filter excludes costs a bit test, and the sort afterwards
sees 328 candidates instead of every document the term matched.

The ranking pass, measured on its own: one Rust call to
`Index::try_search_with_filters` per sample — filter resolution, candidate list and
scoring all inside the timed region, which is what a WASM query does per call —
`limit 1`, 300 samples, one run:

| query `index`, limit 1 | candidates | p50 |
|---|---:|---:|
| no filter | 10,000 | 0.246 ms |
| `section=crewAI` | 328 | **0.133 ms** |
| `section=ds4` | 349 | **0.113 ms** |
| `section=pagefind` | 238 | **0.117 ms** |
| `section=root` | 569 | **0.141 ms** |
| `section=data` | 7,157 | 0.226 ms |
| a filter that matches no document | 0 | 0.003 ms |

A selective filter is 46–54% cheaper than no filter at all — the documents it keeps
are the only ones scored — and a 569-document one is 43% cheaper. `section=data`
keeps 72% of the corpus and is only 8% cheaper than the unfiltered query: at that
breadth the candidate list (7,157 ids collected per call) costs almost what the
narrower scoring pass saves. A filter that matches nothing resolves to an empty list
and returns before scoring anything. The scoring loop on its own is the change from a corpus-sized score array to a
candidate-indexed one.

## Ranking agreement

The engines return the same *kind* of answer but not the same order. Counting a
document as shared by URL:

| | pagefind | ZBSearch |
|---|---:|---:|
| rustsearch's top 5, shared | 46 of 100 (46%) | 44 of 100 (44%) |
| rustsearch's top 5, inside the other engine's top 50 | 94 of 100 | 79 of 100 |

This is the section that v4 moved the most. The pre-v4 build agreed with pagefind on
21 of 100 top-5 slots (58 in its top 50) and with ZBSearch on 25 (79 in its top 50);
scoring the title as a second BM25F field puts the current build at 46 and 44, and
at 94 and 79 in the other engine's top 50. Where the query is discriminative the
lists coincide (`python`: 5/5 with pagefind and 4/5 with ZBSearch; `snippet`: 4/5
with both; `latency`: 4/5 with pagefind, 3/5 with ZBSearch); where it matches
hundreds of documents the ordering diverges (`search`: 3/5 with pagefind, 0/5 with
ZBSearch; `index`: 1/5 and 0/5; `postgres`: 0/5 and 3/5; `cache`: 0/5 with both).
pagefind reports 9,705 matches over the 20 queries (485 per query on average), so a
top-5 comparison is mostly a tie-break comparison.

This section measures overlap, not quality: no relevance judgments were collected, so
it says the rankings differ less than they used to, not that one is better. The
mechanisms are different by design — rustsearch's BM25F is `k1 = 1.2`, `b = 0.75`
over the page body plus a default 2× weight on the title, pagefind scores fragments
and weights page metadata, ZBSearch scores per property with field boosts available.

## The same site, one document per page

The 10,000 documents above are chunks of 990 pages, so no engine ever sees a page.
Point the same three at those 990 pages — one HTML file each, 1.62 MB of HTML, the
shape a static site actually deploys — and every engine indexes the same unit:

| | |
|---|---:|
| pages | 990 |
| HTML | 1,694,189 bytes (1.62 MiB) |
| unique terms, rustsearch `--stemmer en` | 8,102 |
| unique terms, ZBSearch (`language: 'english'`) | 14,411 |
| words, pagefind (`--force-language en`) | 14,214 |

Build, median of three:

| metric | rustsearch | pagefind | ZBSearch |
|---|---:|---:|---:|
| wall time | **0.07 s** (0.07–0.07) | 0.86 s | 0.79 s |
| peak RSS | **25 MiB** | 121 MiB | 180 MiB |
| output | 1,472,687 B in 1 file | 2,109,091 B in 1,027 files | 2,345,194 B in 80 files |

At this size the page phase is 990 pages of 1.6 KB: the parallel build still spends
most of its 0.07 s in the write side (postings and serialization), and its peak RSS
is only a few MiB above the single-threaded build's — that is the workers' caches,
not the documents.

The shard set is 1,818,083 B in 11 parts — 661 bytes over the chunked artifact,
since the manifest and the part headers are all the split adds — and it brings the
cold query down to 589,123 B, 32.4% of the chunked artifact (1,817,422 B) and 40.0%
of the one-document artifact: about 583 KB of metadata parts and manifest, the rest
the text blocks of the five hits, against a 1.14 MB text section that is never
fetched whole.

| | rustsearch (single artifact) | rustsearch (shards, `Range`) | pagefind | ZBSearch |
|---|---:|---:|---:|---:|
| bytes to first 5 results | 1,472,687 | 589,123 | **113,330** | 418,555 |
| requests | 1 | 18 | 9 | 7 |
| time | 11.0 ms | 13.1 ms | 127.1 ms | 32.2 ms |
| warm p50 | **0.095 ms** | 0.095 ms | 1.161 ms | 0.131 ms |
| warm p95 | 0.258 ms | 0.247 ms | 3.390 ms | 0.660 ms |
| warm bytes / requests per query | 0 / 0 | 0.10 KB / 0.45 | 1.2 KB / 0.25 | 3.4 KB / 0.12 |

Top-5 agreement, same 20 queries, five pages each:

| | pagefind | ZBSearch |
|---|---:|---:|
| shared | 69 of 96 (71.9%) | 65 of 96 (67.7%) |
| … inside their top 50 | 95 of 96 | 93 of 96 |

That is the number the chunked corpus hides. When all three rank the same unit,
rustsearch and pagefind put the same page in the top five almost three times out of
four, against 46% on the chunked corpus, and ZBSearch sits at 67.7% against 44%. Part
of the gain is the corpus — a 1.7 KB page (on average) gives a term's importance
something stable to be measured against — and part is that a top-5 list cannot
disagree by picking a different paragraph of the same page. A third of the slots are
still different pages; "different page" is simply the only thing left to measure.

## Page-level agreement

That section indexes the site one document per page; this one indexes it the other
way round — a page split into several documents, which is what rustsearch does when
a page is too long to score as one unit — and collapses them again at query time:

```bash
node tools/make-corpus.mjs /tmp/cmp/pages --target 990 --chunk 100000   # 990 pages, 1.62 MB
rustsearch build /tmp/cmp/pages -o /tmp/cmp/pages.rsx --stemmer en --max-page-chars 500
```

The artifact holds **3,075 documents for 990 URLs** (2–7 chunks per page, ~3 on
average), and `--group` collapses them *before* `limit`, so a query returns one hit
per page with the chunk count in `matches`. Top-5 agreement on that corpus, same 20
queries, five hits each, pagefind and ZBSearch returning pages as they always do:

| | pagefind | ZBSearch |
|---|---:|---:|
| rustsearch **grouped by page** | 50 of 96 (52.1%) | 50 of 96 (52.1%) |
| … inside their top 50 | 94 of 96 | 91 of 96 |
| rustsearch ungrouped (five chunks per hit list) | 69 of 96 (71.9%) | 65 of 96 (67.7%) |

The ungrouped row is the one grouping removes: five chunks of the *same* page count
as five agreements with an engine that returns that one page, so the number is high
for a reason that has nothing to do with ranking. Grouped, the five hits are five
distinct pages — a harder comparison, and the one the document-level table above
cannot give. Grouping costs about 0.02–0.03 ms: 0.095 → 0.116 ms through the single
artifact (0.094–0.102 → 0.114–0.140 across the three runs) and 0.095 → 0.121 ms
through the shard path (0.092–0.097 → 0.112–0.128), inside the run-to-run spread of
the ungrouped number itself. It fetches nothing extra: the page identity is the URL
offset in the record, so a `Range` client groups without reading a string, and the
fetch budget stays at `limit` text blocks — through the shard path, the query reads
589,237 B in 18 range requests grouped against 589,123 B in 18 ungrouped, because
the 15 chunk matches become 5 pages before anything is rendered.

## At 100,000 documents

Ten times the documents. The corpus is generated rather than sampled from real prose
(`tools/make-corpus-100k.mjs`): 100,000 documents of ~850 characters with the same HTML
shape as above — the same `<html lang>` and `<meta name="section">` head, three
sections per generated page, each written to its own file — and a vocabulary drawn
with Zipf-shaped weights from the markdown in the tree, so the term distribution is
realistic even though the sentences repeat. The shape is the 10k corpus's; only the
scale is new.

The generator's choices are seeded, but its vocabulary is whatever markdown is on
disk, so re-running it after editing these documents gives a statistically identical
corpus rather than the same bytes. Every number below belongs to the corpus built on
2026-10-29; the ratios are what the section is about, and they move by well under the
run-to-run spread when the vocabulary shifts by 3%.

| | |
|---|---:|
| documents | 100,000 |
| HTML indexed | 78,392,213 bytes (74.8 MiB) |
| unique terms, rustsearch `--stemmer en` | 67,514 |
| unique terms, ZBSearch (`language: 'english'`) | 111,372 |
| words, pagefind (`--force-language en`) | 203,182 |

### Feasibility: the build fits, pagefind barely

Checked before running anything, because this is the size where an indexer can run out
of memory instead of finishing later: 15.9 GB of RAM on this host, 10.0 GB free when
the runs started.

| | rustsearch | pagefind | ZBSearch |
|---|---:|---:|---:|
| build peak RSS | **737 MiB** | 6,222 MiB | 1,824 MiB |
| wall time | **4.17 s** | 55.96 s | 50.30 s |
| output | 78,730,166 B in 1 file | 76,164,461 B in 100,820 files | 101,925,913 B in 6,864 files |

All three fit, each build run on its own. Peak RSS is the median of three runs, from
the `Maximum resident set size` line of `/usr/bin/time -v`. pagefind is the binding
constraint: 6,222 MiB is 39% of this machine and 7.1× the 872 MiB it needs for the 10k
corpus of the same shape, i.e. it grows roughly linearly, so a 250,000-document site
would need ~15.5 GiB. rustsearch stays flat enough (0.72–0.74 GB at 100k against 126
MiB at 10k) that its next limit is the 4 GiB format, not the machine; ZBSearch's
1.82 GB is the corpus it holds as JS strings before it starts indexing.

The rustsearch build time is the median of three; pagefind 55.96 s (55.75–56.18),
ZBSearch 50.30 s (50.06–50.32), rustsearch 4.17 s (4.12–4.24) for the v5 chunked
build. At this size the rustsearch build is 13× faster than pagefind's and 12×
faster than ZBSearch's, on 8 workers and 0.74 GB instead of 6.2 GB — the same shape
as the 10k table (42 µs per document against 560 and 503 µs).

### Output size

| | rustsearch (v5, 64 KiB chunks) | pagefind | ZBSearch |
|---|---:|---:|---:|
| total | 78,730,166 B | **76,164,461 B** | 101,925,913 B |
| page text | 58,020,472 B (LZ4 blocks) | 47,970,706 B (100,000 fragments) | 72,617,537 B (6,250 JSON fragment files) |
| index | 9,351,378 B (148 chunks) | 26,913,994 B (807 chunks) | 26,917,544 B (612 postings shards) |
| dictionary | — (inside the chunks) | — (inside the chunks) | 2,318,636 B |
| title postings | 1,077,722 B (inside `meta`) | — (folded into the chunks) | — (properties, not a second list) |
| per-document data | 11,358,316 B (meta 3,691,646 + strings 4,466,670 + records 3,200,000) | 634,433 B (entry table + language metas) | 72,196 B (manifest) |
| taxonomy | — (inside `meta`) | — (no filter field on this corpus) | — (not indexed) |
| runtime / UI | — | 645,327 B (WASM 140,233 + JS 441,271 + CSS 63,648) | — |
| all files, `gzip -9` | **42,413,343 B (53.9%)** | 80,403,073 B (105.6%) | 48,812,336 B (47.9%) |

At 100k rustsearch is 3.4% larger than pagefind on disk and 23% smaller than ZBSearch,
and the ranking flips on the wire: gzipped, rustsearch is 42.4 MB against pagefind's
80.4 MB (pagefind's files are already compressed, so gzip grows them 5.6%) and
ZBSearch's 48.8 MB (15% more than rustsearch). The ordering is the 10k one, with every
gap wider: rustsearch is the only engine that ships its text as compressed blocks, so
at 78 MB of HTML its artifact grows 6.6× while pagefind's bundle grows 5.9× and
ZBSearch's 5.4×, and the uncompressed row is the only one where pagefind is ahead.

The shapes diverge more than the totals: 1 file against 100,820 against 6,864. A
100k-document pagefind deploy is 100k fragment objects; rustsearch's index is one
object, or 153 in the shard set (`meta`, `docrecords`, 148 chunks, `strings`, `texts`,
and the manifest).

### Cold start (one query, nothing cached)

| | rustsearch (single artifact) | rustsearch (shards, `Range`) | pagefind | ZBSearch |
|---|---:|---:|---:|---:|
| bytes to first 5 results | 78,730,166 | 4,095,847 | **752,975** | 2,529,009 |
| requests | 1 | 18 | 9 | 9 |
| time | 373.5 ms | 362.7 ms | 2,061.1 ms | **241.5 ms** |

pagefind is still the cheapest first answer (753 KB in 9 requests), but the gap has
narrowed from 14.3× at 10k to 5.4×: its bundle grows faster than rustsearch's queried
subset. Of rustsearch's 4.10 MB, **3.70 MB is the `meta` object plus manifest** —
directory, BM25 lengths, facets, both hash tables, the record page table and the title
chunks, one contiguous run — plus one 63,680-byte term chunk, five 65,536-byte record
pages (one per hit), 223 bytes of strings and 2,147 of text. That is the trade made there: nine metadata round trips become one, at the cost of
carrying the hash tables whether the query needs them or not. On a link where a round
trip costs more than 3 MB, it wins; on a fast one, pagefind's 753 KB is still smaller.
That paragraph measures the shard layout before the chunked rework: on the same surface today
the shard set reads 1,013,335 bytes in 29 requests (see the re-run below), because
the hash tables and the title lists are no longer part of the cold read.

What the other two fetch is visible in their own shims. ZBSearch's 2,529,009 B is the
manifest (72,196), the **whole dictionary** (2,318,636 — it preloads it, which is why
it is fastest on time), the postings shards it needs and the five hit fragments.
pagefind's 752,975 B is 634,433 of page-entry metadata, 72,209 of WASM, one index
chunk and five fragments; its 2,061 ms is instantiating that WASM and decoding the
100k-entry table, against 159 ms at 10k, so this is the row that scales
worst with the corpus — and the one that a cache (the metadata and the WASM are
immutable) fixes on the second visit.

### Warm query latency

20 queries × 30 rounds each, top 5 results with snippets, median of three harness runs
(the byte and ranking numbers below are identical in all three):

| | p50 | p95 | mean |
|---|---:|---:|---:|
| rustsearch, single artifact | **0.726 ms** | 2.031 ms | 0.922 ms |
| rustsearch, shards (local bucket) | 0.852 ms | 2.260 ms | 1.029 ms |
| ZBSearch | 10.255 ms | 32.978 ms | 12.288 ms |
| pagefind | 246.674 ms | 1,376.626 ms | 371.039 ms |

Steady-state cost per query, after each engine's caches are warm (600 queries):

| | rustsearch (shards) | pagefind | ZBSearch |
|---|---:|---:|---:|
| bytes fetched per query | 5.4 KB | **1.3 KB** | 3.4 KB |
| requests per query | 0.4 | 0.2 | 0.2 |

The aggregate hides that the cost is per *match*. At 100k pagefind reports 108,670 matches over the 20
queries (5,433 per query, against 485 at 10k), yet its p50 grows 49× (5.06 → 246.7 ms)
because it materialises a JS result object for every match before the limit applies.
rustsearch's p50 grows 2.9× (0.248 → 0.726 ms): the artifact is 6.6× bigger, ranking
is a dense `f32` array indexed by doc id, and the snippets come from five attached
blocks. ZBSearch grows 13× (0.79 → 10.26 ms), the same candidate growth seen through
per-property shards (26 of 612 shards and 923 fragments fetched over the 600-query
session).

Per matching document that is roughly 45 µs for pagefind at 100k against 0.13 µs for
rustsearch — two to three orders of magnitude — because the scoring loop is not the
same shape. A site whose queries are selective would not notice; a site whose visitors
type the common words of its domain pays a quarter of a second per query at this
corpus size.

This is the row that says pagefind is designed for a site, not a corpus: 247 ms per
query at 100k documents is a product decision (it also ships the UI, 30+ languages and
the metadata), and it is the only number in this file that would change a deployment
choice on its own.

### Ranking agreement

| | pagefind | ZBSearch |
|---|---:|---:|
| rustsearch's top 5, shared | 23 of 100 (23%) | 22 of 100 (22%) |
| rustsearch's top 5, inside the other engine's top 50 | 44 of 100 | 45 of 100 |

Lower than the 10k table (46% / 44%), and for a reason that is about the corpus rather
than the engines: a common query term matches thousands of 100,000 near-identical
synthetic documents, so its top five is a tie-break among documents with the same
length and the same word distribution. The top-50 column is the one to read here —
almost half of rustsearch's five hits are inside both engines' top 50, i.e. the
engines agree on which documents are relevant and disagree on the order of the ones
that tie. At 10k the same columns were 94/100 and 79/100. Nothing in this table says
one ranking is better; no relevance judgments exist for a generated corpus, and
inventing them would only measure the generator.

### What is different at this size

* **The build, not the query, is where memory goes.** 6.2 GB for pagefind against
  0.74 GB for rustsearch and 1.8 GB for ZBSearch, for the same 78 MB of HTML.
* **Files, not bytes, are the deployment cost.** 100,820 objects for pagefind against
  1 for rustsearch and 6,864 for ZBSearch; on R2 or S3 that is 100k PUTs, 100k cache
  entries and a listing an order of magnitude larger than the payload.
* **The query ceiling moved.** 0.54 ms against 10.8 ms against 194 ms is no longer a
  difference a network hides: at 100k, pagefind's client spends longer ranking a
  common word than a Worker spends fetching the 1 MB a rustsearch shard query reads
  (1,013,335 bytes in 29 requests).
* **rustsearch's cold bytes are its metadata.** In the measurement above, 3.70 MB of
  the 4.10 MB is the group that replaced nine round trips; the dictionary, the records
  and the text it actually reads are 0.40 MB of it. That trade has since been paid
  down: on the current shard layout (the re-run below) the same 100k cold query is
  **1,013,335 bytes in 29 requests** — 4× fewer bytes for 11 more requests — because
  the hash tables and the title lists left the cold read.
  Metadata is the bulk again at 1,000,000 documents, where it is **6.11 MB of the
  6.52 MB** cold query: that is the size at which fewer bytes still costs more
  requests, and the first thing to revisit if a site would rather have the bytes.
* **Nothing here was tuned for the 100k run.** Same flags, same defaults, same queries
  as the 10k tables: `--stemmer en` for rustsearch, `--force-language en` for pagefind,
  `language: 'english'` for ZBSearch.

## Re-run at 10k, 100k and 1M: the sharded layout as the default

Everything above was measured before the chunked rework and H7. Those
changes moved what a cold query reads — the two hash tables are entry-addressed
objects, the BM25 lengths are half the bytes, each chunk's title list is its own
object, and a fuzzy plan is capped at 64 chunks — so the shard set, not the single
artifact, is the shape to compare at this size. The tables below are that re-run on
the seeded corpus, with rustsearch measured **sharded**.

Sharded is the measurement configuration, not the build default. `rustsearch build`
still writes the flat artifact — `term_chunk_bytes` is 0 unless `--term-chunk-bytes`
is passed — and `rustsearch shard` is a separate step that writes the manifest and
the parts. The sharded default is a planned profile (2026-10-30), so a
plain `build` today produces the single artifact measured in the sections above.

Corpus: `tools/make-corpus-100k.mjs`, vocabulary from this repository's markdown,
documents of ~850 characters, same HTML shape (`<html lang>`,
`<meta name="section">`, three documents per page), at 10,000, 100,000 and 1,000,000
documents; the 1M corpus is four 250k shards generated in parallel into four
subdirectories (seeds 1000–1003), which every engine walks recursively. 20 queries,
top 5 with snippets, 30 rounds (10 at 1M). One harness run per size on 2026-10-30 and
one `/usr/bin/time` run per build; the tables above remain the median-of-three
reference for the prose corpus.

| | 10k | 100k |
|---|---:|---:|
| documents | 10,000 | 100,000 |
| HTML | 7,887,719 B | 78,932,487 B |
| rustsearch artifact | 7,712,136 B | 75,016,979 B |
| rustsearch shard set | 7,714,044 B / 29 parts | 75,028,032 B / 162 parts |
| pagefind bundle | 7,930,225 B / 10,121 files | 71,869,890 B / 100,692 files |
| ZBSearch bundle | 10,230,431 B / 697 files | 100,548,222 B / 6,818 files |

### Build

| metric | rustsearch | pagefind | ZBSearch |
|---|---:|---:|---:|
| 10k wall / peak RSS | **0.47 s / 87 MiB** | 5.25 s / 625 MiB | 3.42 s / 393 MiB |
| 100k wall / peak RSS | **5.06 s / 702 MiB** | 55.30 s / 5,944 MiB | 35.15 s / 1,684 MiB |

### Cold start (one query, first five results)

| | rustsearch (single artifact) | rustsearch (shards) | pagefind | ZBSearch |
|---|---:|---:|---:|---:|
| 10k bytes / requests | 7,712,136 / 1 | 328,502 / 27 | **161,723 / 9** | 383,362 / 8 |
| 10k time | 35.7 ms | 46.7 ms | 160.8 ms | **34.4 ms** |
| 100k bytes / requests | 75,016,979 / 1 | 1,013,335 / 29 | **771,190 / 10** | 1,826,812 / 8 |
| 100k time | 341.3 ms | 287.3 ms | 611.1 ms | **157.5 ms** |

The 100k cold start is the number §20–§22 moved: 4,095,847 bytes in 18 requests in
the section above, 1,013,335 bytes in 29 now. Requests went up — the metadata run is
cut around the hash tables and the title lists, and a rendered hit reads one 8-byte
entry per table — while the bytes fell 4×. Against pagefind the byte gap at 100k is
now 1.3× (it was 5.4×), and rustsearch reaches the first result faster than pagefind
at both sizes once pagefind's WASM instantiation is in the count. At 10k the shard
set still costs 27 requests for 328 KB against pagefind's 9 for 162 KB: at that size
the routing tables and the per-hit entries are a larger share of the answer than the
metadata they save.

### Warm query latency

20 queries × 30 rounds, top 5 with snippets, one harness run:

| | rustsearch (single artifact) | rustsearch (shards) | pagefind | ZBSearch |
|---|---:|---:|---:|---:|
| 10k p50 / p95 | 0.118 / 0.389 ms | 0.141 / 0.434 ms | 4.015 / 50.792 ms | 0.987 / 5.774 ms |
| 100k p50 / p95 | 0.514 / 2.146 ms | 0.539 / 2.097 ms | 194.440 / 4,144.090 ms | 10.845 / 56.336 ms |

Steady-state cost per query, after each engine's caches are warm (600 queries):

| | rustsearch (shards) | pagefind | ZBSearch |
|---|---:|---:|---:|
| 10k bytes / requests | 1.2 KB / 0.62 | **0.69 KB / 0.18** | 2.6 KB / 0.17 |
| 100k bytes / requests | 5.6 KB / 0.69 | **1.3 KB / 0.19** | 3.2 KB / 0.19 |

### Ranking agreement

| | pagefind | ZBSearch |
|---|---:|---:|
| 10k, rustsearch's top 5 shared | 57 of 95 (60%) | 38 of 95 (40%) |
| 10k, … inside their top 50 | 80 of 95 | 65 of 95 |
| 100k, rustsearch's top 5 shared | 34 of 95 (35.8%) | 22 of 95 (23.2%) |
| 100k, … inside their top 50 | 56 of 95 | 58 of 95 |

One query (`kubernetes`) returns zero hits in all three engines on this corpus, which
is why the denominator is 95 rather than 100. The 100k drop is the generated corpus
rather than the ranking: a common term matches thousands of near-identical documents,
so a top-5 list is a tie-break, and the top-50 column is the one that shows the
engines agree on the documents and disagree on the order.

### At 1,000,000 documents

1,000,000 documents, 789,039,324 bytes of HTML, 98,157 terms, 72,171,082 tokens. Ten
rounds per query instead of thirty.

| | rustsearch (shards) | pagefind | ZBSearch |
|---|---:|---:|---:|
| bundle | 737,572,858 B (703.4 MB) in 1,142 objects | **not built** | **not run** |
| build wall / peak RSS | **77.6 s / 6,558 MiB** | 298.0 s / 12,206 MiB, killed | — |
| cold: bytes / requests | **6,524,644 B / 33** | — | — |
| cold: ms | 1,486.8 | — | — |
| warm p50 / p95 | 6.558 / 25.742 ms | — | — |
| warm bytes / requests per query | 37.4 KB / 2.3 | — | — |
| queries with 0 hits | 1 of 20 | — | — |

pagefind reached `[Parsing files]` and was killed after 298 s with 12.5 GB resident:
at 100k the same build needed 5.94 GB and it grows linearly, so 1M needs roughly 58 GB
on a 15.9 GB host. ZBSearch was not run at 1M at all — its 100k build peaked at
1.68 GB, which scales to about 16 GB, above what the host has — so that cell is a
projection, not a measurement. The 1M row is therefore rustsearch alone, and it is
the row that shows what the shard layout buys at the size it was designed for: a
cold query reads 6.52 MB, i.e. 6.11 MB of metadata, one 64 KiB term chunk, one record
page and the five hits' blocks, against a 737 MB artifact, in 33 requests. The full
artifact would have been 703 MB in one response.

Scaling from 100k: the artifact grows 9.8× (75.0 MB → 737.5 MB), the cold read grows
6.4× (1.01 MB → 6.52 MB) because the metadata is a few bytes per document while the
text and strings are only read for the hits, and the warm p50 grows 12× (0.54 →
6.56 ms, the ranking now touching a million-document score buffer).

## What the numbers do not say

The bullets below are about the 10k tables unless they say otherwise; §"At 100,000
documents" ends with its own.

* **Network latency.** Every byte is served from local disk through a `fetch` shim
  (pagefind, ZBSearch) or an in-memory bucket (rustsearch shards). Behind a CDN each
  request is a round trip that HTTP/2 multiplexes; a Worker reading R2 with `Range`
  pays 20–50 ms per batch. The cold numbers are CPU plus local reads, not wall-clock
  in production, and pagefind's 9 requests and ZBSearch's 9 would each cost a round
  trip on a slow link.
* **The corpus is chunked, not pages.** Each of the 10,000 documents is a ~1.5 KB
  slice of a page, which is the granularity rustsearch is pointed at, so pagefind and
  ZBSearch also index 10,000 "pages" of 1.5 KB and compute their per-page statistics
  on fragments. Pointed at the site they would index 990 pages of 1.7 KB on average,
  with different scores and different fragment sizes; the two sections after "Ranking
  agreement" are that arrangement, measured — one with a page per document, one with
  a page split and regrouped. The `lang`/`section` head added for the filter section
  is 47 bytes per page and enters no engine's scored text.
* **Runtime code is counted only when the build writes it.** pagefind's directory
  includes its WASM and UI (645 KB); rustsearch's artifact does not include the
  234 KB module a page loads, and ZBSearch's does not include the `zbsearch` client
  the page bundles. Subtracting pagefind's runtime leaves 12.32 MB, still 3.2% more
  than rustsearch's whole artifact.
* **Features.** pagefind ships a UI, 30+ languages, filters, sorts, metadata and a
  browser-first API. ZBSearch ships filters, `where` clauses, boosts, typo tolerance,
  per-property indexes and its own plugins. rustsearch ships an index format, a CLI,
  a Worker adapter and a `Range` path, with English and Italian stemming only, one
  title field, filters on build-time taxonomies, grouping of a page's documents and
  no token positions. This is an engine comparison, not a product one.
* **Memory at query time.** The whole-artifact path holds all 11.9 MB in the WASM
  heap; the sparse path holds ~2.4 MB plus the blocks it has attached. Neither is
  measured here beyond the cold byte counts.
* **Configuration.** pagefind ran with `--force-language en` and its default search
  options; ZBSearch with `{ term, limit }` — prefix expansion on, typo tolerance 0,
  no field boosts, its documented defaults; rustsearch with `--stemmer en` and its
  default `SearchOptions` (prefix expansion on, fuzzy off, title boost 2.0).

## Method

```bash
# corpus: this repository's markdown, 10,000 documents of <= 850 characters,
# each carrying <html lang> and <meta name="section">
node tools/make-corpus.mjs /tmp/cmp/site --target 10000 --chunk 850 --facets

# rustsearch: one artifact and the shard set
cargo build --release -p rustsearch-cli
./target/release/rustsearch build /tmp/cmp/site \
  -o /tmp/cmp/index.rsx --stemmer en
./target/release/rustsearch shard /tmp/cmp/index.rsx \
  -o /tmp/cmp/shards --prefix index/ --plan "index" -n 5

# pagefind 1.5.2, into its own directory so the corpus stays clean
npm install --prefix /tmp/pf pagefind@1.5.2
/tmp/pf/node_modules/.bin/pagefind --site /tmp/cmp/site \
  --output-path /tmp/cmp/pf --force-language en

# ZBSearch 4.0.1, through the client it ships to browsers
node tools/zbsearch-build.mjs /tmp/cmp/site /tmp/cmp/zb

# all three, 20 queries x 30 rounds, plus the ranking pass
cargo build -p rustsearch-wasm --target wasm32-unknown-unknown --release
wasm-bindgen --target nodejs --out-dir wasm/pkg-node \
  target/wasm32-unknown-unknown/release/rustsearch_wasm.wasm
node tools/compare-engines.mjs \
  --site /tmp/cmp/site \
  --index /tmp/cmp/index.rsx \
  --shards /tmp/cmp/shards \
  --pagefind /tmp/cmp/pf \
  --zbsearch /tmp/cmp/zb \
  --iterations 30

# filtered latency and bytes
# the limit-1 ranking pass is 300 calls through the same searchJsonWithOptions
# entry point; wasm/bench.mjs is its limit-10 equivalent.
node wasm/bench.mjs /tmp/cmp/index.rsx 500 "index" --filter section=crewAI
node adapters/cloudflare/verify.mjs /tmp/cmp/shards 5 "index" --filter section=crewAI

# page-level agreement: pages split at build time, grouped at query time
node tools/make-corpus.mjs /tmp/cmp/pages --target 990 --chunk 100000
./target/release/rustsearch build /tmp/cmp/pages -o /tmp/cmp/pages.rsx \
  --stemmer en --max-page-chars 500
./target/release/rustsearch shard /tmp/cmp/pages.rsx -o /tmp/cmp/pages-shards --prefix index/
/tmp/pf/node_modules/.bin/pagefind --site /tmp/cmp/pages \
  --output-path /tmp/cmp/pages-pf --force-language en
node tools/zbsearch-build.mjs /tmp/cmp/pages /tmp/cmp/pages-zb
node tools/compare-engines.mjs --site /tmp/cmp/pages --index /tmp/cmp/pages.rsx \
  --shards /tmp/cmp/pages-shards --pagefind /tmp/cmp/pages-pf \
  --zbsearch /tmp/cmp/pages-zb --iterations 20 --group
# and the same corpus ungrouped, for the row grouping removes
node tools/compare-engines.mjs --site /tmp/cmp/pages --index /tmp/cmp/pages.rsx \
  --shards /tmp/cmp/pages-shards --pagefind /tmp/cmp/pages-pf \
  --zbsearch /tmp/cmp/pages-zb --iterations 20

# the fetch budget of a grouped query through the shard path
node adapters/cloudflare/verify.mjs /tmp/cmp/pages-shards 5 "index" --group

# --- 100,000 documents (§"At 100,000 documents") ---
# seeded, so the corpus is byte-identical between runs
node tools/make-corpus-100k.mjs /tmp/cmp100k/corpus --target 100000 --chunk 850

# rustsearch: the chunked v5 build and its shard set
./target/release/rustsearch build /tmp/cmp100k/corpus \
  -o /tmp/cmp100k/r100-64k.rsx --stemmer en --term-chunk-bytes 64k
./target/release/rustsearch shard /tmp/cmp100k/r100-64k.rsx \
  -o /tmp/cmp100k/sh100k --prefix index/ --plan "index" -n 5

# pagefind 1.5.2, into its own directory so the corpus stays clean
/tmp/pf/node_modules/.bin/pagefind --site /tmp/cmp100k/corpus \
  --output-path /tmp/cmp100k/pf --force-language en

# ZBSearch, the same client shape as the 10k run
node tools/zbsearch-build.mjs /tmp/cmp100k/corpus /tmp/cmp100k/zb

# the comparison itself: 20 queries x 30 rounds, three runs, medians in the tables
node tools/compare-engines.mjs --site /tmp/cmp100k/corpus \
  --index /tmp/cmp100k/r100-64k.rsx --shards /tmp/cmp100k/sh100k \
  --pagefind /tmp/cmp100k/pf --zbsearch /tmp/cmp100k/zb \
  --iterations 30
```

The 10k section, the page-level sections and the 100k section were all re-run on
2026-10-29. The 10k and page-level tables are medians of three harness runs; the
100k table is the median of three runs too, and its byte and ranking rows were
identical in all three. Build wall times and peak RSS come from
`/usr/bin/time -f 'wall=%e rss=%M'` around each of the three build commands, median of
three runs, all three re-measured in one session for this revision. The rustsearch
build runs the page phase on `available_parallelism()` workers. The `gzip -9` row is
every file of a bundle compressed separately and summed — what a CDN would serve, not
what an archive of the directory would weigh.

`tools/compare-engines.mjs` drives pagefind through `pagefind.js` and ZBSearch through
`createStaticSearchClient` with a memory transport, so both execute the code and read
the files a browser would, minus the network; rustsearch runs through `wasm/pkg-node`,
the same WASM a browser or a Worker downloads. Latencies are `hrtime` around the call
that returns five fully rendered results; the ranking pass (top 50 per query) runs
after the timed loops so its fragment reads do not pollute the samples. The filter
ranking table is one call per sample to `Index::try_search_with_filters`, filter
resolution included, 300 samples per row.

`tools/zbsearch-build.mjs` exists because `@zbsearch/static` has no CLI: it reads the
corpus with `tools/corpus-records.mjs`, calls `buildStaticIndex` with the same
`{ title, content }` schema the ZBSearch docs use, and writes the file set.
