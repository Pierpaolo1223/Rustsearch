//! `rustsearch`: build an immutable index, inspect it, query it, or run the
//! reproducible benchmark. No argument-parsing dependency: the surface is four
//! subcommands and a handful of flags.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use rustsearch_core::{
    json_escape, search_report_to_json_with_filters, Analyzer, Curations, Filter, Index,
    SearchOptions, Synonyms, MAX_TITLE_BOOST,
};
use rustsearch_index::shard;
use rustsearch_index::{build_site, BuildOptions};

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("build") => build(&args[1..]),
        Some("search") => search(&args[1..]),
        Some("info") => info(&args[1..]),
        Some("verify") => verify(&args[1..]),
        Some("diff") => diff(&args[1..]),
        Some("shard") => shards(&args[1..]),
        Some("bench") => bench(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            usage();
            Ok(())
        }
        Some(other) => Err(format!("unknown command: {other}").into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rustsearch: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    println!(
        "rustsearch 0.1 — static-site full-text search\n\
         \n\
         Usage:\n\
           rustsearch build <site-dir> -o <index.rsx> [--base-url URL] [--stemmer none|en|it]\n\
           \x20              [--max-page-chars N] [--term-chunk-bytes N]\n\
           \x20              [--max-page-bytes N] [--max-total-bytes N] [--positions]
           \x20              [--no-header-checksum]\n\
           \x20              [--cache DIR] [--synonyms FILE]\n\
           rustsearch search <index.rsx> <query> [-n N] [--json] [--fuzzy] [--no-prefix]\n\
           \x20              [--filter field=value ...] [--title-boost N] [--group]\n\
           \x20              [--no-typos] [--curate FILE]\n\
           rustsearch info <index.rsx>\n\
           rustsearch diff <a.rsx> <b.rsx> [--json]\n\
           rustsearch verify <file> [--expect-digest sha256:HEX]\n\
           \x20              [--require-signature SIG --public-key KEY]\n\
           rustsearch shard <index.rsx> -o <dir> [--prefix path/] [--plan QUERY] [-n N]\n\
           \x20              [--meta-part-bytes N]\n\
           rustsearch bench [--docs N] [--words N] [--queries N] [--out FILE]\n         \
         [--title-boost N] [--group] [--term-chunk-bytes N]\n\
         \n\
         --stemmer picks the language analyzer; it is stored in the index, so\n\
         queries are stemmed the same way. --filter is repeatable: fields are\n\
         AND-ed, values inside a field OR-ed (--filter lang=en --filter lang=it).\n\
         --title-boost weighs a title match against a body match (default 2.0,\n\
         0 ignores titles, capped at 10). --max-page-chars splits a long page into\n\
         several documents that share its URL; --group collapses them back into\n\
         one hit per page. --term-chunk-bytes groups the dictionary into chunks of\n\
         at most N bytes (format v5, default 0 = one flat dictionary), so a query\n\
         only fetches the chunks its terms live in. --meta-part-bytes sets the\n\
         budget for a chunked artifact's metadata run: objects of at most N\n\
         bytes (meta00000.bin, …), default 1 MiB; 0 publishes one meta.bin.\n\
         --max-page-bytes refuses a page above N bytes (default 8 MiB, 0 = no\n\
         bound) and --max-total-bytes stops admitting pages once their sizes add\n\
         up to N (default 0 = no budget); a refused page is skipped and listed.\n\
         A query token with no exact match is read as its nearest dictionary\n\
         term one edit away, scored below a real match, and reported as\n\
         \"did you mean\" next to the results; --no-typos turns that off.\n\
         --positions records the token ordinals of every posting, so a quoted\n\
         phrase (\"static site\") requires adjacency; it is the one build flag\n\
         that grows the artifact, and a phrase query on an artifact without it\n\
         is refused rather than answered with the words apart.\n\
         --cache DIR keeps the per-page analysis in DIR/build-cache.bin: a rebuild\n\
         re-reads and hashes every page but only parses the ones whose bytes\n\
         changed, and a rebuild with no change returns the artifact the cache\n\
         already holds. The output is byte-identical to a build without it.\n
         The index is immutable:\n\
         rebuild it when the site changes."
    );
    println!(
        "\n--synonyms FILE expands the index: a page that says \"ship\" also carries a\n\
         posting for \"deploy\" when the file has `deploy => ship`, so the query needs\n\
         no rules. One rule per line, at most 8 targets, 64 bytes per term; it cannot\n\
         be combined with --positions, because a synonym posting has no ordinal to\n\
         check adjacency against.\n\
         --curate FILE reorders the results of an exact query: `install | pin\n\
         /docs/install.html` moves that hit to the front, `install | boost\n\
         /docs/cli.html 1.5` multiplies its score. A path must be on this site, a pin\n\
         for a document the query did not return is ignored, and the file never\n\
         changes what matched."
    );
}

