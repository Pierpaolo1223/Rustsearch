#![no_main]

use libfuzzer_sys::fuzz_target;
use rustsearch_core::{Index, SearchOptions};

// The sparse path a shard client drives: load without the body checksum, then hand
// the engine arbitrary bytes as if they were a fetched block, string, chunk or hash.
// Every attachment must verify and refuse, and no lookup may panic.
fuzz_target!(|data: &[u8]| {
    let Ok(mut index) = Index::from_sparse(data.to_vec()) else {
        return;
    };
    let end = data.len().min(64);
    let query = String::from_utf8_lossy(&data[..end]).to_string();
    let _ = index.search(&query, SearchOptions { limit: 5, ..Default::default() });
    let _ = index.chunks_for_query(
        &query,
        SearchOptions {
            fuzzy: true,
            ..Default::default()
        },
    );
    for chunk in 0..index.chunk_count().min(4) {
        let _ = index.attach_term_chunk(chunk, data.to_vec());
        let _ = index.attach_title_chunk(chunk, data.to_vec());
    }
    for doc in 0..index.doc_count().min(4) {
        let _ = index.attach_text_block(doc, data.to_vec());
        let _ = index.attach_string_block(doc, data.to_vec());
        let _ = index.attach_text_hash(doc, u64::from_le_bytes(data[..8.min(data.len())].try_into().unwrap_or([0; 8])));
        let _ = index.attach_string_hash(doc, u64::from_le_bytes(data[..8.min(data.len())].try_into().unwrap_or([0; 8])));
    }
    let _ = index.text_span(0);
    let _ = index.string_span(0);
    let _ = index.doc_page_of(0);
});
