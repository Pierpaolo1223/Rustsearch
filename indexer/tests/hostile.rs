//! Hostile input at both untrusted boundaries, restated as tests.
//!
//! `README.md` claims that the artifact is untrusted, that queries are bounded and
//! that the indexer treats HTML as hostile. Each test below turns one claim into an
//! assertion that fails in `cargo test` rather than in a deploy.
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rustsearch_core::{facets_to_json, hits_to_json, Hit, Index, SearchOptions};
use rustsearch_index::{build_site, extract_page, validate_base_url, BuildOptions};

fn site(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("rustsearch-hostile-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).expect("temp dir");
    root
}

fn write(root: &Path, name: &str, html: &str) {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("page dir");
    }
    fs::write(path, html).expect("page");
}

fn page(marker: &str) -> String {
    format!(
        "<html lang=\"en\"><head><title>{marker}</title></head>\
         <body><main>{marker} alpha beta</main></body></html>"
    )
}

fn build(root: &Path, options: BuildOptions) -> (Vec<u8>, u32) {
    let build = build_site(root, &options).expect("build");
    (build.bytes, build.stats.documents as u32)
}

/// The contract a hit promises: `highlights` are in-order, non-overlapping byte
/// ranges inside `snippet`, on character boundaries.
fn check_hits(hits: &[Hit], limit: usize) {
    assert!(
        hits.len() <= limit,
        "{} hits for a limit of {limit}",
        hits.len()
    );
    for hit in hits {
        let mut last_end = 0;
        for &(start, end) in &hit.highlights {
            let (start, end) = (start as usize, end as usize);
            assert!(start <= end, "reversed range in {:?}", hit.highlights);
            assert!(
                end <= hit.snippet.len(),
                "range {:?} past a {}-byte snippet",
                hit.highlights,
                hit.snippet.len()
            );
            assert!(
                start >= last_end,
                "ranges out of order in {:?}",
                hit.highlights
            );
            assert!(
                hit.snippet.is_char_boundary(start) && hit.snippet.is_char_boundary(end),
                "range {:?} splits a character",
                hit.highlights
            );
            last_end = end;
        }
    }
}

/// xorshift64: the mutation rounds must be reproducible without a dependency.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

