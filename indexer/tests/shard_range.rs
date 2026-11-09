//! End-to-end check of the shard set over real HTTP `Range` requests.
//!
//! A tiny loopback server answers `GET` with `206 Partial Content`, the way R2
//! does. The test then proves three things:
//!
//! 1. fetching every part and reassembling yields the artifact byte for byte
//!    (and the engine searches it),
//! 2. a query downloads the dictionary, the postings, the records and both hash
//!    tables, plus only the strings and text blocks of the hits it will render, so
//!    it costs less than the whole artifact,
//! 3. every `206` body is the exact slice the artifact has at that offset.
use std::error::Error;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rustsearch_core::{Filter, Index, SearchOptions};
use rustsearch_index::shard::{self, DocRecord, Manifest};

#[test]
fn shards_reassemble_and_a_query_reads_only_what_it_needs() -> Result<(), Box<dyn Error>> {
    let site = fixture_site("flat")?;
    let build = rustsearch_index::build_site(&site, &Default::default())?;
    let artifact = build.bytes;
    let set = shard::split(&artifact, "index/")?;

    // Publish the shard set to a directory the loopback server can read.
    let dir = temp_dir(&site, "shards");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("manifest.txt"), &set.manifest)?;
    for part in &set.parts {
        let name = Path::new(&part.key).file_name().unwrap();
        fs::write(dir.join(name), &part.bytes)?;
    }

    let server = Server::start(&dir)?;
    let manifest = Manifest::parse(&server.fetch_text("manifest.txt")?)?;
    assert_eq!(manifest.total, artifact.len());

    // (1) Full fetch through Range requests, then reassemble and search.
    let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
    for part in &manifest.parts {
        let (status, body) = server.get(&part.key, Some(0..part.len))?;
        assert_eq!(status, 206, "{} should be a partial response", part.key);
        assert_eq!(body.len(), part.len);
        assert_eq!(
            body.as_slice(),
            &artifact[part.off..part.off + part.len],
            "{} is not the artifact slice at {}",
            part.key,
            part.off
        );
        fetched.push((part.key.clone(), body));
    }
    let assembled = shard::assemble(&manifest, &fetched)?;
    assert_eq!(assembled, artifact, "shards must reassemble losslessly");

    let index = Index::from_bytes(assembled)
        .map_err(|error| format!("reassembled artifact is invalid: {error}"))?;
    let hits = index.search(
        "walkdir",
        SearchOptions {
            limit: 3,
            ..Default::default()
        },
    );
    assert!(!hits.is_empty(), "the assembled index must still rank");

    // The v4 taxonomy: two sections, one language, one keyword, and a filter that
    // keeps only the guide pages.
    assert_eq!(index.facet_fields(), vec!["lang", "section", "tags"]);
    assert_eq!(
        index
            .facet_values("section")
            .map(|values| values.iter().map(|(value, _)| *value).collect::<Vec<_>>()),
        Some(vec!["guide", "reference"])
    );
    let filtered = index
        .try_search_with_filters(
            "walkdir",
            SearchOptions {
                limit: 50,
                ..Default::default()
            },
            &[Filter::new("section", "guide")],
        )
        .map_err(|error| format!("filtered search: {error}"))?;
    assert!(!filtered.is_empty());
    assert!(
        filtered.iter().all(|hit| hit.url.contains("/guide/")),
        "a filter must keep only its own documents"
    );
    assert!(
        index
            .try_search_with_filters(
                "walkdir",
                SearchOptions::default(),
                &[Filter::new("lang", "fr")]
            )
            .map(|hits| hits.is_empty())
            .unwrap_or(false),
        "an unknown value is an empty result, not an error"
    );

    let full_fetch_requests = server.log.lock().unwrap().len();

    // (2) The plan a snippet-rendering query follows with Range requests.
    let records = read_records(&artifact, &manifest, index.doc_count())?;
    let mut wanted: Vec<(u32, DocRecord)> = Vec::new();
    for hit in &hits {
        let id = records
            .iter()
            .position(|(_, url)| *url == hit.url)
            .expect("every hit has a record") as u32;
        wanted.push((id, records[id as usize].0));
    }
    let mut query_bytes = set.manifest.len();
    let mut requests = 1usize; // the manifest itself

    // The dictionary, the postings, the records and both hash tables are whole
    // objects: they are small and every query needs them. `strings` and `texts` are
    // read per hit, so they are deliberately absent here.
    let mut whole: Vec<(String, Vec<u8>)> = Vec::new();
    for name in [
        "head",
        "terms",
        "postings",
        "docs",
        "hashes",
        "strhashes",
        "facets",
        "titlepostings",
    ] {
        let Some(part) = manifest.part(name) else {
            assert!(
                name == "facets" || name == "titlepostings",
                "only the optional sections may be missing"
            );
            continue;
        };
        let (status, body) = server.get(&part.key, Some(0..part.len))?;
        assert_eq!(status, 206, "{name}");
        query_bytes += body.len();
        requests += 1;
        whole.push((part.key.clone(), body));
    }
    let hashes_part = &artifact[manifest.part("hashes").unwrap().off
        ..manifest.part("hashes").unwrap().off + manifest.part("hashes").unwrap().len];
    assert_eq!(hashes_part.len(), index.doc_count() as usize * 8);
    let strhashes_part = &artifact[manifest.part("strhashes").unwrap().off
        ..manifest.part("strhashes").unwrap().off + manifest.part("strhashes").unwrap().len];
    assert_eq!(strhashes_part.len(), index.doc_count() as usize * 8);

    // The strings are read one hit at a time: the builder writes the URL and title
    // back to back, so one range covers both, and the header's `strhashes` entry
    // authenticates that read on its own.
    let strings_part = artifact_strings(&artifact, &manifest)?;
    let mut string_blocks: Vec<(u32, Vec<u8>)> = Vec::new();
    for (id, record) in &wanted {
        let range = manifest
            .string_span(record)
            .ok_or("url and title should be adjacent")?;
        let (status, body) = server.get("index/strings.bin", Some(range.clone()))?;
        assert_eq!(status, 206);
        assert_eq!(body.as_slice(), &strings_part[range.clone()]);
        let at = *id as usize * 8;
        assert_eq!(
            u64::from_le_bytes(strhashes_part[at..at + 8].try_into().unwrap()),
            rustsearch_core::format::fnv1a64(&body),
            "the string hash must match its own Range read"
        );
        query_bytes += body.len();
        requests += 1;
        string_blocks.push((*id, body));
    }

    // The text section is read the same way, and each block is checked against its
    // own 8-byte hash, fetched with a `Range` of its own.
    let texts_part = artifact_texts(&artifact, &manifest)?;
    let mut blocks: Vec<(u32, Vec<u8>)> = Vec::new();
    for (id, record) in &wanted {
        let range = manifest.text_ranges(std::slice::from_ref(record))[0].clone();
        let (status, body) = server.get("index/texts.bin", Some(range.clone()))?;
        assert_eq!(status, 206);
        assert_eq!(body.as_slice(), &texts_part[range.clone()]);
        let hash_range = manifest
            .hash_range(*id)
            .ok_or("no hash for this document")?;
        let (status, hash_body) = server.get("index/hashes.bin", Some(hash_range.clone()))?;
        assert_eq!(status, 206);
        assert_eq!(
            hash_body.as_slice(),
            &hashes_part[hash_range.start..hash_range.start + 8]
        );
        assert_eq!(
            u64::from_le_bytes(hash_body[..8].try_into().unwrap()),
            rustsearch_core::format::fnv1a64(&body),
            "the block hash must match its own Range read"
        );
        query_bytes += body.len() + hash_body.len();
        requests += 2;
        blocks.push((*id, body));
    }

    assert!(
        query_bytes < artifact.len(),
        "a query ({query_bytes} bytes) must cost less than the artifact ({} bytes)",
        artifact.len()
    );
    println!(
        "artifact {} bytes / one request\n  \
         query  {query_bytes} bytes / {requests} range requests ({:.0}% of the artifact)\n  \
         texts  {} of {} bytes fetched for {} hits",
        artifact.len(),
        100.0 * query_bytes as f64 / artifact.len() as f64,
        blocks.iter().map(|(_, block)| block.len()).sum::<usize>(),
        manifest.part("texts").unwrap().len,
        blocks.len(),
    );

    // The partial buffer omits the text section entirely; the blocks arrive by
    // `Range` and the engine verifies each one against the header hash table.
    let partial = shard::assemble_partial(&manifest, &whole, &[])?;
    let mut sparse =
        Index::from_sparse(partial).map_err(|error| format!("sparse load: {error}"))?;
    assert!(
        sparse
            .search(
                "crate",
                SearchOptions {
                    limit: 50,
                    ..Default::default()
                }
            )
            .iter()
            .all(|hit| hit.snippet.is_empty() && hit.url.is_empty()),
        "without blocks a hit has neither strings nor a snippet"
    );
    for (id, block) in &string_blocks {
        sparse
            .attach_string_block(*id, block.clone())
            .map_err(|error| format!("string block {id} rejected: {error}"))?;
    }
    for (id, block) in &blocks {
        sparse
            .attach_text_block(*id, block.clone())
            .map_err(|error| format!("block {id} rejected: {error}"))?;
    }
    assert_eq!(sparse.attached_string_blocks(), string_blocks.len());
    assert_eq!(sparse.attached_blocks(), blocks.len());
    let again = sparse.search(
        "walkdir",
        SearchOptions {
            limit: 3,
            ..Default::default()
        },
    );
    assert_eq!(
        again.iter().map(|hit| hit.url.clone()).collect::<Vec<_>>(),
        hits.iter().map(|hit| hit.url.clone()).collect::<Vec<_>>(),
        "the partial index must rank the same documents"
    );
    assert!(
        again.iter().any(|hit| hit.snippet.contains("walkdir")),
        "an attached block must produce a snippet"
    );
    // The facet and title sections rode along with the metadata parts, so neither a
    // filtered nor a boosted query needs an extra request.
    assert!(sparse.has_facets());
    assert!(sparse.has_title_postings());
    assert_eq!(
        sparse.title_token_count(),
        index.title_token_count(),
        "the statistics travel with the lists"
    );
    // Only the blocks of the first three hits are attached, so rank and score are
    // the comparable part: a URL beyond that is empty by design.
    let boosted = |engine: &Index| -> Vec<(u32, String)> {
        engine
            .search(
                "walkdir",
                SearchOptions {
                    limit: 50,
                    ..Default::default()
                },
            )
            .into_iter()
            .map(|hit| (hit.doc_id, format!("{:.4}", hit.score)))
            .collect()
    };
    assert_eq!(
        boosted(&sparse),
        boosted(&index),
        "a sparse index must rank titles exactly like the whole artifact"
    );
    // Grouping needs no strings either: the page identity is the URL offset in the
    // doc record, so the sparse index collapses a page without fetching one.
    let grouped = |engine: &Index| -> Vec<(u32, String, u32)> {
        engine
            .search(
                "walkdir",
                SearchOptions {
                    limit: 50,
                    group_by_url: true,
                    ..Default::default()
                },
            )
            .into_iter()
            .map(|hit| (hit.doc_id, format!("{:.4}", hit.score), hit.matches))
            .collect()
    };
    assert_eq!(
        grouped(&sparse),
        grouped(&index),
        "a sparse index groups the pages exactly like the whole artifact"
    );
    assert!(
        grouped(&index).iter().all(|(_, _, matches)| *matches == 1),
        "this fixture has one document per page"
    );
    let sparse_filtered = sparse
        .try_search_with_filters(
            "walkdir",
            SearchOptions {
                limit: 50,
                ..Default::default()
            },
            &[Filter::new("section", "guide")],
        )
        .map_err(|error| format!("sparse filtered search: {error}"))?;
    assert_eq!(
        sparse_filtered
            .iter()
            .map(|hit| hit.doc_id)
            .collect::<Vec<_>>(),
        filtered.iter().map(|hit| hit.doc_id).collect::<Vec<_>>(),
        "a fetch-once facet section must give the same set as the full artifact"
    );

    // A block that does not match the header hash is refused, text and strings
    // alike.
    let (id, block) = &blocks[0];
    let mut corrupt = block.clone();
    corrupt[5] ^= 0x40;
    assert!(
        sparse.attach_text_block(*id, corrupt).is_err(),
        "a corrupted block must be rejected"
    );
    let (id, block) = &string_blocks[0];
    let mut corrupt = block.clone();
    corrupt[0] ^= 0x20;
    assert!(
        sparse.attach_string_block(*id, corrupt).is_err(),
        "a corrupted string block must be rejected"
    );
    assert!(
        sparse
            .attach_string_block(*id, block[..block.len() - 1].to_vec())
            .is_err(),
        "a truncated string block must be rejected"
    );

    // Nothing read the two big sections whole: every request was a slice.
    let texts_len = manifest.part("texts").unwrap().len;
    let strings_len = manifest.part("strings").unwrap().len;
    let log = server.log.lock().unwrap();
    let after_full_fetch = &log[full_fetch_requests..];
    for (name, section_len) in [("texts.bin", texts_len), ("strings.bin", strings_len)] {
        let section_requests: Vec<_> = after_full_fetch
            .iter()
            .filter(|request| request.path.ends_with(name))
            .collect();
        assert!(
            !section_requests.is_empty(),
            "{name} should be read per hit"
        );
        for request in section_requests {
            let (start, end) = request
                .range
                .expect("the big sections are only read by range");
            assert!(
                end - start < section_len,
                "a {name} request covered the whole section"
            );
        }
    }
    drop(log);

    // (3) A `Range` past the end of an object is refused, not silently clamped.
    let texts = manifest.part("texts").unwrap().len;
    let (status, _) = server.get("index/texts.bin", Some(texts + 10..texts + 20))?;
    assert_eq!(status, 416, "an unsatisfiable range must be a 416");

    server.stop();
    let _ = fs::remove_dir_all(&site);
    Ok(())
}

