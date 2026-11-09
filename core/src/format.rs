//! The on-disk format, v4 and its chunked variants v5 and v6.
//!
//! ```text
//! header (120 bytes)
//! terms      : u32 count, u32 offsets[count], entries       (v3, v4)
//! postings   : for each term, blocks of up to 128 postings (see `postings`)
//! chunks     : v5 only: the chunk directory (see below)
//! chunkdata  : v5 only: the chunks, back to back
//! documents  : u32 count, 40-byte records
//! strings    : URLs and titles, concatenated (url then title, per document)
//! texts      : per-document blocks; each block is u32 raw_len + LZ4 bytes, or
//!              u32 raw_len | 0x8000_0000 + raw bytes when compression would grow it
//! hashes     : one u64 FNV-1a per document, over its encoded text block
//! strhashes  : one u64 FNV-1a per document, over its url + title bytes
//! facets     : optional field/value/doc-list table (see `facets`), empty when the
//!              site has no metadata to filter on
//! titleposts : optional per-term title postings (see `title`), empty when the
//!              build did not index titles
//! ```
//!
//! v5 is v4 with the dictionary split into **term chunks**: the same entry and
//! postings encoding, grouped 1..n terms at a time so that one term's bytes live in
//! one independently fetchable slice of the file (see [`Chunks`]). Only a build given
//! a chunk budget writes it (`--term-chunk-bytes`); a default build still writes v4
//! byte for byte. The two spans v4 uses for `terms` and `postings` are `chunks` and
//! `chunkdata` in v5, so the header is the same size and a v4 reader rejects a v5
//! artifact on the version byte, the safe direction.
//!
//! v6 is v5 with a compact BM25 length table: two `u16`s per document instead of two
//! `u32`s when every length fits, with `FLAG_COMPACT_DOC_TOKENS` saying which. The
//! header is unchanged in size; one length above `u16::MAX` writes the wide table for
//! the whole artifact, so ranking reads the exact value either way.
//!
//! Positions (F6) are a flag, not a version: `FLAG_HAS_POSITIONS` says every body
//! posting list carries a gap-encoded run of token ordinals after its tf stream, and
//! a build without `--positions` writes exactly the bytes it wrote before. The
//! streams live inside the blocks they describe (see [`crate::postings`]), so a
//! phrase query fetches the same chunks as the terms it names and nothing else.
//!
//! v4 adds the `facets` and `titlepostings` sections, the per-document title length
//! and the title token count, and grows the header by two section spans and eight
//! bytes of statistics (96 -> 120 bytes). A v4 reader still opens a v3 artifact
//! (header 96 bytes, no facets, no title postings, `title_df` implied 0), which is
//! what lets a new client outlive an old index on a deploy; a v3 reader rejects v4,
//! the safe direction.
//!
//! Every multi-byte integer is little-endian. `terms` and `documents` carry their
//! own count; the header carries the seven section offsets and lengths. The record
//! layout is fixed, so a reader can binary-search the dictionary and jump to a
//! document without deserialising anything else.
//!
//! v3 changes three things, all of them on the wire:
//!
//! * the postings are block-packed instead of plain LEB128 (see [`crate::postings`]);
//! * `strhashes` is a second per-document hash table, over the URL and title, so a
//!   client can range-read one hit's strings and verify that read on its own;
//! * the low bits of `flags` carry the analyzer id (0 none, 1 English, 2 Italian),
//!   because a stemmed index must be queried with the same stemmer.
//!
//! `hashes` (v2) still covers the text blocks. Both tables are one entry per
//! document, so a reader addresses entry `i` without parsing anything.
//!
//! The checksum is FNV-1a over the body (everything after the header). It detects
//! truncation and bit rot, **not** a deliberate rewrite: deploy the artifact over
//! TLS and keep the object key content-addressed (see `DESIGN.md`).
//!
//! Text blocks are independently decompressed, so a page is only inflated when it
//! is a hit. The 4-byte length word is capped before allocation; a corrupt block
//! returns [`Error::BadDocument`], never an allocation the size of its header.
use core::str;

use crate::analyze::Analyzer;
use crate::error::Error;

/// Bytes before the body. Keep in sync with the builder.
pub const HEADER_LEN: usize = 120;
/// The number of section spans a v4 header carries (v3 has seven).
pub const SPANS: usize = 9;
/// The v3 header, still accepted when reading.
pub const HEADER_LEN_V3: usize = 96;
/// The v5 header: the v4 spans plus the title-chunk area, the length table, the
/// record-page table and the paged document records.
pub const HEADER_LEN_V5: usize = 136;
/// The number of spans a v5 header carries.
pub const SPANS_V5: usize = 11;
/// Bytes of one chunk directory entry: `first_term, term_count, data_off, data_len,
/// title_off, title_len, key_off, key_len (u16), reserved (u16), hash (u64),
/// title_hash (u64)`.
pub const CHUNK_ENTRY: usize = 48;
/// A corrupt header cannot make the reader accept an implausible chunk count.
pub const MAX_CHUNKS: u32 = 1_000_000;
/// Bytes of one block hash in the `hashes` section.
pub const BLOCK_HASH: usize = 8;
/// Bytes of one URL+title hash in the `strhashes` section.
pub const STRING_HASH: usize = 8;
/// `RSRCH01\0`.
pub const MAGIC: [u8; 8] = *b"RSRCH01\0";
/// The version this build writes by default: one flat dictionary.
pub const VERSION: u32 = 4;
/// The version a build writes with a term-chunk budget (`--term-chunk-bytes`): the
/// same sections, with the dictionary and its postings grouped into chunks and the
/// BM25 lengths compact when they fit.
pub const VERSION_CHUNKED: u32 = 6;
/// The flat layout of [`VERSION`] with the checksum extended over the header.
///
/// Nothing moves: the header length and the section table are the same, so only the
/// version word and what the checksum covers change. The body still hashes the same
/// way, which is why the reader can treat v7 as "v4 plus a header check".
pub const VERSION_HEADER_COVERED: u32 = 7;
/// The chunked layout of [`VERSION_CHUNKED`] with the header covered as well.
pub const VERSION_CHUNKED_HEADER_COVERED: u32 = 8;
/// The previous chunked version, still read: the same header, with the length table
/// at two `u32`s per document.
pub const VERSION_V5: u32 = 5;
/// The previous version, still read for one release after v4.
pub const VERSION_V3: u32 = 3;
/// Bits 0–1: analyzer id (0 none, 1 English, 2 Italian). See [`Analyzer`].
pub const FLAG_ANALYZER_MASK: u32 = 0b11;
/// Bit 2: each document text is a block (4-byte header + LZ4 or raw bytes).
pub const FLAG_COMPRESSED_TEXTS: u32 = 1 << 2;
/// Bit 3: the artifact carries a non-empty `facets` section.
pub const FLAG_HAS_FACETS: u32 = 1 << 3;
/// Bit 4: the artifact carries a non-empty `titlepostings` section.
pub const FLAG_HAS_TITLE_POSTINGS: u32 = 1 << 4;
/// Bit 5: the v6 BM25 length table stores `(u16 body, u16 title)` per document
/// instead of two `u32`s. A single length above `u16::MAX` makes the build write the
/// wide table for the whole artifact, so the ranking is never approximate.
pub const FLAG_COMPACT_DOC_TOKENS: u32 = 1 << 5;
/// Bit 6: every body posting list carries token positions after its tf stream, so a
/// phrase query can require adjacency (F6, `build --positions`). A reader that does
/// not want them never decodes them; a phrase query against an artifact without the
/// flag is refused instead of answered with a bag of words.
pub const FLAG_HAS_POSITIONS: u32 = 1 << 6;
/// A corrupt header cannot make the reader allocate for more documents than this.
pub const MAX_DOCS: u32 = 20_000_000;
/// Same guard for the dictionary.
pub const MAX_TERMS: u32 = 10_000_000;
/// `url_off, url_len, title_off, title_len, text_off, text_len, doc_len,
/// title_doc_len, hash`.
pub const DOC_RECORD: usize = 40;
/// The v5 page record: the same minus the two BM25 lengths, which live in
/// `doctokens` so a query can rank without reading a record page at all.
/// `url_off, url_len, title_off, title_len, text_off, text_len, hash`.
pub const DOC_RECORD_V5: usize = 32;
/// One document's BM25 lengths in the wide `doctokens` table: two `u32`s.
pub const DOC_TOKENS: usize = 8;
/// One document's BM25 lengths in the compact table: two `u16`s.
pub const DOC_TOKENS_COMPACT: usize = 4;
/// `df, postings_off, postings_len, term_len`.
pub const TERM_FIXED: usize = 14;

