//! F8 acceptance: a token with no exact match is read as its nearest dictionary
//! term, and a query whose tokens all exist ranks exactly as it did before.
//!
//! The properties the roadmap asks for are checked here against a real build: the
//! correction is reported, the ranking of an exact query does not move, and the pass
//! stays off where it cannot work (a token shorter than four bytes, a chunked
//! artifact whose neighbour chunks the client would have to fetch, `typos: false`).
use std::fs;
use std::path::{Path, PathBuf};

use rustsearch_core::{Correction, Hit, Index, SearchOptions};
use rustsearch_index::{build_site, BuildOptions};

#[test]
fn a_typo_is_read_as_the_nearest_term() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("correct")?;
    let index = flat(&root)?;
    let options = SearchOptions {
        prefix: false,
        ..SearchOptions::default()
    };

    // "postgrs" is one deletion away from "postgres"; "shardd" one from "shard".
    let report = core(index.try_search_report("postgrs", options))?;
    assert_eq!(
        report.corrected,
        vec![Correction {
            from: "postgrs".into(),
            to: "postgres".into(),
        }]
    );
    assert_eq!(report.hits.len(), 4, "every page mentions postgres");

    // A corrected term scores below a real match, so a typo reading never outranks
    // the word the caller typed.
    let exact = core(index.try_search_report("postgres", options))?;
    assert!(report.hits[0].score < exact.hits[0].score);

    // Two typos in one query are two corrections, in the order the tokens appear.
    let both = core(index.try_search_report("postgrs shardd", options))?;
    assert_eq!(
        both.corrected
            .iter()
            .map(|correction| (correction.from.as_str(), correction.to.as_str()))
            .collect::<Vec<_>>(),
        vec![("postgrs", "postgres"), ("shardd", "shard")]
    );

    // Four bytes is the floor: a shorter token is too likely to be another word.
    assert!(core(index.try_search_report("pst", options))?
        .corrected
        .is_empty());
    Ok(())
}

#[test]
fn an_exact_query_ranks_exactly_as_before() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("drift")?;
    let index = flat(&root)?;
    let options = SearchOptions {
        prefix: false,
        ..SearchOptions::default()
    };

    let on = core(index.try_search_report("postgres shard", options))?;
    let off = core(index.try_search_report(
        "postgres shard",
        SearchOptions {
            typos: false,
            ..options
        },
    ))?;
    assert!(on.corrected.is_empty(), "nothing to correct");
    assert_eq!(ranking(&on.hits), ranking(&off.hits));

    // And with the pass off, a typo is simply not a match.
    let none = core(index.try_search_report(
        "postgrs",
        SearchOptions {
            typos: false,
            ..options
        },
    ))?;
    assert!(none.hits.is_empty());
    assert!(none.corrected.is_empty());
    Ok(())
}

#[test]
fn a_chunked_artifact_keeps_the_pass_off() -> Result<(), Box<dyn std::error::Error>> {
    let root = site("chunked")?;
    let chunked = build_site(
        &root,
        &BuildOptions {
            term_chunk_bytes: 4096,
            ..BuildOptions::default()
        },
    )?;
    let index = core(Index::from_bytes(chunked.bytes))?;
    assert!(index.is_chunked(), "the fixture must produce term chunks");

    // The neighbour's chunk is not attached, so the engine has nothing to score it
    // against and says nothing instead of guessing. The explicit fuzzy pass stays the
    // opt-in there (F8's documented limit).
    let report = core(index.try_search_report(
        "postgrs",
        SearchOptions {
            prefix: false,
            ..SearchOptions::default()
        },
    ))?;
    assert!(report.corrected.is_empty());
    assert!(report.hits.is_empty());
    Ok(())
}

/// A four-page site where every page mentions the same two words.
fn site(name: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let root = std::env::temp_dir().join(format!("rustsearch-typo-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root)?;
    for page in 0..4 {
        let mut body = String::new();
        for sentence in 0..30 {
            body.push_str(&format!(
                "<p>Page {page} sentence {sentence}: the postgres shard set keeps the \
                 dictionary in chunks, and the shard set is what a query reads.</p>"
            ));
        }
        let html = format!(
            "<html lang=\"en\"><head><title>Postgres sharding {page}</title></head>\
             <body><main>{body}</main></body></html>"
        );
        fs::write(root.join(format!("page{page}.html")), html)?;
    }
    Ok(root)
}

fn flat(root: &Path) -> Result<Index, Box<dyn std::error::Error>> {
    let build = build_site(root, &BuildOptions::default())?;
    assert!(!core(Index::from_bytes(build.bytes.clone()))?.is_chunked());
    core(Index::from_bytes(build.bytes))
}

/// `rustsearch-core` is `no_std`-first: its error is `Display`, not
/// `std::error::Error`, so a test that returns `Box<dyn Error>` has to widen it.
fn core<T>(result: Result<T, rustsearch_core::Error>) -> Result<T, Box<dyn std::error::Error>> {
    Ok(result.map_err(|error| error.to_string())?)
}

/// Ranking identity: URL and raw score bits, in order.
fn ranking(hits: &[Hit]) -> Vec<(String, u32)> {
    hits.iter()
        .map(|hit| (hit.url.clone(), hit.score.to_bits()))
        .collect()
}
