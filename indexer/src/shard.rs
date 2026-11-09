//! R2-friendly sharding.
//!
//! The `.rsx` artifact is one contiguous run of bytes, which is what a native
//! reader wants. Over the network it is the wrong unit: a query needs the
//! dictionary and the postings, then one 36-byte record per hit, then the URL,
//! title and text block of the handful of hits it will render. Fetching the whole
//! artifact to answer that pays for every page's text and every page's strings.
//!
//! Splitting it into one object per section plus a small manifest lets a client
//! issue `Range` requests for exactly the bytes it needs. The parts concatenate
//! back into the original artifact byte for byte, so the manifest is a routing
//! table, not a new format:
//!
//! ```text
//! artifact = head || terms || postings || docs || strings || texts || hashes
//!            || strhashes [|| facets] [|| titlepostings]
//! ```
//!
//! `facets` is the v4 taxonomy section and `titlepostings` its per-term title
//! lists. Both are metadata like `docs`: fetched once with the small parts, never
//! per hit. A v3 artifact has neither, and its manifest simply lists eight parts.
//!
//! A chunked (v5/v6) artifact is published as `metaNNNNN` slices (header, chunk
//! directory, BM25 lengths, facets and the record page table), `hashes` and
//! `strhashes` (one 8-byte entry per document each), `titleNNNNN` (one title list per
//! chunk that has one), `docrecords`, one object per term chunk, `strings` and
//! `texts`. The builder writes those metadata sections first so the group is one
//! contiguous run, cut around the per-hit tables: a cold client reads the manifest and
//! the `meta` slices, skips the tables, and reads one 8-byte entry per rendered hit.
//!
//! At a million documents that run is tens of megabytes, which is too large to be
//! one object. `--meta-part-bytes N` publishes it instead as consecutive slices
//! `meta00000`, `meta00001`, … of at most `N` bytes each. They are still artifact
//! slices at their offsets, so the manifest format does not change and the set still
//! concatenates back byte for byte; a cold client reads every metadata part before it
//! loads the index, exactly as it read the single `meta` object.
//!
//! The per-hit strings and text blocks are addressed with the record's offsets and
//! verified against the header's `strhashes` and `hashes` entries, so a `Range`
//! read is authenticated without the rest of the artifact.
//!
//! The manifest is line-oriented text so both this crate and a Worker can parse
//! it without a JSON dependency:
//!
//! ```text
//! format rsx-shard-1
//! total 41146
//! checksum 0f2a...
//! part head 0 80 3b1c... index/head.bin
//! ...
//! ```
use std::error::Error;
use std::ops::Range;

use rustsearch_core::format::{fnv1a64, BLOCK_HASH};
use rustsearch_core::Index;

/// The manifest format tag. A reader refuses anything else.
pub const MANIFEST_FORMAT: &str = "rsx-shard-1";

/// The section names, in artifact order, of a flat artifact. `facets` and
/// `titlepostings` are present only when the artifact carries those v4 sections. A
/// chunked (v5/v6) artifact publishes `meta` (everything a cold query needs before it
/// can route), `docrecords` (the paged record table) and one [`chunk_part_name`] per
/// term chunk, followed by `strings` and `texts` — five objects' worth of plan
/// whatever the number of documents (more when the metadata run is split).
pub const SECTIONS: [&str; 10] = [
    "head",
    "terms",
    "postings",
    "docs",
    "strings",
    "texts",
    "hashes",
    "strhashes",
    "facets",
    "titlepostings",
];

/// The manifest name of one term chunk. Five digits, so a chunked artifact can hold
/// up to 100,000 chunks (100 GiB at a 1 MiB budget) before the names stop being
/// fixed-width.
pub fn chunk_part_name(chunk: u32) -> String {
    format!("chunk{chunk:05}")
}

/// The manifest name of the title list that belongs to [`chunk_part_name`]'s chunk.
/// Only the chunks whose terms occur in a title have one.
pub fn title_part_name(chunk: u32) -> String {
    format!("title{chunk:05}")
}

/// The manifest name of one metadata slice: `meta00000`, `meta00001`, … Five digits,
/// so a split metadata run can hold up to 99,999 parts (98 GiB at a 1 MiB budget)
/// before the names stop being fixed-width.
pub fn meta_part_name(part: u32) -> String {
    format!("meta{part:05}")
}