/// One section of the file, already bounds-checked against the file length.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Section {
    pub off: usize,
    pub len: usize,
}

/// The parsed, validated header.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Header {
    /// The version the artifact declares: [`VERSION`], [`VERSION_CHUNKED`] or
    /// [`VERSION_V3`].
    pub version: u32,
    pub flags: u32,
    pub doc_count: u32,
    pub term_count: u32,
    pub token_count: u64,
    /// Total tokens in every title, for the title field's BM25 length normalization.
    pub title_token_count: u64,
    /// Empty in a v5 artifact, whose dictionary lives in the chunks instead.
    pub terms: Section,
    /// Empty in a v5 artifact.
    pub postings: Section,
    /// Empty unless v5: the chunk directory (see [`Chunks`]).
    pub chunks: Section,
    /// Empty unless v5: the chunk bytes the directory points into.
    pub chunk_data: Section,
    /// Empty unless v5: the title list of each chunk, back to back (see [`Chunks`]).
    pub title_data: Section,
    /// Empty unless v5: one body+title length pair per document (see [`DocTokens`]).
    pub doc_tokens: Section,
    /// Empty unless v5: the record page table — count, page size, one hash per page.
    /// Small and always fetched with the rest of the metadata (see [`DocPages`]).
    pub doc_page_index: Section,
    /// Empty unless v5: the paged 32-byte records (see [`DocPages`]).
    pub doc_pages: Section,
    pub docs: Section,
    pub strings: Section,
    pub texts: Section,
    pub hashes: Section,
    pub strings_hashes: Section,
    /// Empty in a v3 artifact.
    pub facets: Section,
    /// Empty in a v3 artifact, or when the build indexed no titles.
    pub title_postings: Section,
    pub checksum: u64,
}