/// The chunked format, end to end: a chunked artifact ranks exactly like the flat
/// one, its shard set reassembles byte for byte, and a sparse client that fetches
/// only the chunks a query routes to ranks the same documents as the whole artifact.
///
/// This is the test that makes the format change safe: every assertion compares
/// against the flat build of the same site, so a chunk boundary that lost a term, a
/// postings offset that forgot its base, or a missing chunk would show up as a
/// different ranking.
#[test]
fn a_chunked_shard_fetches_only_the_chunks_a_query_needs() -> Result<(), Box<dyn Error>> {
    let site = fixture_site("chunks")?;
    let flat = rustsearch_index::build_site(&site, &Default::default())?;
    let chunked = rustsearch_index::build_site(
        &site,
        &rustsearch_index::BuildOptions {
            // Small on purpose: a handful of chunks over a small dictionary.
            term_chunk_bytes: 1024,
            ..Default::default()
        },
    )?;
    let flat_index =
        Index::from_bytes(flat.bytes.clone()).map_err(|error| format!("flat artifact: {error}"))?;
    let whole = Index::from_bytes(chunked.bytes.clone())
        .map_err(|error| format!("chunked artifact: {error}"))?;
    assert!(whole.is_chunked());
    assert_eq!(whole.doc_count(), flat_index.doc_count());
    assert_eq!(whole.term_count(), flat_index.term_count());
    assert!(
        whole.chunk_count() > 1,
        "a 1 KiB budget must split {} terms into several chunks",
        whole.term_count()
    );

    // Rank and score, not URLs: a sparse index has no strings, and this comparison
    // is about the dictionary and its postings.
    let ranked = |engine: &Index| -> Vec<(u32, String)> {
        engine
            .search(
                "walkdir",
                SearchOptions {
                    limit: 50,
                    ..Default::default()
                },
            )
            .into_iter()
            .map(|hit| (hit.doc_id, format!("{:.4}", hit.score)))
            .collect()
    };
    assert_eq!(
        ranked(&whole),
        ranked(&flat_index),
        "a chunked dictionary must rank exactly like a flat one"
    );

    let set = shard::split(&chunked.bytes, "index/")?;
    let dir = temp_dir(&site, "chunks");
    fs::create_dir_all(&dir)?;
    fs::write(dir.join("manifest.txt"), &set.manifest)?;
    for part in &set.parts {
        let name = Path::new(&part.key).file_name().unwrap();
        fs::write(dir.join(name), &part.bytes)?;
    }
    let server = Server::start(&dir)?;
    let manifest = Manifest::parse(&server.fetch_text("manifest.txt")?)?;
    assert_eq!(manifest.total, chunked.bytes.len());

    // One object per chunk, in place of `terms` and `postings`, and none of them near
    // the 1 MiB a CDN caches in.
    assert_eq!(manifest.chunk_parts().len(), whole.chunk_count() as usize);
    for part in manifest.chunk_parts() {
        assert!(
            part.len <= 1024 + 4096,
            "{} is {} bytes for a 1 KiB budget",
            part.name,
            part.len
        );
    }

    // Every part is the artifact slice at its offset, and the set reassembles.
    let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
    for part in &manifest.parts {
        let (status, body) = server.get(&part.key, Some(0..part.len))?;
        assert_eq!(status, 206, "{}", part.key);
        assert_eq!(
            body.as_slice(),
            &chunked.bytes[part.off..part.off + part.len],
            "{} is not the artifact slice at {}",
            part.key,
            part.off
        );
        fetched.push((part.key.clone(), body));
    }
    assert_eq!(
        shard::assemble(&manifest, &fetched)?,
        chunked.bytes,
        "a chunked shard set must reassemble losslessly"
    );

    // The metadata a client fetches cold is every `meta` slice: the header, the
    // chunk directory, the BM25 lengths, the facets, the title chunks and the page
    // table. The two hash tables are their own objects, skipped at cold start and
    // read one 8-byte entry per rendered hit.
    let layout = whole.layout();
    let chunk_data = &chunked.bytes[layout.chunk_data.clone()];
    for name in [
        "head",
        "chunks",
        "terms",
        "postings",
        "docs",
        "facets",
        "titlepostings",
    ] {
        assert!(
            manifest.part(name).is_none(),
            "a chunked set must group {name} into meta"
        );
    }
    let hashes = manifest.part("hashes").ok_or("hashes part is missing")?;
    let strhashes = manifest
        .part("strhashes")
        .ok_or("strhashes part is missing")?;
    assert_eq!(hashes.len, whole.doc_count() as usize * 8);
    assert_eq!(strhashes.len, whole.doc_count() as usize * 8);
    let meta_parts = manifest.meta_parts();
    assert!(
        meta_parts.len() >= 2,
        "the run is cut around the two tables"
    );
    let mut meta_bytes = Vec::new();
    let mut meta_fetched = Vec::new();
    let mut requests = 1usize; // the manifest
    let mut previous_end = 0usize;
    for part in &meta_parts {
        assert!(
            part.off >= previous_end,
            "{} overlaps the previous part",
            part.name
        );
        let (status, body) = server.get(&part.key, Some(0..part.len))?;
        assert_eq!(status, 206, "{}", part.name);
        assert_eq!(
            body.as_slice(),
            &chunked.bytes[part.off..part.off + part.len],
            "{} is not the artifact slice at {}",
            part.name,
            part.off
        );
        meta_bytes.extend_from_slice(&body);
        meta_fetched.push((part.key.clone(), body));
        previous_end = part.off + part.len;
        requests += 1;
    }
    assert_eq!(previous_end, layout.doc_page_index.end);
    assert_eq!(
        meta_bytes.len(),
        layout.doc_page_index.end - hashes.len - strhashes.len - layout.title_data.len(),
        "the slices cover the run without the two hash tables and the title lists"
    );
    let mut query_bytes = set.manifest.len() + meta_bytes.len();
    let partial = shard::assemble_partial(&manifest, &meta_fetched, &[])?;
    let mut sparse =
        Index::from_sparse(partial).map_err(|error| format!("sparse load: {error}"))?;
    assert!(sparse.is_chunked());
    assert_eq!(sparse.attached_chunks(), 0);
    assert!(
        sparse
            .search("walkdir", SearchOptions::default())
            .is_empty(),
        "an unfetched chunk has no terms, so it cannot rank one"
    );

    // The routing is the engine's own: the chunks it names are the ones to fetch.
    let needed = sparse
        .chunks_for_query("walkdir", SearchOptions::default())
        .map_err(|error| format!("chunk plan: {error}"))?;
    assert!(!needed.is_empty(), "a query term must route to a chunk");
    assert!(
        needed.contains(&sparse.term_chunk("walkdir").ok_or("no chunk for walkdir")?),
        "the chunk plan must include the chunk the term lives in"
    );
    for chunk in &needed {
        let part = manifest
            .chunk_part(*chunk)
            .ok_or("no object for a routed chunk")?;
        let (status, body) = server.get(&part.key, Some(0..part.len))?;
        assert_eq!(status, 206, "{}", part.key);
        let (off, len) = sparse
            .chunk_span(*chunk)
            .ok_or("no span for a routed chunk")?;
        assert_eq!(
            body.as_slice(),
            &chunk_data[off as usize..off as usize + len as usize],
            "chunk {chunk} is not the slice the directory points at"
        );
        assert_eq!(
            rustsearch_core::format::fnv1a64(&body),
            sparse
                .chunk_hash(*chunk)
                .ok_or("no hash for a routed chunk")?,
            "each chunk must verify on its own"
        );
        query_bytes += body.len();
        requests += 1;
        sparse
            .attach_term_chunk(*chunk, body)
            .map_err(|error| format!("chunk {chunk} rejected: {error}"))?;
    }
    assert_eq!(sparse.attached_chunks(), needed.len());
    assert_eq!(
        ranked(&sparse),
        ranked(&flat_index),
        "the fetched chunks must reproduce the flat ranking"
    );
    // The title lists are their own objects, one per chunk that has one, and they
    // tile the `titledata` section exactly: nothing is duplicated and nothing is
    // left out of the set.
    let title_parts: Vec<&shard::PartRef> = manifest
        .parts
        .iter()
        .filter(|part| part.name.starts_with("title"))
        .collect();
    assert!(
        !title_parts.is_empty() && !layout.title_data.is_empty(),
        "this fixture indexes titles, so the set must publish them"
    );
    let mut cursor = layout.title_data.start;
    for part in &title_parts {
        assert_eq!(
            part.off, cursor,
            "{} must start where the previous title list ended",
            part.name
        );
        cursor += part.len;
    }
    assert_eq!(
        cursor, layout.title_data.end,
        "the title objects must cover the whole section"
    );

    // A query that scores titles fetches the list of every chunk it fetched, and the
    // result must equal the whole artifact's — the same property the term chunks
    // have. `page` occurs in every title in the fixture, `walkdir` in none, so this
    // covers both a chunk with a title list and one without.
    assert_eq!(
        sparse.attached_title_chunks(),
        0,
        "a cold client has not fetched a title list"
    );
    let scores = |engine: &Index, query: &str| -> Vec<(u32, String)> {
        engine
            .search(
                query,
                SearchOptions {
                    limit: 50,
                    title_boost: rustsearch_core::DEFAULT_TITLE_BOOST,
                    ..Default::default()
                },
            )
            .into_iter()
            .map(|hit| (hit.doc_id, format!("{:.4}", hit.score)))
            .collect()
    };
    let page_chunks = sparse
        .chunks_for_query("page", SearchOptions::default())
        .map_err(|error| format!("title chunk plan: {error}"))?;
    assert!(!page_chunks.is_empty());
    let title_data = &chunked.bytes[layout.title_data.clone()];
    let mut title_fetched = 0usize;
    for chunk in &page_chunks {
        let Some(part) = manifest
            .parts
            .iter()
            .find(|part| part.name == shard::title_part_name(*chunk))
        else {
            continue;
        };
        let (status, body) = server.get(&part.key, Some(0..part.len))?;
        assert_eq!(status, 206, "{}", part.key);
        let (off, len) = sparse
            .title_chunk_span(*chunk)
            .ok_or("a title object without a directory span")?;
        assert_eq!(
            body.as_slice(),
            &title_data[off as usize..off as usize + len as usize],
            "title {chunk} is not the slice the directory points at"
        );
        query_bytes += body.len();
        requests += 1;
        title_fetched += 1;
        sparse
            .attach_title_chunk(*chunk, body)
            .map_err(|error| format!("title chunk {chunk} rejected: {error}"))?;
    }
    assert!(
        title_fetched > 0,
        "the fixture must have a title list to fetch"
    );
    assert_eq!(sparse.attached_title_chunks(), title_fetched);
    assert_eq!(
        scores(&sparse, "page"),
        scores(&flat_index, "page"),
        "fetched title lists must score like the flat title postings"
    );
    // A chunk whose terms never occur in a title has no object; fetching the ones
    // that do must not change a query that only matches bodies.
    assert_eq!(
        scores(&sparse, "walkdir"),
        scores(&flat_index, "walkdir"),
        "a body-only term must score the same with the title lists attached"
    );

    // Grouping reads the record of every candidate before `limit`, so a chunked
    // sparse index cannot group a single page until the record pages are here — the
    // failure is loud, not an empty result that looks like no match.
    let grouped = |engine: &Index| -> Vec<(u32, String, u32)> {
        engine
            .search(
                "walkdir",
                SearchOptions {
                    limit: 50,
                    group_by_url: true,
                    ..Default::default()
                },
            )
            .into_iter()
            .map(|hit| (hit.doc_id, format!("{:.4}", hit.score), hit.matches))
            .collect()
    };
    assert!(
        sparse
            .try_search(
                "walkdir",
                SearchOptions {
                    limit: 50,
                    group_by_url: true,
                    ..Default::default()
                }
            )
            .is_err(),
        "grouping without the record pages must fail, not return an empty list"
    );
    assert_eq!(
        sparse.all_doc_pages(),
        (0..sparse.doc_page_count()).collect::<Vec<_>>(),
        "a client can enumerate the pages it has to fetch"
    );

    // Rendering a hit takes its record page, not the whole record table: one `Range`
    // read for every hit that shares the page.
    let first_hit = ranked(&sparse)[0].0;
    assert!(
        sparse.string_span(first_hit).is_none(),
        "a page that was never fetched has no record to read offsets from"
    );
    let page = sparse
        .doc_page_of(first_hit)
        .ok_or("no record page for a hit")?;
    let (page_off, page_len) = sparse
        .doc_page_span(page)
        .ok_or("no span for a record page")?;
    let pages_part = manifest.part("docrecords").ok_or("no docrecords part")?;
    let (status, page_bytes) = server.get(
        &pages_part.key,
        Some(page_off as usize..(page_off + page_len) as usize),
    )?;
    assert_eq!(status, 206, "{}", pages_part.key);
    assert!(
        (page_len as usize) <= 1024,
        "a record page is bounded by the chunk budget, not by the corpus"
    );
    assert_eq!(
        rustsearch_core::format::fnv1a64(&page_bytes),
        sparse
            .doc_page_hash(page)
            .ok_or("no hash for a record page")?,
        "a record page must verify on its own"
    );
    query_bytes += page_bytes.len();
    requests += 1;
    sparse
        .attach_doc_page(page, page_bytes)
        .map_err(|error| format!("record page rejected: {error}"))?;
    assert_eq!(
        grouped(&sparse),
        grouped(&whole),
        "once every page is attached, grouping must equal the whole artifact"
    );
    let span = sparse
        .string_span(first_hit)
        .ok_or("an attached page must give the hit a string span")?;
    let url_range = span.url_off as usize..(span.url_off + span.url_len) as usize;
    let strings_part = manifest.part("strings").ok_or("no strings part")?;
    let (status, url_bytes) = server.get(&strings_part.key, Some(url_range.clone()))?;
    assert_eq!(status, 206);
    query_bytes += url_bytes.len();
    requests += 1;
    let flat_strings = &flat.bytes[flat_index.layout().strings.clone()];
    assert_eq!(
        url_bytes.as_slice(),
        &flat_strings[url_range],
        "the URL a sparse client reads must be the flat one"
    );

    // A title list that does not match its directory hash is refused, whole or
    // truncated, exactly like a term chunk.
    let (title_chunk, title_part) = page_chunks
        .iter()
        .find_map(|chunk| {
            manifest
                .parts
                .iter()
                .find(|part| part.name == shard::title_part_name(*chunk))
                .map(|part| (*chunk, part))
        })
        .ok_or("no title object for the routed chunks")?;
    let (_, title_body) = server.get(&title_part.key, Some(0..title_part.len))?;
    let mut corrupt_title = title_body.clone();
    corrupt_title[title_body.len() / 2] ^= 0x04;
    assert!(
        sparse
            .attach_title_chunk(title_chunk, corrupt_title)
            .is_err(),
        "a corrupted title list must be rejected"
    );
    assert!(
        sparse
            .attach_title_chunk(title_chunk, title_body[..title_body.len() - 1].to_vec())
            .is_err(),
        "a truncated title list must be rejected"
    );

    // A chunk that does not match its directory hash is refused, whole or truncated.
    let chunk = needed[0];
    let part = manifest
        .chunk_part(chunk)
        .ok_or("no object for a routed chunk")?;
    let (_, body) = server.get(&part.key, Some(0..part.len))?;
    let mut corrupt = body.clone();
    corrupt[body.len() / 2] ^= 0x08;
    assert!(
        sparse.attach_term_chunk(chunk, corrupt).is_err(),
        "a corrupted chunk must be rejected"
    );
    assert!(
        sparse
            .attach_term_chunk(chunk, body[..body.len() - 1].to_vec())
            .is_err(),
        "a truncated chunk must be rejected"
    );

    // What the plan saved: the flat client fetched the whole dictionary and postings.
    let flat_dictionary = flat_index.layout();
    let flat_metadata = (flat_dictionary.terms.end - flat_dictionary.terms.start)
        + (flat_dictionary.postings.end - flat_dictionary.postings.start);
    let chunk_bytes: usize = needed
        .iter()
        .filter_map(|chunk| sparse.chunk_span(*chunk))
        .map(|(_, len)| len as usize)
        .sum();
    println!(
        "chunked artifact {} bytes / {} chunks\n  \
         query {query_bytes} bytes / {requests} requests\n  \
         dictionary: {chunk_bytes} bytes in {} chunk(s) instead of {flat_metadata}",
        chunked.bytes.len(),
        whole.chunk_count(),
        needed.len()
    );
    assert!(
        chunk_bytes < flat_metadata,
        "chunking must shrink the fetch"
    );
    assert!(
        query_bytes < chunked.bytes.len(),
        "a query ({query_bytes} bytes) must cost less than the artifact"
    );

    server.stop();
    let _ = fs::remove_dir_all(&site);
    Ok(())
}

