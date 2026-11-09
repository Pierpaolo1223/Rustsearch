//! F6 acceptance: a quoted phrase requires adjacent tokens, and a build without
//! `--positions` is byte-for-byte what it was before the feature existed.
//!
//! The properties the roadmap asks for are checked here against a real build: a
//! phrase rejects `static ... site`, a phrase that drops a stopword keeps its gap,
//! an artifact without positions refuses the query instead of answering it with the
//! words apart, and a chunked artifact reads the same positions from its chunks.
use std::fs;
use std::path::{Path, PathBuf};

use rustsearch_core::{Analyzer, Error, Index, SearchOptions};
use rustsearch_index::{build_site, BuildOptions};

/// A three-page site: one page has "static site" adjacent, one keeps the two words
/// apart, one has them adjacent in a different position.
fn site(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root =
        std::env::temp_dir().join(format!("rustsearch-phrase-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    page(
        &root,
        "adjacent.html",
        "a static site generator writes html files",
    )?;
    page(
        &root,
        "apart.html",
        "static files and a site map live apart",
    )?;
    page(&root, "other.html", "the static site is fast")?;
    page(
        &root,
        "gap.html",
        "the index of the shard is what a query reads",
    )?;
    page(&root, "closer.html", "an index shard is a partition")?;
    Ok(root)
}

fn page(root: &Path, name: &str, body: &str) -> Result<(), Box<dyn std::error::Error>> {
    let html = format!(
        "<html lang=\"en\"><head><title>{name}</title></head>\
         <body><main><h1>{name}</h1><p>{body}</p></main></body></html>"
    );
    fs::write(root.join(name), html)?;
    Ok(())
}

/// `rustsearch-core` is `no_std`-first: its error is `Display`, not
/// `std::error::Error`, so a test that returns `Box<dyn Error>` has to widen it.
fn core<T>(result: Result<T, Error>) -> Result<T, Box<dyn std::error::Error>> {
    result.map_err(|error| Box::<dyn std::error::Error>::from(error.to_string()))
}

fn with_positions(root: &Path) -> Result<Index, Box<dyn std::error::Error>> {
    let build = build_site(
        root,
        &BuildOptions {
            positions: true,
            ..BuildOptions::default()
        },
    )?;
    let index = core(Index::from_bytes(build.bytes))?;
    assert!(index.has_positions());
    Ok(index)
}

fn urls(index: &Index, query: &str) -> Vec<String> {
    let mut hits: Vec<String> = index
        .search(query, SearchOptions::default())
        .into_iter()
        .map(|hit| hit.url)
        .collect();
    hits.sort();
    hits
}

#[test]
fn a_phrase_requires_adjacent_tokens() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("adjacent")?;
    let index = with_positions(&root)?;

    // The two pages that write the words in order, and only them: `apart.html` has
    // both words but nothing between them is adjacent.
    assert_eq!(
        urls(&index, "\"static site\""),
        ["/adjacent.html", "/other.html"]
    );
    // Unquoted, the same words are a bag, which is the behaviour phrases change.
    assert_eq!(
        urls(&index, "static site"),
        ["/adjacent.html", "/apart.html", "/other.html"]
    );
    // A phrase the corpus does not contain matches nothing, even though both its
    // words are everywhere.
    assert!(urls(&index, "\"site static\"").is_empty());

    // The phrase scores as one more specific term: a page with the phrase beats a
    // page that only holds the words apart when both are asked for.
    let phrase_hits = index.search("\"static site\"", SearchOptions::default());
    assert!(phrase_hits.iter().all(|hit| hit.url != "/apart.html"));
    Ok(())
}

#[test]
fn a_phrase_keeps_the_gap_of_a_dropped_stopword() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("gap")?;
    let build = build_site(
        &root,
        &BuildOptions {
            analyzer: Analyzer::English,
            positions: true,
            ..BuildOptions::default()
        },
    )?;
    let index = core(Index::from_bytes(build.bytes))?;

    // "of the" are stopwords, so the phrase indexes two terms three tokens apart:
    // it matches the page that writes them that way, and the adjacent page is a
    // different phrase, not the same one read loosely.
    assert_eq!(urls(&index, "\"index of the shard\""), ["/gap.html"]);
    assert_eq!(urls(&index, "\"index shard\""), ["/closer.html"]);
    // And the words apart are not the phrase.
    assert!(urls(&index, "\"shard of the index\"").is_empty());
    Ok(())
}

#[test]
fn without_positions_a_phrase_is_refused() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("flat")?;
    let build = build_site(&root, &BuildOptions::default())?;
    let index = core(Index::from_bytes(build.bytes))?;
    assert!(!index.has_positions());

    assert!(matches!(
        index.try_search("\"static site\"", SearchOptions::default()),
        Err(Error::NoPositions)
    ));
    // The same words unquoted are answered exactly as before.
    assert_eq!(
        urls(&index, "static site"),
        ["/adjacent.html", "/apart.html", "/other.html"]
    );
    Ok(())
}

#[test]
fn positions_are_opt_in_and_byte_identical_when_off() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("optin")?;
    let plain = build_site(&root, &BuildOptions::default())?;
    let explicit = build_site(
        &root,
        &BuildOptions {
            positions: false,
            ..BuildOptions::default()
        },
    )?;
    assert_eq!(plain.bytes, explicit.bytes, "off is the old build, exactly");
    assert!(!core(Index::from_bytes(plain.bytes.clone()))?.has_positions());

    let positioned = build_site(
        &root,
        &BuildOptions {
            positions: true,
            ..BuildOptions::default()
        },
    )?;
    assert_ne!(positioned.bytes, plain.bytes);
    assert!(positioned.bytes.len() > plain.bytes.len());
    assert!(core(Index::from_bytes(positioned.bytes))?.has_positions());
    Ok(())
}

#[test]
fn a_chunked_artifact_reads_positions_from_its_chunks() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("chunked")?;
    let build = build_site(
        &root,
        &BuildOptions {
            term_chunk_bytes: 4096,
            positions: true,
            ..BuildOptions::default()
        },
    )?;
    let index = core(Index::from_bytes(build.bytes))?;
    assert!(index.is_chunked(), "the fixture must produce term chunks");
    assert!(index.has_positions());
    assert_eq!(
        urls(&index, "\"static site\""),
        ["/adjacent.html", "/other.html"]
    );

    // The same build without positions is a chunked artifact whose phrase query is
    // refused: the flag is the whole difference on both layouts.
    let flat_phrases = build_site(
        &root,
        &BuildOptions {
            term_chunk_bytes: 4096,
            ..BuildOptions::default()
        },
    )?;
    let index = core(Index::from_bytes(flat_phrases.bytes))?;
    assert!(matches!(
        index.try_search("\"static site\"", SearchOptions::default()),
        Err(Error::NoPositions)
    ));
    Ok(())
}