/// True for the metadata parts of a chunked artifact: the single `meta` object or one
/// of the `metaNNNNN` slices a [`ShardOptions::meta_part_bytes`] budget produces.
pub fn is_meta_part(name: &str) -> bool {
    name == "meta"
        || name.strip_prefix("meta").is_some_and(|suffix| {
            !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
        })
}

/// The smallest metadata-part budget the CLI accepts. Every meta part is fetched on
/// every cold start, so a part worth less than a request only inflates the manifest
/// and the request count.
pub const MIN_META_PART_BYTES: usize = 64 * 1024;

/// The metadata budget a shard set uses when the caller does not pick one: a run up
/// to this size is one `meta` object, a larger one is cut into consecutive slices.
/// 1 MiB is the cache slice a CDN uses, and the split bounds each object without
/// moving any bytes between parts.
pub const DEFAULT_META_PART_BYTES: usize = 1024 * 1024;

/// How a shard set is laid out.
#[derive(Debug, Clone, Copy, Default)]
pub struct ShardOptions {
    /// Split the metadata run of a chunked artifact into parts of at most this many
    /// bytes (`meta00000`, `meta00001`, …). `None` (the default) uses
    /// [`DEFAULT_META_PART_BYTES`]; `Some(0)` publishes one `meta` object.
    ///
    /// The run is already the group of sections a cold query reads, so a budget does
    /// not change what is fetched — only how it is cut into objects. It is rejected
    /// for a flat v3/v4 artifact, which publishes one object per section instead and
    /// has no `meta` to split.
    pub meta_part_bytes: Option<usize>,
}

/// One object in the shard set: a contiguous slice of the artifact.
#[derive(Debug, Clone)]
pub struct Part {
    pub name: String,
    pub key: String,
    pub off: usize,
    pub len: usize,
    pub hash: u64,
    pub bytes: Vec<u8>,
}

/// The manifest text plus the objects it points at.
#[derive(Debug, Clone)]
pub struct ShardSet {
    pub manifest: String,
    pub parts: Vec<Part>,
}

/// A parsed manifest line for a part.
#[derive(Debug, Clone)]
pub struct PartRef {
    pub name: String,
    pub off: usize,
    pub len: usize,
    pub hash: u64,
    pub key: String,
}

/// A parsed manifest.
#[derive(Debug, Clone)]
pub struct Manifest {
    pub total: usize,
    pub checksum: u64,
    pub parts: Vec<PartRef>,
}

/// A 36-byte document record, as it appears in the `docs` section.
#[derive(Debug, Clone, Copy)]
pub struct DocRecord {
    pub url_off: u32,
    pub url_len: u32,
    pub title_off: u32,
    pub title_len: u32,
    pub text_off: u32,
    pub text_len: u32,
}

impl DocRecord {
    /// Parse a record; `bytes` must be exactly [`DOC_RECORD`] long.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < rustsearch_core::format::DOC_RECORD {
            return None;
        }
        let read = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        Some(Self {
            url_off: read(0),
            url_len: read(4),
            title_off: read(8),
            title_len: read(12),
            text_off: read(16),
            text_len: read(20),
        })
    }
}

/// Split a validated artifact into a manifest and one object per section.
///
/// `prefix` is prepended to every key (use `index/` to publish under a folder). This
/// is [`split_with`] with the default layout: a chunked artifact's metadata run is
/// one `meta` object up to [`DEFAULT_META_PART_BYTES`] and consecutive slices above
/// it.
pub fn split(artifact: &[u8], prefix: &str) -> Result<ShardSet, Box<dyn Error>> {
    split_with(artifact, prefix, ShardOptions::default())
}

/// Append the metadata slices of `start .. end`: one part per `budget` bytes, named
/// `metaNNNNN`. `budget` of 0 means no budget, so the range is one part.
fn push_meta_slices(
    sections: &mut Vec<(String, (usize, usize))>,
    start: usize,
    end: usize,
    budget: usize,
    part: &mut u32,
) -> Result<(), Box<dyn Error>> {
    if start >= end {
        return Ok(());
    }
    let mut at = start;
    while at < end {
        let slice_end = if budget == 0 {
            end
        } else {
            at.saturating_add(budget).min(end)
        };
        sections.push((meta_part_name(*part), (at, slice_end)));
        at = slice_end;
        *part = part.checked_add(1).ok_or("too many metadata parts")?;
    }
    Ok(())
}

