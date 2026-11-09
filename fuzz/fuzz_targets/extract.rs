#![no_main]

use libfuzzer_sys::fuzz_target;
use std::path::Path;

// The extractor is the one place a page's own bytes are parsed: malformed HTML,
// unclosed tags, huge attribute values, control characters and invalid UTF-8.
fuzz_target!(|data: &[u8]| {
    let html = String::from_utf8_lossy(data);
    let _ = rustsearch_index::extract_page(&html);
    let _ = rustsearch_index::extract_page_with_facets(&html, Path::new("a/b.html"));
});
