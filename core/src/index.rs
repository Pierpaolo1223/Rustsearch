//! The query engine: BM25 over an immutable, already-validated index.
//!
//! The hot path allocates three things: the query terms, one `Vec<f32>` of
//! document scores, and the result vector. It never builds a `HashMap` and never
//! copies the document text unless a hit is returned.
//!
//! A filtered query allocates a fourth: the candidate set, whose score buffer has
//! one float per candidate rather than one per document. That is what makes a
//! selective filter pay off at query time instead of only at render time — see
//! [`CandidateSet`].
use alloc::borrow::Cow;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::ops::Range;
use core::str;

use crate::analyze::Analyzer;
use crate::error::Error;
use crate::facets::{Facets, Filter, MAX_FILTER_BYTES, MAX_FILTER_FIELDS, MAX_FILTER_VALUES};
use crate::format::{
    u32_at, u64_at, DocPages, DocRecord, DocTokens, Docs, Header, Terms, BLOCK_HASH, STRING_HASH,
};
use crate::postings::{read_list, read_list_any, read_list_positions, Positions};
use crate::title::TitlePostings;
use crate::tokenize::MAX_TOKEN_BYTES;

/// BM25 term-frequency saturation.
pub const K1: f32 = 1.2;
/// BM25 length normalisation.
pub const B: f32 = 0.75;
/// A query longer than this is rejected, not silently truncated.
pub const MAX_QUERY_BYTES: usize = 512;
/// Distinct query terms after tokenization and analysis.
pub const MAX_QUERY_TERMS: usize = 16;
/// The most terms one quoted phrase may carry. A longer quoted run is a paragraph,
/// not a phrase: the engine scores its words instead of refusing the query.
pub const MAX_PHRASE_TERMS: usize = 8;
/// What an exact phrase match is worth against the same terms apart. A phrase is
/// the more specific reading of a query, so it outranks the bag of words that
/// happens to contain the same words.
pub const PHRASE_WEIGHT: f32 = 1.5;
/// Terms a single prefix expansion may contribute.
pub const MAX_PREFIX_TERMS: usize = 32;
/// Terms a single fuzzy expansion may contribute.
pub const MAX_FUZZY_TERMS: usize = 16;
/// The largest result set a caller may ask for.
pub const MAX_LIMIT: usize = 1_000;
/// The default weight of a title match against a body match. A title is a summary
/// of the page, so a term in it is worth more than the same term in the prose.
/// The most term chunks a fuzzy fetch plan names. A fuzzy query walks the whole
/// sorted dictionary, so without a cap the plan is the dictionary size: 824 chunks
/// on the 1M build, i.e. 824 `Range` requests for one query. 64 is one CDN batch
/// and a recall trade the caller can see, because the plan reports it.
pub const MAX_FUZZY_CHUNKS: usize = 64;

pub const DEFAULT_TITLE_BOOST: f32 = 2.0;
/// The largest boost a caller may ask for, so a query cannot bury the body field.
pub const MAX_TITLE_BOOST: f32 = 10.0;
/// Snippet window, in bytes (a few lines of prose, not a page).
pub const SNIPPET_BYTES: usize = 320;
/// Term visits one query may spend on fuzzy expansion. The walk prunes by edit
/// distance, so the budget only binds pathological queries; it replaces the old
/// dictionary-size cap, which silently disabled fuzzy matching above 20,000 terms.
pub const MAX_FUZZY_WORK: usize = 400_000;
/// A token shorter than this many bytes is never corrected: four bytes is enough
/// signal that a one-edit neighbour is a typo and not a different word.
pub const MIN_TYPO_BYTES: usize = 4;
/// The most tokens one query may correct. A query with more unmatched tokens than
/// this is ambiguous, and guessing every one of them is worse than returning the
/// exact matches and saying nothing.
pub const MAX_TYPO_CORRECTIONS: usize = 3;
/// The weight a corrected term scores with: below a prefix match (0.6), so a typo
/// reading never outranks a word the caller actually typed.
pub const TYPO_WEIGHT: f32 = 0.5;
/// The largest uncompressed document text the reader will allocate for one hit.
/// A corrupt length word must not become a decompression bomb.
pub const MAX_DOC_TEXT_BYTES: usize = 8 * 1024 * 1024;

/// Query knobs. The defaults are exact terms + prefix completion, no fuzzy pass.
#[derive(Debug, Clone, Copy)]
pub struct SearchOptions {
    pub limit: usize,
    pub prefix: bool,
    pub fuzzy: bool,
    pub max_edits: u8,
    /// How much a title match is worth relative to a body match (BM25F). `0.0`
    /// ignores the title field entirely, which makes the scores of an artifact the
    /// same as a build that never indexed titles.
    pub title_boost: f32,
    /// Collapse the documents of one page into a single hit: the best-scoring chunk
    /// names the page, `matches` counts the rest, and `limit` applies after the
    /// collapse. A build that writes one document per page (`--max-page-chars 0`,
    /// the default) has nothing to group, so every hit reports `matches: 1` either
    /// way. See [`Hit::matches`].
    pub group_by_url: bool,
    /// Correct a misspelled token automatically (F8). On by default, but only for
    /// a flat artifact: a chunked index cannot fetch the chunks of a neighbour
    /// without a term router, so there the explicit [`SearchOptions::fuzzy`] pass
    /// stays the opt-in way to correct.
    /// `max_edits: 0` also turns it off.
    pub typos: bool,
}

impl Default for SearchOptions {
    fn default() -> Self {
        Self {
            limit: 10,
            prefix: true,
            fuzzy: false,
            max_edits: 1,
            title_boost: DEFAULT_TITLE_BOOST,
            group_by_url: false,
            typos: true,
        }
    }
}

/// The documents one phrase matches, and how often each one writes it.
#[derive(Debug, Clone, Default)]
pub struct PhraseMatches {
    /// Ascending, like every posting list.
    pub docs: Vec<u32>,
    /// Occurrences of the phrase in the document at the same index.
    pub tfs: Vec<u32>,
}

/// One token the engine corrected before ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Correction {
    /// The analyzed token the caller wrote.
    pub from: String,
    /// The dictionary term it was read as.
    pub to: String,
}

/// The ranking plus what the engine changed to produce it.
///
/// A response that needs no correction is exactly [`SearchReport::hits`]; the
/// `corrected` list is empty and the JSON keeps its old shape.
#[derive(Debug, Clone, Default)]
pub struct SearchReport {
    pub hits: Vec<Hit>,
    pub corrected: Vec<Correction>,
}

/// One ranked document.
///
/// With `SearchOptions::group_by_url` this is one ranked **page** instead: the
/// chunk that scored highest names it, and the other chunks of the same page are
/// counted in `matches` rather than returned.
#[derive(Debug, Clone)]
pub struct Hit {
    pub doc_id: u32,
    pub score: f32,
    pub url: String,
    pub title: String,
    pub snippet: String,
    /// Byte ranges **inside `snippet`**, in order and non-overlapping.
    pub highlights: Vec<(u32, u32)>,
    /// Documents in this hit: 1 without grouping, the chunk count of the page with
    /// it. A page built with `--max-page-chars 850` from a 4,000-byte text reports 5.
    pub matches: u32,
}

/// Where a document's URL and title live inside the `strings` section.
///
/// A shard client can fetch exactly these bytes (`url_off .. title_off +
/// title_len` when the builder wrote them back to back, which it does), verify
/// them with [`Index::string_hash`] and hand them to
/// [`Index::attach_string_block`]. Nothing else in the artifact has to be fetched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringSpan {
    pub url_off: u32,
    pub url_len: u32,
    pub title_off: u32,
    pub title_len: u32,
}

impl StringSpan {
    /// True when the URL and title are contiguous, so one `Range` read covers both.
    pub fn is_adjacent(&self) -> bool {
        self.title_off as u64 == self.url_off as u64 + self.url_len as u64
    }

    /// The bytes `attach_string_block` expects: the URL followed by the title.
    pub fn len(&self) -> u32 {
        self.url_len.saturating_add(self.title_len)
    }

    /// True when there is nothing to fetch: an empty URL and title pair.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The single range that covers both halves when they are adjacent.
    pub fn range(&self) -> Option<Range<u32>> {
        if !self.is_adjacent() {
            return None;
        }
        let start = self.url_off;
        let end = self.title_off.checked_add(self.title_len)?;
        Some(start..end)
    }
}

/// Byte ranges of the sections, for the shard builder and diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub terms: Range<usize>,
    pub postings: Range<usize>,
    pub docs: Range<usize>,
    pub strings: Range<usize>,
    pub texts: Range<usize>,
    /// One FNV-1a hash per document, over its encoded text block.
    pub hashes: Range<usize>,
    /// One FNV-1a hash per document, over its URL + title bytes.
    pub strings_hashes: Range<usize>,
    /// Empty in a v3 artifact, or in a v4 one built from a site with no metadata.
    pub facets: Range<usize>,
    /// Empty in a v3 artifact, or in a v4 one built without title postings.
    pub title_postings: Range<usize>,
    /// Empty unless the artifact is a chunked (v5) one: the chunk directory.
    pub chunks: Range<usize>,
    /// Empty unless the artifact is a chunked (v5) one: the bytes the directory
    /// points into.
    pub chunk_data: Range<usize>,
    /// Empty unless the artifact is a chunked (v5) one: the title list of each chunk.
    pub title_data: Range<usize>,
    /// Empty unless the artifact is a chunked (v5) one: the BM25 length table.
    pub doc_tokens: Range<usize>,
    /// Empty unless the artifact is a chunked (v5) one: the record page table.
    pub doc_page_index: Range<usize>,
    /// Empty unless the artifact is a chunked (v5) one: the paged records.
    pub doc_pages: Range<usize>,
}

/// The fetch plan for one query: the chunk objects a client needs, and whether the
/// plan is the whole dictionary or a bounded subset of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkPlan {
    pub chunks: Vec<u32>,
    pub truncated: bool,
}

/// A validated index that owns its bytes.
///
/// Owning the `Vec<u8>` keeps the type usable from WASM without a self-referential
/// struct; the copy is made once when the artifact is loaded.
pub struct Index {
    data: Vec<u8>,
    header: Header,
    /// True when text or string sections may be missing (loaded with
    /// [`Index::from_sparse`]).
    sparse: bool,
    /// Text blocks fetched one at a time by a shard client, keyed by document id.
    blocks: Vec<(u32, Vec<u8>)>,
    /// URL+title blocks fetched one at a time, keyed by document id.
    string_blocks: Vec<(u32, Vec<u8>)>,
    /// Term chunks filled in by a shard client, in attach order.
    chunks: Vec<u32>,
    /// Title chunks filled in by a shard client, in attach order.
    title_chunks: Vec<u32>,
    /// Document-record pages filled in by a shard client, in attach order.
    doc_pages: Vec<u32>,
}

impl Index {
    /// Validate `data` and take ownership of it.
    pub fn from_bytes(data: Vec<u8>) -> Result<Self, Error> {
        Self::parse(data, true)
    }

    /// Load an artifact reassembled from hash-verified shards where some text
    /// blocks or strings were never fetched.
    ///
    /// Every section offset and every document record is validated exactly as in
    /// [`Index::from_bytes`]; only the whole-body checksum is skipped, because the
    /// shard manifest verifies each part that was delivered (see `rustsearch-index`
    /// `shard`). A document whose block was not fetched renders an empty snippet,
    /// and one whose strings were not fetched renders an empty URL and title,
    /// instead of failing the query. Do not use this on bytes from an untrusted
    /// source: use [`Index::from_bytes`], which checks the checksum.
    pub fn from_sparse(data: Vec<u8>) -> Result<Self, Error> {
        Self::parse(data, false)
    }

