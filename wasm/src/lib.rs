//! The browser/Worker boundary.
//!
//! One exported object, one JSON string. Keeping the boundary this small means the
//! wasm module does not need `serde`, `js-sys` or a panic hook: the caller gets
//! `JSON.parse(engine.searchJson(q, 10))` and a documented schema.
//!
//! The bytes are copied once into the `Engine`; after that a query touches only the
//! index and the result set. Call `Engine.free()` (or let the JS GC do it) when the
//! index is replaced.
//!
//! The shard path adds the per-hit accessors: `textSpan`/`attachTextBlock` for the
//! snippet, and `stringSpan`/`attachStringBlock` for the URL and title. Both
//! attachments verify the bytes against a per-document hash in the header, so the
//! caller never has to trust a bare `Range` response.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use rustsearch_core::{
    facets_to_json, search_report_to_json, search_report_to_json_with_filters, Curations, Filter,
    Index, SearchOptions,
};
use wasm_bindgen::prelude::*;

/// Separators of the `filters` string: a unit separator between a field and its
/// value, a record separator between filters.
///
/// Both are non-printable control characters a build-time facet value cannot
/// contain (`rustsearch-index` stores the site's own bytes, and the URL and meta
/// extractors only ever produce text), so no escaping is needed in either
/// direction. One string parameter instead of an array keeps the wasm-bindgen
/// glue small: an array of strings drags in `js_sys` value marshalling for
/// ~25 KB of module.
const FIELD_VALUE: char = '\u{1f}';
const FILTER_FILTER: char = '\u{1e}';

/// A loaded index. Construct it from an `ArrayBuffer`/`Uint8Array`.
#[wasm_bindgen]
pub struct Engine {
    index: Index,
    /// Editorial rules the caller handed in with [`Engine::set_curations`]. They are
    /// applied to the hits of every query, after ranking: a pin reorders, a boost
    /// multiplies, and neither can add a document. `None` is the engine with no
    /// curation, which is byte-for-byte the behaviour before this existed.
    curations: Option<Curations>,
}

#[wasm_bindgen]
impl Engine {
    /// Validate and load an index. Throws a JS `Error` on a corrupt artifact.
    #[wasm_bindgen(constructor)]
    pub fn new(bytes: &[u8]) -> Result<Engine, JsValue> {
        let index = Index::from_bytes(bytes.to_vec())
            .map_err(|error| JsValue::from_str(&format!("invalid rustsearch index: {error}")))?;
        Ok(Self {
            index,
            curations: None,
        })
    }

    /// Load an index assembled from shards where the text section was not fetched.
    ///
    /// Every section offset and record is validated; the body checksum is not,
    /// because the shard manifest verifies each part. Text blocks arrive later via
    /// [`Engine::attachTextBlock`], each against its own hash in the header table.
    #[wasm_bindgen(js_name = fromSparse)]
    pub fn from_sparse(bytes: &[u8]) -> Result<Engine, JsValue> {
        let index = Index::from_sparse(bytes.to_vec())
            .map_err(|error| JsValue::from_str(&format!("invalid sparse index: {error}")))?;
        Ok(Self {
            index,
            curations: None,
        })
    }

    /// `[offset, length]` of a document's block inside the `texts` shard.
    /// Returns an empty array when the id is out of range.
    #[wasm_bindgen(js_name = textSpan)]
    pub fn text_span(&self, doc_id: u32) -> Vec<u32> {
        match self.index.text_span(doc_id) {
            Some((off, len)) => vec![off, len],
            None => Vec::new(),
        }
    }

    /// `[urlOffset, urlLength, titleOffset, titleLength]` of a document inside
    /// the `strings` shard. The builder writes the URL and title back to back, so
    /// one range covers both; a client can check that with
    /// `titleOffset === urlOffset + urlLength`. Returns an empty array when the
    /// id is out of range.
    #[wasm_bindgen(js_name = stringSpan)]
    pub fn string_span(&self, doc_id: u32) -> Vec<u32> {
        match self.index.string_span(doc_id) {
            Some(span) => vec![span.url_off, span.url_len, span.title_off, span.title_len],
            None => Vec::new(),
        }
    }