/// A metadata run split by a budget: the pieces stay consecutive artifact slices, a
/// cold client reads all of them, and a sparse index built from them — plus only the
/// chunks a query routes to — ranks like the flat artifact.
///
/// The budget here is deliberately tiny (the fixture's whole metadata run is a couple
/// of kilobytes) because the property under test is the cut, not the size: each part
/// is at most the budget, the parts tile `0 .. doc_page_index.end`, and reassembly is
/// byte-exact. At a million documents the same code cuts a run of tens of megabytes
/// into objects a CDN can cache.
#[test]
fn a_split_metadata_run_is_bounded_and_still_one_sparse_index() -> Result<(), Box<dyn Error>> {
    let site = fixture_site("meta")?;
    let flat = rustsearch_index::build_site(&site, &Default::default())?;
    let chunked = rustsearch_index::build_site(
        &site,
        &rustsearch_index::BuildOptions {
            term_chunk_bytes: 1024,
            ..Default::default()
        },
    )?;
    let flat_index =
        Index::from_bytes(flat.bytes.clone()).map_err(|error| format!("flat artifact: {error}"))?;
    let whole = Index::from_bytes(chunked.bytes.clone())
        .map_err(|error| format!("chunked artifact: {error}"))?;
    let layout = whole.layout();
    let meta_end = layout.doc_page_index.end;

    // With no flag the default budget applies: a run below it stays one object, a
    // larger one is cut without the caller having to know the size.
    assert!(
        shard::ShardOptions::default().meta_part_bytes.is_none(),
        "no budget is the default; split_with applies DEFAULT_META_PART_BYTES"
    );
    let default_set = shard::split(&chunked.bytes, "index/")?;
    let default_manifest = Manifest::parse(&default_set.manifest)?;
    assert!(
        default_manifest.part("meta").is_none(),
        "the run is named metaNNNNN: it is cut around the hash tables"
    );
    assert!(
        default_manifest.meta_parts().len() >= 2,
        "the two hash tables split the run into at least two slices"
    );
    assert!(default_manifest.part("hashes").is_some());
    assert!(default_manifest.part("strhashes").is_some());

    let budget = 256usize;
    let set = shard::split_with(
        &chunked.bytes,
        "index/",
        shard::ShardOptions {
            meta_part_bytes: Some(budget),
        },
    )?;
    let manifest = Manifest::parse(&set.manifest)?;
    let meta_parts = manifest.meta_parts();
    assert!(
        meta_parts.len() > 1,
        "a {budget}-byte budget must cut the {meta_end}-byte metadata run"
    );
    assert!(
        manifest.part("meta").is_none(),
        "a split run is named meta00000, not meta"
    );
    for part in &meta_parts {
        assert!(
            part.len > 0 && part.len <= budget,
            "{} is {} bytes for a {budget}-byte budget",
            part.name,
            part.len
        );
    }
    // The meta slices plus the two hash objects tile the metadata run: the tables
    // are carved out of it, not duplicated.
    let mut cursor = 0usize;
    for part in manifest.parts.iter().filter(|part| part.off < meta_end) {
        assert!(
            part.off >= cursor,
            "{} overlaps the previous part",
            part.name
        );
        cursor = part.off + part.len;
    }
    assert_eq!(cursor, meta_end, "the parts must cover the metadata run");
    assert!(manifest.part("hashes").is_some());
    assert!(manifest.part("strhashes").is_some());
    assert!(manifest.part("docrecords").is_some());
    assert_eq!(manifest.chunk_parts().len(), whole.chunk_count() as usize);

    // The set still reassembles byte for byte and its parts are artifact slices.
    let mut fetched: Vec<(String, Vec<u8>)> = Vec::new();
    for part in &manifest.parts {
        let object = set
            .parts
            .iter()
            .find(|candidate| candidate.name == part.name)
            .ok_or("a manifest part has no object")?;
        assert_eq!(
            object.bytes.as_slice(),
            &chunked.bytes[part.off..part.off + part.len],
            "{} is not the artifact slice at {}",
            part.name,
            part.off
        );
        fetched.push((part.key.clone(), object.bytes.clone()));
    }
    assert_eq!(
        shard::assemble(&manifest, &fetched)?,
        chunked.bytes,
        "a split metadata run must reassemble losslessly"
    );

    // A cold client reads every metadata slice, then only the chunks the query names.
    let metadata: Vec<(String, Vec<u8>)> = set
        .parts
        .iter()
        .filter(|part| shard::is_meta_part(&part.name))
        .map(|part| (part.key.clone(), part.bytes.clone()))
        .collect();
    assert_eq!(metadata.len(), meta_parts.len());
    let partial = shard::assemble_partial(&manifest, &metadata, &[])?;
    let mut sparse =
        Index::from_sparse(partial).map_err(|error| format!("sparse load: {error}"))?;
    assert!(sparse.is_chunked());
    // The table is zero in a sparse load; one fetched entry fills the slot the block
    // check reads, and a second, different value is refused instead of overwriting it.
    let hash_part = set
        .parts
        .iter()
        .find(|part| part.name == "hashes")
        .ok_or("hashes object is missing")?;
    let entry = u64::from_le_bytes(hash_part.bytes[..8].try_into().expect("8 bytes"));
    sparse
        .attach_text_hash(0, entry)
        .map_err(|error| format!("hash attach: {error}"))?;
    assert_eq!(sparse.block_hash(0), Some(entry));
    assert!(sparse.attach_text_hash(0, entry ^ 1).is_err());
    let ranked = |engine: &Index| -> Vec<(u32, String)> {
        engine
            .search(
                "walkdir",
                SearchOptions {
                    limit: 50,
                    ..Default::default()
                },
            )
            .into_iter()
            .map(|hit| (hit.doc_id, format!("{:.4}", hit.score)))
            .collect()
    };
    assert!(
        ranked(&sparse).is_empty(),
        "metadata alone carries no posting list, so it cannot rank yet"
    );
    let needed = sparse
        .chunks_for_query("walkdir", SearchOptions::default())
        .map_err(|error| format!("chunk plan: {error}"))?;
    assert!(!needed.is_empty());
    for chunk in &needed {
        let part = set
            .parts
            .iter()
            .find(|part| part.name == shard::chunk_part_name(*chunk))
            .ok_or("no object for a routed chunk")?;
        sparse
            .attach_term_chunk(*chunk, part.bytes.clone())
            .map_err(|error| format!("chunk {chunk} rejected: {error}"))?;
    }
    assert_eq!(
        ranked(&sparse),
        ranked(&flat_index),
        "metadata split into slices must rank like the flat artifact"
    );

    // A metadata slice that does not match its manifest hash is refused before the
    // index can be loaded.
    let mut corrupt = metadata[0].1.clone();
    let middle = corrupt.len() / 2;
    corrupt[middle] ^= 0x10;
    assert!(
        shard::assemble_partial(&manifest, &[(metadata[0].0.clone(), corrupt)], &[]).is_err(),
        "a corrupted metadata slice must be rejected"
    );

    // A budget without a chunked artifact has nothing to split.
    assert!(
        shard::split_with(
            &flat.bytes,
            "index/",
            shard::ShardOptions {
                meta_part_bytes: Some(budget)
            }
        )
        .is_err(),
        "a flat artifact publishes one object per section, not a meta run"
    );

    let _ = fs::remove_dir_all(&site);
    Ok(())
}