    fn parse(data: Vec<u8>, verify_checksum: bool) -> Result<Self, Error> {
        let header = if verify_checksum {
            Header::parse(&data)?
        } else {
            Header::parse_sparse(&data)?
        };
        if header.doc_count == 0 {
            // An empty site is legal, but then every section is empty and there is
            // nothing to query. Keep the artifact readable for `info`.
            if header.term_count != 0 || header.token_count != 0 {
                return Err(Error::BadCount);
            }
        }
        let terms = if header.is_chunked() {
            Terms::chunked(
                section(&data, header.chunks),
                section(&data, header.chunk_data),
                header.title_data.len,
                header.term_count,
            )?
        } else {
            Terms::flat(
                section(&data, header.terms),
                section(&data, header.postings),
                header.term_count,
            )?
        };
        terms.validate(header.doc_count, verify_checksum)?;
        if header.is_chunked() {
            // The lengths and the paged records are validated against their own
            // hashes when the bytes are all here; a sparse load checks the pages it
            // was given and trusts the manifest for the rest.
            DocTokens::new(
                section(&data, header.doc_tokens),
                header.doc_count,
                header.compact_doc_tokens(),
            )?;
            // The page table travels with the metadata; the records it indexes may
            // not have been fetched at all, and that is not an error in a sparse
            // load — the client then reads the page of each hit it renders.
            match DocPages::new(
                section(&data, header.doc_page_index),
                section(&data, header.doc_pages),
                header.doc_count,
            ) {
                // A sparse load assumes no page is present: the ones it did fetch
                // are checked when they are attached, one hash each.
                Ok(pages) => pages.validate(
                    header.strings.len,
                    header.texts.len,
                    verify_checksum,
                    |_| verify_checksum,
                )?,
                Err(_) if !verify_checksum => {}
                Err(error) => return Err(error),
            }
            if verify_checksum {
                for chunk in 0..terms.chunk_count() {
                    if let Some(title) = title_chunk_in(&data, header, terms, chunk)? {
                        title.validate(header.term_count, header.doc_count, title.raw_len())?;
                    }
                }
            }
        } else {
            let docs = Docs::new(section(&data, header.docs), header.doc_count)?;
            docs.validate(header.strings.len, header.texts.len)?;
        }
        // A sparse artifact may not have fetched the facet section; a filter on it
        // then fails with `UnsupportedFacet` instead of running unfiltered. When
        // the bytes are all here, the section is checked like every other one.
        if verify_checksum && header.has_facets() {
            Facets::new(section(&data, header.facets))?.validate(header.doc_count)?;
        }
        // A chunked artifact has no flat `titlepostings` section: its title lists were
        // checked one chunk at a time above, and the flag describes them.
        if verify_checksum && header.has_title_postings() && !header.is_chunked() {
            TitlePostings::new(section(&data, header.title_postings))?.validate(
                header.term_count,
                header.doc_count,
                header.title_postings.len,
            )?;
        }
        Ok(Self {
            data,
            header,
            sparse: !verify_checksum,
            blocks: Vec::new(),
            string_blocks: Vec::new(),
            chunks: Vec::new(),
            title_chunks: Vec::new(),
            doc_pages: Vec::new(),
        })
    }

    /// The number of documents in the index.
    pub fn doc_count(&self) -> u32 {
        self.header.doc_count
    }

    /// The number of distinct terms.
    pub fn term_count(&self) -> u32 {
        self.header.term_count
    }

    /// The number of indexed tokens (the BM25 length total).
    pub fn token_count(&self) -> u64 {
        self.header.token_count
    }

    /// The analyzer the artifact was built with. A query is normalized with it.
    pub fn analyzer(&self) -> Analyzer {
        self.header.analyzer()
    }

    /// The size of the loaded artifact in bytes.
    pub fn memory_bytes(&self) -> usize {
        self.data.len()
    }

    /// The raw artifact, including the header (the shard builder needs it).
    pub fn raw_bytes(&self) -> &[u8] {
        &self.data
    }

    /// The section ranges, in artifact order.
    pub fn layout(&self) -> Layout {
        Layout {
            terms: self.header.terms.off..self.header.terms.off + self.header.terms.len,
            postings: self.header.postings.off..self.header.postings.off + self.header.postings.len,
            docs: self.header.docs.off..self.header.docs.off + self.header.docs.len,
            strings: self.header.strings.off..self.header.strings.off + self.header.strings.len,
            texts: self.header.texts.off..self.header.texts.off + self.header.texts.len,
            hashes: self.header.hashes.off..self.header.hashes.off + self.header.hashes.len,
            strings_hashes: self.header.strings_hashes.off
                ..self.header.strings_hashes.off + self.header.strings_hashes.len,
            facets: self.header.facets.off..self.header.facets.off + self.header.facets.len,
            title_postings: self.header.title_postings.off
                ..self.header.title_postings.off + self.header.title_postings.len,
            chunks: self.header.chunks.off..self.header.chunks.off + self.header.chunks.len,
            chunk_data: self.header.chunk_data.off
                ..self.header.chunk_data.off + self.header.chunk_data.len,
            title_data: self.header.title_data.off
                ..self.header.title_data.off + self.header.title_data.len,
            doc_tokens: self.header.doc_tokens.off
                ..self.header.doc_tokens.off + self.header.doc_tokens.len,
            doc_page_index: self.header.doc_page_index.off
                ..self.header.doc_page_index.off + self.header.doc_page_index.len,
            doc_pages: self.header.doc_pages.off
                ..self.header.doc_pages.off + self.header.doc_pages.len,
        }
    }

    /// True when the artifact carries title postings, i.e. a title term can outrank
    /// a body term.
    pub fn has_title_postings(&self) -> bool {
        self.header.has_title_postings()
    }

    /// Total tokens in every indexed title (the title field's BM25 length total).
    pub fn title_token_count(&self) -> u64 {
        self.header.title_token_count
    }

    /// The format version the artifact declares: 4 for a flat dictionary, 5 for a
    /// chunked one. This build also reads 3. A reader can meet an older artifact on
    /// a deploy, so `info` prints it.
    pub fn format_version(&self) -> u32 {
        self.header.version
    }

    /// True when the artifact carries a facet section, i.e. a filter can be applied.
    pub fn has_facets(&self) -> bool {
        self.header.has_facets()
    }

    /// The facet fields the build recorded, in name order.
    pub fn facet_fields(&self) -> Vec<&str> {
        self.facets()
            .map(|facets| facets.names().collect())
            .unwrap_or_default()
    }

    /// The `(value, matching documents)` pairs of one field, in value order.
    pub fn facet_values(&self, field: &str) -> Option<Vec<(&str, u32)>> {
        let facets = self.facets()?;
        let index = facets.field_index(field)?;
        let count = facets.values(index)?;
        (0..count)
            .map(|value| {
                Some((
                    facets.value_name(index, value)?,
                    facets.doc_count(index, value)?,
                ))
            })
            .collect()
    }