#[inline]
pub(crate) fn u32_at(data: &[u8], off: usize) -> Option<u32> {
    let bytes = data.get(off..off.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

#[inline]
pub(crate) fn u64_at(data: &[u8], off: usize) -> Option<u64> {
    let bytes = data.get(off..off.checked_add(8)?)?;
    Some(u64::from_le_bytes([
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
    ]))
}

#[inline]
pub(crate) fn u16_at(data: &[u8], off: usize) -> Option<u16> {
    let bytes = data.get(off..off.checked_add(2)?)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// FNV-1a/64. Small, dependency-free and fast enough for a one-time integrity check.
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// FNV-1a over several slices, as if they were one buffer.
///
/// The artifact checksum covers the header and the body without concatenating them: at
/// 100 MB that is the difference between a hash and a copy, and the reader pays for one
/// pass either way.
pub fn fnv1a64_parts(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for part in parts {
        for byte in *part {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    hash
}

/// `(header_len, spans, has_facet_span, chunked, header_covered)` for a version this
/// build reads; `None` for anything else.
///
/// One table for the reader, the writer's tests and `shard`: a version cannot be
/// half-added, because a layout is never spelled out twice.
pub fn header_layout(version: u32) -> Option<(usize, usize, bool, bool, bool)> {
    Some(match version {
        VERSION => (HEADER_LEN, SPANS, true, false, false),
        VERSION_HEADER_COVERED => (HEADER_LEN, SPANS, true, false, true),
        VERSION_CHUNKED | VERSION_V5 => (HEADER_LEN_V5, SPANS_V5, true, true, false),
        VERSION_CHUNKED_HEADER_COVERED => (HEADER_LEN_V5, SPANS_V5, true, true, true),
        VERSION_V3 => (HEADER_LEN_V3, 7, false, false, false),
        _ => return None,
    })
}

/// The checksum an artifact declares, computed from its bytes alone.
///
/// v3–v6 hash the body. A v7/v8 artifact hashes the header as well, its own checksum
/// field excluded: without that second part a flipped flag or a shifted section span is
/// invisible to the reader, because the body still hashes to the recorded value.
pub fn declared_checksum(data: &[u8]) -> Option<u64> {
    let version = u32_at(data, 8)?;
    let (header_len, _, _, _, header_covered) = header_layout(version)?;
    if data.len() < header_len {
        return None;
    }
    Some(artifact_checksum(data, header_len, header_covered))
}

/// The checksum over the bytes a version covers, with the body starting after the
/// header.
pub fn artifact_checksum(data: &[u8], header_len: usize, header_covered: bool) -> u64 {
    let body = data.get(header_len..).unwrap_or_default();
    if !header_covered {
        return fnv1a64(body);
    }
    let header = data.get(..header_len.saturating_sub(8)).unwrap_or_default();
    fnv1a64_parts(&[header, body])
}

fn checked_section(
    data_len: usize,
    header_len: usize,
    off: u32,
    len: u32,
) -> Result<Section, Error> {
    let off = off as usize;
    let len = len as usize;
    if off < header_len {
        return Err(Error::OutOfBounds);
    }
    let end = off.checked_add(len).ok_or(Error::OutOfBounds)?;
    if end > data_len {
        return Err(Error::OutOfBounds);
    }
    Ok(Section { off, len })
}

impl Header {
    pub(crate) fn parse(data: &[u8]) -> Result<Self, Error> {
        Self::parse_with(data, true)
    }

    /// Parse the header of an artifact assembled from shards, where the body may
    /// contain unfetched text blocks. Every offset and count is still checked;
    /// only the whole-body checksum is skipped, because the shard manifest
    /// verifies the parts it delivered instead.
    pub(crate) fn parse_sparse(data: &[u8]) -> Result<Self, Error> {
        Self::parse_with(data, false)
    }

    fn parse_with(data: &[u8], verify_checksum: bool) -> Result<Self, Error> {
        if data.len() < HEADER_LEN_V3 {
            return Err(Error::TooShort);
        }
        if data.get(0..8) != Some(&MAGIC) {
            return Err(Error::BadMagic);
        }
        let version = u32_at(data, 8).ok_or(Error::TooShort)?;
        let Some((header_len, spans, has_facet_span, chunked, header_covered)) =
            header_layout(version)
        else {
            return Err(Error::UnsupportedVersion(version));
        };
        if data.len() < header_len {
            return Err(Error::TooShort);
        }
        let flags = u32_at(data, 12).ok_or(Error::TooShort)?;
        let mut allowed = FLAG_ANALYZER_MASK | FLAG_COMPRESSED_TEXTS;
        if has_facet_span {
            allowed |= FLAG_HAS_FACETS | FLAG_HAS_TITLE_POSTINGS | FLAG_HAS_POSITIONS;
        }
        if matches!(version, VERSION_CHUNKED | VERSION_CHUNKED_HEADER_COVERED) {
            allowed |= FLAG_COMPACT_DOC_TOKENS;
        }
        if flags & !allowed != 0 {
            return Err(Error::UnsupportedVersion(version));
        }
        if Analyzer::from_id(flags & FLAG_ANALYZER_MASK).is_none() {
            return Err(Error::UnsupportedAnalyzer(flags & FLAG_ANALYZER_MASK));
        }
        let doc_count = u32_at(data, 16).ok_or(Error::TooShort)?;
        let term_count = u32_at(data, 20).ok_or(Error::TooShort)?;
        let token_count = u64_at(data, 24).ok_or(Error::TooShort)?;
        let title_token_count = if has_facet_span {
            u64_at(data, 32).ok_or(Error::TooShort)?
        } else {
            0
        };
        if doc_count > MAX_DOCS || term_count > MAX_TERMS {
            return Err(Error::BadCount);
        }
        if (doc_count == 0) != (term_count == 0) {
            return Err(Error::BadCount);
        }
        // The spans are the last `spans * 8` bytes before the checksum, so the same
        // loop reads a v3 artifact (seven, no statistics in between), a v4 one
        // (nine) and a v5 one (ten).
        let base = header_len
            .checked_sub(8 + spans * 8)
            .ok_or(Error::TooShort)?;
        let mut sections = [Section {
            off: header_len,
            len: 0,
        }; SPANS_V5];
        for (index, section) in sections.iter_mut().enumerate().take(spans) {
            let at = base + index * 8;
            *section = checked_section(
                data.len(),
                header_len,
                u32_at(data, at).ok_or(Error::TooShort)?,
                u32_at(data, at + 4).ok_or(Error::TooShort)?,
            )?;
        }
        let s = sections;
        // The version is the switch, so an artifact can never be half of each layout.
        // v5 spends its spans on the chunked dictionary, the title chunk area, the
        // length table and the paged records; v4 on the flat ones.
        let empty = Section {
            off: header_len,
            len: 0,
        };
        let (
            terms,
            postings,
            chunks,
            chunk_data,
            title_data,
            doc_tokens,
            doc_page_index,
            doc_pages,
            docs,
            strings,
            texts,
            hashes,
            strings_hashes,
            facets,
            title_postings,
        ) = if chunked {
            (
                empty, empty, s[0], s[1], s[2], s[3], s[4], s[5], empty, s[6], s[7], s[8], s[9],
                s[10], empty,
            )
        } else if has_facet_span {
            (
                s[0], s[1], empty, empty, empty, empty, empty, empty, s[2], s[3], s[4], s[5], s[6],
                s[7], s[8],
            )
        } else {
            (
                s[0], s[1], empty, empty, empty, empty, empty, empty, s[2], s[3], s[4], s[5], s[6],
                empty, empty,
            )
        };
        if chunked && chunks.len < 4 {
            return Err(Error::BadCount);
        }
        if (flags & FLAG_HAS_FACETS != 0) != (facets.len != 0) {
            return Err(Error::BadFacets);
        }
        // `titlepostings` in a flat artifact, the `title` area of the chunk data in a
        // chunked one: either way, the flag says whether titles were indexed.
        let titles_len = if chunked {
            title_data.len
        } else {
            title_postings.len
        };
        if (flags & FLAG_HAS_TITLE_POSTINGS != 0) != (titles_len != 0) {
            return Err(Error::BadTitlePostings);
        }
        if titles_len == 0 && title_token_count != 0 {
            // Statistics without the lists they describe cannot be ranked.
            return Err(Error::BadTitlePostings);
        }
        // Both tables are one entry per document: a fixed width, so a reader can
        // address entry `i` without parsing anything.
        if hashes.len as u64 != doc_count as u64 * BLOCK_HASH as u64 {
            return Err(Error::BadCount);
        }
        if strings_hashes.len as u64 != doc_count as u64 * STRING_HASH as u64 {
            return Err(Error::BadCount);
        }
        let token_stride = if flags & FLAG_COMPACT_DOC_TOKENS != 0 {
            DOC_TOKENS_COMPACT
        } else {
            DOC_TOKENS
        };
        if chunked
            && (doc_tokens.len as u64 != 4 + doc_count as u64 * token_stride as u64
                || doc_page_index.len < 8
                || doc_pages.len as u64 != doc_count as u64 * DOC_RECORD_V5 as u64)
        {
            return Err(Error::BadCount);
        }
        let checksum = u64_at(data, header_len - 8).ok_or(Error::TooShort)?;
        if verify_checksum {
            let actual = artifact_checksum(data, header_len, header_covered);
            if actual != checksum {
                return Err(Error::BadChecksum);
            }
        }
        Ok(Self {
            version,
            flags,
            doc_count,
            term_count,
            token_count,
            title_token_count,
            terms,
            postings,
            docs,
            strings,
            texts,
            hashes,
            strings_hashes,
            facets,
            title_postings,
            chunks,
            chunk_data,
            title_data,
            doc_tokens,
            doc_page_index,
            doc_pages,
            checksum,
        })
    }

    /// True for a v5 artifact, whose dictionary is in chunks rather than in one
    /// `terms` section.
    pub(crate) fn is_chunked(self) -> bool {
        matches!(
            self.version,
            VERSION_CHUNKED | VERSION_V5 | VERSION_CHUNKED_HEADER_COVERED
        )
    }

    pub(crate) fn analyzer(self) -> Analyzer {
        // `parse` rejected an id it does not know.
        Analyzer::from_id(self.flags & FLAG_ANALYZER_MASK).unwrap_or(Analyzer::None)
    }

    pub(crate) fn has_compressed_texts(self) -> bool {
        self.flags & FLAG_COMPRESSED_TEXTS != 0
    }

    pub(crate) fn has_positions(self) -> bool {
        self.flags & FLAG_HAS_POSITIONS != 0
    }

    pub(crate) fn has_facets(self) -> bool {
        self.flags & FLAG_HAS_FACETS != 0
    }

    pub(crate) fn has_title_postings(self) -> bool {
        self.flags & FLAG_HAS_TITLE_POSTINGS != 0
    }

    pub(crate) fn compact_doc_tokens(self) -> bool {
        self.flags & FLAG_COMPACT_DOC_TOKENS != 0
    }
}

/// One dictionary entry, resolved against the table that holds it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct RawTerm<'a> {
    pub term: &'a str,
    pub df: u32,
    /// Offset of the postings inside the table's container (the `postings` section in
    /// a v4 artifact, the chunk in a v5 one).
    pub postings_off: u32,
    pub postings_len: u32,
}

/// One term table: `u32 count, u32 offsets[count], entries`.
///
/// This is the whole `terms` section of a v3/v4 artifact and, byte for byte, the body
/// of one v5 chunk. The offsets are relative to the table's own start, so the same
/// code reads a dictionary at either granularity.
///
/// `entries` are laid out back to back; the offset table makes `term(i)` O(1) and
/// binary search O(log n). The terms themselves are sorted by their UTF-8 bytes,
/// which is exactly the order `str`'s `Ord` uses.
#[derive(Clone, Copy)]
pub(crate) struct Table<'a> {
    data: &'a [u8],
    count: usize,
    table: &'a [u8],
}

impl<'a> Table<'a> {
    fn new(data: &'a [u8], count: u32) -> Result<Self, Error> {
        let count = count as usize;
        if u32_at(data, 0).ok_or(Error::OutOfBounds)? as usize != count {
            return Err(Error::BadCount);
        }
        let table_len = count.checked_mul(4).ok_or(Error::BadCount)?;
        let table = data
            .get(4..4usize.saturating_add(table_len))
            .ok_or(Error::OutOfBounds)?;
        Ok(Self { data, count, table })
    }

    #[inline]
    fn entry_off(&self, index: usize) -> Option<usize> {
        u32_at(self.table, index.checked_mul(4)?).map(|v| v as usize)
    }

    fn bytes(&self, index: usize) -> Option<&'a [u8]> {
        if index >= self.count {
            return None;
        }
        let off = self.entry_off(index)?;
        if off < 4 + self.count * 4 {
            return None;
        }
        let term_len = u16_at(self.data, off + 12)? as usize;
        self.data.get(off + TERM_FIXED..off + TERM_FIXED + term_len)
    }

    /// `(term, df, postings_off, postings_len)` for entry `index`.
    fn raw(&self, index: usize) -> Result<RawTerm<'a>, Error> {
        if index >= self.count {
            return Err(Error::OutOfBounds);
        }
        let off = self.entry_off(index).ok_or(Error::OutOfBounds)?;
        let term_len = u16_at(self.data, off + 12).ok_or(Error::OutOfBounds)? as usize;
        let bytes = self
            .data
            .get(off + TERM_FIXED..off + TERM_FIXED + term_len)
            .ok_or(Error::OutOfBounds)?;
        Ok(RawTerm {
            term: str::from_utf8(bytes).map_err(|_| Error::OutOfBounds)?,
            df: u32_at(self.data, off).ok_or(Error::OutOfBounds)?,
            postings_off: u32_at(self.data, off + 4).ok_or(Error::OutOfBounds)?,
            postings_len: u32_at(self.data, off + 8).ok_or(Error::OutOfBounds)?,
        })
    }

    fn validate(&self, postings_len: usize, doc_count: u32) -> Result<(), Error> {
        if self.count == 0 {
            return if self.data.len() == 4 {
                Ok(())
            } else {
                Err(Error::BadCount)
            };
        }
        let mut previous: Option<&[u8]> = None;
        let mut previous_off = 4 + self.count * 4;
        for index in 0..self.count {
            let entry = self.raw(index)?;
            if entry.term.is_empty() || entry.df > doc_count {
                return Err(Error::BadTermOrder);
            }
            let this_off = self.entry_off(index).ok_or(Error::OutOfBounds)?;
            if this_off < previous_off {
                return Err(Error::BadTermOrder);
            }
            if let Some(previous) = previous {
                if previous >= entry.term.as_bytes() {
                    return Err(Error::BadTermOrder);
                }
            }
            if (entry.postings_off as usize)
                .checked_add(entry.postings_len as usize)
                .ok_or(Error::OutOfBounds)?
                > postings_len
            {
                return Err(Error::OutOfBounds);
            }
            previous = Some(entry.term.as_bytes());
            previous_off = this_off;
        }
        Ok(())
    }

    /// First index whose term is `>= needle`.
    fn lower_bound(&self, needle: &[u8]) -> usize {
        let mut low = 0usize;
        let mut high = self.count;
        while low < high {
            let mid = low + (high - low) / 2;
            match self.bytes(mid) {
                Some(term) if term < needle => low = mid + 1,
                Some(_) => high = mid,
                None => return low,
            }
        }
        low
    }
}

/// The v5 chunk directory: which terms each chunk holds, where its bytes are, and
/// the chunk's first term — the key a client binary-searches to know what to fetch
/// without having the dictionary.
///
/// ```text
/// u32 chunk_count
/// entry[chunk_count], 48 bytes each, in term order:
///   u32 first_term      global index of the chunk's first term
///   u32 term_count      terms in this chunk (>= 1)
///   u32 data_off        from the `chunkdata` section start
///   u32 data_len        the chunk's exact length
///   u32 title_off       from the `titledata` section start, 0 when none
///   u32 title_len       0 when none of this chunk's terms occurs in a title
///   u32 key_off         from the directory start
///   u16 key_len         bytes of the chunk's first term
///   u16 reserved        zero
///   u64 hash            FNV-1a of the chunk's bytes
///   u64 title_hash      FNV-1a of its title chunk, 0 when none
/// first-term bytes, back to back
/// ```
///
/// Chunks are contiguous in term order and cover the whole dictionary, so a term's
/// chunk is one binary search away and every term index belongs to exactly one chunk.
/// The title list of the terms in a chunk travels with the chunk, so a query that
/// fetches a chunk's terms can fetch their title lists in the same step.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Chunks<'a> {
    data: &'a [u8],
    count: usize,
    table: &'a [u8],
}

