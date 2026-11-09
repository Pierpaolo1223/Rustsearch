//! The incremental build (ROADMAP §"Next cycle", item 4): `build --cache` must change
//! how long a rebuild takes, never what it produces.
//!
//! Every test here compares the artifact of a cached build with the artifact of a
//! build that has never seen a cache, byte for byte. That is the contract: a cache hit
//! returns the bytes the recorded build produced, and a miss is a full build, so the
//! two can never disagree. A test that only compared "it got faster" would let a
//! stale record ship.
//!
//! The design is deliberately a change *detector*: a build with one changed page is a
//! full build (the flat format assigns document ids by position, so a merge would
//! re-serialize everything anyway — measured slower than a cold build), and only the
//! no-change case short-circuits. The tests below pin both halves of that.

use std::fs;
use std::path::{Path, PathBuf};

use rustsearch_core::{Index, SearchOptions};
use rustsearch_index::{build_site, build_site_with_cache, BuildOptions};

fn site(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "rustsearch-incremental-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

fn page(root: &Path, name: &str, title: &str, body: &str) {
    fs::write(
        root.join(name),
        format!(
            "<html><head><title>{title}</title></head><body><main><p>{body}</p></main></body></html>"
        ),
    )
    .unwrap();
}

fn cache_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "rustsearch-incremental-cache-{name}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    dir
}

#[test]
fn a_cached_build_is_byte_identical_to_a_cold_one() {
    let root = site("cold");
    page(
        &root,
        "a.html",
        "Shard layout",
        "The shard layout splits the dictionary.",
    );
    page(
        &root,
        "b.html",
        "Static site",
        "A static site has no server to query.",
    );
    page(
        &root,
        "c.html",
        "Dictionary",
        "The dictionary is sorted by term.",
    );
    let cache = cache_dir("cold");
    let options = BuildOptions::default();

    let cold = build_site(&root, &options).unwrap();
    let first = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    assert_eq!(
        first.bytes, cold.bytes,
        "the first cached build is a cold build"
    );
    assert_eq!(first.stats.pages_reused, 0, "nothing to reuse yet");

    // Second build: every page matches its content hash, so the artifact comes out
    // of the cache and every page is reported as reused.
    let second = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    assert_eq!(second.bytes, cold.bytes);
    assert_eq!(second.stats.pages_reused, 3);
    assert_eq!(second.stats.documents, cold.stats.documents);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn one_changed_page_produces_the_artifact_of_a_full_build() {
    let root = site("changed");
    page(
        &root,
        "a.html",
        "Shard layout",
        "The shard layout splits the dictionary.",
    );
    page(
        &root,
        "b.html",
        "Static site",
        "A static site has no server to query.",
    );
    page(
        &root,
        "c.html",
        "Dictionary",
        "The dictionary is sorted by term.",
    );
    let cache = cache_dir("changed");
    let options = BuildOptions::default();
    build_site_with_cache(&root, &options, Some(&cache)).unwrap();

    // One page changes: a miss is a full build, and the artifact must be the one a
    // build that never saw a cache produces.
    page(
        &root,
        "b.html",
        "Static site",
        "A static site ships the index as files and needs no query server.",
    );
    let incremental = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    let full = build_site(&root, &options).unwrap();
    assert_eq!(incremental.bytes, full.bytes, "a miss is a full build");
    assert_eq!(
        incremental.stats.pages_reused, 0,
        "one changed page is a miss"
    );

    // The cache is now current: a rebuild with no change reuses every page.
    let again = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    assert_eq!(again.stats.pages_reused, 3);
    assert_eq!(again.bytes, full.bytes);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn a_deleted_page_leaves_no_entry_behind() {
    let root = site("deleted");
    page(&root, "a.html", "Alpha", "Alpha document about shards.");
    page(&root, "b.html", "Beta", "Beta document about dictionaries.");
    let cache = cache_dir("deleted");
    let options = BuildOptions::default();
    build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    fs::remove_file(root.join("b.html")).unwrap();

    let incremental = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    let full = build_site(&root, &options).unwrap();
    assert_eq!(incremental.bytes, full.bytes);
    // Deleting b leaves only a: the entry for the removed page must not be served
    // to a future page that happens to reuse its name.
    page(&root, "b.html", "Beta", "Beta document about dictionaries.");
    let recreated = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    let full_again = build_site(&root, &options).unwrap();
    assert_eq!(recreated.bytes, full_again.bytes);
    assert_eq!(
        recreated.stats.pages_reused, 0,
        "a new page is a miss for the site"
    );
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn a_cache_from_other_options_is_not_used() {
    let root = site("options");
    page(
        &root,
        "a.html",
        "Static site",
        "A static site has no server to query.",
    );
    page(
        &root,
        "b.html",
        "Shard layout",
        "The shard layout splits the dictionary.",
    );
    let cache = cache_dir("options");
    let plain = BuildOptions::default();
    build_site_with_cache(&root, &plain, Some(&cache)).unwrap();

    // `--positions` changes what a document is, so the entries above must not be
    // served to this build: the artifact has to come out as a cold positioned build.
    let positioned = BuildOptions {
        positions: true,
        ..BuildOptions::default()
    };
    let incremental = build_site_with_cache(&root, &positioned, Some(&cache)).unwrap();
    let full = build_site(&root, &positioned).unwrap();
    assert_eq!(incremental.bytes, full.bytes);
    assert_eq!(incremental.stats.pages_reused, 0, "different fingerprint");
    let engine = Index::from_bytes(incremental.bytes).unwrap();
    let hits = engine
        .try_search_with_filters("\"static site\"", SearchOptions::default(), &[])
        .unwrap();
    assert!(!hits.is_empty(), "the positioned artifact answers a phrase");
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&cache);
}

#[test]
fn a_corrupt_cache_slows_the_build_and_changes_nothing() {
    let root = site("corrupt");
    page(
        &root,
        "a.html",
        "Alpha",
        "Alpha document about shards and dictionaries.",
    );
    page(&root, "b.html", "Beta", "Beta document about static sites.");
    let cache = cache_dir("corrupt");
    let options = BuildOptions::default();
    build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    fs::write(cache.join("build-cache.bin"), b"RSCACHE2\xff\xff\xff").unwrap();

    let incremental = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    let full = build_site(&root, &options).unwrap();
    assert_eq!(incremental.bytes, full.bytes);
    assert_eq!(incremental.stats.pages_reused, 0);
    // The build repaired the file for the next run.
    let repaired = build_site_with_cache(&root, &options, Some(&cache)).unwrap();
    assert_eq!(repaired.stats.pages_reused, 2);
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(&cache);
}