    fn facets(&self) -> Option<Facets<'_>> {
        if !self.header.has_facets() {
            return None;
        }
        // `parse` validated the section, so a failure here is not recoverable and
        // also unreachable; treat it as no facets rather than panicking.
        Facets::new(section(&self.data, self.header.facets)).ok()
    }

    /// The title lists of a flat (v3/v4) artifact.
    fn title_postings(&self) -> Option<TitlePostings<'_>> {
        if !self.header.has_title_postings() || self.header.is_chunked() {
            return None;
        }
        TitlePostings::new(section(&self.data, self.header.title_postings)).ok()
    }

    /// The title list of one chunk, as a view over the `titledata` section.
    fn title_chunk(&self, chunk: u32) -> Option<TitlePostings<'_>> {
        title_chunk_in(&self.data, self.header, self.terms(), chunk).ok()?
    }

    /// The title list of one dictionary term, whichever layout holds it. `None` when
    /// the term never occurs in a title, and also when a chunked artifact never
    /// fetched the chunk the term's title list lives in — either way it contributes
    /// nothing to the score.
    fn title_lookup(
        &self,
        terms: Terms<'_>,
        term_index: usize,
    ) -> Result<Option<(u32, &[u8])>, Error> {
        if self.header.is_chunked() {
            let Some(chunk) = terms.chunk_of_index(term_index) else {
                return Ok(None);
            };
            let Some(title) = self.title_chunk(chunk) else {
                return Ok(None);
            };
            return title.lookup(term_index);
        }
        match self.title_postings() {
            Some(title) => title.lookup(term_index),
            None => Ok(None),
        }
    }

    /// Whether a dictionary entry can score under these options: it has body
    /// postings, or title postings and a boost that lets them count. Expanding a
    /// term that cannot contribute would spend one of the 32 prefix slots (or one of
    /// the 16 fuzzy slots) on a term that adds zero to every score.
    fn contributes(&self, terms: Terms<'_>, index: usize, boost: f32) -> Result<bool, Error> {
        let body_df = terms.df(index)?;
        if body_df > 0 {
            return Ok(true);
        }
        if boost <= 0.0 {
            return Ok(false);
        }
        Ok(self.title_lookup(terms, index)?.is_some())
    }

    /// The FNV-1a hash of a document's encoded text block, from the header table.
    ///
    /// A client can compare this with the bytes of one `Range` read, so a block is
    /// verifiable without the rest of the artifact.
    pub fn block_hash(&self, doc: u32) -> Option<u64> {
        if doc >= self.header.doc_count {
            return None;
        }
        let hashes = section(&self.data, self.header.hashes);
        u64_at(hashes, doc as usize * BLOCK_HASH)
    }

    /// The FNV-1a hash of a document's URL + title bytes, from the header table.
    pub fn string_hash(&self, doc: u32) -> Option<u64> {
        if doc >= self.header.doc_count {
            return None;
        }
        let hashes = section(&self.data, self.header.strings_hashes);
        u64_at(hashes, doc as usize * STRING_HASH)
    }

    /// Attach the 8-byte text-block hash of one document, as fetched by a shard
    /// client that did not load the whole `hashes` section.
    ///
    /// A sparse index starts with that table zeroed, so until this is called
    /// [`Index::block_hash`] answers 0 and [`Index::attach_text_block`] refuses the
    /// block. Writing an entry that is already present is allowed only when it is
    /// the same hash, so a full artifact and a per-entry fetch cannot disagree.
    pub fn attach_text_hash(&mut self, doc: u32, hash: u64) -> Result<(), Error> {
        let off = self.header.hashes.off;
        self.attach_hash(off, doc, hash)
    }

    /// Attach the 8-byte URL+title hash of one document, as fetched by a shard
    /// client that did not load the whole `strhashes` section.
    pub fn attach_string_hash(&mut self, doc: u32, hash: u64) -> Result<(), Error> {
        let off = self.header.strings_hashes.off;
        self.attach_hash(off, doc, hash)
    }

    fn attach_hash(&mut self, off: usize, doc: u32, hash: u64) -> Result<(), Error> {
        if doc >= self.header.doc_count {
            return Err(Error::BadDocument);
        }
        let start = off
            .checked_add(
                (doc as usize)
                    .checked_mul(BLOCK_HASH)
                    .ok_or(Error::OutOfBounds)?,
            )
            .ok_or(Error::OutOfBounds)?;
        let end = start.checked_add(BLOCK_HASH).ok_or(Error::OutOfBounds)?;
        let slot = self.data.get_mut(start..end).ok_or(Error::OutOfBounds)?;
        let mut word = [0u8; BLOCK_HASH];
        word.copy_from_slice(slot);
        let current = u64::from_le_bytes(word);
        if current != 0 && current != hash {
            return Err(Error::BadDocument);
        }
        slot.copy_from_slice(&hash.to_le_bytes());
        Ok(())
    }

    /// `(offset, length)` of a document's block inside the `texts` section.
    ///
    /// `None` when the document is out of range **or** when a chunked sparse index
    /// never fetched the record page it lives on: the span is in the record, so there
    /// is nothing to point at until [`Index::attach_doc_page`] fills that page in.
    pub fn text_span(&self, doc: u32) -> Option<(u32, u32)> {
        let record = self.record(doc)?;
        Some((record.text_off, record.text_len))
    }

    /// Where this document's URL and title live inside the `strings` section.
    pub fn string_span(&self, doc: u32) -> Option<StringSpan> {
        let record = self.record(doc)?;
        Some(StringSpan {
            url_off: record.url_off,
            url_len: record.url_len,
            title_off: record.title_off,
            title_len: record.title_len,
        })
    }

    /// The URL and title of one document, as text (H11).
    ///
    /// `None` when the document is out of range. In a sparse index whose `strings`
    /// block was never fetched both strings are empty, exactly as the per-hit path
    /// reports them, so a caller comparing two complete artifacts gets real URLs and
    /// a caller skimming a sparse one gets `""` instead of NUL-filled text.
    pub fn doc_url_title(&self, doc: u32) -> Option<(String, String)> {
        let record = self.record(doc)?;
        self.url_and_title(doc, record)
            .ok()
            .map(|(url, title)| (url.to_string(), title.to_string()))
    }

    /// One document's record, or `None` for an out-of-range id or a page a chunked
    /// sparse index never fetched.
    fn record(&self, doc: u32) -> Option<DocRecord> {
        if doc >= self.header.doc_count {
            return None;
        }
        if self.header.is_chunked() {
            let pages = self.pages()?;
            let page = pages.page_of(doc as usize)?;
            if self.sparse && !self.doc_pages.contains(&(page as u32)) {
                return None;
            }
            return pages.record(doc as usize).ok();
        }
        self.docs().record(doc as usize).ok()
    }

    /// The view ranking reads its lengths through.
    fn lengths_view(&self) -> Result<Lengths<'_>, Error> {
        if self.header.doc_tokens.len != 0 {
            return Ok(Lengths::Tokens(DocTokens::new(
                section(&self.data, self.header.doc_tokens),
                self.header.doc_count,
                self.header.compact_doc_tokens(),
            )?));
        }
        Ok(Lengths::Records(self.docs()))
    }

    fn pages(&self) -> Option<DocPages<'_>> {
        if !self.header.is_chunked() {
            return None;
        }
        DocPages::new(
            section(&self.data, self.header.doc_page_index),
            section(&self.data, self.header.doc_pages),
            self.header.doc_count,
        )
        .ok()
    }

    /// The number of text blocks attached with [`Index::attach_text_block`].
    pub fn attached_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// The number of URL+title blocks attached with [`Index::attach_string_block`].
    pub fn attached_string_blocks(&self) -> usize {
        self.string_blocks.len()
    }

    /// True when the artifact groups its dictionary into term chunks (format v5).
    ///
    /// A chunked artifact publishes the dictionary as a small directory plus one
    /// object per chunk, each far below the 1 MiB granularity a CDN caches in, so a
    /// query fetches the chunks its terms live in and nothing else. See
    /// [`Index::chunks_for_query`].
    pub fn is_chunked(&self) -> bool {
        self.header.is_chunked()
    }

    /// The number of term chunks (0 for a flat v3/v4 dictionary).
    pub fn chunk_count(&self) -> u32 {
        self.terms().chunk_count()
    }

    /// `(offset, length)` of one chunk inside the `chunks` part: exactly the bytes
    /// [`Index::attach_term_chunk`] takes, and what a shard client reads.
    pub fn chunk_span(&self, chunk: u32) -> Option<(u32, u32)> {
        self.terms().chunk_span(chunk)
    }

    /// The FNV-1a hash recorded for one chunk, so a client can verify a chunk it
    /// fetched on its own (the manifest hashes it too; this one travels with the
    /// directory).
    pub fn chunk_hash(&self, chunk: u32) -> Option<u64> {
        self.terms().chunk_hash(chunk)
    }

    /// `(offset, length)` of one chunk's title list inside the `titledata` section,
    /// or `None` when none of the chunk's terms occurs in a title.
    ///
    /// The lists are contiguous in chunk order, so a shard builder publishes each of
    /// these as its own `titleNNNNN` object and a client fetches one for every term
    /// chunk it fetched.
    pub fn title_chunk_span(&self, chunk: u32) -> Option<(u32, u32)> {
        self.terms().title_span(chunk)
    }

    /// The first term of one chunk: the key a client compares its query token
    /// against to decide whether the next chunk is in the same prefix range.
    pub fn chunk_key(&self, chunk: u32) -> Option<&str> {
        self.terms().chunk_key(chunk)
    }

    /// The chunk a client should fetch first for `term`: the last chunk whose first
    /// term sorts at or before it. Read from the directory alone, so it answers
    /// before any chunk is attached.
    pub fn term_chunk(&self, term: &str) -> Option<u32> {
        self.terms().chunk_for_term(term)
    }

    /// The chunks that answer `query` under `options`: every chunk holding a query
    /// term, plus the following chunks of a prefix run while their first term still
    /// starts with the token.
    ///
    /// This is the client's fetch plan, and it is deliberately the same policy the
    /// prefix scan uses: a term's exact match is in the chunk the directory points
    /// at, and the scan can only continue into a chunk whose first term shares the
    /// prefix. A fuzzy query walks the whole sorted dictionary, so it is the one case
    /// where chunking does not save a fetch by itself — [`Index::chunk_plan`] bounds
    /// it to [`MAX_FUZZY_CHUNKS`] around the query's tokens.
    pub fn chunks_for_query(&self, query: &str, options: SearchOptions) -> Result<Vec<u32>, Error> {
        Ok(self.chunk_plan(query, options)?.chunks)
    }

    /// The fetch plan plus whether it was capped.
    ///
    /// A fuzzy plan is bounded by [`MAX_FUZZY_CHUNKS`]: beyond that many chunks the
    /// walk cannot be answered without spending the whole dictionary over the
    /// network, so the plan starts at the chunks the query's tokens live in and
    /// grows outward (one chunk either side per step, nearest first). `truncated` is
    /// true when the plan does not cover every chunk, and a client should say so —
    /// the ranking is then a bounded approximation, not the whole dictionary's.
    pub fn chunk_plan(&self, query: &str, options: SearchOptions) -> Result<ChunkPlan, Error> {
        let terms = self.terms();
        let total = terms.chunk_count();
        if total == 0 || query.len() > MAX_QUERY_BYTES {
            return Ok(ChunkPlan {
                chunks: Vec::new(),
                truncated: false,
            });
        }
        if options.fuzzy {
            return self.fuzzy_plan(query, total);
        }
        let mut out: Vec<u32> = Vec::new();
        for token in self.analyzer().query_terms(query, MAX_QUERY_TERMS) {
            let Some(start) = terms.chunk_for_term(&token) else {
                continue;
            };
            // Prefix completion only runs for tokens of two bytes or more, exactly as
            // `expand` decides it.
            let follow = options.prefix && token.len() >= 2;
            let mut chunk = start;
            loop {
                if !out.contains(&chunk) {
                    out.push(chunk);
                }
                if !follow || chunk + 1 >= total {
                    break;
                }
                match terms.chunk_key(chunk + 1) {
                    Some(key) if key.as_bytes().starts_with(token.as_bytes()) => chunk += 1,
                    _ => break,
                }
            }
        }
        out.sort_unstable();
        Ok(ChunkPlan {
            chunks: out,
            truncated: false,
        })
    }

    /// The capped fuzzy plan: the chunks of the query's tokens, then their
    /// neighbours outward, never more than [`MAX_FUZZY_CHUNKS`].
    fn fuzzy_plan(&self, query: &str, total: u32) -> Result<ChunkPlan, Error> {
        let terms = self.terms();
        let cap = MAX_FUZZY_CHUNKS.min(total as usize);
        let mut starts: Vec<u32> = Vec::new();
        for token in self.analyzer().query_terms(query, MAX_QUERY_TERMS) {
            let Some(chunk) = terms.chunk_for_term(&token) else {
                continue;
            };
            if !starts.contains(&chunk) {
                starts.push(chunk);
            }
        }
        if starts.is_empty() {
            // Nothing to centre on: the first `cap` chunks, in order.
            return Ok(ChunkPlan {
                chunks: (0..cap as u32).collect(),
                truncated: cap < total as usize,
            });
        }
        starts.sort_unstable();
        let mut out = starts.clone();
        let mut distance = 1u32;
        while out.len() < cap {
            let mut added = false;
            for start in &starts {
                for candidate in [start.checked_sub(distance), start.checked_add(distance)] {
                    let Some(chunk) = candidate else { continue };
                    if chunk < total && !out.contains(&chunk) {
                        out.push(chunk);
                        added = true;
                        if out.len() == cap {
                            break;
                        }
                    }
                }
                if out.len() == cap {
                    break;
                }
            }
            if !added {
                break;
            }
            distance += 1;
        }
        out.sort_unstable();
        let truncated = out.len() < total as usize;
        Ok(ChunkPlan {
            chunks: out,
            truncated,
        })
    }

    /// The number of term chunks attached with [`Index::attach_term_chunk`].
    pub fn attached_chunks(&self) -> usize {
        self.chunks.len()
    }

    /// Attach one verified text block, as fetched by a shard client.
    ///
    /// The block must be exactly the `text_len` bytes the record points at and its
    /// FNV-1a hash must match the header table, so a `Range` read is authenticated
    /// on its own. A document whose block is attached renders a snippet even when
    /// the `texts` section was never fetched (see [`Index::from_sparse`]).
    pub fn attach_text_block(&mut self, doc: u32, block: Vec<u8>) -> Result<(), Error> {
        let record = self.record(doc).ok_or(Error::BadDocument)?;
        if block.len() != record.text_len as usize {
            return Err(Error::BadDocument);
        }
        match self.block_hash(doc) {
            Some(hash) if hash == crate::format::fnv1a64(&block) => {}
            _ => return Err(Error::BadDocument),
        }
        if let Some(slot) = self.blocks.iter_mut().find(|(id, _)| *id == doc) {
            slot.1 = block;
        } else {
            self.blocks.push((doc, block));
        }
        Ok(())
    }

    /// Attach one verified URL+title block, as fetched by a shard client.
    ///
    /// `block` is the URL followed by the title — exactly the bytes
    /// [`StringSpan::range`] points at when the two are adjacent. Its length must
    /// match the record and its FNV-1a hash must match the `strhashes` table, so
    /// the `Range` read is authenticated without the rest of the artifact.
    pub fn attach_string_block(&mut self, doc: u32, block: Vec<u8>) -> Result<(), Error> {
        let record = self.record(doc).ok_or(Error::BadDocument)?;
        let expected = record.url_len as u64 + record.title_len as u64;
        if block.len() as u64 != expected {
            return Err(Error::BadDocument);
        }
        match self.string_hash(doc) {
            Some(hash) if hash == crate::format::fnv1a64(&block) => {}
            _ => return Err(Error::BadDocument),
        }
        if let Some(slot) = self.string_blocks.iter_mut().find(|(id, _)| *id == doc) {
            slot.1 = block;
        } else {
            self.string_blocks.push((doc, block));
        }
        Ok(())
    }

    /// Attach one verified term chunk, as fetched by a shard client.
    ///
    /// The bytes are copied over the zeroed chunk the sparse artifact reserved for
    /// them, after their length and FNV-1a hash are checked against the chunk
    /// directory, so a `Range` read is authenticated on its own. Until a chunk is
    /// attached its terms simply do not exist: a query whose chunks were not fetched
    /// ranks the terms it does have instead of reading zeroed bytes as a posting
    /// list. Attaching the same chunk twice is allowed and costs one hash.
    pub fn attach_term_chunk(&mut self, chunk: u32, bytes: Vec<u8>) -> Result<(), Error> {
        let (off, len, hash) = {
            let terms = self.terms();
            let (off, len) = terms.chunk_span(chunk).ok_or(Error::BadDocument)?;
            let hash = terms.chunk_hash(chunk).ok_or(Error::BadDocument)?;
            (off, len, hash)
        };
        if bytes.len() != len as usize || crate::format::fnv1a64(&bytes) != hash {
            return Err(Error::BadDocument);
        }
        let start = self
            .header
            .chunk_data
            .off
            .checked_add(off as usize)
            .ok_or(Error::OutOfBounds)?;
        let end = start.checked_add(len as usize).ok_or(Error::OutOfBounds)?;
        let slot = self.data.get_mut(start..end).ok_or(Error::OutOfBounds)?;
        slot.copy_from_slice(&bytes);
        if !self.chunks.contains(&chunk) {
            self.chunks.push(chunk);
        }
        Ok(())
    }

    /// The number of title chunks attached with [`Index::attach_title_chunk`].
    pub fn attached_title_chunks(&self) -> usize {
        self.title_chunks.len()
    }

    /// The number of document-record pages attached with
    /// [`Index::attach_doc_page`].
    pub fn attached_doc_pages(&self) -> usize {
        self.doc_pages.len()
    }

    /// Attach the title list that belongs to one term chunk, as fetched by a shard
    /// client.
    ///
    /// The title lists of a chunk's terms travel with the chunk in a chunked
    /// artifact, so the ids are the same: a client that fetched `chunk00007` fetches
    /// `title00007` too when titles are scored. Length and FNV-1a are checked against
    /// the directory before the bytes are copied. A chunk with no title list has no
    /// object, and a title list a client did not fetch contributes nothing to the
    /// score — a query then ranks on the body alone.
    pub fn attach_title_chunk(&mut self, chunk: u32, bytes: Vec<u8>) -> Result<(), Error> {
        let (off, len, hash) = {
            let terms = self.terms();
            let (off, len) = terms.title_span(chunk).ok_or(Error::BadDocument)?;
            let hash = terms.title_hash(chunk).ok_or(Error::BadDocument)?;
            (off, len, hash)
        };
        if bytes.len() != len as usize || crate::format::fnv1a64(&bytes) != hash {
            return Err(Error::BadDocument);
        }
        let start = self
            .header
            .title_data
            .off
            .checked_add(off as usize)
            .ok_or(Error::OutOfBounds)?;
        let end = start.checked_add(len as usize).ok_or(Error::OutOfBounds)?;
        let slot = self.data.get_mut(start..end).ok_or(Error::OutOfBounds)?;
        slot.copy_from_slice(&bytes);
        if !self.title_chunks.contains(&chunk) {
            self.title_chunks.push(chunk);
        }
        Ok(())
    }

    /// The number of record pages a chunked artifact publishes (0 for a flat one).
    pub fn doc_page_count(&self) -> u32 {
        self.pages().map_or(0, |pages| pages.page_count() as u32)
    }

    /// How many 32-byte records one page holds.
    pub fn doc_page_records(&self) -> u32 {
        self.pages().map_or(0, |pages| pages.page_records() as u32)
    }

    /// The page a document's record lives on, so a client can fetch it.
    pub fn doc_page_of(&self, doc: u32) -> Option<u32> {
        if doc >= self.header.doc_count {
            return None;
        }
        self.pages()?.page_of(doc as usize).map(|page| page as u32)
    }

    /// `(offset, length)` of one record page inside the `docpages` section.
    pub fn doc_page_span(&self, page: u32) -> Option<(u32, u32)> {
        self.pages()?.page_span(page as usize)
    }

    /// The FNV-1a hash recorded for one record page.
    pub fn doc_page_hash(&self, page: u32) -> Option<u64> {
        self.pages()?.page_hash(page as usize)
    }

    /// Every record page, in order. A grouped query needs all of them on a chunked
    /// sparse index: page identity lives in the record, and grouping runs before
    /// `limit`, so it cannot tell which pages will matter.
    pub fn all_doc_pages(&self) -> Vec<u32> {
        (0..self.doc_page_count()).collect()
    }

    /// Attach one verified record page, as fetched by a shard client.
    ///
    /// Until a page is attached every document on it has no record: it still ranks
    /// (the BM25 lengths are their own table) but it cannot render a URL, a title or
    /// a snippet, and [`Index::string_span`] / [`Index::text_span`] return `None`
    /// instead of the offsets of document 0. Length and FNV-1a are checked against
    /// the page table.
    pub fn attach_doc_page(&mut self, page: u32, bytes: Vec<u8>) -> Result<(), Error> {
        let (off, len, hash) = {
            let pages = self.pages().ok_or(Error::BadDocument)?;
            let (off, len) = pages.page_span(page as usize).ok_or(Error::BadDocument)?;
            let hash = pages.page_hash(page as usize).ok_or(Error::BadDocument)?;
            (off, len, hash)
        };
        if bytes.len() != len as usize || crate::format::fnv1a64(&bytes) != hash {
            return Err(Error::BadDocument);
        }
        let start = self
            .header
            .doc_pages
            .off
            .checked_add(off as usize)
            .ok_or(Error::OutOfBounds)?;
        let end = start.checked_add(len as usize).ok_or(Error::OutOfBounds)?;
        let slot = self.data.get_mut(start..end).ok_or(Error::OutOfBounds)?;
        slot.copy_from_slice(&bytes);
        if !self.doc_pages.contains(&page) {
            self.doc_pages.push(page);
        }
        Ok(())
    }

    /// True when document texts are stored as compressed blocks.
    pub fn has_compressed_texts(&self) -> bool {
        self.header.has_compressed_texts()
    }

    /// True when the artifact carries token positions (`build --positions`, F6).
    ///
    /// A phrase query needs them: without positions there is nothing to check
    /// adjacency against, so [`Index::try_search`] returns [`Error::NoPositions`]
    /// rather than answering a phrase with a bag of words.
    pub fn has_positions(&self) -> bool {
        self.header.has_positions()
    }

    /// The FNV-1a checksum recorded at build time.
    pub fn checksum(&self) -> u64 {
        self.header.checksum
    }

    /// The content hash of a document (the lower 64 bits of its text's FNV), for
    /// an incremental builder or an ETag. `None` for an out-of-range id.
    pub fn content_hash(&self, doc_id: u32) -> Option<u64> {
        self.record(doc_id).map(|record| record.hash)
    }

    /// Search, returning no hits instead of an error on a corrupt query.
    pub fn search(&self, query: &str, options: SearchOptions) -> Vec<Hit> {
        self.try_search(query, options).unwrap_or_default()
    }

    /// Search, reporting a query that exceeds the engine's limits.
    pub fn try_search(&self, query: &str, options: SearchOptions) -> Result<Vec<Hit>, Error> {
        Ok(self.try_search_report(query, options)?.hits)
    }

    /// Search and report the corrections the engine applied (F8).
    ///
    /// A token with no exact match and at least [`MIN_TYPO_BYTES`] bytes is read as
    /// its nearest dictionary term one edit away, at most
    /// [`MAX_TYPO_CORRECTIONS`] tokens per query, and scored at [`TYPO_WEIGHT`].
    /// Exact and prefix matches are collected first, so a query whose tokens all
    /// exist ranks exactly as it did before (the acceptance test for F8). The pass
    /// runs on a flat artifact; a chunked one needs its chunks, so there it is the
    /// explicit `fuzzy` pass or nothing.
    pub fn try_search_report(
        &self,
        query: &str,
        options: SearchOptions,
    ) -> Result<SearchReport, Error> {
        self.search_candidates(query, options, None)
    }

    /// Search restricted to the documents a facet filter selects.
    ///
    /// Fields are AND-ed, values inside a field are OR-ed, and the intersection is
    /// computed as sorted doc-id lists **before** BM25 scores anything: a filtered
    /// query ranks, sorts and limits over the candidates only, and returns the same
    /// order the unfiltered query would give among them. The score buffer covers the
    /// candidates too, so a selective filter costs less than no filter at all. An
    /// unknown field or value is an empty candidate set, not an error; an artifact
    /// without facets rejects the filter instead of ignoring it.
    pub fn try_search_with_filters(
        &self,
        query: &str,
        options: SearchOptions,
        filters: &[Filter<'_>],
    ) -> Result<Vec<Hit>, Error> {
        Ok(self
            .try_search_report_with_filters(query, options, filters)?
            .hits)
    }

    /// The filtered ranking plus the corrections the engine applied.
    pub fn try_search_report_with_filters(
        &self,
        query: &str,
        options: SearchOptions,
        filters: &[Filter<'_>],
    ) -> Result<SearchReport, Error> {
        let candidates = self.candidates(filters)?;
        match candidates {
            Some(candidates) if candidates.is_empty() => Ok(SearchReport::default()),
            other => self.search_candidates(query, options, other.as_deref()),
        }
    }

    /// The documents a filter selects: `None` when there is no filter at all, and an
    /// empty list when the filter matches nothing (or names something the build
    /// never recorded).
    fn candidates(&self, filters: &[Filter<'_>]) -> Result<Option<Vec<u32>>, Error> {
        if filters.is_empty() {
            return Ok(None);
        }
        let facets = self.facets().ok_or(Error::UnsupportedFacet)?;
        if filters.len() > MAX_FILTER_FIELDS {
            return Err(Error::QueryTooLong);
        }
        let bytes = filters
            .iter()
            .map(|filter| filter.field.len() + filter.value.len())
            .sum::<usize>();
        if bytes > MAX_FILTER_BYTES {
            return Err(Error::QueryTooLong);
        }

        // Group by field, keeping the order the caller wrote them in, so the OR
        // inside a field is computed once and the AND is over distinct fields.
        let mut groups: Vec<(&str, Vec<&str>)> = Vec::new();
        for filter in filters {
            match groups.iter_mut().find(|(field, _)| *field == filter.field) {
                Some((_, values)) => values.push(filter.value),
                None => groups.push((filter.field, vec![filter.value])),
            }
        }
        if groups
            .iter()
            .any(|(_, values)| values.len() > MAX_FILTER_VALUES)
        {
            return Err(Error::QueryTooLong);
        }

        let mut selected: Option<Vec<u32>> = None;
        for (field, values) in groups {
            let mut union: Vec<u32> = Vec::new();
            if let Some(index) = facets.field_index(field) {
                for value in values {
                    if let Some(value) = facets.value_index(index, value) {
                        facets.collect_docs(index, value, &mut union)?;
                    }
                }
            }
            // A `from_sparse` load may not have fetched the section, so a doc id
            // that points outside the corpus is dropped here rather than trusted.
            union.retain(|doc| *doc < self.header.doc_count);
            union.sort_unstable();
            union.dedup();
            selected = Some(match selected {
                None => union,
                Some(mut selected) => {
                    selected.retain(|doc| union.binary_search(doc).is_ok());
                    selected
                }
            });
            if selected.as_ref().is_some_and(Vec::is_empty) {
                return Ok(Some(Vec::new()));
            }
        }
        Ok(selected)
    }

    fn search_candidates(
        &self,
        query: &str,
        options: SearchOptions,
        candidates: Option<&[u32]>,
    ) -> Result<SearchReport, Error> {
        if query.len() > MAX_QUERY_BYTES {
            return Err(Error::QueryTooLong);
        }
        // The query is a list of segments: ordinary text, and quoted phrases whose
        // words must sit at the ordinals the analyzer recorded for them. A phrase is
        // never split into its words — that is the whole point of quoting it.
        let analyzer = self.analyzer();
        let mut tokens: Vec<String> = Vec::new();
        let mut phrases: Vec<Vec<(String, u32)>> = Vec::new();
        let mut phrase_terms = 0usize;
        for (quoted, segment) in query_segments(query) {
            if !quoted {
                for term in analyzer.query_terms(segment, MAX_QUERY_TERMS) {
                    if !tokens.iter().any(|seen| seen == &term) {
                        tokens.push(term);
                    }
                }
                continue;
            }
            let phrase = analyzer.analyze_positional(segment);
            if phrase.is_empty() {
                continue;
            }
            if phrase.len() > MAX_PHRASE_TERMS || phrase_terms + phrase.len() > MAX_QUERY_TERMS {
                // Too long for a phrase, and too long to be worth a second pass: score
                // its words like any other segment instead of dropping the query.
                for (term, _) in phrase {
                    if tokens.len() >= MAX_QUERY_TERMS {
                        break;
                    }
                    if !tokens.iter().any(|seen| seen == &term) {
                        tokens.push(term);
                    }
                }
                continue;
            }
            if !self.header.has_positions() {
                return Err(Error::NoPositions);
            }
            phrase_terms += phrase.len();
            phrases.push(phrase);
        }
        if tokens.len() > MAX_QUERY_TERMS {
            tokens.truncate(MAX_QUERY_TERMS);
        }
        if (tokens.is_empty() && phrases.is_empty()) || self.header.doc_count == 0 {
            return Ok(SearchReport::default());
        }

        let terms = self.terms();
        // Ranking reads only a document's BM25 lengths, which a chunked artifact keeps
        // in their own small table (see `DocTokens`), so a query never needs a record
        // page to score.
        let lengths = self.lengths_view()?;
        let doc_count = self.header.doc_count;
        let avgdl = if doc_count == 0 {
            1.0
        } else {
            (self.header.token_count as f64 / doc_count as f64) as f32
        };
        // The title field has its own length normalization; a build without title
        // postings (or with the boost at 0) never reads it.
        let boost = clamp_title_boost(options.title_boost);
        let avg_title_len = if doc_count == 0 {
            1.0
        } else {
            (self.header.title_token_count as f64 / doc_count as f64) as f32
        };
        let title_field =
            (boost > 0.0 && self.header.has_title_postings()).then_some(avg_title_len);

        let mut selected: Vec<(usize, f32)> = Vec::new();
        let mut fuzzy_work = MAX_FUZZY_WORK;
        // F8: a token with no exact match is a candidate for correction, but only
        // on a flat artifact — a chunked one does not hold the neighbour's chunk.
        let typo_pass =
            options.typos && options.max_edits > 0 && !options.fuzzy && !self.header.is_chunked();
        let mut unmatched: Vec<&str> = Vec::new();
        for token in &tokens {
            let exact = self.expand(terms, token, options, &mut selected)?;
            if !exact && typo_pass && token.len() >= MIN_TYPO_BYTES {
                unmatched.push(token.as_str());
            }
            // A prefix match no longer suppresses the correction: the term was not
            // an exact hit, so a neighbour can still be the better reading.
            if options.fuzzy && !exact {
                self.expand_fuzzy(terms, token, options, &mut fuzzy_work, &mut selected)?;
            }
        }
        let mut corrected: Vec<Correction> = Vec::new();
        for token in unmatched.into_iter().take(MAX_TYPO_CORRECTIONS) {
            let Some((index, term)) = self.best_correction(terms, token, &mut fuzzy_work)? else {
                continue;
            };
            push_once(&mut selected, index, TYPO_WEIGHT);
            corrected.push(Correction {
                from: token.to_string(),
                to: term,
            });
        }
        // A phrase contributes its own posting list: the documents where every term
        // sits at its recorded ordinal, with the number of occurrences as tf.
        let mut phrase_lists: Vec<PhraseMatches> = Vec::with_capacity(phrases.len());
        for phrase in &phrases {
            if let Some(list) = self.phrase_postings(terms, phrase)? {
                if !list.docs.is_empty() {
                    phrase_lists.push(list);
                }
            }
        }
        if selected.is_empty() && phrase_lists.is_empty() {
            return Ok(SearchReport::default());
        }

        let mut scores = match candidates {
            Some(candidates) => Scoring::Filtered(CandidateSet::new(candidates, doc_count)),
            None => Scoring::Dense(vec![0.0; doc_count as usize]),
        };
        for (term_index, weight) in &selected {
            if *weight == 0.0 {
                continue;
            }
            // The postings come out of whichever container holds this term: the one
            // `postings` section of a flat artifact, or the chunk of a chunked one.
            // A chunk a client did not fetch has no entry, so the term contributes
            // nothing rather than reading zeroed bytes as a posting list.
            let (df, _, bytes) = terms.entry(*term_index)?;
            if df > 0 {
                let idf =
                    libm::logf(((doc_count as f32 - df as f32 + 0.5) / (df as f32 + 0.5)) + 1.0);
                read_list_any(bytes, df, self.header.has_positions(), |doc, tf| {
                    if doc >= doc_count {
                        return Err(Error::OutOfBounds);
                    }
                    // The filter runs before the score is touched: a document it
                    // excludes costs one bit test and no record load.
                    let Some(slot) = scores.slot(doc) else {
                        return Ok(());
                    };
                    let dl = lengths.of(doc)?.0;
                    let denominator = tf as f32 + K1 * (1.0 - B + B * dl / avgdl.max(1.0));
                    scores.add(slot, idf * (tf as f32 * (K1 + 1.0) / denominator) * weight);
                    Ok(())
                })?;
            }
            // The same term in the title scores in a field of its own: its own df,
            // its own length normalization, and the boost as the field weight. The
            // sum is the BM25F combination: a document that has the word in the
            // title and in the body gets both counts.
            let Some(avg_title_len) = title_field else {
                continue;
            };
            let Some((title_df, bytes)) = self.title_lookup(terms, *term_index)? else {
                continue;
            };
            if title_df == 0 {
                continue;
            }
            let idf = libm::logf(
                ((doc_count as f32 - title_df as f32 + 0.5) / (title_df as f32 + 0.5)) + 1.0,
            );
            read_list(bytes, title_df, |doc, tf| {
                if doc >= doc_count {
                    return Err(Error::OutOfBounds);
                }
                let Some(slot) = scores.slot(doc) else {
                    return Ok(());
                };
                let dl = lengths.of(doc)?.1;
                let denominator = tf as f32 + K1 * (1.0 - B + B * dl / avg_title_len.max(1.0));
                scores.add(
                    slot,
                    boost * idf * (tf as f32 * (K1 + 1.0) / denominator) * weight,
                );
                Ok(())
            })?;
        }

        // A phrase scores like a term with df = the documents it actually matches, so
        // a rare phrase is worth more than a common one and a document that merely
        // contains the words apart is not lifted at all.
        for phrase in &phrase_lists {
            let df = phrase.docs.len() as u32;
            if df == 0 {
                continue;
            }
            let idf = libm::logf(((doc_count as f32 - df as f32 + 0.5) / (df as f32 + 0.5)) + 1.0);
            for (doc, tf) in phrase.docs.iter().zip(&phrase.tfs) {
                let Some(slot) = scores.slot(*doc) else {
                    continue;
                };
                let dl = lengths.of(*doc)?.0;
                let denominator = *tf as f32 + K1 * (1.0 - B + B * dl / avgdl.max(1.0));
                scores.add(
                    slot,
                    idf * (*tf as f32 * (K1 + 1.0) / denominator) * PHRASE_WEIGHT,
                );
            }
        }

        // Both shapes enumerate in ascending doc id, which is the tie-break the
        // ranking below assumes: same scores, same order, filtered or not.
        let mut ranked: Vec<(u32, f32)> = match scores {
            Scoring::Dense(scores) => scores
                .iter()
                .enumerate()
                .filter_map(|(doc, score)| (*score > 0.0).then_some((doc as u32, *score)))
                .collect(),
            Scoring::Filtered(set) => {
                let (candidates, scores) = (set.docs, set.scores);
                candidates
                    .iter()
                    .zip(scores)
                    .filter_map(|(doc, score)| (score > 0.0).then_some((*doc, score)))
                    .collect()
            }
        };
        ranked.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        // Grouping happens after the ranking and before the limit, so `limit` counts
        // pages: the first chunk of a page in score order names it, and the rest only
        // raise its `matches`.
        let mut matches: Vec<u32> = vec![1; ranked.len()];
        if options.group_by_url {
            let mut pages: BTreeMap<(u32, u32), usize> = BTreeMap::new();
            let mut grouped: Vec<(u32, f32)> = Vec::with_capacity(ranked.len());
            let mut counts: Vec<u32> = Vec::with_capacity(ranked.len());
            for (doc, score) in ranked {
                // The page identity is the URL's place in `strings`: the builder
                // writes one copy per page, so its chunks share the offset. That is
                // readable from the metadata alone, so a sparse index groups without
                // fetching a single string — but it does need the record, so a
                // chunked sparse index has to have fetched every record page.
                let record = self.record(doc).ok_or(Error::BadDocument)?;
                let page = (record.url_off, record.url_len);
                match pages.get(&page) {
                    Some(index) => counts[*index] += 1,
                    None => {
                        pages.insert(page, grouped.len());
                        grouped.push((doc, score));
                        counts.push(1);
                    }
                }
            }
            ranked = grouped;
            matches = counts;
        }
        let keep = options.limit.min(MAX_LIMIT).min(ranked.len());
        ranked.truncate(keep);
        matches.truncate(keep);

        let mut query_strings: Vec<String> = selected
            .iter()
            .map(|(index, _)| {
                terms
                    .bytes(*index)
                    .and_then(|bytes| str::from_utf8(bytes).ok())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        // The snippet marks the words of a phrase too: they are what the caller
        // quoted, and a highlight that skipped them would hide the match it found.
        for phrase in &phrases {
            for (term, _) in phrase {
                if !query_strings.iter().any(|seen| seen == term) {
                    query_strings.push(term.clone());
                }
            }
        }

        let mut hits = Vec::with_capacity(ranked.len());
        for ((doc, score), matches) in ranked.into_iter().zip(matches) {
            // A chunked sparse index renders only the documents whose record page it
            // was given: the rest rank, with their score, and an empty title, URL and
            // snippet — the same shape as a hit whose string and text blocks were
            // never fetched.
            let Some(record) = self.record(doc) else {
                hits.push(Hit {
                    doc_id: doc,
                    score,
                    url: String::new(),
                    title: String::new(),
                    snippet: String::new(),
                    highlights: Vec::new(),
                    matches,
                });
                continue;
            };
            let (url, title) = self.url_and_title(doc, record)?;
            let text = self.text(doc, record)?;
            let (snippet, highlights) = snippet(&text, &query_strings);
            hits.push(Hit {
                doc_id: doc,
                score,
                url: url.to_string(),
                title: title.to_string(),
                snippet,
                highlights,
                matches,
            });
        }
        Ok(SearchReport { hits, corrected })
    }

    /// The nearest dictionary term one edit away that can score, for F8.
    ///
    /// The walk is [`walk_fuzzy`] with a one-edit bound, the same trie pruning the
    /// explicit fuzzy pass uses; of the neighbours it finds, the one with the
    /// highest fuzzy weight wins (the walk weights a closer, more common term
    /// higher), and the caller scores it at [`TYPO_WEIGHT`] regardless.
    fn best_correction(
        &self,
        terms: Terms<'_>,
        token: &str,
        work: &mut usize,
    ) -> Result<Option<(usize, String)>, Error> {
        if token.len() > MAX_TOKEN_BYTES || terms.count() == 0 {
            return Ok(None);
        }
        let boost = clamp_title_boost(DEFAULT_TITLE_BOOST);
        let mut contributes = |index: usize| self.contributes(terms, index, boost).unwrap_or(false);
        let needle = token.as_bytes();
        let mut row = [0u16; MAX_TOKEN_BYTES + 1];
        for (j, cell) in row.iter_mut().enumerate().take(needle.len() + 1) {
            *cell = j as u16;
        }
        let mut added = 0usize;
        let mut found: Vec<(usize, f32)> = Vec::new();
        walk_fuzzy(
            terms,
            0,
            terms.count(),
            0,
            needle,
            1,
            &row,
            work,
            &mut added,
            &mut contributes,
            &mut found,
        );
        Ok(found
            .into_iter()
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .and_then(|(index, _)| {
                terms
                    .bytes(index)
                    .and_then(|bytes| str::from_utf8(bytes).ok())
                    .map(|term| (index, term.to_string()))
            }))
    }

    fn text<'a>(&'a self, doc: u32, record: DocRecord) -> Result<Cow<'a, str>, Error> {
        // A block attached by a shard client wins: the `texts` section may never
        // have been fetched at all.
        if let Some((_, block)) = self.blocks.iter().find(|(id, _)| *id == doc) {
            return self.decode(block);
        }
        let texts = section(&self.data, self.header.texts);
        let start = record.text_off as usize;
        let end = start
            .checked_add(record.text_len as usize)
            .ok_or(Error::OutOfBounds)?;
        let block = texts.get(start..end).ok_or(Error::BadDocument)?;
        self.decode(block)
    }

    /// The URL and title of one document: from an attached block when a shard
    /// client fetched one, otherwise from the `strings` section. In a sparse index
    /// whose strings were never fetched, the zeroed slice is reported as empty
    /// strings rather than as a NUL-filled URL.
    fn url_and_title(&self, doc: u32, record: DocRecord) -> Result<(&str, &str), Error> {
        if let Some((_, block)) = self.string_blocks.iter().find(|(id, _)| *id == doc) {
            let split = record.url_len as usize;
            let url = block.get(..split).ok_or(Error::BadDocument)?;
            let title = block.get(split..).ok_or(Error::BadDocument)?;
            return Ok((
                str::from_utf8(url).map_err(|_| Error::BadDocument)?,
                str::from_utf8(title).map_err(|_| Error::BadDocument)?,
            ));
        }
        let strings = section(&self.data, self.header.strings);
        let url = self.strings_slice(strings, record.url_off, record.url_len)?;
        let title = self.strings_slice(strings, record.title_off, record.title_len)?;
        Ok((url, title))
    }

    /// One URL or title slice. In a sparse index an all-zero slice was never
    /// fetched, so it is reported as empty text rather than as a NUL-filled string;
    /// in a complete artifact the same check cannot trigger on real content, which
    /// is UTF-8 without NULs after the indexer's whitespace pass.
    fn strings_slice<'a>(&self, strings: &'a [u8], off: u32, len: u32) -> Result<&'a str, Error> {
        let start = off as usize;
        let end = start.checked_add(len as usize).ok_or(Error::OutOfBounds)?;
        let bytes = strings.get(start..end).ok_or(Error::OutOfBounds)?;
        if self.sparse && bytes.iter().all(|byte| *byte == 0) {
            return Ok("");
        }
        str::from_utf8(bytes).map_err(|_| Error::OutOfBounds)
    }

    /// Decode one block: `u32 raw_len` + LZ4 bytes, or `u32 raw_len | 0x8000_0000`
    /// + raw bytes. An all-zero block is an unfetched shard block, not a document.
    fn decode<'a>(&self, block: &'a [u8]) -> Result<Cow<'a, str>, Error> {
        if !self.header.has_compressed_texts() {
            return str::from_utf8(block)
                .map(Cow::Borrowed)
                .map_err(|_| Error::BadDocument);
        }
        let word = u32_at(block, 0).ok_or(Error::BadDocument)?;
        if self.sparse && word == 0 {
            return Ok(Cow::Borrowed(""));
        }
        let raw_len = (word & 0x7fff_ffff) as usize;
        if raw_len > MAX_DOC_TEXT_BYTES {
            return Err(Error::BadDocument);
        }
        if word & 0x8000_0000 != 0 {
            let bytes = block.get(4..4 + raw_len).ok_or(Error::BadDocument)?;
            return str::from_utf8(bytes)
                .map(Cow::Borrowed)
                .map_err(|_| Error::BadDocument);
        }
        let compressed = block.get(4..).ok_or(Error::BadDocument)?;
        let bytes =
            lz4_flex::block::decompress(compressed, raw_len).map_err(|_| Error::BadDocument)?;
        if bytes.len() != raw_len {
            return Err(Error::BadDocument);
        }
        String::from_utf8(bytes)
            .map(Cow::Owned)
            .map_err(|_| Error::BadDocument)
    }

    // H5: the two `expect`s below are the one place the lint's premise is checked
    // elsewhere: `from_bytes` validates the dictionary and the record table before an
    // `Index` exists, so these constructors cannot fail on a stored `Index`. The
    // allow is on the function, not the crate, so new input-reading code still has
    // to return `Result`.
    #[allow(clippy::expect_used)]
    fn terms(&self) -> Terms<'_> {
        // `parse` already validated the dictionary; constructing the view cannot fail.
        if self.header.is_chunked() {
            Terms::chunked(
                section(&self.data, self.header.chunks),
                section(&self.data, self.header.chunk_data),
                self.header.title_data.len,
                self.header.term_count,
            )
            .expect("validated chunk directory")
        } else {
            Terms::flat(
                section(&self.data, self.header.terms),
                section(&self.data, self.header.postings),
                self.header.term_count,
            )
            .expect("validated term table")
        }
    }

    #[allow(clippy::expect_used)]
    fn docs(&self) -> Docs<'_> {
        Docs::new(section(&self.data, self.header.docs), self.header.doc_count)
            .expect("validated document table")
    }

    /// Exact lookup and prefix range. Returns whether the token is a term in the
    /// dictionary, so the caller knows the fuzzy pass is a correction, not noise.
    fn expand(
        &self,
        terms: Terms<'_>,
        token: &str,
        options: SearchOptions,
        selected: &mut Vec<(usize, f32)>,
    ) -> Result<bool, Error> {
        let boost = clamp_title_boost(options.title_boost);
        let bytes = token.as_bytes();
        let index = terms.lower_bound(bytes);
        let exact = index < terms.count() && terms.bytes(index) == Some(bytes);
        if exact && self.contributes(terms, index, boost)? {
            push_once(selected, index, 1.0);
        }
        if !options.prefix || bytes.len() < 2 {
            return Ok(exact);
        }
        let mut added = 0usize;
        let mut i = index;
        while i < terms.count() && added < MAX_PREFIX_TERMS {
            match terms.bytes(i) {
                Some(term) if term.starts_with(bytes) => {
                    if self.contributes(terms, i, boost)? {
                        push_once(selected, i, 0.6);
                        added += 1;
                    }
                    i += 1;
                }
                _ => break,
            }
        }
        Ok(exact)
    }

    /// Edit-distance neighbours of `token`, found by walking the sorted term table
    /// as a trie: each node is a prefix range plus the Levenshtein DP row of that
    /// prefix against the token. A node is dropped when every prefix of the token is
    /// already more than `max_edits` away (appending bytes never lowers that
    /// distance) or when the prefix is longer than `token + max_edits`.
    #[allow(clippy::too_many_arguments)]
    fn expand_fuzzy(
        &self,
        terms: Terms<'_>,
        token: &str,
        options: SearchOptions,
        work: &mut usize,
        selected: &mut Vec<(usize, f32)>,
    ) -> Result<(), Error> {
        let max_edits = usize::from(options.max_edits.min(2));
        if max_edits == 0 || token.len() > MAX_TOKEN_BYTES || terms.count() == 0 {
            return Ok(());
        }
        let boost = clamp_title_boost(options.title_boost);
        // A neighbour that cannot score would spend one of the 16 slots on a term
        // that adds nothing, so the walk skips it exactly like the prefix range.
        let mut contributes = |index: usize| self.contributes(terms, index, boost).unwrap_or(false);
        let needle = token.as_bytes();
        let mut row = [0u16; MAX_TOKEN_BYTES + 1];
        for (j, cell) in row.iter_mut().enumerate().take(needle.len() + 1) {
            *cell = j as u16;
        }
        let mut added = 0usize;
        walk_fuzzy(
            terms,
            0,
            terms.count(),
            0,
            needle,
            max_edits,
            &row,
            work,
            &mut added,
            &mut contributes,
            selected,
        );
        Ok(())
    }
}