/// One chunk directory entry.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChunkEntry {
    pub first_term: u32,
    pub term_count: u32,
    pub data_off: u32,
    pub data_len: u32,
    pub title_off: u32,
    pub title_len: u32,
    pub hash: u64,
    pub title_hash: u64,
}

impl<'a> Chunks<'a> {
    fn new(data: &'a [u8]) -> Result<Self, Error> {
        let count = u32_at(data, 0).ok_or(Error::BadCount)? as usize;
        if count > MAX_CHUNKS as usize {
            return Err(Error::BadCount);
        }
        let table_len = count.checked_mul(CHUNK_ENTRY).ok_or(Error::BadCount)?;
        let table = data.get(4..4 + table_len).ok_or(Error::OutOfBounds)?;
        Ok(Self { data, count, table })
    }

    fn count(&self) -> usize {
        self.count
    }

    fn entry(&self, index: usize) -> Result<ChunkEntry, Error> {
        if index >= self.count {
            return Err(Error::OutOfBounds);
        }
        let at = index * CHUNK_ENTRY;
        Ok(ChunkEntry {
            first_term: u32_at(self.table, at).ok_or(Error::OutOfBounds)?,
            term_count: u32_at(self.table, at + 4).ok_or(Error::OutOfBounds)?,
            data_off: u32_at(self.table, at + 8).ok_or(Error::OutOfBounds)?,
            data_len: u32_at(self.table, at + 12).ok_or(Error::OutOfBounds)?,
            title_off: u32_at(self.table, at + 16).ok_or(Error::OutOfBounds)?,
            title_len: u32_at(self.table, at + 20).ok_or(Error::OutOfBounds)?,
            hash: u64_at(self.table, at + 32).ok_or(Error::OutOfBounds)?,
            title_hash: u64_at(self.table, at + 40).ok_or(Error::OutOfBounds)?,
        })
    }