fn build(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut site: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut base_url = String::new();
    let mut analyzer = Analyzer::None;
    let mut max_page_chars = 0usize;
    let mut term_chunk_bytes = 0usize;
    let mut max_page_bytes = rustsearch_index::DEFAULT_MAX_PAGE_BYTES;
    let mut max_total_bytes = 0usize;
    let mut positions = false;
    let mut header_checksum = true;
    let mut cache: Option<PathBuf> = None;
    let mut synonyms_path: Option<PathBuf> = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "-o" | "--out" => {
                index += 1;
                out = Some(args.get(index).ok_or("--out needs a path")?.into());
            }
            "--base-url" => {
                index += 1;
                base_url = args.get(index).ok_or("--base-url needs a value")?.clone();
            }
            "--stemmer" => {
                index += 1;
                let value = args.get(index).ok_or("--stemmer needs none, en or it")?;
                analyzer = Analyzer::parse(value)
                    .ok_or_else(|| format!("unknown stemmer {value:?}: use none, en or it"))?;
            }
            "--max-page-chars" => {
                index += 1;
                max_page_chars = args
                    .get(index)
                    .ok_or("--max-page-chars needs a number")?
                    .parse()
                    .map_err(|_| "invalid --max-page-chars")?;
                if max_page_chars > 0 && max_page_chars < 64 {
                    return Err("--max-page-chars must be 0 (never split) or at least 64".into());
                }
            }
            "--max-page-bytes" => {
                index += 1;
                let raw = args.get(index).ok_or("--max-page-bytes needs a number")?;
                max_page_bytes = byte_budget(raw)?;
            }
            "--max-total-bytes" => {
                index += 1;
                let raw = args.get(index).ok_or("--max-total-bytes needs a number")?;
                max_total_bytes = byte_budget(raw)?;
            }
            "--term-chunk-bytes" => {
                index += 1;
                let raw = args.get(index).ok_or("--term-chunk-bytes needs a number")?;
                term_chunk_bytes = byte_budget(raw)?;
                if term_chunk_bytes != 0
                    && term_chunk_bytes < rustsearch_index::MIN_TERM_CHUNK_BYTES
                {
                    return Err(format!(
                        "--term-chunk-bytes must be 0 (off) or at least {}",
                        rustsearch_index::MIN_TERM_CHUNK_BYTES
                    )
                    .into());
                }
            }
            "--positions" => positions = true,
            "--header-checksum" => header_checksum = true,
            "--no-header-checksum" => header_checksum = false,
            "--cache" => {
                index += 1;
                cache = Some(args.get(index).ok_or("--cache needs a directory")?.into());
            }
            "--synonyms" => {
                index += 1;
                synonyms_path = Some(args.get(index).ok_or("--synonyms needs a file")?.into());
            }
            value if value.starts_with('-') => return Err(format!("unknown flag: {value}").into()),
            value => {
                if site.is_some() {
                    return Err("build takes one site directory".into());
                }
                site = Some(value.into());
            }
        }
        index += 1;
    }
    let site = site.ok_or("build needs a site directory")?;
    let out = out.ok_or("build needs -o <index.rsx>")?;
    let started = Instant::now();
    let synonyms = match &synonyms_path {
        Some(path) => {
            let text = fs::read_to_string(path)?;
            Some(Synonyms::parse(&text).map_err(|error| format!("{}: {error}", path.display()))?)
        }
        None => None,
    };
    let build = rustsearch_index::build_site_with_rules(
        &site,
        &BuildOptions {
            base_url,
            analyzer,
            max_page_chars,
            term_chunk_bytes,
            max_page_bytes,
            max_total_bytes,
            positions,
            header_checksum,
        },
        cache.as_deref(),
        synonyms.as_ref(),
    )?;
    let elapsed = started.elapsed();
    if let Some(parent) = out.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)?;
        }
    }
    fs::write(&out, &build.bytes)?;
    // H1: the identity of the artifact is SHA-256, written next to it. The FNV the
    // format uses is a corruption check; whoever can rewrite the artifact can
    // recompute every FNV in it, so a deploy pins this digest instead.
    let digest = rustsearch_core::sha256_hex(&build.bytes);
    let sidecar = sidecar_path(&out);
    fs::write(&sidecar, format!("sha256:{digest}\n"))?;
    println!("sha256: {digest}  ({})", sidecar.display());
    for note in &build.stats.skipped_notes {
        eprintln!("skipped {note}");
    }
    println!(
        "built {} docs / {} terms / {} tokens ({} in titles) -> {} ({:.1} KiB, {:.0} ms)",
        build.stats.documents,
        build.stats.terms,
        build.stats.tokens,
        build.stats.title_tokens,
        out.display(),
        build.stats.bytes as f64 / 1024.0,
        elapsed.as_secs_f64() * 1000.0
    );
    if build.stats.pages_reused > 0 {
        println!(
            "{} page(s) reused from the analysis cache",
            build.stats.pages_reused
        );
    }
    Ok(())
}