    /// Verify one URL+title block (`url bytes ++ title bytes`) against the
    /// `strhashes` table and keep it, so the next `searchJson` can return the hit
    /// with its URL and title. Throws when the block is the wrong length or does
    /// not match its hash.
    #[wasm_bindgen(js_name = attachStringBlock)]
    pub fn attach_string_block(&mut self, doc_id: u32, block: Vec<u8>) -> Result<(), JsValue> {
        self.index
            .attach_string_block(doc_id, block)
            .map_err(|error| JsValue::from_str(&format!("string block rejected: {error}")))
    }

    /// How many URL+title blocks are attached (the Worker caps this).
    #[wasm_bindgen(getter, js_name = attachedStringBlocks)]
    pub fn attached_string_blocks(&self) -> usize {
        self.index.attached_string_blocks()
    }

    /// The analyzer the index was built with: `none`, `english` or `italian`.
    #[wasm_bindgen(getter, js_name = analyzer)]
    pub fn analyzer(&self) -> String {
        self.index.analyzer().name().to_string()
    }

    /// Verify a fetched text block against the header hash table and keep it, so
    /// the next `searchJson` can render its snippet. Throws when the block is
    /// truncated, the wrong length, or does not match the hash.
    #[wasm_bindgen(js_name = attachTextBlock)]
    pub fn attach_text_block(&mut self, doc_id: u32, block: Vec<u8>) -> Result<(), JsValue> {
        self.index
            .attach_text_block(doc_id, block)
            .map_err(|error| JsValue::from_str(&format!("block rejected: {error}")))
    }

    /// Attach the 8-byte text-block hash of one document, fetched on its own from
    /// the `hashes` shard (little-endian, as stored in the artifact). A sparse
    /// index starts with the table zeroed; the next `attachTextBlock` for this
    /// document verifies against the entry written here.
    #[wasm_bindgen(js_name = attachTextHash)]
    pub fn attach_text_hash(&mut self, doc_id: u32, bytes: &[u8]) -> Result<(), JsValue> {
        let hash = Self::hash_word(bytes)?;
        self.index
            .attach_text_hash(doc_id, hash)
            .map_err(|error| JsValue::from_str(&format!("hash rejected: {error}")))
    }

    /// Attach the 8-byte URL+title hash of one document, fetched on its own from
    /// the `strhashes` shard. Same contract as [`Engine::attach_text_hash`].
    #[wasm_bindgen(js_name = attachStringHash)]
    pub fn attach_string_hash(&mut self, doc_id: u32, bytes: &[u8]) -> Result<(), JsValue> {
        let hash = Self::hash_word(bytes)?;
        self.index
            .attach_string_hash(doc_id, hash)
            .map_err(|error| JsValue::from_str(&format!("hash rejected: {error}")))
    }

    fn hash_word(bytes: &[u8]) -> Result<u64, JsValue> {
        let word: [u8; 8] = bytes
            .try_into()
            .map_err(|_| JsValue::from_str("a hash entry is 8 bytes"))?;
        Ok(u64::from_le_bytes(word))
    }

    /// How many text blocks are attached (the Worker caps this).
    #[wasm_bindgen(getter, js_name = attachedBlocks)]
    pub fn attached_blocks(&self) -> usize {
        self.index.attached_blocks()
    }

    /// True when the artifact groups its dictionary into term chunks (format v5):
    /// the client fetches the chunks a query needs instead of the whole dictionary.
    #[wasm_bindgen(getter, js_name = chunked)]
    pub fn chunked(&self) -> bool {
        self.index.is_chunked()
    }

    /// The number of term chunks, `0` for a flat dictionary.
    #[wasm_bindgen(getter, js_name = chunkCount)]
    pub fn chunk_count(&self) -> u32 {
        self.index.chunk_count()
    }

    /// `[offset, length]` of a chunk inside the `chunks` part. Returns an empty
    /// array when the id is out of range or the dictionary is flat.
    #[wasm_bindgen(js_name = chunkSpan)]
    pub fn chunk_span(&self, chunk: u32) -> Vec<u32> {
        match self.index.chunk_span(chunk) {
            Some((off, len)) => vec![off, len],
            None => Vec::new(),
        }
    }

    /// The first term of a chunk: the key a client compares its token against to
    /// decide whether the next chunk is in the same prefix range.
    #[wasm_bindgen(js_name = chunkKey)]
    pub fn chunk_key(&self, chunk: u32) -> String {
        self.index.chunk_key(chunk).unwrap_or_default().to_string()
    }

