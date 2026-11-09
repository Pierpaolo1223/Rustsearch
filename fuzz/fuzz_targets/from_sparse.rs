#![no_main]

use libfuzzer_sys::fuzz_target;

// The shard path, where the only thing between a hostile buffer and a query is the
// per-section validation: `from_sparse` has no whole-body checksum, because the
// manifest is what verifies the parts a client fetched. The header bytes are part of
// the input, so a mutated flags word or span table reaches the decoders exactly the
// way it would reach a client. A query runs after a successful parse to exercise the
// postings, title and record-page readers, not just the parser.
fuzz_target!(|data: &[u8]| {
    let Ok(index) = rustsearch_core::Index::from_sparse(data.to_vec()) else {
        return;
    };
    let _ = index.doc_count();
    let _ = index.search(
        "fuzz query marker",
        rustsearch_core::SearchOptions {
            limit: 8,
            ..Default::default()
        },
    );
    let _ = index.search(
        "\"fuzz query\"",
        rustsearch_core::SearchOptions {
            limit: 8,
            ..Default::default()
        },
    );
});