/// A byte budget on the command line: `65536`, `64k` or `1M`. The two suffixed forms
/// are the budgets worth trying — one request's worth and one CDN cache slice.
fn byte_budget(raw: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let (digits, scale) = match raw.strip_suffix(['k', 'K']) {
        Some(digits) => (digits, 1024usize),
        None => match raw.strip_suffix(['m', 'M']) {
            Some(digits) => (digits, 1024 * 1024),
            None => (raw, 1usize),
        },
    };
    Ok(digits
        .parse::<usize>()
        .map_err(|_| format!("invalid byte budget: {raw}"))?
        .saturating_mul(scale))
}

/// The chunk id of a `chunkNNNNN` part, and `None` for every other part (`chunks`,
/// the directory, is metadata and is always fetched).
fn chunk_id(name: &str) -> Option<u32> {
    name.strip_prefix("chunk")?.parse().ok()
}

fn search(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut path: Option<PathBuf> = None;
    let mut words: Vec<String> = Vec::new();
    let mut options = SearchOptions::default();
    let mut filters: Vec<(String, String)> = Vec::new();
    let mut as_json = false;
    let mut curate: Option<PathBuf> = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "-n" | "--limit" => {
                index += 1;
                options.limit = args
                    .get(index)
                    .ok_or("-n needs a number")?
                    .parse()
                    .map_err(|_| "invalid -n")?;
            }
            "--json" => as_json = true,
            "--fuzzy" => options.fuzzy = true,
            "--title-boost" | "--boost" => {
                index += 1;
                let value: f32 = args
                    .get(index)
                    .ok_or("--title-boost needs a number")?
                    .parse()
                    .map_err(|_| "invalid --title-boost")?;
                if !value.is_finite() || !(0.0..=MAX_TITLE_BOOST).contains(&value) {
                    return Err(
                        format!("--title-boost must be between 0 and {MAX_TITLE_BOOST}").into(),
                    );
                }
                options.title_boost = value;
            }
            "--filter" => {
                index += 1;
                let value = args.get(index).ok_or("--filter needs field=value")?;
                let (field, value) = value.split_once('=').ok_or("--filter needs field=value")?;
                filters.push((field.to_string(), value.to_string()));
            }
            "--group" => options.group_by_url = true,
            "--curate" => {
                index += 1;
                curate = Some(args.get(index).ok_or("--curate needs a file")?.into());
            }
            "--no-typos" => options.typos = false,
            "--no-prefix" => options.prefix = false,
            value if value.starts_with('-') => return Err(format!("unknown flag: {value}").into()),
            value => {
                if path.is_none() {
                    path = Some(value.into());
                } else {
                    words.push(value.to_string());
                }
            }
        }
        index += 1;
    }
    let path = path.ok_or("search needs an index and a query")?;
    if words.is_empty() {
        return Err("search needs a query".into());
    }
    let query = words.join(" ");
    let index = load(&path)?;
    let filters: Vec<Filter<'_>> = filters
        .iter()
        .map(|(field, value)| Filter::new(field, value))
        .collect();
    let started = Instant::now();
    let mut report = index
        .try_search_report_with_filters(&query, options, &filters)
        .map_err(|error| format!("{error}"))?;
    if let Some(path) = &curate {
        let text = fs::read_to_string(path)?;
        let curations =
            Curations::parse(&text).map_err(|error| format!("{}: {error}", path.display()))?;
        curations.apply(&query, &mut report.hits);
    }
    let elapsed = started.elapsed();
    let hits = &report.hits;
    if as_json {
        println!("{}", search_report_to_json_with_filters(&report, &filters));
    } else {
        println!(
            "{} hit(s) in {:.2} ms\n",
            hits.len(),
            elapsed.as_secs_f64() * 1000.0
        );
        for correction in &report.corrected {
            println!(
                "did you mean {}{}?",
                escape_controls(&correction.to),
                if correction.from.is_empty() {
                    String::new()
                } else {
                    format!(" (read {:?} as a typo)", escape_controls(&correction.from))
                }
            );
        }
        for (rank, hit) in hits.iter().enumerate() {
            let chunks = if hit.matches > 1 {
                format!("  [{} chunks]", hit.matches)
            } else {
                String::new()
            };
            println!(
                "{}. {:.4}  {}  {}{chunks}",
                rank + 1,
                hit.score,
                escape_controls(&hit.url),
                escape_controls(&hit.title)
            );
            if !hit.snippet.is_empty() {
                println!("   {}", escape_controls(&hit.snippet));
            }
        }
    }
    Ok(())
}