/// A boost outside `0..=MAX_TITLE_BOOST` is clamped rather than rejected: the value
/// comes from a query string, and `NaN` (an unset option in JS) means the default.
/// The filter's candidates, indexed by document id so the score buffer only covers
/// them.
///
/// A filtered query used to keep a `Vec<f32>` of the whole corpus and binary-search
/// the candidate list once per posting: it ranked fewer documents but did *more*
/// work than the unfiltered query, which is backwards. This keeps one float per
/// candidate and answers [`CandidateSet::slot`] — where a document lives in that
/// buffer — with a word load, a shift and a popcount. A posting the filter excludes
/// costs a bit test and nothing else, and the sort afterwards sees the candidates
/// instead of every document the term matched.
struct CandidateSet<'a> {
    /// The candidates, ascending. Sorted and deduplicated by [`Index::candidates`].
    docs: &'a [u32],
    /// One bit per document: set when the filter selected it.
    words: Vec<u64>,
    /// Candidates with a smaller word index, so a slot is
    /// `bases[word] + popcount(bits below it within the word)`.
    bases: Vec<u32>,
    /// One score per candidate, in `docs` order.
    scores: Vec<f32>,
}

impl<'a> CandidateSet<'a> {
    fn new(docs: &'a [u32], doc_count: u32) -> Self {
        let word_count = (doc_count as usize).div_ceil(64);
        let mut words = vec![0u64; word_count];
        for doc in docs {
            // A candidate outside the corpus cannot have a bit; `candidates`
            // already drops those, and this keeps a hand-built list from panicking.
            if let Some(word) = words.get_mut(*doc as usize / 64) {
                *word |= 1u64 << (*doc % 64);
            }
        }
        let mut bases = vec![0u32; word_count];
        let mut running = 0u32;
        for (word, base) in words.iter().zip(bases.iter_mut()) {
            *base = running;
            running += word.count_ones();
        }
        Self {
            docs,
            words,
            bases,
            scores: vec![0.0; docs.len()],
        }
    }

