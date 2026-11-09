//! Build-time synonyms (ROADMAP §"Next cycle", item 5).
//!
//! The contract: the rule expands the **index**, not the query. A page that says
//! "ship" answers a query for "deploy" because the artifact carries a posting for it,
//! so a client with no rules and no extra code keeps working — and an artifact built
//! without the file is byte-identical to what the engine wrote before the feature.

use std::fs;
use std::path::{Path, PathBuf};

use rustsearch_core::rules::Synonyms;
use rustsearch_core::{Index, SearchOptions};
use rustsearch_index::{build_site, build_site_with_cache, build_site_with_rules, BuildOptions};

fn site(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("rustsearch-rules-{name}-{}", std::process::id()));
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

fn urls(index: &Index, query: &str) -> Vec<String> {
    index
        .search(query, SearchOptions::default())
        .into_iter()
        .map(|hit| hit.url)
        .collect()
}

#[test]
fn a_synonym_is_a_posting_written_at_build_time() {
    let root = site("expand");
    page(
        &root,
        "a.html",
        "Shipping",
        "The release ships the artifact to a bucket.",
    );
    page(
        &root,
        "b.html",
        "Deploying",
        "A deploy uploads the artifact and invalidates it.",
    );
    let options = BuildOptions {
        analyzer: rustsearch_core::Analyzer::English,
        ..BuildOptions::default()
    };
    let synonyms = Synonyms::parse("deploy => ship\n").unwrap();

    let plain_build = build_site(&root, &options).unwrap();
    let plain_index = Index::from_bytes(plain_build.bytes.clone()).unwrap();
    assert!(urls(&plain_index, "ship").contains(&"/a.html".to_string()));
    assert!(
        !urls(&plain_index, "ship").contains(&"/b.html".to_string()),
        "without the rule, the deploy page is not an answer for ship"
    );

    let expanded = build_site_with_rules(&root, &options, None, Some(&synonyms)).unwrap();
    let expanded_index = Index::from_bytes(expanded.bytes.clone()).unwrap();
    let shipped = urls(&expanded_index, "ship");
    assert!(
        shipped.contains(&"/a.html".to_string()) && shipped.contains(&"/b.html".to_string()),
        "the page that says deploy must answer for ship too, got {shipped:?}"
    );
    // The cost of the rule is artifact bytes, paid once at build time.
    assert!(
        expanded.bytes.len() > plain_build.bytes.len(),
        "the synonym adds a posting: {} vs {}",
        expanded.bytes.len(),
        plain_build.bytes.len()
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn synonyms_are_analyzed_like_the_documents() {
    let root = site("stemmed");
    page(
        &root,
        "a.html",
        "Operations",
        "The release deploys the artifact overnight.",
    );
    let options = BuildOptions {
        analyzer: rustsearch_core::Analyzer::English,
        ..BuildOptions::default()
    };
    // Both sides are written in an inflection; the dictionary holds stems.
    let synonyms = Synonyms::parse("deploying => shipping\n").unwrap();
    let build = build_site_with_rules(&root, &options, None, Some(&synonyms)).unwrap();
    let index = Index::from_bytes(build.bytes).unwrap();
    assert!(
        urls(&index, "ships").contains(&"/a.html".to_string()),
        "a stemmed query must meet a stemmed synonym"
    );
    assert!(
        urls(&index, "shipped").contains(&"/a.html".to_string()),
        "the synonym's target is stemmed too"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn without_a_file_the_artifact_is_unchanged() {
    let root = site("unchanged");
    page(
        &root,
        "a.html",
        "Alpha",
        "The shard layout splits the dictionary.",
    );
    page(&root, "b.html", "Beta", "A static site has no server.");
    let options = BuildOptions::default();
    let plain = build_site(&root, &options).unwrap();
    let empty = Synonyms::parse("# nothing\n").unwrap();
    let with_empty = build_site_with_rules(&root, &options, None, Some(&empty)).unwrap();
    assert_eq!(plain.bytes, with_empty.bytes);
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_cache_from_a_build_without_the_rules_is_not_reused() {
    let root = site("cache");
    page(
        &root,
        "a.html",
        "Shipping",
        "The release ships the artifact.",
    );
    let options = BuildOptions::default();
    let cache = root.join("cache");
    build_site_with_cache(&root, &options, Some(&cache)).unwrap();

    let synonyms = Synonyms::parse("deploy => ship\n").unwrap();
    let with_rules = build_site_with_rules(&root, &options, Some(&cache), Some(&synonyms)).unwrap();
    let cold = build_site_with_rules(&root, &options, None, Some(&synonyms)).unwrap();
    assert_eq!(with_rules.bytes, cold.bytes);
    assert_eq!(
        with_rules.stats.pages_reused, 0,
        "the rules change the artifact, so the pages cannot be served from the old cache"
    );
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn synonyms_and_positions_are_refused_together() {
    let root = site("positions");
    page(&root, "a.html", "Alpha", "A page with a phrase in it.");
    let options = BuildOptions {
        positions: true,
        ..BuildOptions::default()
    };
    let synonyms = Synonyms::parse("deploy => ship\n").unwrap();
    let error = match build_site_with_rules(&root, &options, None, Some(&synonyms)) {
        Ok(_) => panic!("a positioned build with synonyms must be refused"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("cannot be combined with --positions"),
        "got {error}"
    );
    // An empty rule file is not a reason to refuse a positioned build.
    let empty = Synonyms::parse("# none\n").unwrap();
    assert!(build_site_with_rules(&root, &options, None, Some(&empty)).is_ok());
    let _ = fs::remove_dir_all(&root);
}