/// Compare two artifacts (H11): the operational half of `verify`.
///
/// `verify` answers *whether* a deploy changed (the digest); `diff` answers *what*
/// changed — the format, the analyzer, the counts, and the documents added, removed
/// or rewritten. A bucket listing cannot tell a deploy from a swap; this can.
fn diff(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut as_json = false;
    for arg in args {
        match arg.as_str() {
            "--json" => as_json = true,
            value if value.starts_with('-') => return Err(format!("unknown flag: {value}").into()),
            value => paths.push(PathBuf::from(value)),
        }
    }
    if paths.len() != 2 {
        return Err("diff takes two artifacts: rustsearch diff a.rsx b.rsx".into());
    }
    let old_bytes = fs::read(&paths[0])?;
    let new_bytes = fs::read(&paths[1])?;
    let old =
        Index::from_bytes(old_bytes).map_err(|error| format!("{}: {error}", paths[0].display()))?;
    let new =
        Index::from_bytes(new_bytes).map_err(|error| format!("{}: {error}", paths[1].display()))?;

    let old_docs = documents(&old);
    let new_docs = documents(&new);
    let added: Vec<&str> = new_docs
        .keys()
        .filter(|url| !old_docs.contains_key(*url))
        .map(String::as_str)
        .collect();
    let removed: Vec<&str> = old_docs
        .keys()
        .filter(|url| !new_docs.contains_key(*url))
        .map(String::as_str)
        .collect();
    let changed: Vec<&str> = new_docs
        .iter()
        .filter(|(url, (hash, _))| {
            old_docs
                .get(*url)
                .is_some_and(|(old_hash, _)| old_hash != hash)
        })
        .map(|(url, _)| url.as_str())
        .collect();

    if as_json {
        // Fixed, documented shape: the two summaries, then three URL lists.
        let mut out = String::from("{\"a\":");
        out.push_str(&summary_json(&old, old_docs.len()));
        out.push_str(",\"b\":");
        out.push_str(&summary_json(&new, new_docs.len()));
        out.push_str(",\"added\":[");
        for (index, url) in added.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&json_escape(url));
        }
        out.push_str("],\"removed\":[");
        for (index, url) in removed.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&json_escape(url));
        }
        out.push_str("],\"changed\":[");
        for (index, url) in changed.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str(&json_escape(url));
        }
        out.push_str("]}");
        println!("{out}");
        return Ok(());
    }

    println!("a  {}", paths[0].display());
    println!("b  {}", paths[1].display());
    let arrow = |left: String, right: String| {
        if left == right {
            left
        } else {
            format!("{left} -> {right}")
        }
    };
    println!(
        "format      {}",
        arrow(
            old.format_version().to_string(),
            new.format_version().to_string()
        )
    );
    println!(
        "analyzer    {}",
        arrow(
            old.analyzer().name().to_string(),
            new.analyzer().name().to_string()
        )
    );
    println!(
        "documents   {}",
        arrow(old.doc_count().to_string(), new.doc_count().to_string())
    );
    println!(
        "terms       {}",
        arrow(old.term_count().to_string(), new.term_count().to_string())
    );
    println!(
        "urls        +{} added, -{} removed, {} changed",
        added.len(),
        removed.len(),
        changed.len()
    );
    print_unless_empty('+', &added);
    print_unless_empty('-', &removed);
    print_unless_empty('~', &changed);
    if added.is_empty() && removed.is_empty() && changed.is_empty() {
        println!("  no document-level change");
    }
    Ok(())
}