    /// The slot `doc` scores in, or `None` when the filter excluded it.
    #[inline]
    fn slot(&self, doc: u32) -> Option<usize> {
        let word = (doc as usize) / 64;
        let bit = doc % 64;
        let bits = *self.words.get(word)?;
        if bits & (1u64 << bit) == 0 {
            return None;
        }
        let below = (bits & ((1u64 << bit) - 1)).count_ones() as usize;
        self.bases
            .get(word)
            .map(|base| *base as usize + below)
            .filter(|slot| *slot < self.scores.len())
    }
}

/// Where a document's BM25 contributions accumulate.
///
/// Without a filter every document is a candidate, so the direct index is the
/// fastest shape there is: one float per document, no mapping. With a filter the
/// buffer is the size of the filter's result, not of the corpus.
enum Scoring<'a> {
    Dense(Vec<f32>),
    Filtered(CandidateSet<'a>),
}

impl Scoring<'_> {
    /// The slot `doc` scores in, or `None` when it is not a candidate.
    #[inline]
    fn slot(&self, doc: u32) -> Option<usize> {
        match self {
            Scoring::Dense(_) => Some(doc as usize),
            Scoring::Filtered(set) => set.slot(doc),
        }
    }

    /// Add one term's contribution. `slot` comes from [`Scoring::slot`].
    #[inline]
    fn add(&mut self, slot: usize, value: f32) {
        match self {
            Scoring::Dense(scores) => scores[slot] += value,
            Scoring::Filtered(set) => set.scores[slot] += value,
        }
    }
}