/// H7: a fuzzy fetch plan is bounded to [`rustsearch_core::MAX_FUZZY_CHUNKS`]
/// chunks, centred on the query's tokens, and it reports that it was capped.
///
/// The fixture is generated here because the cap only exists above it: 80 pages of
/// 120 unique terms make a dictionary that a 1 KiB budget cuts into hundreds of
/// chunks, where the old plan named every one of them.
#[test]
fn a_fuzzy_plan_is_capped_and_reports_truncation() -> Result<(), Box<dyn Error>> {
    let root = std::env::temp_dir().join(format!("rustsearch-plan-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    for page in 0..80 {
        let mut body = String::new();
        for word in 0..120 {
            body.push_str(&format!("term{page:02}x{word:03} "));
        }
        fs::write(
            root.join(format!("p{page}.html")),
            format!(
                "<html><head><title>plan {page}</title></head><body><main>{body}</main></body></html>"
            ),
        )?;
    }
    let build = rustsearch_index::build_site(
        &root,
        &rustsearch_index::BuildOptions {
            term_chunk_bytes: rustsearch_index::MIN_TERM_CHUNK_BYTES,
            ..Default::default()
        },
    )?;
    let index = Index::from_bytes(build.bytes)?;
    let cap = rustsearch_core::MAX_FUZZY_CHUNKS;
    assert!(
        index.chunk_count() as usize > cap,
        "the fixture must exceed the cap, got {} chunks",
        index.chunk_count()
    );

    let fuzzy = SearchOptions {
        fuzzy: true,
        ..Default::default()
    };
    let plan = index.chunk_plan("term00x000", fuzzy)?;
    assert_eq!(plan.chunks.len(), cap, "a fuzzy plan must be bounded");
    assert!(plan.truncated, "a bounded plan must say so");
    assert_eq!(
        index.chunks_for_query("term00x000", fuzzy)?,
        plan.chunks,
        "chunks_for_query is the plan's chunk list"
    );
    let start = index
        .term_chunk("term00x000")
        .ok_or("no chunk for the token")?;
    assert!(
        plan.chunks.contains(&start),
        "the plan must start at the token's own chunk"
    );
    assert!(
        plan.chunks.windows(2).all(|pair| pair[0] < pair[1]),
        "the plan is sorted and deduplicated"
    );
    assert!(plan.chunks.iter().all(|chunk| *chunk < index.chunk_count()));

    // An exact plan is never capped, and a flat dictionary has no chunks at all.
    assert!(
        !index
            .chunk_plan("term00x000", SearchOptions::default())?
            .truncated
    );
    let flat = rustsearch_index::build_site(&root, &Default::default())?;
    let flat = Index::from_bytes(flat.bytes)?;
    assert_eq!(flat.chunk_count(), 0);
    assert!(!flat.chunk_plan("term00x000", fuzzy)?.truncated);

    let _ = fs::remove_dir_all(&root);
    Ok(())
}

fn artifact_texts<'a>(artifact: &'a [u8], manifest: &Manifest) -> Result<&'a [u8], Box<dyn Error>> {
    let part = manifest.part("texts").ok_or("no texts part")?;
    Ok(&artifact[part.off..part.off + part.len])
}

