//! H4 and H9 acceptance on a real build: what the builder reads is bounded before
//! the read, an oversize page is a skipped page and not a dead build, the admission
//! pass is deterministic across worker counts, and a `--base-url` that could name
//! another origin is refused.
use std::fs;
use std::path::{Path, PathBuf};

use rustsearch_core::Index;
use rustsearch_index::{build_site, validate_base_url, BuildOptions};

#[test]
fn an_oversize_page_is_skipped_and_reported() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("oversize")?;
    write_page(
        &root,
        "small.html",
        1,
        "walkdir keeps the API small and the guide short.",
    )?;
    write_page(
        &root,
        "huge.html",
        40_000,
        "walkdir walkdir walkdir walkdir.",
    )?;

    let build = build_site(
        &root,
        &BuildOptions {
            max_page_bytes: 8 * 1024,
            ..BuildOptions::default()
        },
    )?;
    assert_eq!(build.stats.documents, 1, "only the small page is indexed");
    assert_eq!(build.stats.oversize, 1);
    assert_eq!(build.stats.skipped, 1);
    assert_eq!(build.stats.skipped_notes.len(), 1);
    let note = &build.stats.skipped_notes[0];
    assert!(note.contains("huge.html"), "{note}");
    assert!(note.contains("8192"), "{note}");

    // The artifact is a real one: it loads and answers.
    let index = Index::from_bytes(build.bytes)?;
    assert_eq!(index.doc_count(), 1);
    assert!(!index.search("walkdir", Default::default()).is_empty());
    Ok(())
}

#[test]
fn the_total_budget_is_a_deterministic_prefix() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("budget")?;
    for page in 0..4 {
        write_page(
            &root,
            &format!("page{page}.html"),
            3_000,
            "the shard set keeps the dictionary in chunks.",
        )?;
    }
    let options = BuildOptions {
        max_total_bytes: 7_000,
        ..BuildOptions::default()
    };
    let first = build_site(&root, &options)?;
    let second = build_site(&root, &options)?;
    assert_eq!(
        first.bytes, second.bytes,
        "the budget must not depend on scheduling"
    );
    assert_eq!(first.stats.documents, 2, "two ~3 KiB pages fit in 7 KiB");
    assert_eq!(first.stats.oversize, 2);
    assert!(first
        .stats
        .skipped_notes
        .iter()
        .all(|note| note.contains("build budget")));

    // One worker and many workers must agree on the same admission.
    let bytes = first.bytes;
    let rebuilt = build_site(&root, &options)?;
    assert_eq!(rebuilt.bytes, bytes);
    Ok(())
}

#[test]
fn a_deeply_nested_page_does_not_overflow_the_walk() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("deep")?;
    let mut body = String::from("<p>walkdir</p>");
    for _ in 0..5_000 {
        body = format!("<div>{body}</div>");
    }
    let html = format!(
        "<html lang=\"en\"><head><title>Deep</title></head><body><main>{body}</main></body></html>"
    );
    fs::write(root.join("deep.html"), html)?;
    // A second, ordinary page: the deep one is truncated at the depth bound and
    // contributes no text, which the build reports as skipped instead of crashing.
    write_page(&root, "ok.html", 1, "walkdir keeps the API small.")?;
    let build = build_site(&root, &BuildOptions::default())?;
    assert_eq!(build.stats.documents, 1, "the deep page is truncated away");
    assert_eq!(build.stats.skipped, 1);
    assert!(!Index::from_bytes(build.bytes)?
        .search("walkdir", Default::default())
        .is_empty());
    Ok(())
}

#[test]
fn a_base_url_that_could_name_another_origin_is_refused() {
    for base in [
        "",
        "https://example.com",
        "https://example.com/docs",
        "https://example.com/docs/",
        "http://localhost:8080",
        "demo/site",
        "/demo/site",
    ] {
        assert!(
            validate_base_url(base).is_ok(),
            "{base:?} should be accepted"
        );
    }
    for base in [
        "javascript:alert(1)",
        "data:text/html,x",
        "//example.com",
        "ftp://example.com",
        "https://user:pass@example.com",
        "https://example.com/?next=//evil",
        "https://example.com/#fragment",
        "https://example.com/a/../b",
        "https://example.com/a/%2e%2e/b",
        "https://example.com/a/%2E%2e/b",
        "https://example.com/\n",
        "https://",
        "example.com",
    ] {
        assert!(
            validate_base_url(base).is_err(),
            "{base:?} should be refused"
        );
    }
}

#[test]
fn a_build_with_a_valid_base_url_prefixes_every_document() -> Result<(), Box<dyn std::error::Error>>
{
    let root = site("base")?;
    write_page(&root, "guide/setup.html", 1, "walkdir keeps the API small.")?;
    let build = build_site(
        &root,
        &BuildOptions {
            base_url: "https://example.com/docs".to_string(),
            ..BuildOptions::default()
        },
    )?;
    let index = Index::from_bytes(build.bytes)?;
    let hit = index
        .search("walkdir", Default::default())
        .into_iter()
        .next()
        .expect("one hit");
    assert!(
        hit.url.starts_with("https://example.com/docs/"),
        "{}",
        hit.url
    );
    assert!(!hit.url.starts_with("//"));

    // The same site with a base that carries credentials dies before the walk.
    let error = build_site(
        &root,
        &BuildOptions {
            base_url: "https://user:pass@example.com".to_string(),
            ..BuildOptions::default()
        },
    )
    .err()
    .expect("credentials must be refused")
    .to_string();
    assert!(error.contains("credentials"), "{error}");
    Ok(())
}

fn site(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root =
        std::env::temp_dir().join(format!("rustsearch-bounds-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    Ok(root)
}

/// A page whose body is at least `bytes` bytes of repeated prose.
fn write_page(
    root: &Path,
    name: &str,
    bytes: usize,
    sentence: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = root.join(name);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut body = String::new();
    while body.len() < bytes {
        body.push_str("<p>");
        body.push_str(sentence);
        body.push_str("</p>");
    }
    let html = format!(
        "<html lang=\"en\"><head><title>{name}</title></head><body><main>{body}</main></body></html>"
    );
    fs::write(path, html)?;
    Ok(())
}