/// Where a document's BM25 lengths come from: the v5 `doctokens` table, or the two
/// fields of a v3/v4 record. One branch per posting is cheaper than one record load
/// per posting, which is what a chunked artifact saves: 8 bytes of a small dense
/// array instead of 40 bytes of a table the query would otherwise have to fetch whole.
enum Lengths<'a> {
    Tokens(DocTokens<'a>),
    Records(Docs<'a>),
}

impl Lengths<'_> {
    #[inline]
    fn of(&self, doc: u32) -> Result<(f32, f32), Error> {
        match self {
            Self::Tokens(tokens) => {
                let (body, title) = tokens.lengths(doc as usize)?;
                Ok((body as f32, title as f32))
            }
            Self::Records(docs) => {
                let record = docs.record(doc as usize)?;
                Ok((record.token_len as f32, record.title_token_len as f32))
            }
        }
    }
}

/// The title list of one chunk written into the `titledata` section — a whole title
/// section in its own right, with offsets relative to the chunk.
fn title_chunk_in<'a>(
    data: &'a [u8],
    header: Header,
    terms: Terms<'a>,
    chunk: u32,
) -> Result<Option<TitlePostings<'a>>, Error> {
    let Some((off, len)) = terms.title_span(chunk) else {
        return Ok(None);
    };
    let start = header
        .title_data
        .off
        .checked_add(off as usize)
        .ok_or(Error::OutOfBounds)?;
    let end = start.checked_add(len as usize).ok_or(Error::OutOfBounds)?;
    let bytes = data.get(start..end).ok_or(Error::OutOfBounds)?;
    Ok(Some(TitlePostings::new(bytes)?))
}

