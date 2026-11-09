# rustsearch playground

A static page for trying [`rustsearch`](../../README.md) in a browser: type a query,
see BM25F hits, snippets with highlights, facet filters, typo tolerance, title
boost and page grouping. No server-side search, no framework, no build step for
the UI itself — the engine runs as WASM against a format-v5 shard set by default,
or against one whole `.rsx` artifact (`?index=`).

This directory is deliberately a leaf of the workspace: the engine and its tests do
not depend on the UI, and the UI does not change the artifact format. It is a
consumer of the public boundary (`searchJsonWithOptions`, `facetsJson`, the
statistics getters), which is exactly the surface a future `<rustsearch-search>`
component would wrap.

## Starting the UI

Run these commands from the repository root:

```sh
cd adapters/ui
./setup.sh          # first run, and after engine changes
./serve.sh          # http://127.0.0.1:8765/
```

Open <http://127.0.0.1:8765/>. `serve.sh` binds to localhost and serves this
directory over HTTP, because the page fetches the WASM module and the `.rsx`
artifact: opening `index.html` from a `file://` URL will not work.

`setup.sh` is the only build step. It produces the WASM module, the demo corpus,
a format-v5 `demo/index.rsx` and the shard set `demo/shards/`; afterwards
`./serve.sh` is enough unless the engine changes. `serve.sh` exits with an error
if `demo/shards/manifest.txt` is missing. Press Ctrl+C to stop the server, or
pass a port to use something other than 8765:

```sh
./serve.sh 9000     # http://127.0.0.1:9000/
```

The page loads `demo/shards/manifest.txt` automatically (the address bar gains a
`?shards=` parameter). To compare with the whole artifact, open
<http://127.0.0.1:8765/?index=demo/index.rsx>. If the index cannot be loaded,
check that you opened the HTTP URL rather than `file://`, then click **Load
index…** and select `demo/index.rsx`.

`setup.sh` needs `cargo`, the `wasm32-unknown-unknown` target, `wasm-bindgen`
(`cargo install wasm-bindgen-cli`) and `node`; `serve.sh` needs `python3`.
`setup.sh` writes:

| path | what |
|---|---|
| `vendor/pkg-web/` | `wasm-bindgen --target web` output, loaded by `core/engine.worker.js` |
| `demo/site/` | one HTML page per Markdown section in the repository, plus the 8 curated pages |
| `demo/index.rsx` | format v5, `--stemmer en`, facets, `--max-page-chars 2117`, `--term-chunk-bytes 64k`; **~10,000 documents**, 25 chunks |
| `demo/shards/` | `demo/index.rsx` published by `rustsearch shard` (manifest, `meta`, `docrecords`, one object per chunk, `strings`, `texts`); what the page loads |

The demo size follows the Markdown under `SEARCHUI_CORPUS_ROOT` (the workspace by
default). `SEARCHUI_DOCS` caps the total
number of demo pages (repository plus curated, default 9350),
`SEARCHUI_MAX_PAGE_CHARS` controls page splitting (default 2117), and
`SEARCHUI_TARGET_DOCS` asks setup to hit a document count directly; it needs a
corpus large enough to reach it.

### Choosing the demo size

The number in the footer is the number of **documents in the index**, not the
number of HTML pages. The direct way to choose it is:

```sh
cd adapters/ui
SEARCHUI_TARGET_DOCS=5000 ./setup.sh
./serve.sh
```

The curated pages are included in the count. When the target is below the number of
available pages, setup keeps enough pages and disables splitting; above it, setup
searches for the split threshold that lands exactly on the target. If no threshold
can hit the number exactly, setup exits with an error instead of silently rounding.

For finer control, the two underlying variables are:

| variable | default | effect |
|---|---|---|
| `SEARCHUI_CORPUS_ROOT` | the workspace | directory whose Markdown is rendered into the demo corpus |
| `SEARCHUI_TARGET_DOCS` | unset | exact number of documents to aim for; chooses the page cap or split threshold |
| `SEARCHUI_DOCS` | `9350` | maximum total HTML pages in the demo, curated included |
| `SEARCHUI_MAX_PAGE_CHARS` | `2117` | pages longer than this are split into several documents; `0` disables splitting |

To reproduce the 10,000-document demo, point the corpus at the larger parent
workspace (it holds more Markdown than this repository):

```sh
cd adapters/ui
SEARCHUI_CORPUS_ROOT="$PWD/../../.." ./setup.sh
```