/// URL -> (text hash, title) for every document whose strings are readable.
fn documents(index: &Index) -> BTreeMap<String, (u64, String)> {
    let mut out = BTreeMap::new();
    for doc in 0..index.doc_count() {
        let Some((url, title)) = index.doc_url_title(doc) else {
            continue;
        };
        out.insert(url, (index.block_hash(doc).unwrap_or(0), title));
    }
    out
}

fn print_unless_empty(sign: char, urls: &[&str]) {
    const SHOWN: usize = 50;
    for url in urls.iter().take(SHOWN) {
        println!("  {sign} {url}");
    }
    if urls.len() > SHOWN {
        println!("  {sign} … {} more", urls.len() - SHOWN);
    }
}

fn summary_json(index: &Index, urls: usize) -> String {
    format!(
        "{{\"format\":{},\"analyzer\":{},\"documents\":{},\"terms\":{},\"urls\":{}}}",
        index.format_version(),
        json_escape(index.analyzer().name()),
        index.doc_count(),
        index.term_count(),
        urls
    )
}

fn info(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let path = args.first().ok_or("info needs an index")?;
    let index = load(Path::new(path))?;
    println!("file:        {path}");
    println!("documents:   {}", index.doc_count());
    println!("terms:       {}", index.term_count());
    println!("tokens:      {}", index.token_count());
    if index.has_title_postings() {
        println!("title tokens:{}", index.title_token_count());
    }
    println!("bytes:       {}", index.memory_bytes());
    println!("analyzer:    {}", index.analyzer().name());
    println!("version:     {}", index.format_version());
    println!(
        "positions:   {}",
        if index.has_positions() {
            "yes (phrase queries)"
        } else {
            "no (rebuild with --positions for phrase queries)"
        }
    );
    if index.is_chunked() {
        println!(
            "chunks:      {} (dictionary split into fetchable term chunks)",
            index.chunk_count()
        );
        println!(
            "records:     {} pages of {} (read per hit)",
            index.doc_page_count(),
            index.doc_page_records()
        );
    }
    println!("checksum:    {:016x}", index.checksum());
    let fields = index.facet_fields();
    if fields.is_empty() {
        println!("facets:      none (this artifact carries no metadata to filter on)");
    }
    for field in fields {
        let values = index.facet_values(field).unwrap_or_default();
        println!("facet:       {field} ({} values)", values.len());
        for (value, documents) in values.iter().take(12) {
            println!("               {value} ({documents})");
        }
        if values.len() > 12 {
            println!("               … {} more", values.len() - 12);
        }
    }
    Ok(())
}