    /// The chunk a client should fetch first for `term`, read from the directory
    /// alone (so it answers before any chunk is attached). `undefined` when the
    /// dictionary is flat.
    #[wasm_bindgen(js_name = termChunk)]
    pub fn term_chunk(&self, term: &str) -> Option<u32> {
        self.index.term_chunk(term)
    }

    /// The chunks that answer `query`: every chunk holding a query term, plus the
    /// following chunks of a prefix run. A fuzzy query needs all of them.
    #[wasm_bindgen(js_name = chunksForQuery)]
    pub fn chunks_for_query(&self, query: &str, prefix: bool, fuzzy: bool) -> Vec<u32> {
        let options = SearchOptions {
            prefix,
            fuzzy,
            ..SearchOptions::default()
        };
        self.index
            .chunks_for_query(query, options)
            .unwrap_or_default()
    }

    /// Whether the fetch plan for `query` was capped.
    ///
    /// A fuzzy plan is bounded to 64 chunks around the query's tokens, because the
    /// walk otherwise names the whole dictionary: 824 `Range` requests on the 1M
    /// build. `true` means the ranking sees a subset of the dictionary, and the
    /// client should say so instead of presenting it as an exhaustive fuzzy match.
    #[wasm_bindgen(js_name = planTruncated)]
    pub fn plan_truncated(&self, query: &str, prefix: bool, fuzzy: bool) -> bool {
        self.index
            .chunk_plan(
                query,
                SearchOptions {
                    prefix,
                    fuzzy,
                    ..SearchOptions::default()
                },
            )
            .map(|plan| plan.truncated)
            .unwrap_or(false)
    }

    /// Verify one fetched term chunk against the directory hash and keep it, so the
    /// next query can read the terms it holds. Throws when the bytes are the wrong
    /// length or do not match.
    #[wasm_bindgen(js_name = attachTermChunk)]
    pub fn attach_term_chunk(&mut self, chunk: u32, bytes: Vec<u8>) -> Result<(), JsValue> {
        self.index
            .attach_term_chunk(chunk, bytes)
            .map_err(|error| JsValue::from_str(&format!("term chunk rejected: {error}")))
    }

    /// How many term chunks are attached.
    #[wasm_bindgen(getter, js_name = attachedChunks)]
    pub fn attached_chunks(&self) -> usize {
        self.index.attached_chunks()
    }

    /// Verify the title list that belongs to one term chunk against the directory
    /// hash and keep it, so the next query scores title matches for the terms in it.
    ///
    /// A chunked artifact publishes those lists as `titleNNNNN` objects, one per
    /// chunk that has one: a client fetches the list of every chunk it fetched and
    /// skips the rest. Until a list is attached, its terms score on the body alone.
    #[wasm_bindgen(js_name = attachTitleChunk)]
    pub fn attach_title_chunk(&mut self, chunk: u32, bytes: Vec<u8>) -> Result<(), JsValue> {
        self.index
            .attach_title_chunk(chunk, bytes)
            .map_err(|error| JsValue::from_str(&format!("title chunk rejected: {error}")))
    }

    /// How many title chunks are attached.
    #[wasm_bindgen(getter, js_name = attachedTitleChunks)]
    pub fn attached_title_chunks(&self) -> usize {
        self.index.attached_title_chunks()
    }

    /// The format version the loaded artifact declares: `5` for a chunked build,
    /// `4` for a flat one. `format_version()` is the newest this module reads.
    #[wasm_bindgen(getter, js_name = formatVersion)]
    pub fn artifact_format_version(&self) -> u32 {
        self.index.format_version()
    }

    /// The number of record pages a chunked artifact publishes, `0` for a flat one.
    #[wasm_bindgen(getter, js_name = docPageCount)]
    pub fn doc_page_count(&self) -> u32 {
        self.index.doc_page_count()
    }

    /// How many 32-byte records one page holds.
    #[wasm_bindgen(getter, js_name = docPageRecords)]
    pub fn doc_page_records(&self) -> u32 {
        self.index.doc_page_records()
    }

    /// The page a document's record lives on: the unit a client fetches so it can
    /// read a hit's URL, title and text offsets. `undefined` when the record is
    /// paged out of range or the artifact is flat.
    #[wasm_bindgen(js_name = docPageOf)]
    pub fn doc_page_of(&self, doc: u32) -> Option<u32> {
        self.index.doc_page_of(doc)
    }