/// Split an artifact with an explicit layout, e.g. a metadata budget for a chunked
/// artifact whose `meta` run is too large to be one object.
pub fn split_with(
    artifact: &[u8],
    prefix: &str,
    options: ShardOptions,
) -> Result<ShardSet, Box<dyn Error>> {
    let index = Index::from_bytes(artifact.to_vec())
        .map_err(|error| format!("cannot shard an invalid artifact: {error}"))?;
    let layout = index.layout();
    // The optional sections are dropped from the list rather than written as empty
    // objects, but keep their names: a manifest's parts are in artifact order and
    // the names are what a client routes on.
    let mut sections: Vec<(String, (usize, usize))> = Vec::new();
    if index.is_chunked() {
        // A chunked artifact puts everything a cold query needs before it can route —
        // the header, the chunk directory, the BM25 lengths, the facets, both hash
        // tables and the title chunks — in one run at the front, so it publishes as
        // one object up to the default budget: one request instead of seven.
        // The record pages and then the chunks follow,
        // each its own object, because those are read per hit and per term.
        //
        // Above the budget the run is cut into consecutive slices instead: at a
        // million documents it is tens of megabytes, larger than the cache slice a
        // cold client can use anyway. Nothing inside the run points at another part
        // of it — the directory's offsets are relative to sections whose start the
        // header carries — so a slice boundary may fall anywhere, and every part is
        // still the artifact bytes at that offset.
        let meta_end = layout.doc_page_index.end;
        let budget = options.meta_part_bytes.unwrap_or(DEFAULT_META_PART_BYTES);
        // The two hash tables are the part of the run that only a per-hit
        // verification reads, so they are published as their own objects and the
        // client fetches one 8-byte entry per rendered hit. A cold start then skips
        // 16 bytes per document — 1.6 MB at 100k, 16 MB at 1M — at the cost of one
        // request per hit for each table. The rest of the run stays contiguous, cut
        // around the two tables.
        let mut part = 0u32;
        push_meta_slices(&mut sections, 0, layout.hashes.start, budget, &mut part)?;
        sections.push(("hashes".into(), (layout.hashes.start, layout.hashes.end)));
        push_meta_slices(
            &mut sections,
            layout.hashes.end,
            layout.strings_hashes.start,
            budget,
            &mut part,
        )?;
        sections.push((
            "strhashes".into(),
            (layout.strings_hashes.start, layout.strings_hashes.end),
        ));
        // The title lists are read only by a query that scores titles, and then only
        // for the chunks its terms live in — the same routing the term chunks use. So
        // they are published one object per chunk, exactly like the chunks: a cold
        // start skips the whole `titledata` section, and a query pays for the title
        // lists of the chunks it already fetched. A chunk with no title list has no
        // object, and the client asks the engine for a span before it asks for bytes.
        push_meta_slices(
            &mut sections,
            layout.strings_hashes.end,
            layout.title_data.start,
            budget,
            &mut part,
        )?;
        for chunk in 0..index.chunk_count() {
            if let Some((off, len)) = index.title_chunk_span(chunk) {
                let start = layout.title_data.start + off as usize;
                sections.push((title_part_name(chunk), (start, start + len as usize)));
            }
        }
        push_meta_slices(
            &mut sections,
            layout.title_data.end,
            meta_end,
            budget,
            &mut part,
        )?;
        sections.push((
            "docrecords".into(),
            (layout.doc_pages.start, layout.doc_pages.end),
        ));
        // One object per chunk is the point of the chunked format: a chunk is far
        // below a CDN's cache granularity, so the first query that lands in one
        // caches it for every query that follows.
        for chunk in 0..index.chunk_count() {
            let (off, len) = index
                .chunk_span(chunk)
                .ok_or("a chunk the directory lists has no span")?;
            let start = layout.chunk_data.start + off as usize;
            sections.push((chunk_part_name(chunk), (start, start + len as usize)));
        }
        sections.push(("strings".into(), (layout.strings.start, layout.strings.end)));
        sections.push(("texts".into(), (layout.texts.start, layout.texts.end)));
    } else {
        if options.meta_part_bytes.is_some_and(|budget| budget != 0) {
            return Err(
                "--meta-part-bytes needs a chunked artifact (build with --term-chunk-bytes)".into(),
            );
        }
        sections.push(("head".into(), (0, rustsearch_core::format::HEADER_LEN)));
        sections.push(("terms".into(), (layout.terms.start, layout.terms.end)));
        sections.push((
            "postings".into(),
            (layout.postings.start, layout.postings.end),
        ));
        sections.push(("docs".into(), (layout.docs.start, layout.docs.end)));
        sections.push(("strings".into(), (layout.strings.start, layout.strings.end)));
        sections.push(("texts".into(), (layout.texts.start, layout.texts.end)));
        sections.push(("hashes".into(), (layout.hashes.start, layout.hashes.end)));
        sections.push((
            "strhashes".into(),
            (layout.strings_hashes.start, layout.strings_hashes.end),
        ));
        for (name, range) in [
            ("facets".to_string(), &layout.facets),
            ("titlepostings".to_string(), &layout.title_postings),
        ] {
            if !range.is_empty() {
                sections.push((name, (range.start, range.end)));
            }
        }
    }

    // A gap or an overlap would make concatenation lossy, so refuse it instead
    // of silently shipping a manifest that cannot be reassembled.
    let mut expected = 0usize;
    for (name, (start, end)) in &sections {
        if *start != expected || end < start {
            return Err(format!("section {name} is not contiguous at {start}").into());
        }
        expected = *end;
    }
    if expected != artifact.len() {
        return Err(format!("artifact has {} trailing bytes", artifact.len() - expected).into());
    }

    let mut parts = Vec::with_capacity(sections.len());
    let mut lines = Vec::with_capacity(sections.len());
    for (name, (start, end)) in sections {
        let bytes = artifact[start..end].to_vec();
        let hash = fnv1a64(&bytes);
        let key = format!("{prefix}{name}.bin");
        lines.push(format!(
            "part {name} {start} {} {hash:016x} {key}",
            bytes.len()
        ));
        parts.push(Part {
            name: name.clone(),
            key,
            off: start,
            len: bytes.len(),
            hash,
            bytes,
        });
    }

    let mut manifest = String::new();
    manifest.push_str(&format!("format {MANIFEST_FORMAT}\n"));
    manifest.push_str(&format!("total {}\n", artifact.len()));
    manifest.push_str(&format!("checksum {:016x}\n", body_checksum(artifact)));
    for line in lines {
        manifest.push_str(&line);
        manifest.push('\n');
    }
    Ok(ShardSet { manifest, parts })
}

