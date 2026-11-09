use core::fmt;

/// Everything that can go wrong while opening or querying an index.
///
/// The variants are deliberately coarse: a caller cannot repair a corrupt index,
/// so the useful distinction is *why* the bytes were rejected (magic, version,
/// checksum, a range) and not the exact byte offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Fewer bytes than a header.
    TooShort,
    /// The first eight bytes are not [`crate::format::MAGIC`].
    BadMagic,
    /// The format version is newer (or older) than this build understands.
    UnsupportedVersion(u32),
    /// The header selects an analyzer id this build does not implement.
    UnsupportedAnalyzer(u32),
    /// The FNV-1a checksum of the body does not match the header.
    BadChecksum,
    /// A section, offset or length points outside the file.
    OutOfBounds,
    /// A count is implausible for the file size (a corrupt header).
    BadCount,
    /// The term dictionary is not strictly sorted, or an offset is not monotonic.
    BadTermOrder,
    /// A document record, text block or string block does not match its table.
    BadDocument,
    /// The `facets` section is malformed: bad offsets, unsorted names or a doc list
    /// that does not end on its entry boundary.
    BadFacets,
    /// A filter was applied to an artifact that carries no facet section (built
    /// before v4, or from a site with no metadata).
    UnsupportedFacet,
    /// The `titlepostings` section is malformed: unsorted term indices, a count that
    /// overruns the section, or statistics without the lists they describe.
    BadTitlePostings,
    /// The query exceeds the engine's hard limits.
    QueryTooLong,
    /// A phrase query was applied to an artifact built without `--positions`, so the
    /// engine has no ordinals to check adjacency against.
    NoPositions,
    /// A shard store failed to return the requested bytes.
    Source,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooShort => f.write_str("index is shorter than its header"),
            Error::BadMagic => f.write_str("not a rustsearch index (bad magic)"),
            Error::UnsupportedVersion(v) => write!(f, "unsupported index version {v}"),
            Error::UnsupportedAnalyzer(id) => write!(f, "unsupported analyzer id {id}"),
            Error::BadChecksum => f.write_str("index checksum mismatch (corrupt or truncated)"),
            Error::OutOfBounds => f.write_str("index section points outside the file"),
            Error::BadCount => f.write_str("index count is implausible"),
            Error::BadTermOrder => f.write_str("term dictionary is not sorted"),
            Error::BadDocument => f.write_str("document record points outside its section"),
            Error::BadFacets => f.write_str("facet section is malformed"),
            Error::UnsupportedFacet => f.write_str("this index carries no facets to filter on"),
            Error::BadTitlePostings => f.write_str("title postings are malformed"),
            Error::QueryTooLong => f.write_str("query exceeds the engine limits"),
            Error::NoPositions => f.write_str(
                "this index has no token positions: rebuild with --positions for phrase queries",
            ),
            Error::Source => f.write_str("a shard could not be fetched"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn every_variant_has_a_message() {
        for error in [
            Error::TooShort,
            Error::BadMagic,
            Error::UnsupportedVersion(9),
            Error::UnsupportedAnalyzer(3),
            Error::BadChecksum,
            Error::OutOfBounds,
            Error::BadCount,
            Error::BadTermOrder,
            Error::BadDocument,
            Error::BadFacets,
            Error::UnsupportedFacet,
            Error::BadTitlePostings,
            Error::QueryTooLong,
            Error::NoPositions,
            Error::Source,
        ] {
            assert!(!format!("{error}").is_empty());
        }
    }
}