With `SEARCHUI_MAX_PAGE_CHARS=0`, all hits have `matches: 1` and **Group by page**
has nothing to collapse. Because the count depends on the source pages, run
`rustsearch info demo/index.rsx` after changing them and use
`SEARCHUI_TARGET_DOCS` if you need an exact number. The 10,000-document demo in
`COMPARISON.md` was built from the larger parent workspace, not from this repository
alone.

The corpus intentionally has a clickable URL per hit (`--base-url demo/site`), so a
result opens the page it was built from.

### Rebuilding the demo index

`demo/index.rsx` and `demo/shards/` are generated, not committed. Run
`./setup.sh` again after a change to the engine or to the corpus generator: it
rebuilds the WASM module, regenerates `demo/site/` from the repository docs,
writes a new `demo/index.rsx` and re-publishes `demo/shards/`. The UI itself has
no build step.

```sh
cd adapters/ui
./setup.sh
./serve.sh
```

If `serve.sh` is already running, reload the page after `setup.sh`; it serves
the files directly from disk. Changes to `index.html`, `app.js`, `styles.css` or
`core/` need only a reload, not `./setup.sh`. Check the rebuilt artifact with:

```sh
../../target/release/rustsearch info demo/index.rsx
```

## Loading another index

The demo is one `.rsx` artifact, published as a shard set. Build another one from
any directory of HTML pages, from `adapters/ui/`:

```sh
cd adapters/ui
cargo build --release -p rustsearch-cli
../../target/release/rustsearch build /path/to/site -o my-site.rsx \
  --base-url https://example.com --stemmer en
```

`--base-url` is the prefix prepended to every result URL; `--stemmer none|en|it`
selects the analyzer stored in the artifact (default `none`). Keep both consistent
with the collection, because the page does not re-analyze documents at query time.

Then load the artifact in either way:

1. **Local file** — click **Load index…** and select the `.rsx`. The page reads it
   with `File.arrayBuffer()` in the browser; the file is not uploaded.
2. **URL** — put the artifact under `adapters/ui/` and open
   `http://127.0.0.1:8765/?index=my-site.rsx`. A remote URL also works if it is
   CORS-readable and served over `https:`. The page CSP permits `https:` for this
   fetch.

`?index=` is read on load and kept in the address bar, so the resulting URL can be
shared. Build the artifact with the same `rustsearch` revision as the WASM module;
run `rustsearch info` first if the file comes from a different engine version.

### A huge artifact: shards

The page loads a **shard set** by default, so it fetches only what a query needs.
`rustsearch shard` splits the artifact into a manifest plus one object per section
(`DESIGN.md` §7 of the engine docs), and `setup.sh` already runs it:

```sh
cd adapters/ui
../../target/release/rustsearch shard demo/index.rsx \
  -o demo/shards --prefix index/
```

`./serve.sh` serves this set at `?shards=demo/shards/manifest.txt`. The client
fetches the independent metadata slices in parallel, at most six at a time (the
per-origin connection budget; `loadShardIndex` takes `metaConcurrency`, `0` for no
cap), and verifies each one with FNV in the WASM module. The page sends `?q=`
searches to the same engine; what changes is the traffic:

1. the **manifest** and the **metadata** object (`meta`) — header, chunk directory,
   BM25 lengths, facets, both hash tables, the record page table and the title
   chunks (430 KB of the demo's 10.7 MiB);
2. the **dictionary chunks** the query's terms live in and the **record page** of
   each hit, one `Range` request each, kept for later queries;
3. after ranking, the **URL+title** and the **text block** of each rendered hit, as
   one `Range` request per hit into `strings.bin` and `texts.bin` (the 8.5 MiB the
   page never downloads whole).

On the 10,000-document demo a cold five-hit query reads **619 KiB in 15 requests** against
10.7 MiB for the whole artifact, with the same hits. `--meta-part-bytes N` publishes
a metadata run too large for one object as `meta00000`, … and the client reads every
slice. To measure the set against the artifact loaded whole, with the real client and
engine:

```sh
cd adapters/ui
node tools/bench-shards.mjs --flat demo/index.rsx \
  --shards demo/shards --label 10k
# cold-start wall clock over HTTP, with a modelled 20 ms RTT per response:
node tools/bench-shards.mjs --flat demo/index.rsx --shards demo/shards \
  --label 10k --http --http-latency 20 --meta-concurrency 6
```

The footer reports the running totals, so the cost of a query — and the format
version — is visible on the page.

This needs a server that honours `Range`: `./serve.sh` runs `serve.py`, which answers
`206` with `Content-Range`. A host that ignores the header still works — the client
slices the body itself — but then the transfer is the whole object, and the byte
counter says so. The client is the browser port of
`adapters/cloudflare/shard-source.js`: same manifest, same FNV-1a/64 part
hashes, same order of operations, verified in `tests/shards.test.mjs`.

### From a folder of Markdown

`rustsearch build` indexes HTML. The repository ships `tools/make-corpus.mjs`, the
converter `setup.sh` uses for its demo:
it reads every `.md` under the roots, splits each file at headings, and writes one
HTML page per section.

From the repository root, with notes in `~/notes`, after `./setup.sh` has built the
CLI and the WASM module:

```sh
REPO=$PWD
NOTES="$HOME/notes"

# 1. Markdown -> HTML under adapters/ui/demo/md-site/
(cd "$NOTES" && node "$REPO/tools/make-corpus.mjs" \
  "$REPO/adapters/ui/demo/md-site" --roots . --chunk 850 --facets)

# 2. HTML -> one .rsx artifact
cd "$REPO/adapters/ui"
../../target/release/rustsearch build demo/md-site \
  -o demo/md.rsx --base-url demo/md-site --stemmer en

# 3. Check it and load it
../../target/release/rustsearch info demo/md.rsx
```

The example uses `--stemmer en`; use `--stemmer it` for Italian notes, or `none`
for mixed-language content. Open <http://127.0.0.1:8765/?index=demo/md.rsx>.
Result links point at `demo/md-site/doc-NNNNN.html`, which `serve.sh` already
serves.

The converter is a corpus tool, not a full Markdown renderer: it renders fenced code
blocks as `<pre><code>` plus inline code, bold and emphasis, but not links, lists,
tables or blockquotes, and skips files shorter than 400 characters and sections
shorter than 300. Use your static-site generator (Hugo, MkDocs, Jekyll) when you
need real HTML; point `rustsearch build` at its output directory instead.

## What the page exercises

* **Query** — no input debounce: every keystroke runs a query, so the results never
  describe a prefix the user has moved past. The engine lives in a Web Worker, so the
  keystroke itself costs a `postMessage` and the page keeps painting while the query
  runs; at most one query is in flight and the one waiting is replaced by the newer
  one (a superseded call resolves to `null`). The address bar is written once typing
  pauses, because `history.replaceState` is rate-limited by the browser. Form submit,
  `/` focus shortcut, `Escape` to clear.
* **Results** — title, URL, 320-byte snippet with `<mark>` highlights, BM25F score,
  document id, and the number of chunks a grouped hit stands for.
* **Facets** — chips come from `facetsJson()`, counts included. A filter is AND
  across fields and OR inside a field, resolved before ranking.
* **Options** — typo tolerance (`fuzzy`, one edit), page grouping, title boost
  0–10 (2 when the URL says nothing). Each one is a single engine argument; the
  timing is the WASM query plus its JSON parse, both in the worker.
* **Pagination** — `Show 10 more` re-queries with a larger `limit` (the engine has
  no offset; `limit` is capped at 100).
* **URL state** — `?q=`, `?limit=`, `?fuzzy=1`, `?group=1`, `?boost=`,
  `?f=field=value` (repeatable), `?index=`, `?shards=`, `?curate=`; hydrated on load
  and kept with `history.replaceState`.
* **States** — loading skeleton, initial suggestions, empty result with recovery
  actions, and an error card that names the problem and the fix.

## Curation

The playground applies the same editorial file as the CLI (`rustsearch search
--curate FILE`), fetched from `?curate=` and handed to the engine worker:

```
http://127.0.0.1:8765/?curate=curated/curate.txt
```

```
ranking | pin /demo/site/curated-ranking.html
typo | boost /demo/site/curated-typos.html 2
```

`pin` moves a hit the query **already returned** to the front, `boost` multiplies its
score (0.1–10). A pin for a URL the query did not match is ignored — curation
reorders, it never adds a document — and a curated path must be on this site, so the
file cannot point a reader at another origin.

A rule names a site path (`/demo/site/curated-ranking.html`) and the engine matches it
against a hit URL ignoring that one leading slash: the demo artifact is built with
`--base-url demo/site`, so its hits are `demo/site/curated-ranking.html`, and the same
rule file works unchanged on an artifact built with `--base-url /demo/site`. The
engine parses the text with `core/src/rules.rs`, the same implementation the CLI and
the Worker use; the UI only fetches it. The demo rules in `curated/curate.txt` target
the eight hand-written pages, which `setup.sh` copies into `demo/site/`.

The footer is the receipt: it reads `3 curated queries` once the file is in force, and
`curation rejected: line 15: …` when the engine refuses it, while the plain BM25F
ranking keeps working. A malformed file is a configuration error, not a silent
ranking change.

**Seeing the difference.** On the demo, `ranking` returns `curated-ranking.html` at
position 2 and the pin moves it to position 1; `start here` puts it at position 11, so
the pin moves it to the front only once `limit` includes it (the engine curates the
hits it was asked for — click *Show 10 more*); `typo` already returns
`curated-typos.html` first, and the boost is visible in its score (34.7 → 69.4), not in
the order.

Curation is optional: without `?curate=`, or with a 404, the page runs the plain
BM25F ranking. The rules are fetched once per index load, not per keystroke, and
`setCurations("")` drops them again.

## Layout

```
adapters/ui/
├── index.html        page shell; CSP, landmarks, forms and native controls
├── styles.css        design tokens (light/dark), layout, states
├── core/search.js    headless API: openIndex / openSharded -> SearchSession.search() (async)
├── core/engine.worker.js  the WASM engine, off the main thread
├── core/shards.js    shard manifest, Range reads, per-hit attachments
├── core/format.js    pure helpers: URL safety, filter encoding, highlight split
├── app.js            DOM wiring: state, URL, facets, keyboard, rendering
├── curated/          eight hand-written pages copied into the demo corpus
├── tests/            node:test for the helpers, the queue, the shard client and a
│                     real v5 artifact over HTTP Range (tests/fixtures/v5/)
├── tools/bench-shards.mjs  the same client and engine, flat v5 against sharded v5
├── setup.sh          build the WASM module and the demo artifact
├── serve.py          static server with Range support (the shard path needs it)
└── serve.sh          runs serve.py on localhost
```

`core/search.js` is the seam: a component or a framework wrapper can call it
directly, and the worker behind it is an implementation detail — `search()` is a
promise, the statistics and facets arrive once with the handshake, and nothing
about the rendering code knows where the engine runs.

## Safety and accessibility

Design rules came from a consultation with the local design/security corpus
(`askd.sh`): contrast 4.5:1 for text and 3:1 for borders/focus, visible focus
rings, 24 × 24 px minimum targets, five list states, `prefers-reduced-motion`,
labels and declared language.

* **No `innerHTML`.** Every title, URL, snippet and facet value is inserted with
  `textContent` or `createTextNode`. Highlights are built by slicing the
  *UTF-8 bytes* the engine returns and decoding each slice; a range that is out of
  bounds, overlaps or cuts a code point is dropped (tested in
  `tests/format.test.mjs`).
* **Links are validated.** `javascript:`, `data:`, `vbscript:` and any URL with
  control characters are rendered as text, not as an `href`.
* **CSP** without `unsafe-inline`; the only script is the module and the only
  style sheet is `styles.css`. WASM needs `'wasm-unsafe-eval'`.
* **Native controls.** The theme is three states (auto/light/dark, persisted),
  the options are a checkbox, a second checkbox and a range, the facets are
  `aria-pressed` buttons in a labelled group.
* **Results are a list of links, not a fake combobox.** Tab reaches every hit,
  ArrowUp/Down/Home/End move between hits, and the result count is announced
  through a polite live region. A real `role="combobox"` popup would duplicate
  that behaviour and take the link semantics away; the APG contract belongs to
  the future reusable component, not to a results page.

## Limits

* One index at a time. Either the whole artifact or a shard set (`?shards=`); the
  shard path fetches the metadata, then the dictionary chunks the query's terms
  live in, the record page of each hit, and its URL+title and text block. The
  default demo is a v5 shard set; `?index=demo/index.rsx` loads the whole artifact
  instead. A **grouped** query on a chunked shard set reads all record pages before
  ranking, because page identity lives in the record and grouping runs before
  `limit` — the one query shape whose metadata cost is the whole record table.
* Typo tolerance is still opt-in, and "did you mean" is not in the
  JSON yet, so the empty state offers a retry instead of a correction.
* A query already running in the worker cannot be interrupted — the WASM call is
  synchronous there. A newer keystroke replaces the one *waiting*, and the answer
  to a superseded query is discarded, so the UI never shows stale results; hard
  cancellation would mean terminating the worker and reloading the artifact.
* The demo corpus is English prose generated from this repository's docs; the
  page declares `lang="en"` and ships English strings only. An i18n dictionary is
  not implemented.
* No highlighting outside the 320-byte snippet: the engine has no
  `highlight(docId, query)` yet.