fn clamp_title_boost(boost: f32) -> f32 {
    if boost.is_nan() {
        DEFAULT_TITLE_BOOST
    } else {
        boost.clamp(0.0, MAX_TITLE_BOOST)
    }
}

/// Depth-first walk of one prefix range. `row[j]` is the edit distance between the
/// prefix walked so far and `needle[..j]`; `depth` is that prefix's length.
#[allow(clippy::too_many_arguments)]
fn walk_fuzzy(
    terms: Terms<'_>,
    lo: usize,
    hi: usize,
    depth: usize,
    needle: &[u8],
    max_edits: usize,
    row: &[u16; MAX_TOKEN_BYTES + 1],
    work: &mut usize,
    added: &mut usize,
    contributes: &mut dyn FnMut(usize) -> bool,
    selected: &mut Vec<(usize, f32)>,
) {
    let m = needle.len();
    if row[..=m]
        .iter()
        .all(|distance| usize::from(*distance) > max_edits)
    {
        return;
    }
    if depth > m + max_edits || *added >= MAX_FUZZY_TERMS {
        return;
    }

    // `lo` is the first term with this prefix, so it is the prefix itself exactly
    // when its length is `depth` (every term in the range starts with the prefix).
    if depth > 0
        && usize::from(row[m]) <= max_edits
        && terms.bytes(lo).map(|term| term.len() == depth) == Some(true)
        && contributes(lo)
        && push_once(selected, lo, 0.5)
    {
        *added += 1;
    }

    let mut i = lo;
    while i < hi && *added < MAX_FUZZY_TERMS {
        if *work == 0 {
            return;
        }
        *work -= 1;
        let Some(term) = terms.bytes(i) else {
            return;
        };
        if term.len() <= depth {
            i += 1;
            continue;
        }
        let byte = term[depth];
        let start = i;
        i += 1;
        while i < hi {
            if *work == 0 {
                return;
            }
            *work -= 1;
            match terms.bytes(i) {
                Some(next) if next.len() > depth && next[depth] == byte => i += 1,
                _ => break,
            }
        }

        let mut child = [0u16; MAX_TOKEN_BYTES + 1];
        child[0] = row[0].saturating_add(1);
        for j in 1..=m {
            let cost = u16::from(byte != needle[j - 1]);
            let substitute = row[j - 1].saturating_add(cost);
            let insert = row[j].saturating_add(1);
            let delete = child[j - 1].saturating_add(1);
            child[j] = substitute.min(insert).min(delete);
        }
        walk_fuzzy(
            terms,
            start,
            i,
            depth + 1,
            needle,
            max_edits,
            &child,
            work,
            added,
            contributes,
            selected,
        );
    }
}

impl Index {
    /// The documents where a phrase occurs, with the number of occurrences in each.
    ///
    /// Each term's positional list is walked once, in ascending doc order: the rarest
    /// term drives the candidate documents and every other term keeps a cursor that
    /// only moves forward, so the pass is linear in the postings the phrase names and
    /// needs no index of its own and no second read of the corpus.
    ///
    /// A term the dictionary does not hold makes the whole group match nothing: an
    /// exact phrase cannot be approximated with a bag of words.
    fn phrase_postings(
        &self,
        terms: Terms<'_>,
        phrase: &[(String, u32)],
    ) -> Result<Option<PhraseMatches>, Error> {
        let mut entries: Vec<(usize, u32)> = Vec::with_capacity(phrase.len());
        for (term, offset) in phrase {
            let index = terms.lower_bound(term.as_bytes());
            if index >= terms.count() || terms.bytes(index) != Some(term.as_bytes()) {
                return Ok(None);
            }
            entries.push((index, *offset));
        }
        let base = entries.iter().map(|(_, offset)| *offset).min().unwrap_or(0);
        // The rarest term is the driver: its list is the shortest to walk, and every
        // other cursor then advances only as far as the driver asks.
        let mut driver = 0usize;
        for (position, (index, _)) in entries.iter().enumerate() {
            if terms.df(*index)? < terms.df(entries[driver].0)? {
                driver = position;
            }
        }
        let (driver_index, driver_offset) = entries[driver];
        let (df, _, bytes) = terms.entry(driver_index)?;
        if df == 0 {
            return Ok(None);
        }
        let driver_offset = driver_offset - base;
        let mut cursors: Vec<(u32, Positions<'_>)> = Vec::with_capacity(entries.len());
        for (position, (index, offset)) in entries.iter().enumerate() {
            if position == driver {
                continue;
            }
            let (other_df, _, other) = terms.entry(*index)?;
            cursors.push((*offset - base, Positions::new(other, other_df)?));
        }
        let mut docs: Vec<u32> = Vec::new();
        let mut tfs: Vec<u32> = Vec::new();
        read_list_positions(bytes, df, |doc, _, positions| {
            let mut count = 0u32;
            for position in positions {
                let Some(start) = position.checked_sub(driver_offset) else {
                    continue;
                };
                let mut hit = true;
                for (offset, cursor) in cursors.iter_mut() {
                    let Some(target) = start.checked_add(*offset) else {
                        hit = false;
                        break;
                    };
                    match cursor.at(doc)? {
                        Some(found) if found.binary_search(&target).is_ok() => {}
                        _ => {
                            hit = false;
                            break;
                        }
                    }
                }
                if hit {
                    count += 1;
                }
            }
            if count > 0 {
                docs.push(doc);
                tfs.push(count);
            }
            Ok(())
        })?;
        Ok(Some(PhraseMatches { docs, tfs }))
    }
}

/// Split a query into `(quoted, text)` segments.
///
/// A quoted run is a phrase; everything else is ordinary query text. An unterminated
/// quote is read as plain text rather than rejected: a stray quote must never turn a
/// valid query into an error or into a silently different one.
fn query_segments(query: &str) -> Vec<(bool, &str)> {
    let mut out: Vec<(bool, &str)> = Vec::new();
    let mut quoted = false;
    let mut start = 0usize;
    for (at, byte) in query.bytes().enumerate() {
        if byte != b'"' {
            continue;
        }
        let segment = &query[start..at];
        if quoted || !segment.trim().is_empty() {
            out.push((quoted, segment));
        }
        quoted = !quoted;
        start = at + 1;
    }
    let tail = &query[start..];
    if !tail.trim().is_empty() {
        out.push((false, tail));
    }
    out
}

#[inline]
/// Add a term unless it is already selected. Returns whether the term was new, so
/// the fuzzy walk's candidate cap counts distinct terms rather than re-adds.
fn push_once(selected: &mut Vec<(usize, f32)>, index: usize, weight: f32) -> bool {
    if let Some(existing) = selected.iter_mut().find(|(seen, _)| *seen == index) {
        if weight > existing.1 {
            existing.1 = weight;
        }
        false
    } else {
        selected.push((index, weight));
        true
    }
}

fn section(data: &[u8], section: crate::format::Section) -> &[u8] {
    &data[section.off..section.off + section.len]
}

/// Case-insensitive search that never has to allocate a lowercased copy of a page.
///
/// ASCII is folded byte by byte; a non-ASCII needle falls back to an exact match,
/// because Unicode case folding can change the byte length and therefore the
/// highlight offsets.
fn find_ci(haystack: &str, needle: &str, from: usize) -> Option<usize> {
    if needle.is_empty() || from >= haystack.len() {
        return None;
    }
    let hay = &haystack[from..];
    if needle.is_ascii() {
        hay.char_indices().find_map(|(i, _)| {
            let candidate = hay.as_bytes().get(i..i + needle.len())?;
            candidate
                .eq_ignore_ascii_case(needle.as_bytes())
                .then_some(from + i)
        })
    } else {
        hay.find(needle).map(|i| from + i)
    }
}

/// Find `needle` at the start of a word, and return the whole word it begins.
///
/// A stemmed index matches `"index"` against a page that says `"indexes"`, so the
/// engine looks for the stored term as the *prefix of a token* and highlights the
/// full token. This is the cheap version of position-based highlighting: no
/// positions in the artifact, no tokenizer pass over the stored text, and the
/// result is what a reader expects to see marked.
fn find_word(text: &str, needle: &str, from: usize) -> Option<(usize, usize)> {
    let mut at = from;
    while let Some(found) = find_ci(text, needle, at) {
        let boundary = found == 0
            || !text[..found]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_alphanumeric());
        if boundary {
            let mut end = found + needle.len();
            while end < text.len() {
                match text[end..].chars().next() {
                    Some(ch) if ch.is_alphanumeric() => end += ch.len_utf8(),
                    _ => break,
                }
            }
            return Some((found, end));
        }
        // Skip past the byte we matched on, never into the middle of a char.
        at = found + text[found..].chars().next().map_or(1, |ch| ch.len_utf8());
        if at >= text.len() {
            return None;
        }
    }
    None
}

/// A plain-text window around the first query match, plus byte ranges to mark.
///
/// `terms` are the dictionary terms the query expanded to. Each is matched at a
/// word boundary and the whole word is highlighted, so a stemmed term still marks
/// the inflected word in the snippet. When a term never appears at a boundary (a
/// script without spaces, for example), the plain substring match is used instead.
///
/// The returned string is **not HTML**. A renderer must escape it and wrap the
/// ranges in its own `<mark>` elements; the engine never emits markup (XSS is a
/// rendering concern, not a ranking one).
pub(crate) fn snippet(text: &str, terms: &[String]) -> (String, Vec<(u32, u32)>) {
    if text.is_empty() {
        return (String::new(), Vec::new());
    }
    let mut first: Option<usize> = None;
    for term in terms {
        if let Some((at, _)) = find_word(text, term, 0) {
            first = Some(first.map_or(at, |current| current.min(at)));
        }
    }
    if first.is_none() {
        for term in terms {
            if let Some(at) = find_ci(text, term, 0) {
                first = Some(first.map_or(at, |current| current.min(at)));
            }
        }
    }
    let Some(first) = first else {
        let end = floor_boundary(text, SNIPPET_BYTES.min(text.len()));
        return (text[..end].to_string(), Vec::new());
    };
    let mut start = floor_boundary(text, first.saturating_sub(SNIPPET_BYTES / 2));
    if start > 0 && start < first {
        if let Some(offset) = text[start..first].find(char::is_whitespace) {
            start = floor_boundary(text, start + offset + 1);
        }
    }
    let mut end = ceil_boundary(text, (start + SNIPPET_BYTES).min(text.len()));
    if end < text.len() {
        if let Some(offset) = text[..end].rfind(char::is_whitespace) {
            if offset > first {
                end = floor_boundary(text, offset);
            }
        }
    }
    if end <= start {
        start = floor_boundary(text, first.saturating_sub(SNIPPET_BYTES / 2));
        end = ceil_boundary(text, (start + SNIPPET_BYTES).min(text.len()));
    }
    let prefix = if start > 0 { "… " } else { "" };
    let suffix = if end < text.len() { " …" } else { "" };
    let mut out = String::with_capacity(prefix.len() + (end - start) + suffix.len());
    out.push_str(prefix);
    out.push_str(&text[start..end]);
    out.push_str(suffix);

    let mut ranges: Vec<(u32, u32)> = Vec::new();
    for term in terms {
        let mut found = false;
        let mut from = 0usize;
        while let Some((start, end)) = find_word(&out, term, from) {
            ranges.push((start as u32, end as u32));
            found = true;
            from = end.max(start + 1);
        }
        if found {
            continue;
        }
        let mut from = 0usize;
        while let Some(at) = find_ci(&out, term, from) {
            ranges.push((at as u32, (at + term.len()) as u32));
            from = at + term.len().max(1);
        }
    }
    ranges.sort_unstable();
    let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
    for (start, end) in ranges {
        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
                continue;
            }
        }
        merged.push((start, end));
    }
    (out, merged)
}