    fn key(&self, index: usize) -> Result<&'a [u8], Error> {
        if index >= self.count {
            return Err(Error::OutOfBounds);
        }
        let at = index * CHUNK_ENTRY;
        let off = u32_at(self.table, at + 24).ok_or(Error::OutOfBounds)? as usize;
        let len = u16_at(self.table, at + 28).ok_or(Error::OutOfBounds)? as usize;
        let end = off.checked_add(len).ok_or(Error::OutOfBounds)?;
        self.data.get(off..end).ok_or(Error::OutOfBounds)
    }

    /// Index of the last chunk whose first term is `<= needle`, and `0` when `needle`
    /// sorts before the whole dictionary. The directory is sorted, so that one
    /// binary search tells a client which chunk to fetch.
    fn chunk_of(&self, needle: &[u8]) -> Result<usize, Error> {
        let mut low = 0usize;
        let mut high = self.count;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.key(mid)? <= needle {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low.saturating_sub(1))
    }

    /// Chunk holding global term index `term_index`.
    fn find(&self, term_index: usize) -> Result<usize, Error> {
        let mut low = 0usize;
        let mut high = self.count;
        while low < high {
            let mid = low + (high - low) / 2;
            if self.entry(mid)?.first_term as usize <= term_index {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        Ok(low.saturating_sub(1))
    }

    /// The directory invariants the reader relies on: chunks are contiguous in term
    /// order, their byte ranges are contiguous from zero, every key sorts after the
    /// previous one, the title chunks are contiguous among themselves, and the last
    /// chunk ends on the dictionary and on both sections.
    fn validate(
        &self,
        term_count: u32,
        chunk_data_len: usize,
        title_data_len: usize,
    ) -> Result<(), Error> {
        if self.count == 0 {
            return if term_count == 0 {
                Ok(())
            } else {
                Err(Error::BadCount)
            };
        }
        let mut first = 0u32;
        let mut data_off = 0usize;
        let mut title_off = 0usize;
        let mut previous_key: Option<&[u8]> = None;
        for index in 0..self.count {
            let entry = self.entry(index)?;
            if entry.first_term != first || entry.term_count == 0 {
                return Err(Error::BadCount);
            }
            if entry.data_off as usize != data_off {
                return Err(Error::BadCount);
            }
            let end = (entry.data_off as usize)
                .checked_add(entry.data_len as usize)
                .ok_or(Error::OutOfBounds)?;
            if end > chunk_data_len {
                return Err(Error::OutOfBounds);
            }
            // A chunk with no title list stores 0/0 and no hash; the ones that have
            // one are contiguous from zero.
            if entry.title_len == 0 {
                if entry.title_off != 0 || entry.title_hash != 0 {
                    return Err(Error::BadTitlePostings);
                }
            } else {
                if entry.title_off as usize != title_off || entry.title_hash == 0 {
                    return Err(Error::BadTitlePostings);
                }
                title_off = entry
                    .title_off
                    .checked_add(entry.title_len)
                    .ok_or(Error::OutOfBounds)? as usize;
                if title_off > title_data_len {
                    return Err(Error::OutOfBounds);
                }
            }
            let key = self.key(index)?;
            if key.is_empty() || str::from_utf8(key).is_err() {
                return Err(Error::BadTermOrder);
            }
            if let Some(previous) = previous_key {
                if previous >= key {
                    return Err(Error::BadTermOrder);
                }
            }
            first = first.checked_add(entry.term_count).ok_or(Error::BadCount)?;
            data_off = end;
            previous_key = Some(key);
        }
        if first != term_count || data_off != chunk_data_len || title_off != title_data_len {
            return Err(Error::BadCount);
        }
        Ok(())
    }

    /// `(offset, length)` of a chunk's title list inside the `titledata` section.
    pub(crate) fn title_span(&self, chunk: usize) -> Option<(u32, u32)> {
        let entry = self.entry(chunk).ok()?;
        (entry.title_len != 0).then_some((entry.title_off, entry.title_len))
    }

    pub(crate) fn title_hash(&self, chunk: usize) -> Option<u64> {
        let entry = self.entry(chunk).ok()?;
        (entry.title_len != 0).then_some(entry.title_hash)
    }
}

/// One chunk, resolved for a term index: the table inside it and the slot to read.
#[derive(Clone, Copy)]
struct ChunkSlice<'a> {
    table: Table<'a>,
    local: usize,
    /// The chunk's bytes: what an entry's `postings_off` is relative to.
    bytes: &'a [u8],
}

/// The chunked dictionary: a directory plus the area its chunks live in.
#[derive(Clone, Copy)]
pub(crate) struct ChunkTable<'a> {
    chunks: Chunks<'a>,
    data: &'a [u8],
    /// Total terms in the dictionary (the header count, which `validate` checks
    /// against the directory's last chunk).
    count: usize,
}

impl<'a> ChunkTable<'a> {
    /// The chunk holding `index`, its table and the slot inside it. `None` when the
    /// chunk was never fetched — a sparse artifact leaves it zeroed, which fails the
    /// table's own count check — so every lookup in an absent chunk reads as "no such
    /// term" instead of as a term with an empty posting list.
    fn slice(&self, index: usize) -> Option<ChunkSlice<'a>> {
        let chunk = self.chunks.find(index).ok()?;
        let entry = self.chunks.entry(chunk).ok()?;
        let local = index.checked_sub(entry.first_term as usize)?;
        if local >= entry.term_count as usize {
            return None;
        }
        let start = entry.data_off as usize;
        let bytes = self
            .data
            .get(start..start.checked_add(entry.data_len as usize)?)?;
        Some(ChunkSlice {
            table: Table::new(bytes, entry.term_count).ok()?,
            local,
            bytes,
        })
    }

    fn bytes(&self, index: usize) -> Option<&'a [u8]> {
        let slice = self.slice(index)?;
        slice.table.bytes(slice.local)
    }

    fn raw(&self, index: usize) -> Result<RawTerm<'a>, Error> {
        let slice = self.slice(index).ok_or(Error::OutOfBounds)?;
        slice.table.raw(slice.local)
    }

    /// First index whose term is `>= needle`, across chunks. An unfetched chunk
    /// answers with its own first index, where the prefix scan then stops.
    fn lower_bound(&self, needle: &[u8]) -> usize {
        let Ok(chunk) = self.chunks.chunk_of(needle) else {
            return 0;
        };
        let Ok(entry) = self.chunks.entry(chunk) else {
            return 0;
        };
        let first = entry.first_term as usize;
        match self.slice(first) {
            Some(slice) => first + slice.table.lower_bound(needle),
            None => first,
        }
    }
}

