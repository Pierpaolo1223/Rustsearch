//! The runtime of `rustsearch`: an immutable, checksummed index and a BM25 query
//! engine that does no I/O.
//!
//! The crate is `no_std` + `alloc` on purpose. The same code is compiled into the
//! browser/Worker WASM module and into the native CLI, so the format and the
//! ranking are identical in both places. Nothing here opens a file, starts a
//! thread or assumes a 64-bit word: the caller hands the bytes in.
//!
//! ```text
//! let bytes = std::fs::read("index.rsx")?;          // native
//! let index = rustsearch_core::Index::from_bytes(bytes)?;
//! let hits = index.search("bounded concurrency", Default::default());
//! ```
//!
//! The format is described in `DESIGN.md` and in [`format`]. It is little-endian,
//! offset-addressed and validated before any query runs.
#![no_std]
#![forbid(unsafe_code)]
// H5: the reader and the ranking never panic on input. `unwrap`/`expect` are denied
// in this crate; the generated Snowball stemmer modules are the one exception (see
// the `stemmer` declaration below), because they are a transcription of an upstream
// table and not code this project writes.
#![deny(clippy::unwrap_used, clippy::expect_used)]
// Tests assert on results with `unwrap`; the shipped code is the part the lint is
// about, not the harness.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

extern crate alloc;

#[cfg(any(feature = "std", test))]
extern crate std;

pub mod analyze;
pub mod error;
pub mod facets;
pub mod format;
pub mod index;
pub mod json;

pub mod postings;
pub mod rules;
pub mod sha256;
#[cfg(test)]
mod snippet_props;
#[allow(clippy::all)]
pub mod stemmer;
pub mod stopwords;
pub mod title;
pub mod tokenize;
pub mod varint;

pub use analyze::Analyzer;
pub use error::Error;
pub use facets::Filter;
pub use index::{
    ChunkPlan, Correction, Hit, Index, Layout, SearchOptions, SearchReport, StringSpan, B,
    DEFAULT_TITLE_BOOST, K1, MAX_FUZZY_CHUNKS, MAX_TITLE_BOOST, MAX_TYPO_CORRECTIONS,
    MIN_TYPO_BYTES, TYPO_WEIGHT,
};
pub use json::{
    facets_to_json, hits_to_json, hits_to_json_with_filters, json_escape, search_report_to_json,
    search_report_to_json_with_filters,
};
pub use rules::{Curations, Rule, RuleError, Synonyms};
pub use sha256::{sha256, sha256_hex};
pub use tokenize::tokenize;