fn floor_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

fn ceil_boundary(text: &str, mut at: usize) -> usize {
    at = at.min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Bounded Levenshtein distance over two rows of 65 `usize`s. The reference the
/// trie walk is tested against; the query path no longer scans the dictionary.
#[cfg(test)]
fn bounded_levenshtein(a: &[u8], b: &[u8], max: usize) -> bool {
    if a.len().abs_diff(b.len()) > max || a.len() > MAX_TOKEN_BYTES || b.len() > MAX_TOKEN_BYTES {
        return false;
    }
    let mut previous = [0usize; MAX_TOKEN_BYTES + 1];
    let mut current = [0usize; MAX_TOKEN_BYTES + 1];
    for (j, cell) in previous.iter_mut().enumerate().take(b.len() + 1) {
        *cell = j;
    }
    for (i, a_byte) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, b_byte) in b.iter().enumerate() {
            let cost = usize::from(a_byte != b_byte);
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + cost);
        }
        core::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()] <= max
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::{fnv1a64, HEADER_LEN};
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    #[test]
    fn a_candidate_slot_is_its_rank_and_a_miss_is_none() {
        // 0, 63, 64, 65, 127 and 200 span four words and both edges of each: the
        // popcount below the bit is the only thing that can go wrong here.
        let docs = [0u32, 63, 64, 65, 127, 200];
        let set = CandidateSet::new(&docs, 512);
        for (rank, doc) in docs.iter().enumerate() {
            assert_eq!(set.slot(*doc), Some(rank), "doc {doc}");
        }
        for doc in [1u32, 62, 66, 126, 128, 199, 201, 511] {
            assert_eq!(set.slot(doc), None, "doc {doc} is not a candidate");
        }
        // Past the last word the mapping is a miss, not a panic.
        assert_eq!(set.slot(512), None);
        assert_eq!(set.scores.len(), docs.len());
    }

    #[test]
    fn a_candidate_outside_the_corpus_gets_no_bit() {
        // `candidates` drops these; the mapping must survive one anyway.
        let docs = [3u32, 900];
        let set = CandidateSet::new(&docs, 64);
        assert_eq!(set.slot(3), Some(0));
        assert_eq!(set.slot(900), None);
    }

    #[test]
    fn a_snippet_marks_a_whole_word() {
        let text = "Rust walks a directory tree with walkdir; the tree is visited once.";
        let (snippet, ranges) = snippet(text, &[String::from("walkdir")]);
        assert!(snippet.contains("walkdir"));
        assert_eq!(ranges.len(), 1);
        let (start, end) = ranges[0];
        assert_eq!(&snippet[start as usize..end as usize], "walkdir");
    }

    #[test]
    fn a_stem_marks_the_inflected_word() {
        let text = "The indexer walks the tree; the indexes are written once.";
        let (snippet, ranges) = snippet(text, &[String::from("index")]);
        let marked: Vec<&str> = ranges
            .iter()
            .map(|(start, end)| &snippet[*start as usize..*end as usize])
            .collect();
        assert_eq!(marked, ["indexer", "indexes"]);
    }

    #[test]
    fn a_term_inside_a_larger_word_is_not_marked() {
        let text = "brunch is not run";
        let (_, ranges) = snippet(text, &[String::from("run")]);
        assert_eq!(ranges.len(), 1);
        assert_eq!(&text[ranges[0].0 as usize..ranges[0].1 as usize], "run");
    }

    #[test]
    fn a_snippet_without_a_match_is_the_head_of_the_text() {
        let (snippet, ranges) = snippet("hello world", &[String::from("missing")]);
        assert_eq!(snippet, "hello world");
        assert!(ranges.is_empty());
    }

    fn full_levenshtein(a: &[u8], b: &[u8]) -> usize {
        let mut previous: Vec<usize> = (0..=b.len()).collect();
        let mut current = vec![0usize; b.len() + 1];
        for (i, a_byte) in a.iter().enumerate() {
            current[0] = i + 1;
            for (j, b_byte) in b.iter().enumerate() {
                current[j + 1] = (previous[j + 1] + 1)
                    .min(current[j] + 1)
                    .min(previous[j] + usize::from(a_byte != b_byte));
            }
            core::mem::swap(&mut previous, &mut current);
        }
        previous[b.len()]
    }

    #[test]
    fn bounded_levenshtein_agrees_with_the_full_distance() {
        let words = ["", "a", "b", "ab", "ba", "aab", "abb", "abc", "walkdir"];
        for a in words {
            for b in words {
                let distance = full_levenshtein(a.as_bytes(), b.as_bytes());
                for max in 0..=3 {
                    assert_eq!(
                        bounded_levenshtein(a.as_bytes(), b.as_bytes(), max),
                        distance <= max,
                        "{a:?} vs {b:?} max {max}"
                    );
                }
            }
        }
    }

    #[test]
    fn levenshtein_is_bounded() {
        assert!(bounded_levenshtein(b"walkdir", b"walkdi", 1));
        assert!(bounded_levenshtein(b"walkdir", b"wakldir", 2));
        assert!(!bounded_levenshtein(b"walkdir", b"tokio", 2));
        assert!(!bounded_levenshtein(b"a", b"abcdef", 2));
    }

    /// A term table in the artifact layout: count, offset table, entries.
    fn term_table(terms: &[&str]) -> Vec<u8> {
        let count = terms.len();
        let first = 4 + count * 4;
        let mut entries = Vec::new();
        let mut offsets = Vec::new();
        for term in terms {
            offsets.push((first + entries.len()) as u32);
            entries.extend_from_slice(&1u32.to_le_bytes()); // df
            entries.extend_from_slice(&0u32.to_le_bytes()); // postings offset
            entries.extend_from_slice(&0u32.to_le_bytes()); // postings length
            entries.extend_from_slice(&(term.len() as u16).to_le_bytes());
            entries.extend_from_slice(term.as_bytes());
        }
        let mut data = Vec::new();
        data.extend_from_slice(&(count as u32).to_le_bytes());
        for offset in offsets {
            data.extend_from_slice(&offset.to_le_bytes());
        }
        data.extend_from_slice(&entries);
        data
    }

    fn walk_matches(terms: &[&str], needle: &str, max_edits: usize, budget: usize) -> Vec<usize> {
        let data = term_table(terms);
        let table = Terms::flat(&data, &[], terms.len() as u32).unwrap();
        let mut row = [0u16; MAX_TOKEN_BYTES + 1];
        for (j, cell) in row.iter_mut().enumerate().take(needle.len() + 1) {
            *cell = j as u16;
        }
        let mut work = budget;
        let mut added = 0usize;
        let mut selected = Vec::new();
        // Every term can contribute here: the test table has no title postings.
        let mut contributes = |_index: usize| true;
        walk_fuzzy(
            table,
            0,
            table.count(),
            0,
            needle.as_bytes(),
            max_edits,
            &row,
            &mut work,
            &mut added,
            &mut contributes,
            &mut selected,
        );
        let mut indices: Vec<usize> = selected.iter().map(|(index, _)| *index).collect();
        indices.sort_unstable();
        indices
    }

    fn scan_matches(terms: &[&str], needle: &str, max_edits: usize) -> Vec<usize> {
        terms
            .iter()
            .enumerate()
            .filter(|(_, term)| bounded_levenshtein(needle.as_bytes(), term.as_bytes(), max_edits))
            .map(|(index, _)| index)
            .take(MAX_FUZZY_TERMS)
            .collect()
    }

    #[test]
    fn the_trie_walk_finds_what_the_linear_scan_finds() {
        let terms = [
            "indx",
            "index",
            "indexes",
            "indexing",
            "postgre",
            "postgres",
            "postgresql",
            "run",
            "runtime",
            "rust",
            "serde_json",
            "tokio",
            "tree",
            "walk",
            "walkdir",
            "walked",
            "walker",
            "walking",
            "war",
            "work",
        ];
        for needle in ["walkdir", "postgres", "indx", "walkr", "tokio", "zzz"] {
            for max_edits in 0..=2 {
                let mut expected = scan_matches(&terms, needle, max_edits);
                let mut actual = walk_matches(&terms, needle, max_edits, MAX_FUZZY_WORK);
                expected.sort_unstable();
                assert_eq!(actual, expected, "{needle:?} max {max_edits}");
                // The walk reports the same weight for everything it adds.
                actual.dedup();
            }
        }
    }

    #[test]
    fn the_trie_walk_agrees_with_the_scan_on_random_dictionaries() {
        // Deterministic LCG: no dependency, reproducible failures.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };
        let alphabet = b"abcde";
        for round in 0..40 {
            let mut words: Vec<String> = Vec::new();
            for _ in 0..300 {
                let length = 1 + next() % 10;
                let mut word = String::new();
                for _ in 0..length {
                    word.push(alphabet[next() % alphabet.len()] as char);
                }
                words.push(word);
            }
            words.sort_unstable();
            words.dedup();
            let terms: Vec<&str> = words.iter().map(String::as_str).collect();
            for _ in 0..8 {
                let needle = &words[next() % words.len()];
                for max_edits in 1..=2 {
                    let expected = scan_matches(&terms, needle, max_edits);
                    let actual = walk_matches(&terms, needle, max_edits, MAX_FUZZY_WORK);
                    assert_eq!(actual, expected, "round {round} {needle:?} {max_edits}");
                }
            }
        }
    }

    #[test]
    fn the_trie_walk_keeps_working_above_twenty_thousand_terms() {
        // The old linear scan was skipped above 20,000 terms, which is below the
        // 22,602 terms of the documented corpus: fuzzy matching silently did nothing.
        let owned: Vec<String> = (0..=20_000).map(|n| format!("t{n:05}")).collect();
        let terms: Vec<&str> = owned.iter().map(String::as_str).collect();
        let matches = walk_matches(&terms, "t1999z", 1, MAX_FUZZY_WORK);
        let expected = terms.iter().position(|term| *term == "t19990").unwrap();
        assert!(matches.contains(&expected), "{matches:?} lacks {expected}");
    }

    #[test]
    fn the_trie_walk_stops_at_its_budget() {
        let terms = ["tokio", "walk", "walkdi", "walkdir", "walkdirs", "walkdxr"];
        let full = walk_matches(&terms, "walkdir", 1, MAX_FUZZY_WORK);
        assert_eq!(full, scan_matches(&terms, "walkdir", 1));
        assert!(walk_matches(&terms, "walkdir", 1, 0).is_empty());
        let partial = walk_matches(&terms, "walkdir", 1, 2);
        assert!(
            partial.iter().all(|index| full.contains(index)),
            "{partial:?} is not a prefix of {full:?}"
        );
        assert!(partial.len() <= full.len());
    }

    #[test]
    fn a_non_ascii_page_does_not_split_a_char() {
        let text = "日本語のページを検索する。";
        let (snippet, _) = snippet(text, &[String::from("ページ")]);
        assert!(snippet.starts_with('日'));
        assert!(snippet.contains("ページ"));
    }

    #[test]
    fn checksum_detects_a_flipped_byte() {
        let bytes = [0u8; HEADER_LEN];
        let checksum = fnv1a64(&bytes[HEADER_LEN..]);
        assert_ne!(checksum, fnv1a64(&[1u8]));
    }

    #[test]
    fn a_string_span_is_one_range_when_url_and_title_are_adjacent() {
        let span = StringSpan {
            url_off: 10,
            url_len: 5,
            title_off: 15,
            title_len: 7,
        };
        assert!(span.is_adjacent());
        assert_eq!(span.range(), Some(10..22));
        assert_eq!(span.len(), 12);
        let split = StringSpan {
            title_off: 40,
            ..span
        };
        assert!(!split.is_adjacent());
        assert_eq!(split.range(), None);
    }
}