/// The artifact checksum as its own version defines it: FNV-1a over the body, and over
/// the header too for a v7/v8 artifact. The layout — and therefore what the checksum
/// covers — comes from `rustsearch_core`, so the manifest and the reader cannot
/// disagree about which bytes are authenticated.
fn body_checksum(artifact: &[u8]) -> u64 {
    rustsearch_core::format::declared_checksum(artifact).unwrap_or(0)
}

impl Manifest {
    /// Parse a manifest, refusing anything that is not [`MANIFEST_FORMAT`].
    pub fn parse(text: &str) -> Result<Self, Box<dyn Error>> {
        let mut format: Option<&str> = None;
        let mut total: Option<usize> = None;
        let mut checksum: Option<u64> = None;
        let mut parts: Vec<PartRef> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            match fields.as_slice() {
                ["format", value] => format = Some(value),
                ["total", value] => total = Some(value.parse()?),
                ["checksum", value] => checksum = Some(u64::from_str_radix(value, 16)?),
                ["part", name, off, len, hash, key] => parts.push(PartRef {
                    name: (*name).to_string(),
                    off: off.parse()?,
                    len: len.parse()?,
                    hash: u64::from_str_radix(hash, 16)?,
                    key: (*key).to_string(),
                }),
                _ => return Err(format!("malformed manifest line: {line}").into()),
            }
        }
        match format {
            Some(MANIFEST_FORMAT) => {}
            Some(other) => return Err(format!("unsupported manifest format: {other}").into()),
            None => return Err("manifest has no format line".into()),
        }
        let total = total.ok_or("manifest has no total")?;
        let checksum = checksum.ok_or("manifest has no checksum")?;
        if parts.is_empty() {
            return Err("manifest lists no parts".into());
        }
        let mut expected = 0usize;
        for part in &parts {
            if part.off != expected {
                return Err(format!("part {} does not start at {expected}", part.name).into());
            }
            expected = part
                .off
                .checked_add(part.len)
                .ok_or("part length overflows")?;
        }
        if expected != total {
            return Err(format!("parts cover {expected} of {total} bytes").into());
        }
        Ok(Self {
            total,
            checksum,
            parts,
        })
    }

    /// The part with this name.
    pub fn part(&self, name: &str) -> Option<&PartRef> {
        self.parts.iter().find(|part| part.name == name)
    }

    /// The object that holds one term chunk of a chunked artifact.
    pub fn chunk_part(&self, chunk: u32) -> Option<&PartRef> {
        self.part(&chunk_part_name(chunk))
    }

    /// The metadata parts of a chunked artifact, in artifact order: the single `meta`
    /// object, or the `metaNNNNN` slices a metadata budget produced.
    pub fn meta_parts(&self) -> Vec<&PartRef> {
        self.parts
            .iter()
            .filter(|part| is_meta_part(&part.name))
            .collect()
    }

    /// The chunk parts of a chunked artifact, in chunk order.
    pub fn chunk_parts(&self) -> Vec<&PartRef> {
        self.parts
            .iter()
            .filter(|part| part.name.starts_with("chunk") && part.name != "chunks")
            .collect()
    }

    /// Every part, i.e. the bytes of a full fetch.
    pub fn all(&self) -> Vec<(usize, Range<usize>)> {
        self.parts
            .iter()
            .enumerate()
            .map(|(index, part)| (index, 0..part.len))
            .collect()
    }

    /// The byte ranges of one document record inside the `docs` part.
    pub fn record_range(&self, doc_id: u32) -> Option<Range<usize>> {
        let docs = self.part("docs")?;
        let start = 4 + (doc_id as usize).checked_mul(rustsearch_core::format::DOC_RECORD)?;
        let end = start + rustsearch_core::format::DOC_RECORD;
        (end <= docs.len).then_some(start..end)
    }

    /// The ranges of `count` consecutive records starting at `doc_id`.
    pub fn record_ranges(&self, doc_id: u32, count: usize) -> Vec<Range<usize>> {
        let end = doc_id as usize + count;
        merge(
            (doc_id as usize..end)
                .filter_map(|id| self.record_range(id as u32))
                .collect(),
        )
    }

    /// The 8-byte block hash of a document, inside the `hashes` part.
    ///
    /// Fetched with the rest of the small parts, it lets the client verify a single
    /// text `Range` read without any other section.
    pub fn hash_range(&self, doc_id: u32) -> Option<Range<usize>> {
        let hashes = self.part("hashes")?;
        let start = (doc_id as usize).checked_mul(BLOCK_HASH)?;
        let end = start.checked_add(BLOCK_HASH)?;
        (end <= hashes.len).then_some(start..end)
    }

    /// The 8-byte URL+title hash of a document, inside the `strhashes` part.
    pub fn string_hash_range(&self, doc_id: u32) -> Option<Range<usize>> {
        let hashes = self.part("strhashes")?;
        let start = (doc_id as usize).checked_mul(rustsearch_core::format::STRING_HASH)?;
        let end = start.checked_add(rustsearch_core::format::STRING_HASH)?;
        (end <= hashes.len).then_some(start..end)
    }

    /// The block hash of each document, as little-endian bytes, from a fetched
    /// `hashes` part.
    pub fn block_hashes(&self, bytes: &[u8]) -> Vec<u64> {
        bytes
            .chunks_exact(BLOCK_HASH)
            .map(|chunk| {
                let mut word = [0u8; 8];
                word.copy_from_slice(chunk);
                u64::from_le_bytes(word)
            })
            .collect()
    }

    /// The text block of each record, inside the `texts` part.
    pub fn text_ranges(&self, records: &[DocRecord]) -> Vec<Range<usize>> {
        let Some(texts) = self.part("texts") else {
            return Vec::new();
        };
        merge(
            records
                .iter()
                .filter_map(|record| {
                    let start = record.text_off as usize;
                    let end = start.checked_add(record.text_len as usize)?;
                    (end <= texts.len).then_some(start..end)
                })
                .collect(),
        )
    }

    /// The bytes of one record's URL followed by its title, inside the `strings`
    /// part, as the single range a client reads when they are adjacent.
    pub fn string_span(&self, record: &DocRecord) -> Option<Range<usize>> {
        let strings = self.part("strings")?;
        let start = record.url_off as usize;
        let url_end = start.checked_add(record.url_len as usize)?;
        let title_end = (record.title_off as usize).checked_add(record.title_len as usize)?;
        if url_end > strings.len || title_end > strings.len || url_end != record.title_off as usize
        {
            return None;
        }
        Some(start..title_end)
    }

    /// The url and title slices of each record, inside the `strings` part.
    pub fn string_ranges(&self, records: &[DocRecord]) -> Vec<Range<usize>> {
        let Some(strings) = self.part("strings") else {
            return Vec::new();
        };
        let mut ranges = Vec::with_capacity(records.len() * 2);
        for record in records {
            for (off, len) in [
                (record.url_off as usize, record.url_len as usize),
                (record.title_off as usize, record.title_len as usize),
            ] {
                if let Some(end) = off.checked_add(len) {
                    if end <= strings.len {
                        ranges.push(off..end);
                    }
                }
            }
        }
        merge(ranges)
    }

    /// The total bytes a set of ranges costs, counted once per part.
    pub fn bytes_of(&self, ranges: &[(usize, Range<usize>)]) -> usize {
        ranges.iter().map(|(_, range)| range.len()).sum()
    }
}