/// The dictionary, flat (v3, v4) or chunked (v5).
///
/// Both shapes answer the same three questions: how many terms, what is term `i`, and
/// where are its postings. The difference is only how many bytes a client has to hold
/// to ask: a flat dictionary is one section, a chunked one is as many chunks as the
/// query touches.
#[derive(Clone, Copy)]
pub(crate) enum Terms<'a> {
    Flat {
        table: Table<'a>,
        postings: &'a [u8],
    },
    Chunked(ChunkTable<'a>),
}

impl<'a> Terms<'a> {
    pub(crate) fn flat(section: &'a [u8], postings: &'a [u8], count: u32) -> Result<Self, Error> {
        Ok(Self::Flat {
            table: Table::new(section, count)?,
            postings,
        })
    }

    pub(crate) fn chunked(
        directory: &'a [u8],
        data: &'a [u8],
        title_data_len: usize,
        count: u32,
    ) -> Result<Self, Error> {
        let chunks = Chunks::new(directory)?;
        chunks.validate(count, data.len(), title_data_len)?;
        Ok(Self::Chunked(ChunkTable {
            chunks,
            data,
            count: count as usize,
        }))
    }

    pub(crate) fn count(&self) -> usize {
        match self {
            Self::Flat { table, .. } => table.count,
            Self::Chunked(table) => table.count,
        }
    }

    pub(crate) fn bytes(&self, index: usize) -> Option<&'a [u8]> {
        match self {
            Self::Flat { table, .. } => table.bytes(index),
            Self::Chunked(table) => table.bytes(index),
        }
    }

    /// `df`, the term and its posting bytes, ready for `read_list`.
    pub(crate) fn entry(&self, index: usize) -> Result<(u32, &'a str, &'a [u8]), Error> {
        let (container, raw) = match self {
            Self::Flat { table, postings } => (*postings, table.raw(index)?),
            Self::Chunked(table) => {
                let slice = table.slice(index).ok_or(Error::OutOfBounds)?;
                (slice.bytes, slice.table.raw(slice.local)?)
            }
        };
        let start = raw.postings_off as usize;
        let end = start
            .checked_add(raw.postings_len as usize)
            .ok_or(Error::OutOfBounds)?;
        let postings = container.get(start..end).ok_or(Error::OutOfBounds)?;
        Ok((raw.df, raw.term, postings))
    }

    /// The document frequency of entry `index`, without touching its postings.
    pub(crate) fn df(&self, index: usize) -> Result<u32, Error> {
        match self {
            Self::Flat { table, .. } => Ok(table.raw(index)?.df),
            Self::Chunked(table) => Ok(table.raw(index)?.df),
        }
    }

    pub(crate) fn lower_bound(&self, needle: &[u8]) -> usize {
        match self {
            Self::Flat { table, .. } => table.lower_bound(needle),
            Self::Chunked(table) => table.lower_bound(needle),
        }
    }

    /// Validate the whole dictionary. `verify_chunks` hashes every loaded chunk's
    /// bytes; a sparse load skips the ones it did not fetch (they are zeroed) but
    /// still checks any chunk that is present.
    pub(crate) fn validate(&self, doc_count: u32, verify_chunks: bool) -> Result<(), Error> {
        match self {
            Self::Flat { table, postings } => table.validate(postings.len(), doc_count),
            Self::Chunked(table) => {
                for chunk in 0..table.chunks.count() {
                    let entry = table.chunks.entry(chunk)?;
                    let start = entry.data_off as usize;
                    let end = start
                        .checked_add(entry.data_len as usize)
                        .ok_or(Error::OutOfBounds)?;
                    let bytes = table.data.get(start..end).ok_or(Error::OutOfBounds)?;
                    let present = u32_at(bytes, 0) == Some(entry.term_count);
                    if !present {
                        if verify_chunks {
                            return Err(Error::BadCount);
                        }
                        continue;
                    }
                    if fnv1a64(bytes) != entry.hash {
                        return Err(Error::BadChecksum);
                    }
                    Table::new(bytes, entry.term_count)?.validate(bytes.len(), doc_count)?;
                }
                Ok(())
            }
        }
    }

    pub(crate) fn chunk_count(&self) -> u32 {
        match self {
            Self::Flat { .. } => 0,
            Self::Chunked(table) => table.chunks.count() as u32,
        }
    }

    /// `(offset, length)` of a chunk inside the `chunkdata` section.
    pub(crate) fn chunk_span(&self, chunk: u32) -> Option<(u32, u32)> {
        match self {
            Self::Flat { .. } => None,
            Self::Chunked(table) => {
                let entry = table.chunks.entry(chunk as usize).ok()?;
                Some((entry.data_off, entry.data_len))
            }
        }
    }

    pub(crate) fn chunk_hash(&self, chunk: u32) -> Option<u64> {
        match self {
            Self::Flat { .. } => None,
            Self::Chunked(table) => table.chunks.entry(chunk as usize).ok().map(|e| e.hash),
        }
    }

    /// The chunk that holds term `index`, in a chunked dictionary.
    pub(crate) fn chunk_of_index(&self, index: usize) -> Option<u32> {
        match self {
            Self::Flat { .. } => None,
            Self::Chunked(table) => {
                if table.chunks.count() == 0 {
                    return None;
                }
                table.chunks.find(index).ok().map(|chunk| chunk as u32)
            }
        }
    }

    /// `(offset, length)` of a chunk's title list inside the `titledata` section.
    pub(crate) fn title_span(&self, chunk: u32) -> Option<(u32, u32)> {
        match self {
            Self::Flat { .. } => None,
            Self::Chunked(table) => table.chunks.title_span(chunk as usize),
        }
    }

    pub(crate) fn title_hash(&self, chunk: u32) -> Option<u64> {
        match self {
            Self::Flat { .. } => None,
            Self::Chunked(table) => table.chunks.title_hash(chunk as usize),
        }
    }

    /// The first term of a chunk: the key a client compares its own query term
    /// against to decide whether the next chunk is in the same prefix range.
    pub(crate) fn chunk_key(&self, chunk: u32) -> Option<&'a str> {
        match self {
            Self::Flat { .. } => None,
            Self::Chunked(table) => {
                let key = table.chunks.key(chunk as usize).ok()?;
                str::from_utf8(key).ok()
            }
        }
    }

    /// The chunk a client should fetch first for `term`.
    pub(crate) fn chunk_for_term(&self, term: &str) -> Option<u32> {
        match self {
            Self::Flat { .. } => None,
            Self::Chunked(table) => {
                if table.chunks.count() == 0 {
                    return None;
                }
                table
                    .chunks
                    .chunk_of(term.as_bytes())
                    .ok()
                    .map(|chunk| chunk as u32)
            }
        }
    }
}