fn artifact_strings<'a>(
    artifact: &'a [u8],
    manifest: &Manifest,
) -> Result<&'a [u8], Box<dyn Error>> {
    let part = manifest.part("strings").ok_or("no strings part")?;
    Ok(&artifact[part.off..part.off + part.len])
}

/// Every document record plus its URL, read straight from the sections.
fn read_records(
    artifact: &[u8],
    manifest: &Manifest,
    count: u32,
) -> Result<Vec<(DocRecord, String)>, Box<dyn Error>> {
    let docs = manifest.part("docs").ok_or("no docs part")?;
    let strings = manifest.part("strings").ok_or("no strings part")?;
    let records = &artifact[docs.off..docs.off + docs.len];
    let strings_bytes = &artifact[strings.off..strings.off + strings.len];
    let mut out = Vec::with_capacity(count as usize);
    for id in 0..count {
        let start = 4 + id as usize * rustsearch_core::format::DOC_RECORD;
        let size = rustsearch_core::format::DOC_RECORD;
        let record = DocRecord::from_bytes(&records[start..start + size]).ok_or("short record")?;
        let url = std::str::from_utf8(
            &strings_bytes
                [record.url_off as usize..record.url_off as usize + record.url_len as usize],
        )?;
        out.push((record, url.to_string()));
    }
    Ok(out)
}