#[test]
fn a_damaged_artifact_is_refused_and_a_sparse_one_never_panics() {
    let root = site("damaged");
    write(&root, "index.html", &page("walkdir marker"));
    write(&root, "guide.html", &page("shard marker"));
    let (bytes, documents) = build(&root, BuildOptions::default());
    assert!(documents >= 2);

    let clean = Index::from_bytes(bytes.clone()).expect("the clean artifact loads");
    assert_eq!(clean.doc_count(), documents);
    assert!(
        !clean.search("marker", Default::default()).is_empty(),
        "control: the clean artifact answers"
    );

    // Every proper prefix is refused: a truncated artifact is not a smaller one.
    for len in 0..bytes.len() {
        assert!(
            Index::from_bytes(bytes[..len].to_vec()).is_err(),
            "a {len}-byte prefix of a {}-byte artifact was accepted",
            bytes.len()
        );
    }

    // The checksum covers the body (every byte after the header), so a flipped body
    // bit is refused whatever it changes.
    let header_len = rustsearch_core::format::HEADER_LEN;
    assert!(header_len < bytes.len());
    for at in header_len..bytes.len() {
        let mut copy = bytes.clone();
        copy[at] ^= 0x40;
        assert!(
            Index::from_bytes(copy).is_err(),
            "a flip at body byte {at} was accepted"
        );
    }

    // The header is validated structurally — magic, version, analyzer id, section
    // bounds — not checksummed. A flipped header bit is either refused or must still
    // answer within bounds: never a read past the buffer. `from_sparse` has no
    // checksum at all, so it is the path where a decoder bug would show first.
    let quote = '"';
    let phrase = format!("{quote}marker{quote}");
    for at in 0..header_len {
        let mut copy = bytes.clone();
        copy[at] ^= 0x40;
        if let Ok(index) = Index::from_bytes(copy.clone()) {
            check_hits(
                &index.search(
                    "marker",
                    SearchOptions {
                        limit: 8,
                        ..Default::default()
                    },
                ),
                8,
            );
            // A flipped flag bit can claim positions the postings never carried.
            check_hits(
                &index.search(
                    &phrase,
                    SearchOptions {
                        limit: 8,
                        ..Default::default()
                    },
                ),
                8,
            );
        }
        if let Ok(sparse) = Index::from_sparse(copy) {
            check_hits(
                &sparse.search(
                    "marker",
                    SearchOptions {
                        limit: 8,
                        ..Default::default()
                    },
                ),
                8,
            );
            check_hits(
                &sparse.search(
                    &phrase,
                    SearchOptions {
                        limit: 8,
                        ..Default::default()
                    },
                ),
                8,
            );
        }
    }

    // The two fields a reader must never take on trust: an unknown version selects
    // a different header layout, and a bad magic means the bytes are not an artifact.
    for at in 8..12 {
        let mut copy = bytes.clone();
        copy[at] ^= 0x01;
        assert!(
            Index::from_bytes(copy).is_err(),
            "a version bit flip at byte {at} was accepted"
        );
    }
    let mut copy = bytes.clone();
    copy[0] ^= 0x01;
    assert!(
        Index::from_bytes(copy).is_err(),
        "a magic byte flip was accepted"
    );

    // Several flips at once, the way a hostile or half-written object arrives.
    let mut rng = Rng(0x5eed_1234);
    for round in 0..200 {
        let mut copy = bytes.clone();
        for _ in 0..(1 + rng.next_u64() % 8) {
            let at = header_len + (rng.next_u64() as usize) % (copy.len() - header_len);
            copy[at] ^= (rng.next_u64() as u8) | 1;
        }
        assert!(
            Index::from_bytes(copy.clone()).is_err(),
            "round {round} was accepted"
        );
        if let Ok(sparse) = Index::from_sparse(copy) {
            let hits = sparse.search(
                "marker",
                SearchOptions {
                    limit: 16,
                    ..Default::default()
                },
            );
            check_hits(&hits, 16);
            assert!(hits.len() <= sparse.doc_count() as usize);
        }
    }
}

#[test]
fn a_hostile_query_is_bounded_and_never_panics() {
    let root = site("query");
    for at in 0..8 {
        write(&root, &format!("page{at}.html"), &page("bounded marker"));
    }
    let (bytes, documents) = build(&root, BuildOptions::default());
    let index = Index::from_bytes(bytes).expect("artifact");

    let controls: String = (0u8..=0x1f).map(char::from).collect();
    let invisibles: String = [0x200b_u32, 0x202e, 0xfeff, 0x2066]
        .iter()
        .filter_map(|ch| char::from_u32(*ch))
        .collect();
    let queries: Vec<String> = vec![
        "a".repeat(1_000_000),
        "term ".repeat(150_000),
        controls.repeat(1_000),
        String::from('"').repeat(10_000),
        format!("{}{}", "(".repeat(20_000), ")".repeat(20_000)),
        invisibles.repeat(1_000),
        "*?[]^$|{}".repeat(5_000),
        "😀".repeat(10_000),
        "-".repeat(5_000),
        "AND OR NOT NEAR".to_string(),
        format!("{}marker", invisibles),
        "https://example.com/?q=<script>".to_string(),
    ];

    for (at, query) in queries.iter().enumerate() {
        let options = SearchOptions {
            limit: 1_000,
            prefix: true,
            fuzzy: true,
            max_edits: if at % 2 == 0 { 2 } else { 255 },
            ..Default::default()
        };
        let started = Instant::now();
        let hits = index.search(query, options);
        let spent = started.elapsed();
        check_hits(&hits, 1_000);
        assert!(hits.len() <= documents as usize);
        assert!(
            spent < Duration::from_secs(5),
            "query {at} took {spent:?}: the bound is not a bound"
        );
    }

    // The limit is the caller's, not a suggestion.
    for limit in [0, 1, 2] {
        let hits = index.search(
            "marker",
            SearchOptions {
                limit,
                ..Default::default()
            },
        );
        check_hits(&hits, limit);
    }
}