/// Write the artifact as a manifest plus one object per section, and print what
/// a query would actually download.
fn shards(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut path: Option<PathBuf> = None;
    let mut out: Option<PathBuf> = None;
    let mut prefix = String::new();
    let mut plan: Option<String> = None;
    let mut limit = 5usize;
    let mut meta_part_bytes: Option<usize> = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "-o" | "--out" => {
                index += 1;
                out = Some(args.get(index).ok_or("-o needs a directory")?.into());
            }
            "--prefix" => {
                index += 1;
                prefix = args.get(index).ok_or("--prefix needs a value")?.clone();
            }
            "--plan" => {
                index += 1;
                plan = Some(args.get(index).ok_or("--plan needs a query")?.clone());
            }
            "-n" | "--limit" => {
                index += 1;
                limit = args
                    .get(index)
                    .ok_or("-n needs a number")?
                    .parse()
                    .map_err(|_| "invalid -n")?;
            }
            "--meta-part-bytes" => {
                index += 1;
                let budget =
                    byte_budget(args.get(index).ok_or("--meta-part-bytes needs a number")?)?;
                if budget != 0 && budget < shard::MIN_META_PART_BYTES {
                    return Err(format!(
                        "--meta-part-bytes must be 0 (one meta object) or at least {}",
                        shard::MIN_META_PART_BYTES
                    )
                    .into());
                }
                meta_part_bytes = Some(budget);
            }
            value if value.starts_with('-') => return Err(format!("unknown flag: {value}").into()),
            value => {
                if path.is_some() {
                    return Err("shard takes one index".into());
                }
                path = Some(value.into());
            }
        }
        index += 1;
    }
    let path = path.ok_or("shard needs an index")?;
    let out = out.ok_or("shard needs -o <dir>")?;
    let bytes = fs::read(&path)?;
    let set = shard::split_with(&bytes, &prefix, shard::ShardOptions { meta_part_bytes })?;
    fs::create_dir_all(&out)?;
    fs::write(out.join("manifest.txt"), &set.manifest)?;
    // The manifest is the routing table a cold client trusts, so it gets the same
    // identity: a deploy pins this digest, and a substituted manifest is refused
    // before a single part is parsed (`loadSparseIndex({ expectManifestDigest })`).
    let manifest_digest = rustsearch_core::sha256_hex(set.manifest.as_bytes());
    fs::write(
        out.join("manifest.txt.sha256"),
        format!("sha256:{manifest_digest}\n"),
    )?;
    for part in &set.parts {
        let name = Path::new(&part.key)
            .file_name()
            .ok_or("part key has no file name")?;
        fs::write(out.join(name), &part.bytes)?;
    }
    let manifest_bytes = set.manifest.len();
    println!("manifest.txt  {manifest_bytes:>7} bytes  (always fetched)");
    println!("manifest.txt.sha256  sha256:{manifest_digest}");
    for part in &set.parts {
        println!("{:<12} {:>7} bytes  {}", part.key, part.len, part.name);
    }
    let total: usize = set.parts.iter().map(|part| part.len).sum();
    println!("artifact     {total:>7} bytes in one request before sharding");

    // What a query actually reads: the manifest, every part except the text
    // blocks, and the block of each hit it will render.
    if let Some(query) = plan {
        let index = load(&path)?;
        let options = SearchOptions {
            limit,
            ..Default::default()
        };
        let hits = index.search(&query, options);
        // A chunked artifact publishes one object per term chunk, so a query fetches
        // only the chunks its terms live in; a flat one fetches its dictionary whole.
        // Everything else is the same in both.
        let needed = index.chunks_for_query(&query, options)?;
        // The record pages are read per hit like the strings and the text blocks, so
        // they are counted below, page by page, not as one metadata object.
        let is_metadata = |name: &str| {
            name != "texts"
                && name != "strings"
                && name != "docrecords"
                && chunk_id(name).map_or(true, |chunk| needed.contains(&chunk))
        };
        let mut pages: Vec<u32> = hits
            .iter()
            .filter_map(|hit| index.doc_page_of(hit.doc_id))
            .collect();
        pages.sort_unstable();
        pages.dedup();
        let page_bytes: usize = pages
            .iter()
            .filter_map(|page| index.doc_page_span(*page))
            .map(|(_, len)| len as usize)
            .sum();
        let metadata: usize = set
            .parts
            .iter()
            .filter(|part| is_metadata(&part.name))
            .map(|part| part.len)
            .sum();
        let strings: usize = hits
            .iter()
            .map(|hit| {
                index
                    .string_span(hit.doc_id)
                    .map_or(0, |span| span.len() as usize)
            })
            .sum();
        let texts: usize = hits
            .iter()
            .map(|hit| {
                index
                    .text_span(hit.doc_id)
                    .map_or(0, |(_, len)| len as usize)
            })
            .sum();
        let mut requests = 1
            + set
                .parts
                .iter()
                .filter(|part| is_metadata(&part.name))
                .count()
            + pages.len();
        for hit in &hits {
            // One range for the text block, one for the URL+title (two when a
            // builder wrote them apart, which `rustsearch build` never does).
            requests += 1;
            requests += match index.string_span(hit.doc_id) {
                Some(span) if span.is_adjacent() => 1,
                Some(_) => 2,
                None => 0,
            };
        }
        let bytes = set.manifest.len() + metadata + page_bytes + strings + texts;
        println!(
            "plan {query:?}: {} hit(s) -> {} bytes in {} range requests ({:.1}% of the artifact)",
            hits.len(),
            bytes,
            requests,
            100.0 * bytes as f64 / total.max(1) as f64
        );
        println!(
            "  manifest {} · metadata {} · record pages {} ({} of {}) · strings {} · text blocks {}",
            set.manifest.len(),
            metadata,
            page_bytes,
            pages.len(),
            index.doc_page_count(),
            strings,
            texts
        );
        if index.is_chunked() {
            println!(
                "  chunks {} of {} fetched ({} bytes), the rest of the dictionary is never read",
                needed.len(),
                index.chunk_count(),
                needed
                    .iter()
                    .filter_map(|chunk| index.chunk_span(*chunk))
                    .map(|(_, len)| len as usize)
                    .sum::<usize>()
            );
        }
    }
    Ok(())
}