    /// Every record page, in order. A grouped query needs all of them on a chunked
    /// sparse index: page identity lives in the record, and grouping runs before
    /// `limit`, so the client cannot know which pages will matter.
    #[wasm_bindgen(js_name = allDocPages)]
    pub fn all_doc_pages(&self) -> Vec<u32> {
        self.index.all_doc_pages()
    }

    /// `[offset, length]` of one record page inside the `docrecords` part.
    #[wasm_bindgen(js_name = docPageSpan)]
    pub fn doc_page_span(&self, page: u32) -> Vec<u32> {
        match self.index.doc_page_span(page) {
            Some((off, len)) => vec![off, len],
            None => Vec::new(),
        }
    }

    /// Verify one fetched record page against the page table and keep it. Until a
    /// page is attached its documents rank but cannot render a URL or a snippet.
    #[wasm_bindgen(js_name = attachDocPage)]
    pub fn attach_doc_page(&mut self, page: u32, bytes: Vec<u8>) -> Result<(), JsValue> {
        self.index
            .attach_doc_page(page, bytes)
            .map_err(|error| JsValue::from_str(&format!("record page rejected: {error}")))
    }

    /// How many record pages are attached.
    #[wasm_bindgen(getter, js_name = attachedDocPages)]
    pub fn attached_doc_pages(&self) -> usize {
        self.index.attached_doc_pages()
    }

    /// Load the editorial rules (`search --curate FILE`), applied to every result.
    ///
    /// The parser is the one the CLI uses, so a file that the build tooling accepts
    /// behaves the same here: `query | pin /path` moves a hit the query already
    /// returned to the front, `query | boost /path 1.5` multiplies its score. A
    /// malformed file throws, and nothing is changed: a caller that catches the
    /// error keeps the uncurated ranking.
    ///
    /// Returns the number of curated queries, so a Worker can report whether it is
    /// running with rules and a UI can say so.
    #[wasm_bindgen(js_name = setCurations)]
    pub fn set_curations(&mut self, text: &str) -> Result<usize, JsValue> {
        let curations = Curations::parse(text)
            .map_err(|error| JsValue::from_str(&format!("invalid curation: {error}")))?;
        let count = curations.len();
        self.curations = Some(curations);
        Ok(count)
    }

    /// Drop the rules and go back to the plain ranking.
    #[wasm_bindgen(js_name = clearCurations)]
    pub fn clear_curations(&mut self) {
        self.curations = None;
    }

    /// How many queries the loaded rules name; `0` when there are none.
    #[wasm_bindgen(getter, js_name = curationCount)]
    pub fn curation_count(&self) -> usize {
        self.curations.as_ref().map(Curations::len).unwrap_or(0)
    }

    /// Search and return `{"results":[...]}`. Snippets are plain text.
    ///
    /// A token with no exact match is read as its nearest dictionary term one edit
    /// away and reported as `"corrected":[{"from":"postgress","to":"postgres"}]`
    /// next to the results (F8). That list is absent when nothing was corrected.
    #[wasm_bindgen(js_name = searchJson)]
    pub fn search_json(&self, query: &str, limit: usize) -> Result<String, JsValue> {
        let options = SearchOptions {
            limit: limit.clamp(1, 100),
            ..SearchOptions::default()
        };
        let mut report = self
            .index
            .try_search_report(query, options)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        if let Some(curations) = &self.curations {
            curations.apply(query, &mut report.hits);
        }
        Ok(search_report_to_json(&report))
    }

