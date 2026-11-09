#![no_main]

use libfuzzer_sys::fuzz_target;

// The whole reader on arbitrary bytes: magic, version, flags, every section span,
// the dictionary, the record table, the page table, the checksum.
fuzz_target!(|data: &[u8]| {
    let _ = rustsearch_core::Index::from_bytes(data.to_vec());
});