/// Twelve pages of prose, enough for the text section to dominate the artifact.
///
/// `name` keeps each test's site in its own directory: cargo runs the tests of one
/// binary in parallel, and a shared root would let one test delete the site another
/// is reading.
fn fixture_site(name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let root = std::env::temp_dir().join(format!("rustsearch-shard-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    for page in 0..12 {
        let topic = if page % 2 == 0 {
            "walkdir"
        } else {
            "serde_json"
        };
        // Two sections and a shared keyword, so the fixture has a taxonomy to
        // filter on and the manifest carries a `facets` part.
        let section = if page % 2 == 0 { "guide" } else { "reference" };
        let section_dir = root.join(section);
        fs::create_dir_all(&section_dir)?;
        let mut body = String::new();
        for sentence in 0..40 {
            body.push_str(&format!(
                "<p>Page {page} sentence {sentence}: the {topic} crate keeps its API small, \
                 and every example in the guide shows the same three calls in a row.</p>"
            ));
        }
        let html = format!(
            "<html lang=\"en\"><head><title>Page {page}</title>\
             <meta name=\"keywords\" content=\"crate, API\"></head>\
             <body><main>{body}</main></body></html>"
        );
        fs::write(section_dir.join(format!("page{page}.html")), html)?;
    }
    Ok(root)
}

fn temp_dir(root: &Path, name: &str) -> PathBuf {
    root.parent().unwrap_or(root).join(format!(
        "{}-{name}",
        root.file_name().unwrap().to_string_lossy()
    ))
}

struct Request {
    path: String,
    range: Option<(usize, usize)>,
}

struct Server {
    addr: String,
    log: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    fn start(dir: &Path) -> Result<Self, Box<dyn Error>> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?.to_string();
        let dir = dir.to_path_buf();
        let log = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log_in, stop_in) = (Arc::clone(&log), Arc::clone(&stop));
        let handle = thread::spawn(move || {
            while !stop_in.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let _ = answer(stream, &dir, &log_in);
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(1));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            addr,
            log,
            stop,
            handle: Some(handle),
        })
    }

    fn fetch_text(&self, path: &str) -> Result<String, Box<dyn Error>> {
        let (status, body) = self.get(path, None)?;
        assert_eq!(status, 200, "{path}");
        Ok(String::from_utf8(body)?)
    }

    fn get(
        &self,
        path: &str,
        range: Option<Range<usize>>,
    ) -> Result<(u16, Vec<u8>), Box<dyn Error>> {
        let mut stream = TcpStream::connect(&self.addr)?;
        let mut request =
            format!("GET /{path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
        if let Some(range) = &range {
            request.push_str(&format!(
                "Range: bytes={}-{}\r\n",
                range.start,
                range.end - 1
            ));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes())?;
        stream.flush()?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw)?;
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .ok_or("no header terminator")?;
        let head = std::str::from_utf8(&raw[..split])?;
        let body = raw[split + 4..].to_vec();
        let status: u16 = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .ok_or("no status")?
            .parse()?;

        if let Some(range) = &range {
            if status == 206 {
                let expected = format!("bytes {}-{}", range.start, range.end - 1);
                assert!(
                    head.to_ascii_lowercase()
                        .contains(&format!("content-range: {expected}")),
                    "{path}: wrong Content-Range\n{head}"
                );
                assert_eq!(body.len(), range.len());
            }
        }
        self.log.lock().unwrap().push(Request {
            path: path.to_string(),
            range: range.map(|range| (range.start, range.end)),
        });
        Ok((status, body))
    }

    fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Serve one file with `Range` support, the way an object store does.