    /// Search with the full policy: prefix, fuzzy, edits, title boost and filters.
    ///
    /// `title_boost` is how much a title match is worth against a body match
    /// (BM25F); `NaN` — an unset option in JS — means the engine default (2.0), `0`
    /// ignores titles entirely, and anything above 10 clamps to 10. `filters` is
    /// `"lang\u{1f}en\u{1e}section\u{1f}guide"`: fields and values joined by a unit
    /// separator, filters by a record separator, i.e.
    /// `filters.map(f => `${f.field}\u{1f}${f.value}`).join("\u{1e}")`. Both
    /// separators are control characters no build-time value contains. An empty
    /// string means no filter, and the returned JSON carries the applied filters
    /// next to the results.
    ///
    /// Fields are AND-ed and values inside a field OR-ed, exactly as
    /// `rustsearch_core::Index::try_search_with_filters` documents.
    ///
    /// `group` collapses the documents of one page into one hit (the build has to
    /// have split pages with `--max-page-chars` for that to do anything) and reports
    /// the chunk count in each hit's `matches` field; `limit` then counts pages.
    #[wasm_bindgen(js_name = searchJsonWithOptions)]
    #[allow(clippy::too_many_arguments)]
    pub fn search_json_with_options(
        &self,
        query: &str,
        limit: usize,
        prefix: bool,
        fuzzy: bool,
        max_edits: u8,
        title_boost: f32,
        filters: &str,
        group: bool,
    ) -> Result<String, JsValue> {
        let parsed: Vec<(&str, &str)> = if filters.is_empty() {
            Vec::new()
        } else {
            filters
                .split(FILTER_FILTER)
                .map(|entry| {
                    entry
                        .split_once(FIELD_VALUE)
                        .ok_or_else(|| JsValue::from_str("a filter must be \"field\u{1f}value\""))
                })
                .collect::<Result<_, _>>()?
        };
        let borrowed: Vec<Filter<'_>> = parsed
            .iter()
            .map(|(field, value)| Filter::new(field, value))
            .collect();
        let options = SearchOptions {
            limit: limit.clamp(1, 100),
            prefix,
            fuzzy,
            max_edits: max_edits.min(2),
            title_boost,
            group_by_url: group,
            // F8: `max_edits` is the edit budget the caller allowed, so `0` — the
            // "exact only" setting — is also the switch that turns typo tolerance
            // off. On a chunked artifact the engine ignores the flag anyway (it
            // cannot fetch a neighbour's chunks without a term router) and the
            // explicit `fuzzy` pass stays the opt-in; the flag is what keeps a
            // flat artifact's ranking reproducible for a caller that asked for
            // none.
            typos: max_edits > 0,
        };
        let mut report = self
            .index
            .try_search_report_with_filters(query, options, &borrowed)
            .map_err(|error| JsValue::from_str(&error.to_string()))?;
        if let Some(curations) = &self.curations {
            curations.apply(query, &mut report.hits);
        }
        Ok(search_report_to_json_with_filters(&report, &borrowed))
    }

    /// The taxonomies the artifact carries, as `{"lang":{"en":812},...}`.
    /// `{}` when the build recorded none.
    #[wasm_bindgen(js_name = facetsJson)]
    pub fn facets_json(&self) -> String {
        facets_to_json(&self.index)
    }

    /// Total tokens in the indexed titles, i.e. what the title field's BM25
    /// normalization is measured against. `0` when the build indexed no titles.
    #[wasm_bindgen(getter, js_name = titleTokenCount)]
    pub fn title_token_count(&self) -> f64 {
        self.index.title_token_count() as f64
    }

    #[wasm_bindgen(getter, js_name = docCount)]
    pub fn doc_count(&self) -> u32 {
        self.index.doc_count()
    }

    #[wasm_bindgen(getter, js_name = termCount)]
    pub fn term_count(&self) -> u32 {
        self.index.term_count()
    }

    #[wasm_bindgen(getter, js_name = memoryBytes)]
    pub fn memory_bytes(&self) -> usize {
        self.index.memory_bytes()
    }

    #[wasm_bindgen(getter, js_name = checksum)]
    pub fn checksum(&self) -> String {
        format!("{:016x}", self.index.checksum())
    }
}

/// The format version a build writes by default: v7 (v8 when the artifact is chunked).
///
/// The reader itself accepts v3–v8, so this is the writer's default, not a ceiling.
#[wasm_bindgen]
pub fn format_version() -> u32 {
    rustsearch_core::format::VERSION_HEADER_COVERED
}

/// FNV-1a/64 of arbitrary bytes, as the 16 hex digits a shard manifest stores.
///
/// A cold shard start verifies every metadata slice against the manifest; doing
/// that in Rust instead of a per-byte JS loop is the difference between ~260 ms and
/// a few milliseconds for a 35 MiB metadata run.
#[wasm_bindgen(js_name = fnv1a64)]
pub fn fnv1a64_hex(bytes: &[u8]) -> String {
    format!("{:016x}", rustsearch_core::format::fnv1a64(bytes))
}