/// Sort and coalesce touching or overlapping ranges, so a fetch does not split
/// one object into a dozen requests.
fn merge(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.retain(|range| range.start < range.end);
    ranges.sort_unstable_by_key(|range| range.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => merged.push(range),
        }
    }
    merged
}

/// Reassemble an artifact where only some text blocks were fetched.
///
/// Whole parts are checked against their FNV hash. Slices are placed at their
/// absolute offset inside the part and inherit its trust: a client that fetched
/// `texts` in one piece has hashed it, while a client that fetched three blocks
/// relies on TLS for those three ranges. The caller loads the result with
/// [`rustsearch_core::Index::from_sparse`].
///
/// ```text
/// let bytes = assemble_partial(&manifest, &whole, &slices)?;
/// let index = Index::from_sparse(bytes)?;
/// ```
pub fn assemble_partial(
    manifest: &Manifest,
    whole: &[(String, Vec<u8>)],
    slices: &[(String, Range<usize>, Vec<u8>)],
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut out = vec![0u8; manifest.total];
    for part in &manifest.parts {
        if let Some((_, bytes)) = whole.iter().find(|(key, _)| *key == part.key) {
            if bytes.len() != part.len {
                return Err(format!(
                    "part {} is {} bytes, expected {}",
                    part.key,
                    bytes.len(),
                    part.len
                )
                .into());
            }
            if fnv1a64(bytes) != part.hash {
                return Err(format!("part {} does not match its hash", part.key).into());
            }
            out[part.off..part.off + part.len].copy_from_slice(bytes);
            continue;
        }
        for (key, range, bytes) in slices.iter().filter(|(key, ..)| *key == part.key) {
            let end = range.start.saturating_add(bytes.len());
            if end > part.len || bytes.len() != range.len() {
                return Err(format!("slice {range:?} does not fit {key}").into());
            }
            out[part.off + range.start..part.off + end].copy_from_slice(bytes);
        }
    }
    Ok(out)
}

/// Reassemble the artifact from parts fetched by key.
///
/// Every part is checked against its length and FNV hash, so a stale object or a
/// truncated `Range` response is rejected before the engine sees it.
pub fn assemble(
    manifest: &Manifest,
    fetched: &[(String, Vec<u8>)],
) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut out = vec![0u8; manifest.total];
    for part in &manifest.parts {
        let bytes = fetched
            .iter()
            .find(|(key, _)| *key == part.key)
            .map(|(_, bytes)| bytes)
            .ok_or_else(|| format!("missing part {}", part.key))?;
        if bytes.len() != part.len {
            return Err(format!(
                "part {} is {} bytes, expected {}",
                part.key,
                bytes.len(),
                part.len
            )
            .into());
        }
        if fnv1a64(bytes) != part.hash {
            return Err(format!("part {} does not match its hash", part.key).into());
        }
        out[part.off..part.off + part.len].copy_from_slice(bytes);
    }
    if body_checksum(&out) != manifest.checksum {
        return Err("reassembled artifact does not match the manifest checksum".into());
    }
    Ok(out)
}