#[test]
fn text_a_reader_cannot_see_does_not_enter_the_index() {
    let html = r#"<html lang="en"><head><title>Visible</title></head><body>
      <nav>navmarker</nav>
      <main>visiblemarker alpha beta</main>
      <script>var scriptmarker = 1;</script>
      <style>.stylemarker { color: red }</style>
      <template><p>templatemarker</p></template>
      <footer>footermarker</footer>
      <aside>asidemarker</aside>
      <form>formmarker</form>
    </body></html>"#;
    let root = site("invisible");
    write(&root, "page.html", html);
    let (bytes, _) = build(&root, BuildOptions::default());
    let index = Index::from_bytes(bytes).expect("artifact");

    assert!(
        !index.search("visiblemarker", Default::default()).is_empty(),
        "control: the readable text is indexed"
    );
    for marker in [
        "scriptmarker",
        "stylemarker",
        "templatemarker",
        "navmarker",
        "footermarker",
        "asidemarker",
        "formmarker",
    ] {
        assert!(
            index.search(marker, Default::default()).is_empty(),
            "{marker} is not readable: indexing it lets a page plant text a reader never saw"
        );
    }

    let (title, text) = extract_page(html);
    assert_eq!(title, "Visible");
    assert!(text.contains("visiblemarker"));
    for marker in ["scriptmarker", "stylemarker", "templatemarker"] {
        assert!(!text.contains(marker), "{marker} leaked into the page text");
    }
}

#[test]
fn a_title_cannot_break_out_of_the_json_a_deploy_inlines() {
    let title = r#"A "quoted" </script><img src=x onerror=alert(1)> title"#;
    let root = site("title");
    write(
        &root,
        "page.html",
        &format!(
            "<html lang=\"en\"><head><title>{title}</title>\
             <meta name=\"keywords\" content=\"red,blue&quot;&lt;/script&gt;\">\
             </head><body><main>titlemarker alpha</main></body></html>"
        ),
    );
    let (bytes, _) = build(&root, BuildOptions::default());
    let index = Index::from_bytes(bytes).expect("artifact");

    let hits = index.search("titlemarker", Default::default());
    assert!(!hits.is_empty(), "control: the page answers");
    let json = hits_to_json(&hits);
    assert!(json.starts_with("{\"results\":["));
    // A static-site deploy inlines this payload in `<script type="application/json">`.
    // A title the page controls must not be able to close that block or open a tag.
    assert!(
        !json.contains("</script>"),
        "a title closed an inline script block: {json}"
    );
    assert!(!json.contains('<'), "raw markup reached the JSON: {json}");
    assert!(
        !json.contains('&'),
        "a raw ampersand can start an entity in an HTML context: {json}"
    );

    let facets = facets_to_json(&index);
    assert!(
        facets.contains("section") || facets.contains("lang"),
        "{facets}"
    );
    assert!(
        !facets.contains('<') && !facets.contains("</script>"),
        "a facet value the page controls reached the JSON unescaped: {facets}"
    );
}

#[test]
fn a_page_path_cannot_inject_markup_into_a_url() {
    let root = site("paths");
    let names = [
        r#"a"b.html"#,
        "c<d>.html",
        "sp ace.html",
        "e'f.html",
        "semi;colon.html",
        "hash#tag.html",
        "query?x.html",
    ];
    for name in names {
        write(&root, name, &page("pathmarker"));
    }
    let (bytes, documents) = build(
        &root,
        BuildOptions {
            base_url: "https://example.com".to_string(),
            ..Default::default()
        },
    );
    let index = Index::from_bytes(bytes).expect("artifact");
    let hits = index.search(
        "pathmarker",
        SearchOptions {
            limit: 32,
            ..Default::default()
        },
    );
    assert_eq!(hits.len(), documents as usize);

    for hit in &hits {
        let url = &hit.url;
        assert!(
            url.starts_with("https://example.com/"),
            "a page escaped the base URL: {url}"
        );
        for bad in ['"', '<', '>', ' ', '#', '?'] {
            assert!(
                !url.contains(bad),
                "{bad} in {url}: the path is rendered in an attribute, not in text"
            );
        }
    }
}

