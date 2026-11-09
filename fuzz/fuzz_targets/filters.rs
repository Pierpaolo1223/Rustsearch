#![no_main]

use libfuzzer_sys::fuzz_target;
use rustsearch_core::{Filter, Index, SearchOptions};

// The filter pairs arrive from a query string: arbitrary UTF-8, arbitrary lengths,
// arbitrary unicode. Resolving them must answer or fail, never over-read a table.
fuzz_target!(|data: &[u8]| {
    let Ok(index) = Index::from_bytes(data.to_vec()) else {
        return;
    };
    let text = String::from_utf8_lossy(data);
    let mut parts = text.split('\u{1e}');
    let field = parts.next().unwrap_or("");
    let value = parts.next().unwrap_or("");
    let filters = [Filter::new(field, value)];
    let _ = index.try_search_with_filters(
        field,
        SearchOptions {
            limit: 5,
            fuzzy: true,
            ..Default::default()
        },
        &filters,
    );
    let _ = index.facet_values(field);
    let _ = index.facet_fields();
});