fn answer(
    mut stream: TcpStream,
    dir: &Path,
    log: &Mutex<Vec<Request>>,
) -> Result<(), Box<dyn Error>> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
    let mut range: Option<Range<usize>> = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let header = header.trim_end();
        if header.is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("range:") {
            let spec = value.trim().trim_start_matches("bytes=");
            let (start, end) = spec.split_once('-').unwrap_or((spec, ""));
            let start: usize = start.parse()?;
            let end: usize = if end.is_empty() {
                usize::MAX
            } else {
                end.parse()?
            };
            range = Some(start..end.saturating_add(1));
        }
    }
    log.lock().unwrap().push(Request {
        path: path.clone(),
        range: range.as_ref().map(|r| (r.start, r.end)),
    });

    let name = Path::new(&path)
        .file_name()
        .ok_or("no file name")?
        .to_string_lossy()
        .to_string();
    let bytes = match fs::read(dir.join(&name)) {
        Ok(bytes) => bytes,
        Err(_) => {
            stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")?;
            return Ok(());
        }
    };
    let total = bytes.len();
    let (status, start, end) = match range {
        Some(range) if range.start >= total => (416, total, total),
        Some(range) => (206, range.start, range.end.min(total)),
        None => (200, 0, total),
    };
    let body = &bytes[start.min(total)..end.min(total)];
    let mut head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n",
        if status == 206 {
            "Partial Content"
        } else {
            "OK"
        },
        body.len()
    );
    if status == 206 {
        head.push_str(&format!(
            "Content-Range: bytes {start}-{}/{total}\r\n",
            end - 1
        ));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    Ok(())
}