/// One fixed 40-byte document record.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DocRecord {
    pub url_off: u32,
    pub url_len: u32,
    pub title_off: u32,
    pub title_len: u32,
    pub text_off: u32,
    pub text_len: u32,
    /// Tokens in the body text (the BM25 length of the body field).
    pub token_len: u32,
    /// Tokens in the title (the BM25 length of the title field). Zero in a v3
    /// artifact, whose titles were never indexed.
    pub title_token_len: u32,
    pub hash: u64,
}

impl DocRecord {
    /// The v5 page record: the same fields minus the two BM25 lengths, which live in
    /// [`DocTokens`] instead.
    pub(crate) fn parse_v5(data: &[u8], off: usize) -> Result<Self, Error> {
        Ok(Self {
            url_off: u32_at(data, off).ok_or(Error::OutOfBounds)?,
            url_len: u32_at(data, off + 4).ok_or(Error::OutOfBounds)?,
            title_off: u32_at(data, off + 8).ok_or(Error::OutOfBounds)?,
            title_len: u32_at(data, off + 12).ok_or(Error::OutOfBounds)?,
            text_off: u32_at(data, off + 16).ok_or(Error::OutOfBounds)?,
            text_len: u32_at(data, off + 20).ok_or(Error::OutOfBounds)?,
            token_len: 0,
            title_token_len: 0,
            hash: u64_at(data, off + 24).ok_or(Error::OutOfBounds)?,
        })
    }

    pub(crate) fn parse(data: &[u8], index: usize) -> Result<Self, Error> {
        let off = 4usize
            .checked_add(index.checked_mul(DOC_RECORD).ok_or(Error::BadCount)?)
            .ok_or(Error::BadCount)?;
        Ok(Self {
            url_off: u32_at(data, off).ok_or(Error::OutOfBounds)?,
            url_len: u32_at(data, off + 4).ok_or(Error::OutOfBounds)?,
            title_off: u32_at(data, off + 8).ok_or(Error::OutOfBounds)?,
            title_len: u32_at(data, off + 12).ok_or(Error::OutOfBounds)?,
            text_off: u32_at(data, off + 16).ok_or(Error::OutOfBounds)?,
            text_len: u32_at(data, off + 20).ok_or(Error::OutOfBounds)?,
            token_len: u32_at(data, off + 24).ok_or(Error::OutOfBounds)?,
            title_token_len: u32_at(data, off + 28).ok_or(Error::OutOfBounds)?,
            hash: u64_at(data, off + 32).ok_or(Error::OutOfBounds)?,
        })
    }
}

pub(crate) struct Docs<'a> {
    data: &'a [u8],
    count: usize,
}

impl<'a> Docs<'a> {
    pub(crate) fn new(section: &'a [u8], count: u32) -> Result<Self, Error> {
        let count = count as usize;
        if section.len() < 4 {
            return Err(Error::OutOfBounds);
        }
        if u32_at(section, 0).ok_or(Error::OutOfBounds)? as usize != count {
            return Err(Error::BadCount);
        }
        Ok(Self {
            data: section,
            count,
        })
    }

    pub(crate) fn record(&self, index: usize) -> Result<DocRecord, Error> {
        if index >= self.count {
            return Err(Error::OutOfBounds);
        }
        DocRecord::parse(self.data, index)
    }

    pub(crate) fn validate(&self, strings_len: usize, texts_len: usize) -> Result<(), Error> {
        let expected = 4usize
            .checked_add(self.count.checked_mul(DOC_RECORD).ok_or(Error::BadCount)?)
            .ok_or(Error::BadCount)?;
        if self.data.len() != expected {
            return Err(Error::BadCount);
        }
        for index in 0..self.count {
            let record = self.record(index)?;
            let url_end = (record.url_off as usize)
                .checked_add(record.url_len as usize)
                .ok_or(Error::OutOfBounds)?;
            let title_end = (record.title_off as usize)
                .checked_add(record.title_len as usize)
                .ok_or(Error::OutOfBounds)?;
            let text_end = (record.text_off as usize)
                .checked_add(record.text_len as usize)
                .ok_or(Error::OutOfBounds)?;
            if url_end > strings_len || title_end > strings_len || text_end > texts_len {
                return Err(Error::BadDocument);
            }
        }
        Ok(())
    }
}

/// The v5 BM25 length table: one `(body tokens, title tokens)` pair per document.
///
/// Ranking needs a matched document's length and nothing else from its record, so the
/// lengths live on their own: 8 bytes per document instead of 40, in one small object
/// that is fetched once, while the 32-byte records are paged and read only for the
/// documents a client actually renders. A fixed stride makes a document one multiply
/// away.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DocTokens<'a> {
    data: &'a [u8],
    count: usize,
    compact: bool,
}

impl<'a> DocTokens<'a> {
    pub(crate) fn new(section: &'a [u8], doc_count: u32, compact: bool) -> Result<Self, Error> {
        let count = doc_count as usize;
        let stride = if compact {
            DOC_TOKENS_COMPACT
        } else {
            DOC_TOKENS
        };
        let expected = 4usize
            .checked_add(count.checked_mul(stride).ok_or(Error::BadCount)?)
            .ok_or(Error::BadCount)?;
        if u32_at(section, 0).ok_or(Error::OutOfBounds)? as usize != count
            || section.len() != expected
        {
            return Err(Error::BadCount);
        }
        Ok(Self {
            data: section,
            count,
            compact,
        })
    }

    /// `(body tokens, title tokens)` of one document.
    pub(crate) fn lengths(&self, doc: usize) -> Result<(u32, u32), Error> {
        if doc >= self.count {
            return Err(Error::OutOfBounds);
        }
        if self.compact {
            let off = 4 + doc * DOC_TOKENS_COMPACT;
            let body = u16_at(self.data, off).ok_or(Error::OutOfBounds)?;
            let title = u16_at(self.data, off + 2).ok_or(Error::OutOfBounds)?;
            return Ok((body as u32, title as u32));
        }
        let off = 4 + doc * DOC_TOKENS;
        Ok((
            u32_at(self.data, off).ok_or(Error::OutOfBounds)?,
            u32_at(self.data, off + 4).ok_or(Error::OutOfBounds)?,
        ))
    }
}

/// The v5 paged document records, in two sections.
///
/// ```text
/// docpageindex
///   u32 doc_count
///   u32 page_records           records per page (>= 1, the page size in records)
///   u64 page_hash[page_count]  FNV-1a of each page's bytes
///
/// docrecords
///   record[doc_count], 32 bytes each
/// ```
///
/// The table and the records are separate sections because they have different
/// readers. The table is metadata — small, fixed, and needed to address any page at
/// all — so it travels with the rest of the metadata in one object. The records are
/// the payload: a shard client reads the page of each document it renders and never
/// the other pages. The page of a document is `doc / page_records`, so no directory
/// is needed, and each page is hashed so a single page read is verifiable on its own.
#[derive(Debug, Clone, Copy)]
pub(crate) struct DocPages<'a> {
    /// The page table: count, page size, hashes.
    table: &'a [u8],
    /// The records; zeroed at the offsets a sparse client never fetched.
    data: &'a [u8],
    count: usize,
    page_records: usize,
}