#[test]
fn a_base_url_that_could_name_another_origin_is_refused() {
    for bad in [
        "javascript:alert(1)",
        "data:text/html,x",
        "//evil.example",
        "ftp://example.com",
        "https://exa mple.com",
        "https://user:pass@example.com",
        "https://example.com/?x=1",
        "https://example.com/#a",
        "https:/evil.example",
        "https://example.com/../evil",
    ] {
        assert!(
            validate_base_url(bad).is_err(),
            "{bad} was accepted as a base URL"
        );
    }
    for good in [
        "",
        "https://example.com",
        "https://example.com/docs",
        "https://example.com/docs/",
        "http://localhost:8080",
        "demo/site",
        "/demo/site",
    ] {
        assert!(
            validate_base_url(good).is_ok(),
            "{good} was refused: {:?}",
            validate_base_url(good)
        );
    }
}

#[test]
fn a_chunk_from_elsewhere_is_refused_before_it_is_used() {
    let root = site("chunks");
    write(&root, "index.html", &page("chunkmarker"));
    write(&root, "guide.html", &page("chunkmarker"));
    let (bytes, _) = build(
        &root,
        BuildOptions {
            term_chunk_bytes: 4 * 1024,
            ..Default::default()
        },
    );
    let mut sparse = Index::from_sparse(bytes).expect("chunked metadata loads");

    // A chunk is trusted only after its directory hash matches: a foreign buffer is
    // a mismatch, not a decode of whatever the numbers in it claim.
    assert!(sparse.attach_term_chunk(0, vec![0xff; 256]).is_err());
    assert!(sparse.attach_term_chunk(u32::MAX, vec![0xff; 256]).is_err());
    assert!(sparse.attach_term_chunk(0, Vec::new()).is_err());

    // A refused attachment leaves the engine usable: nothing half-applied.
    let hits = sparse.search(
        "chunkmarker",
        SearchOptions {
            limit: 4,
            ..Default::default()
        },
    );
    check_hits(&hits, 4);
}

#[test]
fn an_artifact_that_covers_its_header_refuses_every_header_flip() {
    let root = site("covered");
    for at in 0..4 {
        write(&root, &format!("page{at}.html"), &page("coveredmarker"));
    }
    for chunked in [false, true] {
        let (bytes, documents) = build(
            &root,
            BuildOptions {
                header_checksum: true,
                term_chunk_bytes: if chunked { 4 * 1024 } else { 0 },
                ..Default::default()
            },
        );
        let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        assert_eq!(version, if chunked { 8 } else { 7 });
        let header_len = rustsearch_core::format::header_layout(version)
            .expect("a layout this build knows")
            .0;
        let clean = Index::from_bytes(bytes.clone()).expect("the clean artifact loads");
        assert_eq!(clean.doc_count(), documents);
        assert!(!clean.search("coveredmarker", Default::default()).is_empty());

        // This is the test the v4/v6 layout cannot pass: every byte of the header,
        // flags word included, is now inside the checksum.
        for at in 0..header_len {
            let mut copy = bytes.clone();
            copy[at] ^= 0x40;
            assert!(
                Index::from_bytes(copy).is_err(),
                "header byte {at} of a v{version} artifact was accepted"
            );
        }
    }

    // A header-covered artifact is what a build writes unless told otherwise, and the
    // opt-out gives back the bytes a client deployed before this build already reads.
    let (default, _) = build(&root, BuildOptions::default());
    assert_eq!(
        u32::from_le_bytes([default[8], default[9], default[10], default[11]]),
        7
    );
    let (plain, _) = build(
        &root,
        BuildOptions {
            header_checksum: false,
            ..Default::default()
        },
    );
    assert_eq!(
        u32::from_le_bytes([plain[8], plain[9], plain[10], plain[11]]),
        4
    );
    assert!(Index::from_bytes(plain).is_ok());
}