/// Check an artifact (or a shard manifest) against a pinned digest and, when asked,
/// a detached minisign signature.
///
/// With no `--expect-digest` the sidecar `<file>.sha256` written by `build` or
/// `shard` is used, so `rustsearch verify dist/index.rsx` answers "is this the
/// artifact this directory says it is". `--require-signature` verifies the
/// signature over the same bytes with `--public-key`; a signature that does not
/// match is an error, never a warning.
fn verify(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut path: Option<PathBuf> = None;
    let mut expect: Option<String> = None;
    let mut signature: Option<PathBuf> = None;
    let mut public_key: Option<PathBuf> = None;
    let mut index = 0usize;
    while index < args.len() {
        match args[index].as_str() {
            "--expect-digest" => {
                index += 1;
                expect = Some(
                    args.get(index)
                        .ok_or("--expect-digest needs sha256:HEX")?
                        .to_ascii_lowercase(),
                );
            }
            "--require-signature" => {
                index += 1;
                signature = Some(
                    args.get(index)
                        .ok_or("--require-signature needs a signature file")?
                        .into(),
                );
            }
            "--public-key" => {
                index += 1;
                public_key = Some(
                    args.get(index)
                        .ok_or("--public-key needs a key file")?
                        .into(),
                );
            }
            value if value.starts_with('-') => return Err(format!("unknown flag: {value}").into()),
            value => {
                if path.is_some() {
                    return Err("verify takes one file".into());
                }
                path = Some(value.into());
            }
        }
        index += 1;
    }
    let path = path.ok_or("verify needs a file")?;
    let bytes = fs::read(&path)?;
    let digest = rustsearch_core::sha256_hex(&bytes);
    println!("file:   {}", path.display());
    println!("sha256: {digest}");

    let pinned = match expect {
        Some(value) => Some(value),
        None => fs::read_to_string(sidecar_path(&path))
            .ok()
            .map(|text| text.trim().to_ascii_lowercase()),
    };
    if let Some(pinned) = pinned {
        let pinned = pinned
            .strip_prefix("sha256:")
            .unwrap_or(&pinned)
            .trim()
            .to_string();
        if pinned != digest {
            return Err(format!("digest mismatch: pinned {pinned}, computed {digest}").into());
        }
        println!("digest: ok");
    }
    if let Some(signature) = signature {
        let key_path = public_key.ok_or("--require-signature needs --public-key")?;
        let key = minisign_verify::PublicKey::from_base64(&base64_payload(&key_path)?)?;
        let sig = minisign_verify::Signature::from_file(&signature)?;
        key.verify(&bytes, &sig, false)?;
        println!("signature: ok");
    }
    Ok(())
}

/// `<file>.sha256`: the sidecar `build` and `shard` write and `verify` reads.
fn sidecar_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(".sha256");
    PathBuf::from(name)
}

/// The base64 payload of a minisign key or signature file, skipping the comments.
fn base64_payload(path: &Path) -> Result<String, Box<dyn std::error::Error>> {
    let text = fs::read_to_string(path)?;
    text.lines()
        .map(str::trim)
        .find(|line| line.len() > 40 && line.starts_with("RW"))
        .map(str::to_string)
        .ok_or_else(|| format!("{} holds no minisign base64 line", path.display()).into())
}

/// Render a character that must never reach a terminal as a visible escape.
///
/// The build strips the control, bidi and zero-width set (`is_unsafe_char`), so
/// this is the second layer, for an artifact built before that change: stdout is
/// the one sink a browser's `textContent` guidance does not cover.
fn escape_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        let code = ch as u32;
        let unsafe_char = ch.is_control()
            || matches!(
                code,
                0x200b..=0x200f | 0x202a..=0x202e | 0x2060..=0x2064 | 0x2066..=0x2069 | 0xfeff
            );
        if unsafe_char {
            out.push_str(&format!("\\u{{{code:04x}}}"));
        } else {
            out.push(ch);
        }
    }
    out
}

fn load(path: &Path) -> Result<Index, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    Ok(Index::from_bytes(bytes)?)
}