impl<'a> DocPages<'a> {
    pub(crate) fn new(table: &'a [u8], records: &'a [u8], doc_count: u32) -> Result<Self, Error> {
        let count = doc_count as usize;
        if u32_at(table, 0).ok_or(Error::OutOfBounds)? as usize != count {
            return Err(Error::BadCount);
        }
        let page_records = u32_at(table, 4).ok_or(Error::OutOfBounds)? as usize;
        if page_records == 0 {
            return Err(Error::BadCount);
        }
        let pages = count.div_ceil(page_records);
        let table_len = 8usize
            .checked_add(pages.checked_mul(BLOCK_HASH).ok_or(Error::BadCount)?)
            .ok_or(Error::BadCount)?;
        if table.len() != table_len {
            return Err(Error::BadCount);
        }
        if records.len() != count.checked_mul(DOC_RECORD_V5).ok_or(Error::BadCount)? {
            return Err(Error::BadCount);
        }
        Ok(Self {
            table,
            data: records,
            count,
            page_records,
        })
    }

    pub(crate) fn page_records(&self) -> usize {
        self.page_records
    }

    pub(crate) fn page_count(&self) -> usize {
        self.count.div_ceil(self.page_records)
    }

    /// The page a document lives on.
    pub(crate) fn page_of(&self, doc: usize) -> Option<usize> {
        (doc < self.count).then(|| doc / self.page_records)
    }

    /// Records in one page (the last page may be shorter).
    fn records_in(&self, page: usize) -> usize {
        self.count
            .saturating_sub(page * self.page_records)
            .min(self.page_records)
    }

    /// `(offset, length)` of a page's records inside the `docrecords` section.
    pub(crate) fn page_span(&self, page: usize) -> Option<(u32, u32)> {
        if page >= self.page_count() {
            return None;
        }
        let off = page * self.page_records * DOC_RECORD_V5;
        Some((off as u32, (self.records_in(page) * DOC_RECORD_V5) as u32))
    }

    pub(crate) fn page_hash(&self, page: usize) -> Option<u64> {
        if page >= self.page_count() {
            return None;
        }
        u64_at(self.table, 8 + page * BLOCK_HASH)
    }

    pub(crate) fn record(&self, doc: usize) -> Result<DocRecord, Error> {
        if doc >= self.count {
            return Err(Error::OutOfBounds);
        }
        let off = doc * DOC_RECORD_V5;
        DocRecord::parse_v5(self.data, off)
    }

    /// Every record of every page checked against its own bytes' hash and against the
    /// string and text sections. `verify_pages` skips the pages a sparse client never
    /// fetched (its records are zeroed, which is indistinguishable from a document at
    /// offset 0 of length 0 — the range check still passes, the hash does not).
    pub(crate) fn validate(
        &self,
        strings_len: usize,
        texts_len: usize,
        verify_pages: bool,
        present: impl Fn(usize) -> bool,
    ) -> Result<(), Error> {
        for page in 0..self.page_count() {
            let (off, len) = self.page_span(page).ok_or(Error::BadCount)?;
            let bytes = self
                .data
                .get(off as usize..off as usize + len as usize)
                .ok_or(Error::OutOfBounds)?;
            let loaded = present(page);
            if !loaded && !verify_pages {
                continue;
            }
            if fnv1a64(bytes) != self.page_hash(page).ok_or(Error::BadCount)? {
                return Err(Error::BadChecksum);
            }
        }
        for doc in 0..self.count {
            let record = self.record(doc)?;
            let url_end = (record.url_off as usize)
                .checked_add(record.url_len as usize)
                .ok_or(Error::OutOfBounds)?;
            let title_end = (record.title_off as usize)
                .checked_add(record.title_len as usize)
                .ok_or(Error::OutOfBounds)?;
            let text_end = (record.text_off as usize)
                .checked_add(record.text_len as usize)
                .ok_or(Error::OutOfBounds)?;
            if url_end > strings_len || title_end > strings_len || text_end > texts_len {
                return Err(Error::BadDocument);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_split_hash_is_the_hash_of_the_concatenation() {
        let (a, b) = (b"header bytes".as_slice(), b"body bytes".as_slice());
        let mut joined = std::vec::Vec::new();
        joined.extend_from_slice(a);
        joined.extend_from_slice(b);
        assert_eq!(fnv1a64_parts(&[a, b]), fnv1a64(&joined));
    }

    #[test]
    fn checksum_changes_with_one_bit() {
        let mut bytes = b"static search".to_vec();
        let before = fnv1a64(&bytes);
        bytes[0] ^= 1;
        assert_ne!(before, fnv1a64(&bytes));
    }

    #[test]
    fn a_short_file_is_rejected_before_reading() {
        assert!(matches!(Header::parse(&[]), Err(Error::TooShort)));
        assert!(matches!(Header::parse(&[0u8; 79]), Err(Error::TooShort)));
    }

    #[test]
    fn the_analyzer_lives_in_the_low_flag_bits() {
        assert_eq!(Analyzer::None.id(), 0);
        assert_eq!(Analyzer::English.id(), 1);
        assert_eq!(Analyzer::Italian.id(), 2);
        for analyzer in [Analyzer::None, Analyzer::English, Analyzer::Italian] {
            assert_eq!(Analyzer::from_id(analyzer.id()), Some(analyzer));
        }
        assert_eq!(Analyzer::from_id(3), None);
    }

    #[test]
    fn doc_tokens_reads_both_widths() {
        let mut compact = alloc::vec::Vec::new();
        compact.extend_from_slice(&2u32.to_le_bytes());
        compact.extend_from_slice(&7u16.to_le_bytes());
        compact.extend_from_slice(&3u16.to_le_bytes());
        compact.extend_from_slice(&u16::MAX.to_le_bytes());
        compact.extend_from_slice(&1u16.to_le_bytes());
        let tokens = DocTokens::new(&compact, 2, true).expect("compact table");
        assert_eq!(tokens.lengths(0).expect("doc 0"), (7, 3));
        assert_eq!(tokens.lengths(1).expect("doc 1"), (u16::MAX as u32, 1));

        let mut wide = alloc::vec::Vec::new();
        wide.extend_from_slice(&1u32.to_le_bytes());
        wide.extend_from_slice(&70_000u32.to_le_bytes());
        wide.extend_from_slice(&2u32.to_le_bytes());
        let tokens = DocTokens::new(&wide, 1, false).expect("wide table");
        assert_eq!(tokens.lengths(0).expect("doc 0"), (70_000, 2));
        assert!(DocTokens::new(&wide, 1, true).is_err());
    }
}