fn bench(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut documents = 2_000usize;
    let mut words = 120usize;
    let mut queries = 200usize;
    let mut out: Option<PathBuf> = None;
    // The title weight is a query knob: `--title-boost 0` measures the body-only
    // path, which is what the same query cost before the title field existed.
    let mut options = SearchOptions::default();
    let mut term_chunk_bytes = 0usize;
    let mut index = 0usize;
    while index < args.len() {
        if args[index] == "--group" {
            options.group_by_url = true;
            index += 1;
            continue;
        }
        let value = args.get(index + 1).ok_or("bench flag needs a value")?;
        match args[index].as_str() {
            "--docs" => documents = value.parse()?,
            "--words" => words = value.parse()?,
            "--queries" => queries = value.parse()?,
            "--out" => out = Some(value.into()),
            "--term-chunk-bytes" => term_chunk_bytes = byte_budget(value)?,
            "--title-boost" | "--boost" => {
                let boost: f32 = value.parse().map_err(|_| "invalid --title-boost")?;
                if !boost.is_finite() || !(0.0..=MAX_TITLE_BOOST).contains(&boost) {
                    return Err(
                        format!("--title-boost must be between 0 and {MAX_TITLE_BOOST}").into(),
                    );
                }
                options.title_boost = boost;
            }
            other => return Err(format!("unknown flag: {other}").into()),
        }
        index += 2;
    }
    let dir = env::temp_dir().join(format!("rustsearch-bench-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;
    let vocabulary: Vec<String> = (0..2_000).map(|n| format!("term{n:04}")).collect();
    for doc in 0..documents {
        let mut body = String::with_capacity(words * 9);
        for word in 0..words {
            let pick = (doc * 31 + word * 17) % vocabulary.len();
            body.push_str(&vocabulary[pick]);
            body.push(' ');
        }
        let html = format!(
            "<html><head><title>Document {doc}</title></head><body><main><h1>Document {doc}</h1><p>{body}</p></main></body></html>"
        );
        let subdir = dir.join(format!("section{:02}", doc % 20));
        fs::create_dir_all(&subdir)?;
        fs::write(subdir.join(format!("page{doc:05}.html")), html)?;
    }

    let build_started = Instant::now();
    let build = build_site(
        &dir,
        &BuildOptions {
            term_chunk_bytes,
            ..BuildOptions::default()
        },
    )?;
    let build_elapsed = build_started.elapsed();
    let index = Index::from_bytes(build.bytes.clone())?;
    if index.is_chunked() {
        println!(
            "term chunks:   {} ({} bytes each on average)",
            index.chunk_count(),
            build.stats.bytes / index.chunk_count().max(1) as usize
        );
    }
    let mut durations: Vec<Duration> = Vec::with_capacity(queries);
    let mut hits = 0usize;
    for query in 0..queries {
        let term = &vocabulary[(query * 37) % vocabulary.len()];
        let started = Instant::now();
        hits += index.search(term, options).len();
        durations.push(started.elapsed());
    }
    durations.sort_unstable();

    println!("documents:     {documents}");
    println!("words/doc:     {words}");
    println!(
        "index bytes:   {} ({:.1} KiB)",
        build.stats.bytes,
        build.stats.bytes as f64 / 1024.0
    );
    println!(
        "build:         {:.0} ms",
        build_elapsed.as_secs_f64() * 1000.0
    );
    println!(
        "build RSS:     {}",
        peak_rss_kib().map_or("n/a".into(), |kib| format!("{kib} KiB"))
    );
    println!("queries:       {queries} ({} hits)", hits);
    println!("query p50:     {:.3} ms", percentile(&durations, 50));
    println!("query p95:     {:.3} ms", percentile(&durations, 95));
    println!("query max:     {:.3} ms", percentile(&durations, 100));
    if let Some(out) = out {
        fs::write(&out, &build.bytes)?;
        println!("artifact:      {}", out.display());
    }
    let _ = fs::remove_dir_all(&dir);
    Ok(())
}

fn percentile(sorted: &[Duration], percentile: usize) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) * percentile / 100).min(sorted.len() - 1);
    sorted[index].as_secs_f64() * 1000.0
}

fn peak_rss_kib() -> Option<u64> {
    let status = fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(value) = line.strip_prefix("VmHWM:") {
            return value.split_whitespace().next()?.parse().ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::escape_controls;

    #[test]
    fn the_cli_sink_never_prints_a_control_character() {
        // H2, second layer: an artifact built before the extraction filter can still
        // carry these, and the terminal is not protected by the browser guidance.
        assert_eq!(escape_controls("a\u{1b}]52;c;ZWJj"), "a\\u{001b}]52;c;ZWJj");
        assert_eq!(escape_controls("b\u{202e}d"), "b\\u{202e}d");
        assert_eq!("plain text", escape_controls("plain text"));
    }
}
