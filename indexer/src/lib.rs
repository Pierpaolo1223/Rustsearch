//! Build-time indexing: walk a static site, extract its readable text, and write
//! the immutable `.rsx` artifact.
//!
//! This crate is native-only (it reads files and parses HTML with `scraper`). The
//! runtime in `rustsearch-core` has no such dependency, which is what keeps the
//! WASM module small.
//!
//! The builder is deterministic: paths are visited in sorted order, doc ids follow
//! that order, and the dictionary is byte-sorted. Building the same site twice
//! produces byte-identical output, so the artifact can be content-addressed. The page
//! phase runs on several threads, which changes nothing about that: pages are
//! independent, each worker keeps its own term cache, and the results are merged back
//! in page order.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod cache;
pub mod shard;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

use ego_tree::NodeRef;
use rustsearch_core::analyze::Analyzer;
use rustsearch_core::facets::{
    MAX_FACET_FIELDS, MAX_FACET_NAME_BYTES, MAX_FACET_VALUES, MAX_FACET_VALUE_BYTES,
};
use rustsearch_core::format::{
    fnv1a64, CHUNK_ENTRY, DOC_RECORD, DOC_RECORD_V5, FLAG_COMPACT_DOC_TOKENS,
    FLAG_COMPRESSED_TEXTS, FLAG_HAS_FACETS, FLAG_HAS_POSITIONS, FLAG_HAS_TITLE_POSTINGS,
    HEADER_LEN, HEADER_LEN_V5, MAGIC, SPANS, SPANS_V5, TERM_FIXED, VERSION, VERSION_CHUNKED,
};
use rustsearch_core::postings::{write_list, write_list_positions};
use rustsearch_core::rules::Synonyms;
use rustsearch_core::title::{MAX_TITLE_TERMS, TITLE_ENTRY};
use scraper::node::Node;
use scraper::Html;
use walkdir::WalkDir;

/// What to call the site root in the emitted URLs.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// `https://example.com` (or a subpath). Empty means root-relative URLs.
    pub base_url: String,
    /// The language analyzer. It is stored in the artifact, so a query is stemmed
    /// with the same tables as the pages.
    pub analyzer: Analyzer,
    /// Split a page longer than this many bytes into several documents that share
    /// its URL. `0` (the default) keeps one document per page.
    ///
    /// A query can then group the results back to one hit per page
    /// (`SearchOptions::group_by_url`), which is what a site with long pages wants:
    /// ranking at paragraph granularity, presenting at page granularity.
    pub max_page_chars: usize,
    /// Group the dictionary and its postings into independently fetchable chunks of
    /// at most this many bytes each, and write format v5 instead of v4. `0` (the
    /// default) writes the flat v4 dictionary byte for byte.
    ///
    /// A chunk holds a run of terms and the postings they point at, so one term's
    /// name and its posting list travel together and a `Range` client fetches only
    /// the chunks its query touches. Anything below [`MIN_TERM_CHUNK_BYTES`] is
    /// rejected by [`build_site`]: a chunk has to be worth a request.
    pub term_chunk_bytes: usize,
    /// Refuse a page larger than this many bytes, checked with `metadata()` before
    /// the file is read (H4). `0` disables the bound. The default is
    /// [`DEFAULT_MAX_PAGE_BYTES`]: the same 8 MiB the runtime caps a document's text
    /// at, so a page the builder accepts is a page the artifact can hold.
    pub max_page_bytes: usize,
    /// Stop admitting pages once their on-disk sizes reach this many bytes. `0`
    /// disables the budget. The admission pass runs in walk order on the caller's
    /// thread, so which pages are dropped never depends on how many workers the
    /// build has — two builds of one site stay byte-identical.
    pub max_total_bytes: usize,
    /// Record the token ordinal of every body posting, so a phrase query can require
    /// adjacency (F6, `build --positions`). `false` (the default) writes exactly the
    /// bytes a build wrote before: no flag, no stream, no per-document cost. `true`
    /// raises the artifact by the encoded ordinals and costs a little build time;
    /// only a query that names a phrase reads them.
    pub positions: bool,
    /// Extend the checksum over the header, writing format v7 (v8 when
    /// `term_chunk_bytes` is set) instead of v4/v6. The bytes do not move: the version
    /// word changes and the checksum covers the header too, so a flipped flags word or
    /// a shifted span is refused instead of answered.
    ///
    /// `true` by default: an artifact is only as trustworthy as the flags and spans that
    /// describe it. A deploy whose clients cannot be rebuilt yet sets it to `false`
    /// (`build --no-header-checksum`) and gets the v4/v6 bytes those clients already read.
    pub header_checksum: bool,
}

/// The default per-page ceiling (H4): 8 MiB, aligned with the runtime's
/// `MAX_DOC_TEXT_BYTES`. An HTML file above it is skipped and reported, never read.
pub const DEFAULT_MAX_PAGE_BYTES: usize = 8 * 1024 * 1024;

/// How many skip reasons a build keeps: enough to see the pattern in the summary,
/// not so many that a corpus of oversize files fills the terminal.
const MAX_SKIP_NOTES: usize = 50;

/// How deep the text walk descends. A page nested deeper than this is almost always
/// generated (or hostile), and the recursion is ours to bound, not the parser's.
const MAX_DOM_DEPTH: usize = 512;

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            base_url: String::new(),
            analyzer: Analyzer::None,
            max_page_chars: 0,
            term_chunk_bytes: 0,
            max_page_bytes: DEFAULT_MAX_PAGE_BYTES,
            max_total_bytes: 0,
            positions: false,
            header_checksum: true,
        }
    }
}

/// The smallest term-chunk budget a build accepts. A chunk carries at least a
/// directory entry and a request, so a four-term chunk is not a trade worth making.
pub const MIN_TERM_CHUNK_BYTES: usize = 1024;

/// Counts worth printing after a build.
#[derive(Debug, Clone, Default)]
pub struct BuildStats {
    pub documents: usize,
    pub terms: usize,
    /// Body tokens. Title tokens are counted separately because they exist twice in
    /// the artifact: once in the dictionary, once in the title postings.
    pub tokens: u64,
    pub title_tokens: u64,
    pub bytes: usize,
    /// Pages that contributed no document or were refused by a bound.
    pub skipped: usize,
    /// The subset of `skipped` a byte bound refused (H4), so a summary can say
    /// "skipped for size" without re-deriving it from the notes.
    pub oversize: usize,
    /// Why a page was refused, one line per page, at most [`MAX_SKIP_NOTES`].
    pub skipped_notes: Vec<String>,
    /// Pages whose analysis came from the cache (`build --cache`), so a summary can
    /// say how much of the build was skipped instead of repeating it.
    pub pages_reused: usize,
}

/// The artifact plus its stats.
pub struct Build {
    pub bytes: Vec<u8>,
    pub stats: BuildStats,
}

#[derive(Debug)]
struct Doc {
    url: String,
    title: String,
    text: String,
    /// Body tokens: the dictionary and the body postings. Each element is a handle to
    /// the one allocation of that stem (see [`TermCache`]), not a copy of it. The
    /// handle is an `Arc` rather than an `Rc` because a worker hands its documents to
    /// the thread that serializes them.
    tokens: Vec<Arc<str>>,
    /// Title tokens: the same dictionary, a field of their own for BM25F.
    title_tokens: Vec<Arc<str>>,
    /// One token ordinal per body token (F6), parallel to `tokens`. Empty when the
    /// build did not ask for positions.
    positions: Vec<u32>,
    hash: u64,
    /// `(field, value)` pairs: `lang`, `section`, one per tag. The writer turns
    /// them into the facet section's doc lists.
    facets: Vec<(String, String)>,
}

#[derive(Default)]
struct Postings {
    docs: Vec<u32>,
    tfs: Vec<u32>,
    /// One run of ordinals per document, parallel to `docs`; empty unless the build
    /// records positions (F6).
    positions: Vec<Vec<u32>>,
}

/// Crawl `root` and build the index.
pub fn build_site(root: &Path, options: &BuildOptions) -> Result<Build, Box<dyn Error>> {
    build_site_with_rules(root, options, None, None)
}

/// [`build_site`] with the build cache in `cache_dir` (`build --cache`).
pub fn build_site_with_cache(
    root: &Path,
    options: &BuildOptions,
    cache_dir: Option<&Path>,
) -> Result<Build, Box<dyn Error>> {
    build_site_with_rules(root, options, cache_dir, None)
}

/// The full builder: cache, and the build-time synonym table (`build --synonyms`).
///
/// The artifact is a function of the site, the options and the rules alone. The cache
/// short-circuits the whole build when no page changed; a cache from another
/// fingerprint — options **or** synonyms — is ignored. Synonyms are expanded into the
/// postings, so the artifact answers for them without any query-time rule.
pub fn build_site_with_rules(
    root: &Path,
    options: &BuildOptions,
    cache_dir: Option<&Path>,
    synonyms: Option<&Synonyms>,
) -> Result<Build, Box<dyn Error>> {
    if synonyms.is_some_and(|synonyms| !synonyms.is_empty()) && options.positions {
        return Err(
            "--synonyms cannot be combined with --positions: a synonym posting has \
                    no ordinal, and a phrase must match the words the page actually has"
                .into(),
        );
    }
    if !root.is_dir() {
        return Err(format!("not a directory: {}", root.display()).into());
    }
    // H9: the base is the one piece of the artifact that names another origin, so it
    // is checked once, before any page is read.
    validate_base_url(&options.base_url).map_err(|error| -> Box<dyn Error> { error.into() })?;
    let (pages, non_html) = html_pages(root)?;
    let mut skipped = non_html;
    let mut oversize = 0usize;
    let mut skipped_notes: Vec<String> = Vec::new();
    // H4: admission is decided here, sequentially, from `metadata()` alone. The
    // workers then see a fixed list, so the artifact does not depend on scheduling —
    // the same property the page phase already has.
    let mut admitted: Vec<PathBuf> = Vec::with_capacity(pages.len());
    let mut total_bytes = 0usize;
    for path in pages {
        let size = fs::metadata(&path)?.len();
        if options.max_page_bytes > 0 && size > options.max_page_bytes as u64 {
            skipped += 1;
            oversize += 1;
            if skipped_notes.len() < MAX_SKIP_NOTES {
                skipped_notes.push(format!(
                    "{}: {size} bytes over the {} byte page limit",
                    path.display(),
                    options.max_page_bytes
                ));
            }
            continue;
        }
        if options.max_total_bytes > 0
            && total_bytes.saturating_add(size as usize) > options.max_total_bytes
        {
            skipped += 1;
            oversize += 1;
            if skipped_notes.len() < MAX_SKIP_NOTES {
                skipped_notes.push(format!(
                    "{}: the {} byte build budget is spent",
                    path.display(),
                    options.max_total_bytes
                ));
            }
            continue;
        }
        total_bytes += size as usize;
        admitted.push(path);
    }
    let fingerprint = cache_fingerprint(options, synonyms);
    let loaded = cache_dir.map(|dir| cache::BuildCache::load(dir, fingerprint));
    // Pass 1 hashes every admitted page: read, FNV, no parsing. It is what makes a
    // no-change rebuild cheap — when every hash matches and the artifact was kept,
    // the bytes already on disk are the answer and nothing is re-serialized.
    let hashes = page_hashes(root, &admitted)?;
    if let Some(analysis) = &loaded {
        if analysis.covers(&hashes) {
            if let Some(bytes) = analysis.artifact() {
                let cached = analysis.stats();
                return Ok(Build {
                    bytes: bytes.to_vec(),
                    stats: BuildStats {
                        documents: cached.documents,
                        terms: cached.terms,
                        tokens: cached.tokens,
                        title_tokens: cached.title_tokens,
                        bytes: bytes.len(),
                        pages_reused: hashes.len(),
                        ..BuildStats::default()
                    },
                });
            }
        }
    }
    // The worker count reaches the artifact through nothing else than the order these
    // come back in, which `index_pages` fixes at page order.
    let mut docs: Vec<Doc> = Vec::with_capacity(admitted.len());
    for (_, page) in index_pages(&admitted, root, options) {
        match page {
            Ok(page_docs) => {
                if page_docs.is_empty() {
                    skipped += 1;
                } else {
                    docs.extend(page_docs);
                }
            }
            // The first unreadable page in walk order is the one a sequential build
            // would have stopped at.
            Err(error) => return Err(error.into()),
        }
    }
    if docs.is_empty() {
        return Err(format!(
            "no HTML with readable text under {} (skipped {skipped})",
            root.display()
        )
        .into());
    }

    let token_count: u64 = docs.iter().map(|doc| doc.tokens.len() as u64).sum();
    let title_token_count: u64 = docs.iter().map(|doc| doc.title_tokens.len() as u64).sum();
    let mut postings: HashMap<Arc<str>, Postings> = HashMap::new();
    let mut title_postings: HashMap<Arc<str>, Postings> = HashMap::new();
    // The build-time synonym table, stemmed with the artifact's analyzer: a rule
    // written as `deploy => publishing` has to meet the terms a query produces.
    let synonyms = synonym_map(synonyms, options.analyzer);
    for (doc_id, doc) in docs.iter().enumerate() {
        let doc_id = doc_id as u32;
        for (ordinal, token) in doc.tokens.iter().enumerate() {
            // The recorded ordinal when the build asked for positions (it counts
            // stopwords, so a phrase keeps its gaps); the token index otherwise,
            // which only keeps the call uniform.
            let position = doc
                .positions
                .get(ordinal)
                .copied()
                .unwrap_or(u32::try_from(ordinal).unwrap_or(u32::MAX));
            post(&mut postings, token, doc_id, position, options.positions);
        }
        for token in &doc.title_tokens {
            post(&mut title_postings, token, doc_id, 0, false);
        }
        if !synonyms.is_empty() {
            // One posting per distinct source term per document: a synonym says "this
            // page is about that word", not "it repeats it", so the tf it adds is 1
            // and BM25 does not treat a synonym as emphasis.
            // Look the token up first and deduplicate only the ones that matched: a
            // document that shares no term with the rules pays one miss per token
            // instead of a set insert per token — on 10k pages the insert-first shape
            // cost 286 ms of a 1.3 s build, most of it hashing words no rule names.
            let mut seen: HashSet<&str> = HashSet::new();
            for token in &doc.tokens {
                let Some(targets) = synonyms.get(token.as_ref()) else {
                    continue;
                };
                if seen.insert(token.as_ref()) {
                    for target in targets {
                        post(&mut postings, target, doc_id, 0, false);
                    }
                }
            }
            let mut seen_title: HashSet<&str> = HashSet::new();
            for token in &doc.title_tokens {
                let Some(targets) = synonyms.get(token.as_ref()) else {
                    continue;
                };
                if seen_title.insert(token.as_ref()) {
                    for target in targets {
                        post(&mut title_postings, target, doc_id, 0, false);
                    }
                }
            }
        }
    }

    if options.term_chunk_bytes != 0 && options.term_chunk_bytes < MIN_TERM_CHUNK_BYTES {
        return Err(format!(
            "a term-chunk budget of {} bytes is below the {MIN_TERM_CHUNK_BYTES}-byte floor",
            options.term_chunk_bytes
        )
        .into());
    }
    let mut bytes = serialize(
        &docs,
        postings,
        title_postings,
        token_count,
        title_token_count,
        options.analyzer,
        options.positions,
        options.term_chunk_bytes,
    )?;
    if options.header_checksum {
        seal_header(&mut bytes)?;
    }
    let stats = BuildStats {
        documents: docs.len(),
        terms: count_terms(&bytes),
        tokens: token_count,
        title_tokens: title_token_count,
        bytes: bytes.len(),
        skipped,
        oversize,
        skipped_notes,
        pages_reused: 0,
    };
    if let Some(dir) = cache_dir {
        // The whole site changed as far as this design is concerned: record the new
        // hashes and the new artifact. Merging the untouched pages is the chunk-level
        // rewrite with stable document ids that the roadmap lists as the next step.
        let mut updated = loaded.unwrap_or_else(|| cache::BuildCache::new(fingerprint));
        updated.record(&hashes, &bytes, &stats);
        if let Err(error) = updated.save(dir) {
            // A cache is an optimization: failing to write it must never fail a build
            // that produced a correct artifact.
            eprintln!("rustsearch: build cache not written: {error}");
        }
    }
    Ok(Build { bytes, stats })
}

/// The cache key of a build: everything that changes what a page's analysis means.
///
/// Two builds with the same fingerprint produce the same documents from the same
/// bytes, so an entry written by one is valid for the other. The artifact format is
/// in here through `positions`; a future option that changes analysis must be added
/// or the cache would serve documents from the old policy.
fn cache_fingerprint(options: &BuildOptions, synonyms: Option<&Synonyms>) -> u64 {
    let mut seed = String::from("rustsearch-build-cache-3\0");
    seed.push_str(options.analyzer.name());
    seed.push('\0');
    seed.push_str(&options.base_url);
    seed.push('\0');
    seed.push_str(&format!(
        "{}:{}:{}:{}:{}",
        options.max_page_chars,
        options.term_chunk_bytes,
        options.positions,
        options.max_page_bytes,
        options.max_total_bytes
    ));
    // The rules change the postings, so they change the artifact: a cache built
    // without them must not answer a build with them.
    if let Some(synonyms) = synonyms {
        seed.push('\0');
        for (source, targets) in synonyms.entries() {
            seed.push_str(source);
            seed.push('=');
            for target in targets {
                seed.push_str(target);
                seed.push(',');
            }
            seed.push(';');
        }
    }
    fnv1a64(seed.as_bytes())
}

/// The synonym table in dictionary terms: `stem -> stems`, sorted out by construction
/// because [`Synonyms::entries`] is.
fn synonym_map(
    synonyms: Option<&Synonyms>,
    analyzer: Analyzer,
) -> HashMap<Arc<str>, Vec<Arc<str>>> {
    let mut map: HashMap<Arc<str>, Vec<Arc<str>>> = HashMap::new();
    let Some(synonyms) = synonyms else {
        return map;
    };
    for (source, targets) in synonyms.entries() {
        if analyzer.is_stopword(source) {
            continue;
        }
        let source: Arc<str> = Arc::from(analyzer.stem(source).as_str());
        let list = map.entry(source).or_default();
        for target in targets {
            if analyzer.is_stopword(target) {
                continue;
            }
            let target: Arc<str> = Arc::from(analyzer.stem(target).as_str());
            if !list
                .iter()
                .any(|existing| existing.as_ref() == target.as_ref())
            {
                list.push(target);
            }
        }
    }
    map
}

/// `(relative path, content hash)` for every admitted page, in walk order.
///
/// The path is stored with `/` separators so a cache written on one platform is a
/// cache miss on another (rather than a wrong hit), and the hash is of the bytes the
/// indexer would read, not of the metadata.
fn page_hashes(root: &Path, pages: &[PathBuf]) -> Result<Vec<(String, u64)>, Box<dyn Error>> {
    let mut out = Vec::with_capacity(pages.len());
    for path in pages {
        let raw = fs::read(path)?;
        let relative = path.strip_prefix(root)?;
        out.push((relative.to_string_lossy().replace('\\', "/"), fnv1a64(&raw)));
    }
    Ok(out)
}

/// Every HTML file under `root`, in the walk's deterministic order, with the number of
/// entries that were skipped for not being HTML.
fn html_pages(root: &Path) -> Result<(Vec<PathBuf>, usize), Box<dyn Error>> {
    let mut pages = Vec::new();
    let mut skipped = 0usize;
    for entry in WalkDir::new(root)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|entry| !is_hidden(entry.path(), root))
    {
        let entry = entry?;
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let is_html = path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.eq_ignore_ascii_case("html") || ext.eq_ignore_ascii_case("htm"))
            .unwrap_or(false);
        if is_html {
            pages.push(path.to_path_buf());
        } else {
            skipped += 1;
        }
    }
    Ok((pages, skipped))
}

/// Read, extract and analyze every page, in page order, and return each page's
/// documents next to the index of the page they came from.
///
/// Nothing in a page's analysis depends on another page, so this — the phase the
/// profile puts two thirds of the build in — is the one that spreads over the cores. A
/// worker keeps its own [`TermCache`], so the only state the threads share is the work
/// counter, and the results are put back in page order whatever order they finished
/// in: the doc ids the caller assigns, the dictionary built from them and the artifact
/// written from that are all the single-threaded ones.
fn index_pages(
    pages: &[PathBuf],
    root: &Path,
    options: &BuildOptions,
) -> Vec<(usize, Result<Vec<Doc>, String>)> {
    index_pages_with(pages, root, options, worker_count(pages.len()))
}

/// [`index_pages`] with the worker count passed in: how wide the phase runs is decided
/// in one place — [`worker_count`] reads the core count — and a test can compare the
/// one-worker merge against the many-worker one.
fn index_pages_with(
    pages: &[PathBuf],
    root: &Path,
    options: &BuildOptions,
    workers: usize,
) -> Vec<(usize, Result<Vec<Doc>, String>)> {
    let mut indexed = Vec::with_capacity(pages.len());
    if workers <= 1 {
        let mut cache = TermCache::default();
        for (index, path) in pages.iter().enumerate() {
            indexed.push((index, index_page(path, root, options, &mut cache)));
        }
        return indexed;
    }
    // One counter over the whole list rather than a slice per worker: pages differ in
    // size by more than an order of magnitude, and a worker that drew a run of long
    // ones should not hold up the merge.
    let cursor = AtomicUsize::new(0);
    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(workers);
        for _ in 0..workers {
            handles.push(scope.spawn(|| {
                let mut cache = TermCache::default();
                let mut done = Vec::new();
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = pages.get(index) else {
                        break;
                    };
                    done.push((index, index_page(path, root, options, &mut cache)));
                }
                done
            }));
        }
        for handle in handles {
            match handle.join() {
                Ok(page) => indexed.extend(page),
                // A panic in a worker is the build's panic: let it through.
                Err(panic) => std::panic::resume_unwind(panic),
            }
        }
    });
    // Page order, whatever order the workers finished in.
    indexed.sort_unstable_by_key(|(index, _)| *index);
    indexed
}

/// How many workers the page phase gets: one per core, and never more workers than
/// there are pages to read.
fn worker_count(pages: usize) -> usize {
    let cores = thread::available_parallelism()
        .map(|cores| cores.get())
        .unwrap_or(1);
    cores.min(pages).max(1)
}

/// One page: read it, extract its readable text, and analyze every chunk of it.
///
/// An empty vector means the page contributed no document — no readable text, or text
/// that is entirely markup, punctuation or stopwords — which counts as skipped. The
/// error is a message rather than a trait object because a worker hands it back across
/// a thread boundary.
fn index_page(
    path: &Path,
    root: &Path,
    options: &BuildOptions,
    cache: &mut TermCache,
) -> Result<Vec<Doc>, String> {
    // H4: `metadata()` before the read. `build_site` already refused an oversize
    // page, so this is the second lock on the same door — the one `index_page` keeps
    // for a caller that reaches it directly.
    if options.max_page_bytes > 0 {
        let size = std::fs::metadata(path)
            .map_err(|error| format!("{}: {error}", path.display()))?
            .len();
        if size > options.max_page_bytes as u64 {
            return Err(format!(
                "{}: {size} bytes over the {} byte page limit",
                path.display(),
                options.max_page_bytes
            ));
        }
    }
    let raw = fs::read(path).map_err(|error| error.to_string())?;
    let html = String::from_utf8_lossy(&raw);
    let relative = path.strip_prefix(root).map_err(|error| error.to_string())?;
    let (title, text, facets) = extract_page_with_facets(&html, relative);
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let url = document_url(relative, &options.base_url).map_err(|error| error.to_string())?;
    let title = if title.is_empty() {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Untitled")
            .to_string()
    } else {
        title
    };
    // The title is indexed too, in its own field: the same analyzer (so a stemmed query
    // matches a stemmed title) and the same dictionary, so a word that is only ever a
    // title is still an exact hit.
    let title_tokens = cache.analyze(options.analyzer, &title);
    // A long page becomes several documents that share its URL, its title and its
    // facets; `strings` writes that pair once, so the chunks also share one `url_off`,
    // which is how a query recognises them as one page.
    // The chunks are analyzed once each: a page whose every chunk is punctuation,
    // markup or stopwords produces no document at all, and is counted as skipped, which
    // is what a pre-check of the whole text used to do — for the price of analyzing
    // every page twice.
    let mut docs = Vec::new();
    for chunk in page_chunks(&text, options.max_page_chars) {
        let (tokens, positions) = if options.positions {
            cache.analyze_positional(options.analyzer, chunk)
        } else {
            (cache.analyze(options.analyzer, chunk), Vec::new())
        };
        if tokens.is_empty() {
            continue;
        }
        docs.push(Doc {
            url: url.clone(),
            title: title.clone(),
            text: chunk.to_string(),
            tokens,
            title_tokens: title_tokens.clone(),
            positions,
            hash: fnv1a64(chunk.as_bytes()),
            facets: facets.clone(),
        });
    }
    Ok(docs)
}

/// The build's term arena: one interned copy of every distinct stem, keyed by the
/// spelling it came from.
///
/// The same word occurs many times across a corpus — 1,547,496 body tokens for the
/// 29,535 distinct spellings of the 10k corpus — and both the Snowball stemmer and the
/// allocation behind it cost the same every time. Each spelling is analyzed once
/// (stopword check included) and stored as an `Arc<str>`; an occurrence then clones the
/// handle, which is a refcount bump, not a copy. The postings maps key on the same
/// handle, so a token that reaches the dictionary is not copied again there either.
///
/// The arena lives for one build, one language, with no eviction, and the artifact
/// cannot tell that it was there: the stem of a word does not depend on how often it
/// appears. A parallel build has one cache per worker, so a spelling is interned once
/// per worker — a few hundred bytes against the copies the handle replaces.
#[derive(Default)]
struct TermCache {
    /// spelling -> its stem, or `None` when the spelling is a stopword.
    stems: HashMap<String, Option<Arc<str>>>,
}

impl TermCache {
    /// `Analyzer::analyze_positional`, with the same per-spelling cache: the terms are
    /// the ones [`TermCache::analyze`] returns and `positions` their ordinal in the
    /// whole token stream (stopwords included), which is what keeps a phrase's gaps.
    fn analyze_positional(&mut self, analyzer: Analyzer, text: &str) -> (Vec<Arc<str>>, Vec<u32>) {
        let mut out = Vec::new();
        let mut positions = Vec::new();
        for (ordinal, token) in rustsearch_core::tokenize(text).into_iter().enumerate() {
            let known = match self.stems.get(token.as_ref()) {
                Some(Some(stem)) => {
                    out.push(Arc::clone(stem));
                    positions.push(u32::try_from(ordinal).unwrap_or(u32::MAX));
                    true
                }
                Some(None) => true,
                None => false,
            };
            if known {
                continue;
            }
            let term = (!analyzer.is_stopword(&token)).then(|| Arc::from(analyzer.stem(&token)));
            if let Some(stem) = &term {
                out.push(Arc::clone(stem));
                positions.push(u32::try_from(ordinal).unwrap_or(u32::MAX));
            }
            self.stems.insert(token.into_owned(), term);
        }
        (out, positions)
    }

    /// `Analyzer::analyze`, with the work done once per distinct spelling.
    fn analyze(&mut self, analyzer: Analyzer, text: &str) -> Vec<Arc<str>> {
        let mut out = Vec::new();
        for token in rustsearch_core::tokenize(text) {
            let known = match self.stems.get(token.as_ref()) {
                Some(Some(stem)) => {
                    out.push(Arc::clone(stem));
                    true
                }
                Some(None) => true,
                None => false,
            };
            if known {
                continue;
            }
            let term = (!analyzer.is_stopword(&token)).then(|| Arc::from(analyzer.stem(&token)));
            if let Some(stem) = &term {
                out.push(Arc::clone(stem));
            }
            // The spelling is only copied when it is new: 29,535 times, not 1.2M.
            self.stems.insert(token.into_owned(), term);
        }
        out
    }
}

/// Add one occurrence of `token` in `doc_id`, keeping the list sorted and its term
/// frequency in step with the document.
///
/// The key is the interned handle itself, so this adds a term to the map without
/// copying its text; the dictionary is built from those handles later, once per term.
fn post(
    postings: &mut HashMap<Arc<str>, Postings>,
    token: &Arc<str>,
    doc_id: u32,
    position: u32,
    with_positions: bool,
) {
    let entry = postings.entry(Arc::clone(token)).or_default();
    if entry.docs.last() == Some(&doc_id) {
        if let Some(tf) = entry.tfs.last_mut() {
            *tf += 1;
        }
        if with_positions {
            if let Some(run) = entry.positions.last_mut() {
                run.push(position);
            }
        }
    } else {
        entry.docs.push(doc_id);
        entry.tfs.push(1);
        if with_positions {
            entry.positions.push(vec![position]);
        }
    }
}

/// The text of one page, split into documents of at most `max` bytes each.
///
/// `0`, or a page that already fits, is one chunk. Otherwise the split happens after
/// a space inside the window, so no word is cut and no UTF-8 sequence is split; a
/// page with no space at all (a CJK run, a base64 blob) is cut on the last char
/// boundary. Chunks partition the text: every token lands in exactly one of them.
fn page_chunks(text: &str, max: usize) -> Vec<&str> {
    if max == 0 || text.len() <= max {
        return vec![text];
    }
    let mut chunks: Vec<&str> = Vec::with_capacity(text.len() / max + 1);
    let mut rest = text;
    while rest.len() > max {
        let window = &rest.as_bytes()[..max];
        let cut = match window.iter().rposition(|byte| *byte == b' ') {
            Some(at) => at + 1,
            None => {
                let mut end = max;
                while end > 0 && !rest.is_char_boundary(end) {
                    end -= 1;
                }
                end.max(1)
            }
        };
        let (chunk, tail) = rest.split_at(cut);
        let chunk = chunk.trim_end();
        if !chunk.is_empty() {
            chunks.push(chunk);
        }
        rest = tail.trim_start();
    }
    if !rest.is_empty() {
        chunks.push(rest);
    }
    chunks
}

fn is_hidden(path: &Path, root: &Path) -> bool {
    if path == root {
        return false;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.starts_with('.'))
        .unwrap_or(false)
}

/// Characters a page must never be able to put into the artifact: C0/C1 controls,
/// DEL, the bidi overrides and isolates, and the zero-width set.
///
/// A page is third-party input. `ESC ] 52 ; c ; …` is a clipboard write, `ESC [ 2 J`
/// clears a terminal, and U+202E (right-to-left override) reverses the title printed
/// above a hit — none of that has a legitimate place in extracted text, and the
/// browser's `textContent` guidance does not protect a terminal at all.
fn is_unsafe_char(ch: char) -> bool {
    matches!(
        ch as u32,
        0x00..=0x1f
            | 0x7f..=0x9f
            | 0x200b..=0x200f
            | 0x202a..=0x202e
            | 0x2060..=0x2064
            | 0x2066..=0x2069
            | 0xfeff
    )
}

fn normalize_whitespace(text: &str) -> String {
    // One pass into one buffer. The `split_whitespace().collect::<Vec<_>>().join(" ")`
    // this replaced built a 1.7M-entry vector of `&str` per build (27 MB of pointers
    // for the reference corpus) to produce the string it was going to write anyway.
    //
    // The same pass drops the control and bidi set: they are not whitespace, so
    // `split_whitespace` would keep them inside a word, and a page that emits them
    // would travel into the artifact and out of every terminal that prints a hit.
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        if is_unsafe_char(ch) {
            continue;
        }
        if ch.is_whitespace() {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        out.push(ch);
    }
    out
}

/// The selectors the extractor needs, parsed once per process.
///
/// `Selector::parse` costs about 2 µs and the extractor asks for ten selectors per
/// page, which is 20 ms of a 216 ms extraction on the 10k corpus. They are immutable
/// constants of the extraction rules, not of the page, so a process-wide cache is
/// the right shape.
mod selectors {
    use scraper::Selector;
    use std::sync::OnceLock;

    // The patterns are literals in this module; a parse failure is a build-time bug,
    // not input. The lint is allowed here and nowhere else in the crate.
    #[allow(clippy::expect_used)]
    fn cached(slot: &'static OnceLock<Selector>, pattern: &str) -> &'static Selector {
        slot.get_or_init(|| Selector::parse(pattern).expect("static selector parses"))
    }

    macro_rules! selector {
        ($name:ident, $pattern:literal) => {
            pub fn $name() -> &'static Selector {
                static SLOT: OnceLock<Selector> = OnceLock::new();
                cached(&SLOT, $pattern)
            }
        };
    }

    selector!(title, "title");
    selector!(h1, "h1");
    selector!(main, "main");
    selector!(article, "article");
    selector!(role_main, "[role=main]");
    selector!(id_content, "#content");
    selector!(class_content, ".content");
    selector!(body, "body");
    selector!(meta, "meta");
    selector!(html, "html");
}

/// The readable text of one page: `<main>` (or the closest fallback), traversed
/// while skipping the navigation and script subtrees.
pub fn extract_page(html: &str) -> (String, String) {
    let document = Html::parse_document(html);
    extract_page_from(&document)
}

/// The page text plus the taxonomies the v4 facet section records.
///
/// One parse serves both, so a build pays for the document once.
pub fn extract_page_with_facets(
    html: &str,
    relative: &Path,
) -> (String, String, Vec<(String, String)>) {
    let document = Html::parse_document(html);
    let (title, text) = extract_page_from(&document);
    (title, text, page_facets(&document, relative))
}

/// `lang`, `section` and one `tags` value per keyword, in that order.
///
/// * `lang` is `<html lang>`; a page without one has no `lang` value.
/// * `section` is `<meta name="section">` when the site sets it, otherwise the
///   first directory of the page's path (`/guide/setup.html` → `guide`).
/// * `tags` come from `<meta name="keywords">`, split on commas.
///
/// Values are opaque to the engine: they are trimmed and whitespace-collapsed here
/// and stored byte for byte. A value longer than the format's cap is dropped rather
/// than truncated, so a filter can never match half a value.
fn page_facets(document: &Html, relative: &Path) -> Vec<(String, String)> {
    let mut facets: Vec<(String, String)> = Vec::new();
    if let Some(lang) = html_lang(document).or_else(|| meta_content(document, "lang")) {
        facets.push(("lang".to_string(), lang.to_ascii_lowercase()));
    }
    if let Some(section) = meta_content(document, "section").or_else(|| path_section(relative)) {
        facets.push(("section".to_string(), section));
    }
    if let Some(keywords) = meta_content(document, "keywords") {
        for tag in keywords.split(',') {
            let tag = facet_value(tag);
            if tag.is_empty() {
                continue;
            }
            let tag = tag.to_string();
            if !facets
                .iter()
                .any(|(field, value)| field == "tags" && *value == tag)
            {
                facets.push(("tags".to_string(), tag));
            }
        }
    }
    // A field/value pair has to fit the format; `facets_section` prunes whole
    // values once a field has more than `MAX_FACET_VALUES`, but a name that is too
    // long or empty is dropped here, where the reason is visible.
    facets.retain(|(field, value)| {
        !field.is_empty()
            && field.len() <= MAX_FACET_NAME_BYTES
            && value.len() <= MAX_FACET_VALUE_BYTES
    });
    facets.dedup();
    facets
}

/// Trim a value. Anything inside it is the site's bytes, kept as they are.
fn facet_value(value: &str) -> &str {
    value.trim()
}

fn meta_content(document: &Html, name: &str) -> Option<String> {
    document
        .select(selectors::meta())
        .find(|element| {
            element
                .value()
                .attr("name")
                .is_some_and(|value| value.eq_ignore_ascii_case(name))
        })
        .and_then(|element| element.value().attr("content"))
        .map(normalize_whitespace)
        .filter(|value| !value.is_empty())
}

fn html_lang(document: &Html) -> Option<String> {
    let lang = document
        .select(selectors::html())
        .next()?
        .value()
        .attr("lang")?;
    let lang = lang.trim().to_ascii_lowercase();
    (!lang.is_empty()).then_some(lang)
}

/// `/guide/setup.html` → `guide`; `/index.html` and a flat `page.html` → none.
///
/// Only a directory names a section: taking the file name would make every page of
/// a flat site its own value, which is a value per document, not a taxonomy.
fn path_section(relative: &Path) -> Option<String> {
    let mut components = relative.components();
    let Component::Normal(first) = components.next()? else {
        return None;
    };
    components.next()?;
    let section = facet_value(&first.to_string_lossy()).to_string();
    (!section.is_empty()).then_some(section)
}

fn extract_page_from(document: &Html) -> (String, String) {
    let title = [selectors::title(), selectors::h1()]
        .iter()
        .find_map(|selector| {
            let node = document.select(selector).next()?;
            let text: String = node.text().collect::<Vec<_>>().join(" ");
            let text = normalize_whitespace(&text);
            (!text.is_empty()).then_some(text)
        })
        .unwrap_or_default();
    let root = [
        selectors::main(),
        selectors::article(),
        selectors::role_main(),
        selectors::id_content(),
        selectors::class_content(),
        selectors::body(),
    ]
    .iter()
    .find_map(|selector| document.select(selector).next());
    let mut raw = String::new();
    if let Some(root) = root {
        collect_text(&root, &mut raw);
    }
    (title, normalize_whitespace(&raw))
}

fn is_noise(name: &str) -> bool {
    matches!(
        name,
        "script"
            | "style"
            | "noscript"
            | "nav"
            | "header"
            | "footer"
            | "aside"
            | "form"
            | "svg"
            | "template"
            | "iframe"
            | "button"
    )
}

/// Append the text of `node`, skipping the navigation/script furniture entirely.
/// Traversing the `ego_tree` is cheaper than serialising and reparsing the page.
fn collect_text(node: &NodeRef<Node>, out: &mut String) {
    collect_text_at(node, out, 0);
}

/// The recursive half of [`collect_text`], with the depth bound H4 asks for: a tree
/// nested past [`MAX_DOM_DEPTH`] is truncated instead of overflowing the stack. The
/// text below the cut is lost, which is the right failure for a page that deep.
fn collect_text_at(node: &NodeRef<Node>, out: &mut String, depth: usize) {
    if depth >= MAX_DOM_DEPTH {
        return;
    }
    match node.value() {
        Node::Text(text) => {
            out.push_str(text);
            out.push(' ');
        }
        Node::Element(element) => {
            if is_noise(element.name()) {
                return;
            }
            for child in node.children() {
                collect_text_at(&child, out, depth + 1);
            }
        }
        _ => {
            for child in node.children() {
                collect_text_at(&child, out, depth + 1);
            }
        }
    }
}

/// Validate a `--base-url` value (H9).
///
/// The base is the only place the artifact names another origin, so it is held to the
/// two schemes a static site can serve, `http` and `https`, or to a relative path
/// resolved against the page that loads the index (the playground builds `demo/site`).
/// Credentials, a query, a fragment, control characters, a protocol-relative prefix and
/// any path segment that is `.`, `..` or a percent-encoded dot are refused, because each
/// one turns a document path into something other than a path on the site the base names.
/// An empty base is valid and means root-relative URLs.
pub fn validate_base_url(base_url: &str) -> Result<(), String> {
    let base = base_url.trim_end_matches('/');
    if base.is_empty() {
        return Ok(());
    }
    if base.len() > 2048 {
        return Err(format!("--base-url is longer than 2048 bytes: {base_url}"));
    }
    if base.chars().any(|ch| ch.is_control() || ch.is_whitespace()) {
        return Err(format!(
            "--base-url must not contain whitespace or control characters: {base_url}"
        ));
    }
    if base.contains('?') || base.contains('#') {
        return Err(format!(
            "--base-url must not carry a query or a fragment: {base_url}"
        ));
    }
    if base.contains('\\') {
        return Err(format!("--base-url must not use backslashes: {base_url}"));
    }
    let lower = base.to_ascii_lowercase();
    let (authority, path) = match lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
    {
        Some(rest) => match rest.find('/') {
            Some(at) => (Some(&rest[..at]), &rest[at..]),
            None => (Some(rest), ""),
        },
        None => {
            // A relative base is a path, so it must look like one: at least one `/` and
            // no scheme. A bare host (`example.com`) is ambiguous, `//host` names another
            // origin, and `javascript:`/`data:` are schemes whatever follows them.
            if base.starts_with("//") {
                return Err(format!(
                    "--base-url must not be protocol-relative: {base_url}"
                ));
            }
            let first_slash = base.find('/');
            if let Some(colon) = base.find(':') {
                if first_slash.map_or(true, |slash| colon < slash) {
                    return Err(format!(
                        "--base-url must be an http or https URL or a relative path: {base_url}"
                    ));
                }
            }
            if first_slash.is_none() {
                return Err(format!(
                    "--base-url must be an http or https URL or a relative path: {base_url}"
                ));
            }
            (None, base)
        }
    };
    if let Some(authority) = authority {
        if authority.is_empty() {
            return Err(format!("--base-url has no host: {base_url}"));
        }
        if authority.contains('@') {
            return Err(format!("--base-url must not carry credentials: {base_url}"));
        }
    }
    for segment in path.split('/') {
        let segment = segment.to_ascii_lowercase();
        if segment == "." || segment == ".." || segment.contains("%2e") {
            return Err(format!(
                "--base-url must not contain dot segments: {base_url}"
            ));
        }
    }
    Ok(())
}

fn document_url(relative: &Path, base_url: &str) -> Result<String, Box<dyn Error>> {
    let mut parts: Vec<String> = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                parts.push(percent_encode(&part.to_string_lossy()));
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "refusing a path outside the site root: {}",
                    relative.display()
                )
                .into())
            }
        }
    }
    let mut path = parts.join("/");
    if path == "index.html" || path == "index.htm" {
        path.clear();
    } else if let Some(stripped) = path.strip_suffix("/index.html") {
        path.truncate(stripped.len() + 1);
    } else if let Some(stripped) = path.strip_suffix("/index.htm") {
        path.truncate(stripped.len() + 1);
    }
    let base = base_url.trim_end_matches('/');
    let url = if base.is_empty() {
        format!("/{path}")
    } else if path.is_empty() {
        format!("{base}/")
    } else {
        format!("{base}/{path}")
    };
    // The last check before the URL reaches an artifact: a root-relative path, an
    // http(s) URL, or a relative path built from a relative base (`--base-url demo/site`
    // gives `demo/site/page.html`), never `//host` and never a scheme the renderer might
    // execute. The components above cannot produce a scheme; a regression that could
    // would stop here instead of at a reader's browser.
    let readable = (url.starts_with('/') && !url.starts_with("//"))
        || url.starts_with("http://")
        || url.starts_with("https://")
        || (!url.starts_with("//") && !url.contains(':') && url.contains('/'));
    if !readable {
        return Err(format!("refusing an unsafe document URL: {url}").into());
    }
    Ok(url)
}

fn percent_encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for byte in segment.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(*byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The span indices of a v5 header, in the order `format::Header` reads them.
const CHUNKS_SPAN: usize = 0;
const CHUNKDATA_SPAN: usize = 1;
const TITLEDATA_SPAN: usize = 2;
const DOC_TOKENS_SPAN: usize = 3;
const DOC_PAGE_INDEX_SPAN: usize = 4;
const DOCPAGES_SPAN: usize = 5;
const STRINGS_SPAN: usize = 6;
const TEXTS_SPAN: usize = 7;
const HASHES_SPAN: usize = 8;
const STRHASHES_SPAN: usize = 9;
const FACETS_SPAN: usize = 10;

#[allow(clippy::too_many_arguments)]
fn serialize(
    docs: &[Doc],
    postings: HashMap<Arc<str>, Postings>,
    title_postings: HashMap<Arc<str>, Postings>,
    token_count: u64,
    title_token_count: u64,
    analyzer: Analyzer,
    positions: bool,
    term_chunk_bytes: usize,
) -> Result<Vec<u8>, Box<dyn Error>> {
    // One dictionary for both fields: a term that only occurs in titles must still
    // be findable by an exact lookup, so it gets an entry with a body df of 0 and no
    // body postings. `BTreeMap` iterates in byte order, which is exactly the order
    // the term table requires and the same order the old sort produced.
    let mut dictionary: BTreeMap<String, (Postings, Postings)> = BTreeMap::new();
    for (term, postings) in postings {
        dictionary.entry(term.as_ref().to_string()).or_default().0 = postings;
    }
    for (term, postings) in title_postings {
        dictionary.entry(term.as_ref().to_string()).or_default().1 = postings;
    }
    let terms: Vec<(String, (Postings, Postings))> = dictionary.into_iter().collect();

    let mut postings_bytes: Vec<u8> = Vec::new();
    let mut title_bytes: Vec<u8> = Vec::new();
    let mut term_meta: Vec<(u32, u32, u32)> = Vec::with_capacity(terms.len());
    // `(title_df, postings_off, postings_len)` per term, for the ones a title has.
    let mut title_meta: Vec<(u32, u32, u32)> = Vec::new();
    for ((_, (body, title)), term_index) in terms.iter().zip(0u32..) {
        let offset = postings_bytes.len() as u32;
        if positions {
            write_list_positions(&body.docs, &body.tfs, &body.positions, &mut postings_bytes)
                .map_err(|error| format!("cannot encode postings: {error}"))?;
        } else {
            write_list(&body.docs, &body.tfs, &mut postings_bytes)
                .map_err(|error| format!("cannot encode postings: {error}"))?;
        }
        term_meta.push((
            body.docs.len() as u32,
            offset,
            (postings_bytes.len() as u32) - offset,
        ));
        if title.docs.is_empty() {
            continue;
        }
        let offset = title_bytes.len() as u32;
        write_list(&title.docs, &title.tfs, &mut title_bytes)
            .map_err(|error| format!("cannot encode title postings: {error}"))?;
        title_meta.push((
            term_index,
            title.docs.len() as u32,
            (title_bytes.len() as u32) - offset,
        ));
    }

    let chunked = term_chunk_bytes > 0;

    let mut strings: Vec<u8> = Vec::new();
    let mut texts: Vec<u8> = Vec::new();
    let mut hashes: Vec<u8> = Vec::with_capacity(docs.len() * 8);
    let mut strings_hashes: Vec<u8> = Vec::with_capacity(docs.len() * 8);
    let mut records: Vec<u8> = Vec::with_capacity(4 + docs.len() * DOC_RECORD);
    // A v5 build splits the same information in two: the BM25 lengths, which ranking
    // needs for every hit, and the 32-byte records, which rendering needs only for the
    // handful of documents it shows.
    let mut doc_tokens: Vec<u8> = Vec::with_capacity(4 + docs.len() * 2 * 4);
    let mut page_records: Vec<u8> = Vec::with_capacity(docs.len() * DOC_RECORD_V5);
    // A compact table halves the 8 bytes per document when every length fits in a
    // `u16`; one longer document falls back to the wide table for the whole artifact,
    // so the ranking reads the exact value either way.
    let compact_tokens = chunked
        && docs.iter().all(|doc| {
            doc.tokens.len() <= u16::MAX as usize && doc.title_tokens.len() <= u16::MAX as usize
        });
    push_u32(&mut records, docs.len() as u32);
    push_u32(&mut doc_tokens, docs.len() as u32);
    // One copy of each `url ++ title` pair. A page split into several documents gets
    // the same offsets in every record, which is what makes `url_off` a page identity
    // a query can group on without reading `strings` at all.
    let mut shared: HashMap<(&str, &str), (u32, u32)> = HashMap::new();
    for doc in docs {
        // The URL and the title are written back to back, so one `Range` covers
        // both and one hash authenticates that read on its own.
        let key = (doc.url.as_str(), doc.title.as_str());
        let (url_off, title_off) = match shared.get(&key) {
            Some(offsets) => *offsets,
            None => {
                let url_off = strings.len() as u32;
                strings.extend_from_slice(doc.url.as_bytes());
                let title_off = strings.len() as u32;
                strings.extend_from_slice(doc.title.as_bytes());
                shared.insert(key, (url_off, title_off));
                (url_off, title_off)
            }
        };
        let string_end = title_off as usize + doc.title.len();
        strings_hashes
            .extend_from_slice(&fnv1a64(&strings[url_off as usize..string_end]).to_le_bytes());
        let text_off = texts.len() as u32;
        let block = encode_text(&doc.text)?;
        // One hash per document, so a client can verify a single `Range` read.
        hashes.extend_from_slice(&fnv1a64(&block).to_le_bytes());
        texts.extend_from_slice(&block);
        push_u32(&mut records, url_off);
        push_u32(&mut records, doc.url.len() as u32);
        push_u32(&mut records, title_off);
        push_u32(&mut records, doc.title.len() as u32);
        push_u32(&mut records, text_off);
        push_u32(&mut records, block.len() as u32);
        if chunked {
            if compact_tokens {
                doc_tokens.extend_from_slice(&(doc.tokens.len() as u16).to_le_bytes());
                doc_tokens.extend_from_slice(&(doc.title_tokens.len() as u16).to_le_bytes());
            } else {
                push_u32(&mut doc_tokens, doc.tokens.len() as u32);
                push_u32(&mut doc_tokens, doc.title_tokens.len() as u32);
            }
            push_u32(&mut page_records, url_off);
            push_u32(&mut page_records, doc.url.len() as u32);
            push_u32(&mut page_records, title_off);
            push_u32(&mut page_records, doc.title.len() as u32);
            push_u32(&mut page_records, text_off);
            push_u32(&mut page_records, block.len() as u32);
            page_records.extend_from_slice(&doc.hash.to_le_bytes());
        } else {
            push_u32(&mut records, doc.tokens.len() as u32);
            push_u32(&mut records, doc.title_tokens.len() as u32);
            records.extend_from_slice(&doc.hash.to_le_bytes());
        }
    }

    let facets_section = facets_section(docs)?;
    let mut flags = analyzer.id() | FLAG_COMPRESSED_TEXTS;
    if !facets_section.is_empty() {
        flags |= FLAG_HAS_FACETS;
    }
    if compact_tokens {
        flags |= FLAG_COMPACT_DOC_TOKENS;
    }
    if positions {
        flags |= FLAG_HAS_POSITIONS;
    }

    // The sections as `(span, bytes)`, in the order they are written to the body. A
    // flat build keeps the v4 order exactly, so `--term-chunk-bytes 0` still writes
    // the bytes v4 always wrote. A chunked one puts everything a cold query needs
    // first — directory, length table, facets, both hash tables, the title chunks — so
    // a shard set can publish that run as one object (`meta.bin`), then the paged
    // records, then the chunks, the strings and the texts.
    let (version, header_len, spans, sections): (u32, usize, usize, Vec<(usize, Vec<u8>)>) =
        if chunked {
            let dictionary = term_chunks(
                &terms,
                &term_meta,
                &postings_bytes,
                &title_meta,
                &title_bytes,
                term_chunk_bytes,
            )?;
            if !dictionary.titles.is_empty() {
                flags |= FLAG_HAS_TITLE_POSTINGS;
            }
            // One page per document a client is likely to render, capped by the chunk
            // budget: the same knob, because both trade a bigger object for a smaller
            // read. The page table is metadata and stays with the rest of it; the
            // records are what a client pages through.
            let per_page = (term_chunk_bytes / DOC_RECORD_V5).max(1);
            let pages = docs.len().div_ceil(per_page);
            let mut doc_page_index: Vec<u8> = Vec::with_capacity(8 + pages * 8);
            push_u32(&mut doc_page_index, docs.len() as u32);
            push_u32(&mut doc_page_index, per_page as u32);
            for page in 0..pages {
                let start = page * per_page * DOC_RECORD_V5;
                let end = (start + per_page * DOC_RECORD_V5).min(page_records.len());
                doc_page_index.extend_from_slice(&fnv1a64(&page_records[start..end]).to_le_bytes());
            }
            (
                VERSION_CHUNKED,
                HEADER_LEN_V5,
                SPANS_V5,
                vec![
                    (CHUNKS_SPAN, dictionary.directory),
                    (DOC_TOKENS_SPAN, doc_tokens),
                    (FACETS_SPAN, facets_section),
                    (HASHES_SPAN, hashes),
                    (STRHASHES_SPAN, strings_hashes),
                    (TITLEDATA_SPAN, dictionary.titles),
                    (DOC_PAGE_INDEX_SPAN, doc_page_index),
                    (DOCPAGES_SPAN, page_records),
                    (CHUNKDATA_SPAN, dictionary.chunks),
                    (STRINGS_SPAN, strings),
                    (TEXTS_SPAN, texts),
                ],
            )
        } else {
            let table_len = 4 + terms.len() * 4;
            let mut entries: Vec<u8> = Vec::new();
            let mut entry_offsets: Vec<u32> = Vec::with_capacity(terms.len());
            let mut next = table_len;
            for ((term, _), (df, offset, length)) in terms.iter().zip(&term_meta) {
                entry_offsets.push(next as u32);
                push_u32(&mut entries, *df);
                push_u32(&mut entries, *offset);
                push_u32(&mut entries, *length);
                let term_bytes = term.as_bytes();
                entries.extend_from_slice(&(term_bytes.len() as u16).to_le_bytes());
                entries.extend_from_slice(term_bytes);
                next += 14 + term_bytes.len();
            }
            let mut terms_section = Vec::with_capacity(next);
            push_u32(&mut terms_section, terms.len() as u32);
            for offset in entry_offsets {
                push_u32(&mut terms_section, offset);
            }
            terms_section.extend_from_slice(&entries);
            let title_section = title_section(&title_meta, &title_bytes)?;
            if !title_section.is_empty() {
                flags |= FLAG_HAS_TITLE_POSTINGS;
            }
            (
                VERSION,
                HEADER_LEN,
                SPANS,
                vec![
                    (0, terms_section),
                    (1, postings_bytes),
                    (2, records),
                    (3, strings),
                    (4, texts),
                    (5, hashes),
                    (6, strings_hashes),
                    (7, facets_section),
                    (8, title_section),
                ],
            )
        };

    let mut body: Vec<u8> = Vec::new();
    let mut offsets: Vec<Option<(u32, u32)>> = vec![None; spans];
    for (span, data) in &sections {
        let offset = header_len + body.len();
        if offset > u32::MAX as usize || data.len() > u32::MAX as usize {
            return Err("index exceeds the 4 GiB format limit".into());
        }
        offsets[*span] = Some((offset as u32, data.len() as u32));
        body.extend_from_slice(data);
    }
    if header_len + body.len() > u32::MAX as usize {
        return Err("index exceeds the 4 GiB format limit".into());
    }

    let mut header = vec![0u8; header_len];
    header[0..8].copy_from_slice(&MAGIC);
    put_u32(&mut header, 8, version);
    put_u32(&mut header, 12, flags);
    put_u32(&mut header, 16, docs.len() as u32);
    put_u32(&mut header, 20, terms.len() as u32);
    header[24..32].copy_from_slice(&token_count.to_le_bytes());
    header[32..40].copy_from_slice(&title_token_count.to_le_bytes());
    let base_of_spans = header_len - 8 - spans * 8;
    for (index, slot) in offsets.iter().enumerate() {
        let (offset, length) = slot.ok_or("a header span has no section")?;
        let base = base_of_spans + index * 8;
        put_u32(&mut header, base, offset);
        put_u32(&mut header, base + 4, length);
    }
    let checksum = fnv1a64(&body);
    header[header_len - 8..header_len].copy_from_slice(&checksum.to_le_bytes());

    let mut out = Vec::with_capacity(header_len + body.len());
    out.extend_from_slice(&header);
    out.extend_from_slice(&body);
    Ok(out)
}

/// Rewrites a freshly built artifact so its checksum covers the header too.
///
/// The layout is byte for byte the one `serialize` wrote: only the version word moves
/// (v4 → v7, v6 → v8) and the checksum is recomputed over the header and the body.
/// Nothing else shifts, so a reader that knows the version reads the artifact exactly
/// as before — with the flags word and the span table now inside the check.
fn seal_header(bytes: &mut [u8]) -> Result<(), Box<dyn Error>> {
    use rustsearch_core::format::{
        artifact_checksum, header_layout, VERSION, VERSION_CHUNKED, VERSION_CHUNKED_HEADER_COVERED,
        VERSION_HEADER_COVERED,
    };
    let version = bytes
        .get(8..12)
        .map(|raw| u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
        .ok_or("artifact is shorter than its header")?;
    let sealed = match version {
        VERSION => VERSION_HEADER_COVERED,
        VERSION_CHUNKED => VERSION_CHUNKED_HEADER_COVERED,
        // Already covered, or a layout this writer does not produce.
        _ => return Err(format!("cannot seal a v{version} artifact").into()),
    };
    let (header_len, _, _, _, header_covered) =
        header_layout(sealed).ok_or("the sealed version has no layout")?;
    if !header_covered || bytes.len() < header_len {
        return Err("the sealed layout does not cover the header".into());
    }
    bytes[8..12].copy_from_slice(&sealed.to_le_bytes());
    let checksum = artifact_checksum(bytes, header_len, true);
    bytes[header_len - 8..header_len].copy_from_slice(&checksum.to_le_bytes());
    Ok(())
}

/// The optional `facets` section: one entry per field, one doc list per value.
///
/// The layout is documented in `rustsearch_core::facets` and mirrors the reader:
/// offset tables for fields and values, sorted by name, then delta-encoded doc
/// ids. An empty result means the site had no metadata and the section is omitted.
///
/// A field with more values than the format allows keeps the most frequent ones
/// (ties broken by name), so a site with hundreds of tags still gets a usable
/// taxonomy rather than a build error.
fn facets_section(docs: &[Doc]) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut fields: BTreeMap<String, BTreeMap<String, Vec<u32>>> = BTreeMap::new();
    for (doc_id, doc) in docs.iter().enumerate() {
        for (field, value) in &doc.facets {
            let values = fields.entry(field.clone()).or_default();
            let list = values.entry(value.clone()).or_default();
            // One entry per value per document, ids ascending by construction.
            if list.last() != Some(&(doc_id as u32)) {
                list.push(doc_id as u32);
            }
        }
    }
    if fields.is_empty() {
        return Ok(Vec::new());
    }
    if fields.len() > MAX_FACET_FIELDS {
        return Err(format!(
            "{} facet fields, the format allows {MAX_FACET_FIELDS}",
            fields.len()
        )
        .into());
    }
    for values in fields.values_mut() {
        if values.len() <= MAX_FACET_VALUES {
            continue;
        }
        let mut ranked: Vec<(&String, usize)> = values
            .iter()
            .map(|(value, docs)| (value, docs.len()))
            .collect();
        ranked.sort_unstable_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));
        let keep: std::collections::BTreeSet<String> = ranked
            .into_iter()
            .take(MAX_FACET_VALUES)
            .map(|(value, _)| value.clone())
            .collect();
        values.retain(|value, _| keep.contains(value));
    }

    let header_len = 2 + fields.len() * 4;
    let mut entries: Vec<u8> = Vec::new();
    let mut field_offsets: Vec<u32> = Vec::with_capacity(fields.len());
    for (name, values) in &fields {
        field_offsets.push((header_len + entries.len()) as u32);
        entries.extend_from_slice(&(name.len() as u16).to_le_bytes());
        entries.extend_from_slice(name.as_bytes());
        entries.extend_from_slice(&(values.len() as u16).to_le_bytes());
        let table = entries.len();
        entries.resize(table + values.len() * 4, 0);
        let mut value_offsets: Vec<u32> = Vec::with_capacity(values.len());
        for (value, docs) in values {
            value_offsets.push((header_len + entries.len()) as u32);
            entries.extend_from_slice(&(value.len() as u16).to_le_bytes());
            entries.extend_from_slice(value.as_bytes());
            entries.extend_from_slice(&(docs.len() as u32).to_le_bytes());
            let mut previous = 0u32;
            for (index, doc) in docs.iter().enumerate() {
                let delta = if index == 0 { *doc } else { doc - previous };
                rustsearch_core::varint::write_u32(&mut entries, delta);
                previous = *doc;
            }
        }
        for (index, offset) in value_offsets.iter().enumerate() {
            let at = table + index * 4;
            entries[at..at + 4].copy_from_slice(&offset.to_le_bytes());
        }
    }

    let mut out = Vec::with_capacity(header_len + entries.len());
    out.extend_from_slice(&(fields.len() as u16).to_le_bytes());
    for offset in field_offsets {
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(&entries);
    Ok(out)
}

/// The optional `titlepostings` section (F3): the title list of every term that
/// occurs in a title, indexed by dictionary term index.
///
/// `meta` is `(term_index, title_df, postings_len)` in dictionary order, which is
/// the order the reader's binary search and its boundary validation require. The
/// offsets are derived here from the running length instead of being carried along
/// in `serialize`, so the table and the blob can never disagree.
fn title_section(meta: &[(u32, u32, u32)], blobs: &[u8]) -> Result<Vec<u8>, Box<dyn Error>> {
    if meta.is_empty() {
        return Ok(Vec::new());
    }
    if meta.len() as u32 > MAX_TITLE_TERMS {
        return Err(format!("{} title entries, cap {MAX_TITLE_TERMS}", meta.len()).into());
    }
    let table = 4 + meta.len() * TITLE_ENTRY;
    let mut offset = table;
    let mut out: Vec<u8> = Vec::with_capacity(table + blobs.len());
    push_u32(&mut out, meta.len() as u32);
    for (term_index, df, len) in meta {
        if *df == 0 || *len == 0 {
            return Err("a title postings entry cannot be empty".into());
        }
        push_u32(&mut out, *term_index);
        push_u32(&mut out, *df);
        push_u32(&mut out, offset as u32);
        push_u32(&mut out, *len);
        offset = offset
            .checked_add(*len as usize)
            .ok_or("title postings exceed 4 GiB")?;
    }
    if offset - table != blobs.len() {
        return Err("title postings blobs do not match their entries".into());
    }
    out.extend_from_slice(blobs);
    Ok(out)
}

/// The v5 dictionary: a chunk directory plus the chunks themselves.
///
/// Each chunk is `u32 count, u32 offsets[count], entries, postings` — exactly the v4
/// `terms` section, with the slice of `postings` its entries point at appended — so a
/// term's name and its list are one `Range` read apart and a chunk never refers to
/// bytes outside itself. `budget` bounds a chunk's size; the loop only starts a new
/// chunk when the current one is already non-empty, so a single term whose postings
/// exceed the budget still gets a chunk of its own rather than an encoding error.
///
/// The three sections a chunked build derives from the dictionary.
struct ChunkedDictionary {
    /// `u32 count, entry[count], first-term keys` (see `format::Chunks`).
    directory: Vec<u8>,
    /// The chunks, back to back.
    chunks: Vec<u8>,
    /// The title list of each chunk that has one, back to back.
    titles: Vec<u8>,
}

/// Returns the directory, the chunks and their title lists: the sections a v5 header
/// calls `chunks`, `chunkdata` and `titledata`.
///
/// The title list of a chunk is the v4 `titlepostings` section restricted to that
/// chunk's terms, in the same layout, so a query that fetches a chunk's terms and
/// their title lists in one step pays for one round trip instead of two — and the
/// title bytes of a term travel with the term instead of with the whole dictionary.
fn term_chunks(
    terms: &[(String, (Postings, Postings))],
    meta: &[(u32, u32, u32)],
    postings: &[u8],
    title_meta: &[(u32, u32, u32)],
    title_blobs: &[u8],
    budget: usize,
) -> Result<ChunkedDictionary, Box<dyn Error>> {
    let mut chunks: Vec<u8> = Vec::new();
    let mut titles: Vec<u8> = Vec::new();
    let mut firsts: Vec<u32> = Vec::new();
    let mut counts: Vec<u32> = Vec::new();
    let mut offsets: Vec<u32> = Vec::new();
    let mut lengths: Vec<u32> = Vec::new();
    let mut title_offsets: Vec<u32> = Vec::new();
    let mut title_lengths: Vec<u32> = Vec::new();
    let mut title_hashes: Vec<u64> = Vec::new();
    let mut hashes: Vec<u64> = Vec::new();
    let mut keys: Vec<u8> = Vec::new();
    let mut key_offsets: Vec<u32> = Vec::new();
    let mut key_lengths: Vec<u16> = Vec::new();

    // `title_meta` is in term order like the chunks, so one cursor serves both: the
    // entries of a chunk are the ones whose term index falls inside it, and their
    // blobs are the next `title_blobs` bytes in the same order.
    let mut title_index = 0usize;
    let mut title_blob_off = 0usize;

    let mut index = 0usize;
    while index < terms.len() {
        let first = index as u32;
        let mut entries: Vec<u8> = Vec::new();
        let mut entry_offsets: Vec<u32> = Vec::new();
        let mut blobs: Vec<u8> = Vec::new();
        let mut count = 0usize;
        while index < terms.len() {
            let (term, _) = &terms[index];
            let (df, post_off, post_len) = meta[index];
            let entry_len = TERM_FIXED + term.len();
            let table_len = 4 + 4 * (count + 1);
            let projected = table_len + entries.len() + entry_len + blobs.len() + post_len as usize;
            if count > 0 && projected > budget {
                break;
            }
            entry_offsets.push(entries.len() as u32);
            push_u32(&mut entries, df);
            push_u32(&mut entries, blobs.len() as u32);
            push_u32(&mut entries, post_len);
            entries.extend_from_slice(&(term.len() as u16).to_le_bytes());
            entries.extend_from_slice(term.as_bytes());
            let start = post_off as usize;
            blobs.extend_from_slice(
                postings
                    .get(start..start + post_len as usize)
                    .ok_or("a postings offset points outside its own list")?,
            );
            count += 1;
            index += 1;
        }
        if count == 0 {
            return Err("a term chunk would be empty".into());
        }
        let table_len = 4 + 4 * count;
        // The entries were written with blob-relative postings offsets; the reader
        // slices the chunk itself, so rebase each one onto the chunk start.
        let blob_base = (table_len + entries.len()) as u32;
        for offset in &entry_offsets {
            let at = *offset as usize + 4;
            let current = u32::from_le_bytes(
                entries[at..at + 4]
                    .try_into()
                    .map_err(|_| "a term entry is shorter than its header")?,
            );
            entries[at..at + 4].copy_from_slice(&(blob_base + current).to_le_bytes());
        }
        let mut chunk: Vec<u8> = Vec::with_capacity(table_len + entries.len() + blobs.len());
        push_u32(&mut chunk, count as u32);
        for offset in &entry_offsets {
            push_u32(&mut chunk, table_len as u32 + *offset);
        }
        chunk.extend_from_slice(&entries);
        chunk.extend_from_slice(&blobs);

        let end_term = first + count as u32;
        let mut chunk_titles: Vec<(u32, u32, u32)> = Vec::new();
        let mut chunk_title_blobs: Vec<u8> = Vec::new();
        while title_index < title_meta.len() && title_meta[title_index].0 < end_term {
            let (term_index, df, len) = title_meta[title_index];
            chunk_titles.push((term_index, df, len));
            let start = title_blob_off;
            let end = start + len as usize;
            chunk_title_blobs.extend_from_slice(
                title_blobs
                    .get(start..end)
                    .ok_or("a title offset points outside the title section")?,
            );
            title_blob_off = end;
            title_index += 1;
        }
        let chunk_title = title_section(&chunk_titles, &chunk_title_blobs)?;

        let key = terms[first as usize].0.as_bytes();
        firsts.push(first);
        counts.push(count as u32);
        offsets.push(chunks.len() as u32);
        lengths.push(chunk.len() as u32);
        hashes.push(fnv1a64(&chunk));
        // A chunk with no title list stores 0/0 and no hash, which is how the reader
        // tells "no titles here" from "a title list of length zero".
        if chunk_title.is_empty() {
            title_offsets.push(0);
            title_lengths.push(0);
            title_hashes.push(0);
        } else {
            title_offsets.push(titles.len() as u32);
            title_lengths.push(chunk_title.len() as u32);
            title_hashes.push(fnv1a64(&chunk_title));
            titles.extend_from_slice(&chunk_title);
        }
        key_offsets.push(keys.len() as u32);
        key_lengths.push(key.len() as u16);
        keys.extend_from_slice(key);
        chunks.extend_from_slice(&chunk);
    }
    if title_index != title_meta.len() {
        return Err("a title entry names a term outside the dictionary".into());
    }

    let table_len = 4 + firsts.len() * CHUNK_ENTRY;
    let mut directory: Vec<u8> = Vec::with_capacity(table_len + keys.len());
    push_u32(&mut directory, firsts.len() as u32);
    for index in 0..firsts.len() {
        push_u32(&mut directory, firsts[index]);
        push_u32(&mut directory, counts[index]);
        push_u32(&mut directory, offsets[index]);
        push_u32(&mut directory, lengths[index]);
        push_u32(&mut directory, title_offsets[index]);
        push_u32(&mut directory, title_lengths[index]);
        push_u32(&mut directory, table_len as u32 + key_offsets[index]);
        directory.extend_from_slice(&key_lengths[index].to_le_bytes());
        directory.extend_from_slice(&0u16.to_le_bytes());
        directory.extend_from_slice(&hashes[index].to_le_bytes());
        directory.extend_from_slice(&title_hashes[index].to_le_bytes());
    }
    directory.extend_from_slice(&keys);
    Ok(ChunkedDictionary {
        directory,
        chunks,
        titles,
    })
}

/// A self-contained text block: `u32 raw_len` + LZ4 bytes, or `u32 raw_len |
/// 0x8000_0000` + raw bytes when compression would grow the block.
fn encode_text(text: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let raw = text.as_bytes();
    if raw.len() > 0x7fff_ffff {
        return Err("a document text exceeds the 2 GiB v3 block limit".into());
    }
    if raw.len() >= 32 {
        let compressed = lz4_flex::block::compress(raw);
        if compressed.len() < raw.len() {
            let mut out = Vec::with_capacity(compressed.len() + 4);
            push_u32(&mut out, raw.len() as u32);
            out.extend_from_slice(&compressed);
            return Ok(out);
        }
    }
    let mut out = Vec::with_capacity(raw.len() + 4);
    push_u32(&mut out, (raw.len() as u32) | 0x8000_0000);
    out.extend_from_slice(raw);
    Ok(out)
}

fn count_terms(bytes: &[u8]) -> usize {
    let count = u32::from_le_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    count as usize
}

fn push_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(out: &mut [u8], offset: usize, value: u32) {
    out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_never_reach_the_artifact() {
        // H2: an escape sequence, a CSI clear, a bidi override and a zero-width
        // space, in the title and in the body.
        let html = "<html><head><title>a\u{202e}b\u{200b}c</title></head><body><main>\
                    <p>safe\u{1b}]52;c;ZWJj then\u{9b}31m and done</p></main></body></html>";
        let (title, text) = extract_page(html);
        for value in [&title, &text] {
            assert!(
                !value.chars().any(is_unsafe_char),
                "a control or bidi character survived: {value:?}"
            );
        }
        assert_eq!(title, "abc");
        // The removed bytes glue their neighbours: the text keeps the printable
        // remainders and nothing else.
        assert_eq!(text, "safe]52;c;ZWJj then31m and done");
    }

    #[test]
    fn extracts_main_and_drops_navigation() {
        let html = r#"<html><head><title>Docs</title></head><body>
            <nav>menu</nav><main><h1>Title</h1><p>Hello <b>world</b>.</p></main>
            <footer>legal</footer><script>bad()</script></body></html>"#;
        let (title, text) = extract_page(html);
        assert_eq!(title, "Docs");
        assert_eq!(text, "Title Hello world .");
        assert!(!text.contains("menu"));
        assert!(!text.contains("bad()"));
    }

    #[test]
    fn urls_are_relative_percent_encoded_and_indexes_collapse() {
        assert_eq!(
            document_url(Path::new("guide/index.html"), "").unwrap(),
            "/guide/"
        );
        assert_eq!(
            document_url(Path::new("index.html"), "https://x.dev").unwrap(),
            "https://x.dev/"
        );
        assert_eq!(
            document_url(Path::new("a b.html"), "").unwrap(),
            "/a%20b.html"
        );
        // A relative base keeps the URL relative, which is how the playground links
        // its pages without naming an origin.
        assert_eq!(
            document_url(Path::new("curated-ranking.html"), "demo/site").unwrap(),
            "demo/site/curated-ranking.html"
        );
        assert!(document_url(Path::new("x.html"), "javascript:alert(1)").is_err());
        assert!(document_url(Path::new("x.html"), "//evil.example").is_err());
        assert!(document_url(Path::new("../escape.html"), "").is_err());
    }

    /// `(offset, length)` of section `index` in a v4 header, in artifact order:
    /// terms, postings, docs, strings, texts, hashes, strhashes, facets, titles.
    fn span(bytes: &[u8], index: usize) -> (usize, usize) {
        let base = HEADER_LEN - 8 - rustsearch_core::format::SPANS * 8;
        let at = base + index * 8;
        let read = |at: usize| u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        (read(at), read(at + 4))
    }

    fn tiny_index() -> Vec<u8> {
        let html = r#"<html><head><title>Walking</title></head><body><main>
            <p>Rust walks a directory tree with walkdir.</p></main></body></html>"#;
        let (title, text) = extract_page(html);
        let tokens: Vec<Arc<str>> = Analyzer::None
            .analyze(&text)
            .into_iter()
            .map(Arc::from)
            .collect();
        let mut postings: HashMap<Arc<str>, Postings> = HashMap::new();
        for (ordinal, token) in tokens.iter().enumerate() {
            post(&mut postings, token, 0, ordinal as u32, false);
        }
        let docs = vec![Doc {
            url: "/".into(),
            title,
            text,
            tokens: tokens.clone(),
            title_tokens: Vec::new(),
            positions: Vec::new(),
            hash: 0,
            facets: Vec::new(),
        }];
        serialize(
            &docs,
            postings,
            HashMap::new(),
            tokens.len() as u64,
            0,
            Analyzer::None,
            false,
            0,
        )
        .unwrap()
    }

    #[test]
    fn a_multi_page_site_builds_and_ranks() {
        use rustsearch_core::{Index, SearchOptions};
        let root = std::env::temp_dir().join(format!("rustsearch-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("a.html"),
            "<html><head><title>Walk</title></head><body><main><p>walkdir walks a tree</p></main></body></html>",
        )
        .unwrap();
        fs::write(
            root.join("b.html"),
            "<html><head><title>JSON</title></head><body><main><p>serde and serde_json write JSON</p></main></body></html>",
        )
        .unwrap();
        fs::write(
            root.join("c.html"),
            "<html><head><title>Runtime</title></head><body><main><p>tokio runs async tasks</p></main></body></html>",
        )
        .unwrap();
        let build = build_site(&root, &BuildOptions::default()).unwrap();
        let again = build_site(&root, &BuildOptions::default()).unwrap();
        assert_eq!(
            build.bytes, again.bytes,
            "the artifact must be deterministic"
        );
        let index = Index::from_bytes(build.bytes).unwrap();
        assert_eq!(index.doc_count(), 3);
        assert!(index.has_compressed_texts());
        let hits = index.search("walkdir", SearchOptions::default());
        assert_eq!(hits.first().map(|hit| hit.url.as_str()), Some("/a.html"));
        let json = index.search("JSON", SearchOptions::default());
        assert_eq!(json.first().map(|hit| hit.url.as_str()), Some("/b.html"));
        let prefix = index.search("to", SearchOptions::default());
        assert!(prefix.iter().any(|hit| hit.url == "/c.html"));
        let _ = fs::remove_dir_all(&root);
    }

    /// The documents of a page phase, reduced to something comparable.
    #[allow(clippy::type_complexity)]
    fn page_digest(
        indexed: Vec<(usize, Result<Vec<Doc>, String>)>,
    ) -> Vec<(usize, Vec<String>, Vec<String>, Vec<String>)> {
        indexed
            .into_iter()
            .map(|(index, page)| {
                let docs = page.expect("every page of the test site is readable");
                (
                    index,
                    docs.iter().map(|doc| doc.url.clone()).collect(),
                    docs.iter().map(|doc| doc.text.clone()).collect(),
                    docs.iter()
                        .flat_map(|doc| doc.tokens.iter().map(|token| token.to_string()))
                        .collect(),
                )
            })
            .collect()
    }

    /// The parallel page phase has to be the single-threaded one, whatever the number
    /// of workers and whatever order they finish in. The count is passed in so the test
    /// does not depend on how many cores the machine running it has.
    #[test]
    fn the_page_phase_is_the_same_on_one_worker_and_on_many() {
        let root = std::env::temp_dir().join(format!("rustsearch-jobs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // Written back to front and of growing sizes, so a merge that trusted the order
        // the workers finished in would produce different documents here: the page
        // names sort by path, not by the order they were created.
        for page in 0..24 {
            let body = format!(
                "<p>{} rustsearch walks a tree</p>",
                "word ".repeat(page + 1)
            );
            let title = format!("Page {page}");
            fs::write(
                root.join(format!("page-{:02}.html", 23 - page)),
                format!(
                    "<html><head><title>{title}</title></head><body><main>{body}</main></body></html>"
                ),
            )
            .unwrap();
        }
        let options = BuildOptions {
            analyzer: Analyzer::English,
            ..Default::default()
        };
        let (pages, skipped) = html_pages(&root).unwrap();
        assert_eq!((pages.len(), skipped), (24, 0));
        let one = page_digest(index_pages_with(&pages, &root, &options, 1));
        let many = page_digest(index_pages_with(&pages, &root, &options, 8));
        assert_eq!(one, many, "the worker count must not reach the artifact");
        assert_eq!(
            many.first().map(|page| page.1.as_slice()),
            Some(["/page-00.html".to_string()].as_slice()),
            "documents follow page order, not completion order"
        );
        assert_eq!(many.last().map(|page| page.0), Some(23));
        let _ = fs::remove_dir_all(&root);
    }

    /// The chunked dictionary has to answer every query shape exactly like the flat
    /// one — exact terms, a prefix range (which can cross a chunk boundary), a term
    /// that is in no chunk, and the fuzzy walk — and a sparse index given only the
    /// chunks the engine routes to has to rank the same documents.
    #[test]
    fn a_chunked_dictionary_answers_like_a_flat_one() {
        use rustsearch_core::{Index, SearchOptions};
        let root = std::env::temp_dir().join(format!("rustsearch-chunks-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // A zipped vocabulary: consecutive pages share stems and prefixes, so the
        // prefix range and the sorted dictionary meet chunk boundaries.
        let words = [
            "walkdir",
            "walking",
            "walker",
            "walkway",
            "wall",
            "wallet",
            "want",
            "ward",
            "warehouse",
            "warm",
            "warning",
            "warp",
            "warrant",
            "wash",
            "waste",
            "watch",
            "water",
            "wave",
            "way",
            "weak",
            "wealth",
            "weapon",
            "wear",
            "weather",
            "weave",
            "web",
            "wedge",
            "week",
            "weight",
            "weird",
            "welcome",
            "weld",
        ];
        for page in 0..40 {
            let mut body = String::new();
            for offset in 0..12 {
                body.push_str(&format!(
                    "<p>{} {} rustsearch crate</p>",
                    words[(page * 7 + offset) % words.len()],
                    words[(page * 13 + offset * 5) % words.len()]
                ));
            }
            fs::write(
                root.join(format!("page-{page:02}.html")),
                format!(
                    "<html><head><title>{} page {page}</title></head><body><main>{body}</main></body></html>",
                    words[page % words.len()]
                ),
            )
            .unwrap();
        }
        let flat = build_site(&root, &BuildOptions::default()).unwrap();
        let options = BuildOptions {
            term_chunk_bytes: 1024,
            ..Default::default()
        };
        let chunked = build_site(&root, &options).unwrap();
        assert_eq!(chunked.bytes, build_site(&root, &options).unwrap().bytes);
        let flat = Index::from_bytes(flat.bytes).unwrap();
        let whole = Index::from_bytes(chunked.bytes.clone()).unwrap();
        assert!(whole.is_chunked());
        assert!(whole.chunk_count() > 2, "{} chunks", whole.chunk_count());

        let ranked = |engine: &Index, query: &str, options: SearchOptions| -> Vec<(u32, String)> {
            engine
                .search(query, options)
                .into_iter()
                .map(|hit| (hit.doc_id, format!("{:.4}", hit.score)))
                .collect()
        };
        let queries = [
            "walkdir",
            "wa",
            "ware",
            "we",
            "crate",
            "rustsearch",
            "zz",
            "walkdix",
        ];
        for query in queries {
            for options in [
                SearchOptions::default(),
                SearchOptions {
                    prefix: false,
                    ..Default::default()
                },
                SearchOptions {
                    fuzzy: true,
                    ..Default::default()
                },
            ] {
                // F8: the typo pass needs the neighbour's postings, so it runs only
                // on a flat artifact; the chunked engine ignores the flag. "Chunked
                // answers like flat" therefore holds under the policy the sparse
                // client can actually run, which is `typos: false`. The feature's own
                // test (`tests/typo.rs`) covers what changes when it is on.
                let flat_options = SearchOptions {
                    typos: false,
                    ..options
                };
                assert_eq!(
                    ranked(&whole, query, options),
                    ranked(&flat, query, flat_options),
                    "query {query:?} with {options:?}"
                );
            }
        }

        // The same dictionary, with only the routed chunks attached, ranks the same.
        let layout = whole.layout();
        let mut sparse_bytes = chunked.bytes.clone();
        sparse_bytes[layout.chunk_data.clone()].fill(0);
        for query in ["walkdir", "wa", "crate"] {
            let mut sparse = Index::from_sparse(sparse_bytes.clone()).unwrap();
            assert_eq!(sparse.attached_chunks(), 0);
            let needed = sparse
                .chunks_for_query(query, SearchOptions::default())
                .unwrap();
            assert!(!needed.is_empty());
            for chunk in needed {
                let (off, len) = sparse.chunk_span(chunk).unwrap();
                let start = layout.chunk_data.start + off as usize;
                let bytes = chunked.bytes[start..start + len as usize].to_vec();
                sparse.attach_term_chunk(chunk, bytes).unwrap();
            }
            assert_eq!(
                ranked(&sparse, query, SearchOptions::default()),
                ranked(&flat, query, SearchOptions::default()),
                "a sparse chunked index must rank {query:?} like the whole artifact"
            );
        }

        // A prefix range that continues past its first chunk needs the next one: the
        // plan asks for it, and the scan then finds the same terms.
        let plan = whole
            .chunks_for_query("wa", SearchOptions::default())
            .unwrap();
        assert!(
            plan.len() > 1,
            "the plan must follow the prefix across chunks"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn text_blocks_compress_repetition_and_keep_a_raw_fallback() {
        let repetitive = "walkdir walks the tree, walkdir walks the tree, ".repeat(20);
        let block = encode_text(&repetitive).unwrap();
        assert!(
            block.len() < repetitive.len(),
            "LZ4 should shrink repetition"
        );
        let raw = encode_text("hi").unwrap();
        let word = u32::from_le_bytes(raw[0..4].try_into().unwrap());
        assert_ne!(word & 0x8000_0000, 0, "a tiny text stays raw");
        assert_eq!(&raw[4..], b"hi");
    }

    #[test]
    fn a_tampered_text_length_is_refused_before_allocation() {
        use rustsearch_core::{Index, SearchOptions};
        let mut bytes = tiny_index();
        let (docs_off, _) = span(&bytes, 2);
        let (texts_off, _) = span(&bytes, 4);
        let text_off = u32::from_le_bytes(
            bytes[docs_off + 4 + 16..docs_off + 4 + 20]
                .try_into()
                .unwrap(),
        ) as usize;
        bytes[texts_off + text_off..texts_off + text_off + 4]
            .copy_from_slice(&0x7fff_ffffu32.to_le_bytes());
        let checksum = fnv1a64(&bytes[HEADER_LEN..]);
        bytes[HEADER_LEN - 8..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
        let index = Index::from_bytes(bytes).unwrap();
        assert!(index
            .try_search("walkdir", SearchOptions::default())
            .is_err());
    }

    #[test]
    fn a_truncated_or_corrupt_index_is_rejected_without_panicking() {
        use rustsearch_core::Index;
        let bytes = tiny_index();
        for length in (0..bytes.len()).step_by(97) {
            assert!(
                Index::from_bytes(bytes[..length].to_vec()).is_err(),
                "len {length}"
            );
        }
        for index in (0..bytes.len()).step_by(53) {
            let mut corrupt = bytes.clone();
            corrupt[index] ^= 0x80;
            assert!(Index::from_bytes(corrupt).is_err(), "byte {index}");
        }
        assert!(Index::from_bytes(vec![0; 200]).is_err());
    }

    #[test]
    fn a_tampered_section_offset_is_rejected_after_a_valid_checksum() {
        use rustsearch_core::format::fnv1a64;
        use rustsearch_core::Index;
        let mut bytes = tiny_index();
        let terms_off = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;
        bytes[terms_off + 4..terms_off + 8].copy_from_slice(&0xFFFF_FF00u32.to_le_bytes());
        let checksum = fnv1a64(&bytes[HEADER_LEN..]);
        bytes[HEADER_LEN - 8..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
        assert!(Index::from_bytes(bytes).is_err());
    }

    #[test]
    fn an_english_index_matches_inflections_and_drops_stopwords() {
        use rustsearch_core::{Index, SearchOptions};
        let root = std::env::temp_dir().join(format!("rustsearch-stem-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("a.html"),
            "<html><head><title>Indexes</title></head><body><main><p>The indexes are built by the indexer.</p></main></body></html>",
        )
        .unwrap();
        fs::write(
            root.join("b.html"),
            "<html><head><title>Runtime</title></head><body><main><p>A running process is not indexing.</p></main></body></html>",
        )
        .unwrap();
        let options = BuildOptions {
            analyzer: Analyzer::English,
            ..BuildOptions::default()
        };
        let build = build_site(&root, &options).unwrap();
        let again = build_site(&root, &options).unwrap();
        assert_eq!(build.bytes, again.bytes, "a stemmed build is deterministic");
        let index = Index::from_bytes(build.bytes).unwrap();
        assert_eq!(index.analyzer(), Analyzer::English);
        let url = |query: &str| {
            index
                .search(query, SearchOptions::default())
                .first()
                .map(|hit| hit.url.clone())
        };
        assert_eq!(url("indexes").as_deref(), Some("/a.html"));
        assert_eq!(url("index").as_deref(), Some("/a.html"));
        let spellings: Vec<String> = index
            .search("indexing", SearchOptions::default())
            .iter()
            .map(|hit| hit.url.clone())
            .collect();
        assert_eq!(
            spellings,
            ["/a.html", "/b.html"],
            "one stem matches every spelling, in both pages"
        );
        assert_eq!(url("running").as_deref(), Some("/b.html"));
        assert_eq!(url("run").as_deref(), Some("/b.html"));
        assert_eq!(url("the"), None, "a query of stopwords matches nothing");
        let hit = index.search("index", SearchOptions::default()).remove(0);
        assert!(
            hit.snippet.contains("indexes"),
            "a stemmed hit still marks the inflected word: {}",
            hit.snippet
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// One document for the `serialize`-level tests: `body` is tokenized as it is,
    /// `title_tokens` are given directly so a test can decide what a title holds.
    fn doc(url: &str, title: &str, body: &str, title_tokens: &[&str]) -> Doc {
        Doc {
            url: url.to_string(),
            title: title.to_string(),
            text: body.to_string(),
            tokens: Analyzer::None
                .analyze(body)
                .into_iter()
                .map(Arc::from)
                .collect(),
            title_tokens: title_tokens.iter().map(|token| Arc::from(*token)).collect(),
            positions: Vec::new(),
            hash: fnv1a64(body.as_bytes()),
            facets: Vec::new(),
        }
    }

    /// Body and title postings for `docs`, as `build_site` computes them.
    fn postings_of(docs: &[Doc]) -> (HashMap<Arc<str>, Postings>, HashMap<Arc<str>, Postings>) {
        let mut body: HashMap<Arc<str>, Postings> = HashMap::new();
        let mut title: HashMap<Arc<str>, Postings> = HashMap::new();
        for (doc_id, doc) in docs.iter().enumerate() {
            for (ordinal, token) in doc.tokens.iter().enumerate() {
                post(&mut body, token, doc_id as u32, ordinal as u32, false);
            }
            for token in &doc.title_tokens {
                post(&mut title, token, doc_id as u32, 0, false);
            }
        }
        (body, title)
    }

    fn serialize_docs(docs: &[Doc]) -> Vec<u8> {
        let (body, title) = postings_of(docs);
        let tokens: u64 = docs.iter().map(|doc| doc.tokens.len() as u64).sum();
        let title_tokens: u64 = docs.iter().map(|doc| doc.title_tokens.len() as u64).sum();
        serialize(
            docs,
            body,
            title,
            tokens,
            title_tokens,
            Analyzer::None,
            false,
            0,
        )
        .unwrap()
    }

    #[test]
    fn a_title_boost_moves_a_title_match_above_a_body_match() {
        use rustsearch_core::{Index, SearchOptions};
        // Both pages mention `walkdir` once; only the second has it in the title.
        let docs = vec![
            doc(
                "/body.html",
                "Runtime notes",
                "walkdir walks a tree, walkdir walks a tree again",
                &["runtime", "notes"],
            ),
            doc("/title.html", "Walkdir", "a tree walker", &["walkdir"]),
        ];
        let index = Index::from_bytes(serialize_docs(&docs)).unwrap();
        assert!(index.has_title_postings());
        assert_eq!(index.title_token_count(), 3);
        let ranked = |boost: f32| -> Vec<(String, f32)> {
            index
                .search(
                    "walkdir",
                    SearchOptions {
                        title_boost: boost,
                        ..Default::default()
                    },
                )
                .into_iter()
                .map(|hit| (hit.url, hit.score))
                .collect()
        };
        let boosted = ranked(2.0);
        assert_eq!(boosted[0].0, "/title.html", "the title match ranks first");
        assert!(boosted[0].1 > boosted[1].1);
        // Without the boost the body page wins on term frequency, which is what the
        // title field is for: the BM25F sum is per field, not a re-weighting of one
        // frequency.
        let flat = ranked(0.0);
        assert_eq!(flat[0].0, "/body.html");
        assert!(flat.iter().all(|(_, score)| score.is_finite()));
        // A term that only exists in a title is findable, and only with a boost.
        let docs = vec![doc("/a.html", "Quixotic", "plain prose", &["quixotic"])];
        let index = Index::from_bytes(serialize_docs(&docs)).unwrap();
        assert_eq!(
            index
                .search(
                    "quixotic",
                    SearchOptions {
                        title_boost: 2.0,
                        ..Default::default()
                    }
                )
                .len(),
            1
        );
        assert!(index
            .search(
                "quixotic",
                SearchOptions {
                    title_boost: 0.0,
                    ..Default::default()
                }
            )
            .is_empty());
    }

    #[test]
    fn title_boost_zero_matches_a_build_without_title_postings() {
        use rustsearch_core::{Index, SearchOptions};
        let body = "walkdir walks a tree and walkdir walks it again";
        let titles = ["walkdir", "trees"];
        let with_titles = vec![doc("/a.html", "Walkdir trees", body, &titles)];
        let without = vec![doc("/a.html", "Walkdir trees", body, &[])];
        let boosted_index = Index::from_bytes(serialize_docs(&with_titles)).unwrap();
        let plain_index = Index::from_bytes(serialize_docs(&without)).unwrap();
        let options = SearchOptions {
            title_boost: 0.0,
            ..Default::default()
        };
        for query in ["walkdir", "tree", "walks"] {
            let boosted = boosted_index.search(query, options);
            let plain = plain_index.search(query, options);
            assert_eq!(boosted.len(), plain.len(), "{query}");
            for (with, without) in boosted.iter().zip(&plain) {
                assert_eq!(with.url, without.url);
                assert_eq!(
                    with.score, without.score,
                    "{query}: a zero boost must not touch the scores"
                );
            }
        }
        assert!(plain_index.title_token_count() == 0);
        assert!(!plain_index.has_title_postings());
    }

    #[test]
    fn a_page_reaches_its_title_terms_through_the_real_builder() {
        use rustsearch_core::{Filter, Index, SearchOptions};
        let root = std::env::temp_dir().join(format!("rustsearch-title-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("guide")).unwrap();
        fs::write(
            root.join("guide/a.html"),
            "<html lang=\"en\"><head><title>Deploy previews</title></head>\
             <body><main><p>staging builds and rollbacks</p></main></body></html>",
        )
        .unwrap();
        fs::write(
            root.join("guide/b.html"),
            "<html lang=\"en\"><head><title>Staging notes</title></head>\
             <body><main><p>a preview environment is rebuilt on every push</p></main></body></html>",
        )
        .unwrap();
        let build = build_site(&root, &BuildOptions::default()).unwrap();
        assert_eq!(
            build.stats.title_tokens, 4,
            "deploy + previews + staging + notes"
        );
        let index = Index::from_bytes(build.bytes).unwrap();
        assert!(index.has_title_postings());
        assert!(index.title_token_count() > 0);
        // `deploy` is in a title only: it is an exact dictionary hit with no body
        // postings, and it still finds the page.
        let hits = index.search("deploy", SearchOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "/guide/a.html");
        // The filter runs on the same candidates: a facet filter composes with the
        // boost without changing which documents titles can reach.
        let filtered = index
            .try_search_with_filters(
                "deploy",
                SearchOptions::default(),
                &[Filter::new("section", "guide")],
            )
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert!(index
            .try_search_with_filters(
                "deploy",
                SearchOptions::default(),
                &[Filter::new("section", "reference")]
            )
            .unwrap()
            .is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_page_of_stopwords_produces_no_document_and_a_corpus_of_them_is_an_error() {
        let root = std::env::temp_dir().join(format!("rustsearch-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        fs::write(
            root.join("empty.html"),
            "<html><head><title>Nothing</title></head><body><p>the and of</p></body></html>",
        )
        .unwrap();
        let options = BuildOptions {
            analyzer: Analyzer::English,
            ..Default::default()
        };
        let error = build_site(&root, &options)
            .err()
            .expect("a corpus of stopwords alone is an error")
            .to_string();
        assert!(error.contains("skipped 1"), "{error}");

        fs::write(
            root.join("real.html"),
            "<html><head><title>Indexing</title></head><body><p>walkdir walks</p></body></html>",
        )
        .unwrap();
        let build = build_site(&root, &options).unwrap();
        assert_eq!(
            build.stats.documents, 1,
            "the stopword page is counted as skipped, not indexed"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn page_chunks_partition_the_text_without_splitting_a_word() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa";
        let chunks = page_chunks(text, 20);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 20), "{chunks:?}");
        assert_eq!(chunks.join(" "), text, "the chunks partition the text");
        assert_eq!(page_chunks(text, 0), vec![text], "0 never splits");
        assert_eq!(page_chunks(text, text.len()), vec![text]);
        // A run with no space is cut on a char boundary, never inside a code point.
        let unbroken = "ααααααααααα";
        let chunks = page_chunks(unbroken, 5);
        assert_eq!(chunks.concat(), unbroken);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 5));
    }

    #[test]
    fn a_long_page_becomes_documents_that_share_one_url_and_group_back_into_one_hit() {
        use rustsearch_core::{Index, SearchOptions};
        let root = std::env::temp_dir().join(format!("rustsearch-group-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        // One long page whose every paragraph mentions `walkdir`, and a second page
        // with a single mention. `long.html` sorts before `short.html`, so doc ids
        // 0..n-1 are its chunks and the last document is the short page.
        let long = (0..60)
            .map(|index| format!("paragraph {index} says walkdir walks a directory tree."))
            .collect::<Vec<_>>()
            .join(" ");
        fs::write(
            root.join("long.html"),
            format!("<html><head><title>Walking</title></head><body><main><p>{long}</p></main></body></html>"),
        )
        .unwrap();
        fs::write(
            root.join("short.html"),
            "<html><head><title>Other</title></head><body><main><p>walkdir once</p></main></body></html>",
        )
        .unwrap();
        let options = BuildOptions {
            max_page_chars: 300,
            ..Default::default()
        };
        let build = build_site(&root, &options).unwrap();
        let again = build_site(&root, &options).unwrap();
        assert_eq!(build.bytes, again.bytes, "a chunked build is deterministic");
        let index = Index::from_bytes(build.bytes).unwrap();
        assert!(index.doc_count() > 2, "the long page was split");
        // Every chunk of the long page points at the same URL bytes: that offset is
        // the page identity the engine groups on, and it is readable from the doc
        // record alone.
        let chunks = index.doc_count() - 1;
        let spans: Vec<_> = (0..index.doc_count())
            .map(|doc| index.string_span(doc).unwrap())
            .collect();
        assert!(chunks > 2, "the long page was split into {chunks} chunks");
        assert!(
            spans[..chunks as usize]
                .iter()
                .all(|span| span.url_off == spans[0].url_off && span.url_len == spans[0].url_len),
            "chunks of one page share a url_off"
        );
        assert_ne!(spans[chunks as usize].url_off, spans[0].url_off);
        for span in &spans[..chunks as usize] {
            assert!(span.is_adjacent(), "URL and title stay adjacent per page");
        }

        // Ungrouped: every chunk is a hit, all with the same URL.
        let flat = index.search(
            "walkdir",
            SearchOptions {
                limit: 50,
                ..Default::default()
            },
        );
        assert_eq!(flat.len(), index.doc_count() as usize);
        assert!(flat.iter().all(|hit| hit.matches == 1));
        assert_eq!(flat[0].url, "/long.html");
        assert_eq!(flat[1].url, "/long.html", "the page is not collapsed");

        // Grouped: one hit per page, with the chunk count, and `limit` counts pages.
        let grouped = index.search(
            "walkdir",
            SearchOptions {
                group_by_url: true,
                ..Default::default()
            },
        );
        assert_eq!(grouped.len(), 2, "one hit per page");
        assert_eq!(grouped[0].url, "/long.html");
        assert_eq!(
            grouped[0].matches as usize,
            index.doc_count() as usize - 1,
            "every chunk of the long page is counted"
        );
        assert_eq!(grouped[1].matches, 1);
        assert_eq!(
            grouped[0].doc_id, flat[0].doc_id,
            "the best chunk names the page"
        );
        let limited = index.search(
            "walkdir",
            SearchOptions {
                group_by_url: true,
                limit: 1,
                ..Default::default()
            },
        );
        assert_eq!(limited.len(), 1, "limit applies after the grouping");
        assert_eq!(limited[0].url, "/long.html");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_section_comes_from_a_directory_not_a_file_name() {
        assert_eq!(
            path_section(Path::new("guide/setup.html")).as_deref(),
            Some("guide")
        );
        assert_eq!(
            path_section(Path::new("docs/zbsearch/static.html")).as_deref(),
            Some("docs"),
            "the first directory is the section"
        );
        assert_eq!(path_section(Path::new("page.html")), None);
        assert_eq!(path_section(Path::new("index.html")), None);
    }

    #[test]
    fn a_field_with_too_many_values_keeps_the_most_populated_ones() {
        use rustsearch_core::facets::MAX_FACET_VALUES;
        use rustsearch_core::Index;
        let root = std::env::temp_dir().join(format!("rustsearch-cap-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        // 520 sections: the first 30 have two pages, the rest one.
        for section in 0..520 {
            let dir = root.join(format!("s{section:03}"));
            fs::create_dir_all(&dir).unwrap();
            let page = |name: &str| {
                format!(
                    "<html lang=\"en\"><head><title>S{section} {name}</title></head>\
                     <body><main><p>walkdir walks a tree in section {section}.</p></main></body></html>"
                )
            };
            fs::write(dir.join("a.html"), page("a")).unwrap();
            if section < 30 {
                fs::write(dir.join("b.html"), page("b")).unwrap();
            }
        }
        let build = build_site(&root, &BuildOptions::default()).unwrap();
        let index = Index::from_bytes(build.bytes).unwrap();
        let sections = index.facet_values("section").unwrap();
        assert_eq!(
            sections.len(),
            MAX_FACET_VALUES,
            "a field is capped at the format's value count"
        );
        assert_eq!(sections[0].0, "s000");
        assert_eq!(sections[0].1, 2, "the most populated values survive");
        assert_eq!(sections[29].0, "s029");
        assert_eq!(sections[29].1, 2);
        assert_eq!(sections[MAX_FACET_VALUES - 1].0, "s511");
        assert_eq!(
            sections.iter().map(|(_, docs)| *docs).sum::<u32>(),
            30 * 2 + (MAX_FACET_VALUES as u32 - 30),
            "the kept values account for their documents; the rest are dropped whole"
        );
        assert_eq!(index.doc_count(), 550);
        assert_eq!(sections.len() + 8, 520, "eight sections were dropped");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn facets_are_built_and_filter_the_ranking() {
        use rustsearch_core::{Filter, Index, SearchOptions};
        let root = std::env::temp_dir().join(format!("rustsearch-facet-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("guide")).unwrap();
        fs::create_dir_all(root.join("reference")).unwrap();
        fs::write(
            root.join("guide/a.html"),
            "<html lang=\"en\"><head><title>A</title>\
             <meta name=\"keywords\" content=\"tree, api\"></head>\
             <body><main><p>walkdir walks a tree</p></main></body></html>",
        )
        .unwrap();
        fs::write(
            root.join("guide/b.html"),
            "<html lang=\"EN\"><head><title>B</title>\
             <meta name=\"keywords\" content=\"api\"></head>\
             <body><main><p>walkdir and serde write JSON</p></main></body></html>",
        )
        .unwrap();
        fs::write(
            root.join("reference/c.html"),
            "<html lang=\"it\"><head><title>C</title>\
             <meta name=\"keywords\" content=\"tree\"></head>\
             <body><main><p>walkdir walks serde</p></main></body></html>",
        )
        .unwrap();
        let build = build_site(&root, &BuildOptions::default()).unwrap();
        let again = build_site(&root, &BuildOptions::default()).unwrap();
        assert_eq!(build.bytes, again.bytes, "a faceted build is deterministic");
        let index = Index::from_bytes(build.bytes).unwrap();
        assert!(index.has_facets());
        assert_eq!(index.facet_fields(), vec!["lang", "section", "tags"]);
        let values = |field: &str| {
            index
                .facet_values(field)
                .unwrap()
                .into_iter()
                .map(|(value, _)| value.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(values("lang"), ["en", "it"], "lang is lowercased");
        assert_eq!(values("section"), ["guide", "reference"]);
        assert_eq!(values("tags"), ["api", "tree"]);
        assert_eq!(index.facet_values("lang").unwrap()[0].1, 2, "two en pages");

        // The same order the unfiltered query gives, restricted to the matches.
        let all: Vec<u32> = index
            .search("walkdir", SearchOptions::default())
            .iter()
            .map(|hit| hit.doc_id)
            .collect();
        assert_eq!(all.len(), 3);
        let filtered = |filters: &[Filter<'_>]| -> Vec<u32> {
            index
                .try_search_with_filters("walkdir", SearchOptions::default(), filters)
                .unwrap()
                .iter()
                .map(|hit| hit.doc_id)
                .collect()
        };
        let guide = filtered(&[Filter::new("section", "guide")]);
        assert_eq!(guide.len(), 2);
        assert!(guide.iter().all(|doc| all.contains(doc)));
        assert_eq!(
            guide,
            all.iter()
                .copied()
                .filter(|doc| guide.contains(doc))
                .collect::<Vec<_>>(),
            "filtering must not reorder the ranking"
        );
        // OR inside a field, AND across fields.
        assert_eq!(
            filtered(&[Filter::new("lang", "en"), Filter::new("lang", "it")]).len(),
            3
        );
        assert_eq!(filtered(&[Filter::new("tags", "tree")]).len(), 2);
        assert!(filtered(&[
            Filter::new("lang", "en"),
            Filter::new("section", "reference")
        ])
        .is_empty());
        // Unknown field or value: an empty result, not an error.
        assert!(filtered(&[Filter::new("mood", "happy")]).is_empty());
        assert!(filtered(&[Filter::new("lang", "fr")]).is_empty());

        // The caps are errors, not silently truncated filters: nine values in one
        // field, nine fields, 513 bytes of filters.
        let too_many_values: Vec<Filter<'_>> = (0..9)
            .map(|index| Filter::new("section", ["guide", "reference"][index % 2]))
            .collect();
        let nine_fields: Vec<Filter<'_>> = (0..9)
            .map(|index| Filter::new(["a", "b", "c", "d", "e", "f", "g", "h", "i"][index], "x"))
            .collect();
        let long = "v".repeat(300);
        for (name, filters) in [
            ("nine values", too_many_values),
            ("nine fields", nine_fields),
            (
                "512+ bytes",
                vec![Filter::new("lang", &long), Filter::new("section", &long)],
            ),
        ] {
            assert!(
                matches!(
                    index.try_search_with_filters("walkdir", SearchOptions::default(), &filters),
                    Err(rustsearch_core::Error::QueryTooLong)
                ),
                "{name} must be refused"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_filter_on_a_v3_artifact_is_refused_not_ignored() {
        use rustsearch_core::format::VERSION;
        use rustsearch_core::{Error, Filter, Index, SearchOptions};
        let bytes = tiny_index();
        assert_eq!(VERSION, 4);
        // The same artifact as a v3 one: a 96-byte header whose seven spans point 24
        // bytes lower (the header is smaller), the checksum where the facet span sits
        // in v4, and a body that ends with `strhashes`.
        let (strhashes_off, strhashes_len) = span(&bytes, 6);
        let body = &bytes[HEADER_LEN..strhashes_off + strhashes_len];
        let shift = (HEADER_LEN - 96) as u32;
        let mut v3 = bytes[..88].to_vec();
        v3[8..12].copy_from_slice(&3u32.to_le_bytes());
        for index in 0..7 {
            let (off, len) = span(&bytes, index);
            let at = 32 + index * 8;
            v3[at..at + 4].copy_from_slice(&(off as u32 - shift).to_le_bytes());
            v3[at + 4..at + 8].copy_from_slice(&(len as u32).to_le_bytes());
        }
        v3.extend_from_slice(&fnv1a64(body).to_le_bytes());
        v3.extend_from_slice(body);
        let index = Index::from_bytes(v3).expect("a v3 artifact must still load");
        assert_eq!(index.format_version(), 3);
        assert!(!index.has_facets());
        assert_eq!(index.search("walkdir", SearchOptions::default()).len(), 1);
        assert!(
            matches!(
                index.try_search_with_filters(
                    "walkdir",
                    SearchOptions::default(),
                    &[Filter::new("lang", "en")]
                ),
                Err(Error::UnsupportedFacet)
            ),
            "a filter must not silently run unfiltered"
        );
    }

    #[test]
    fn a_v2_or_unknown_analyzer_artifact_is_refused() {
        use rustsearch_core::Index;
        let mut bytes = tiny_index();
        bytes[8..12].copy_from_slice(&2u32.to_le_bytes());
        assert!(
            Index::from_bytes(bytes).is_err(),
            "v2 must not be read as v3"
        );

        let mut bytes = tiny_index();
        let flags = u32::from_le_bytes(bytes[12..16].try_into().unwrap());
        bytes[12..16].copy_from_slice(&(flags | 3).to_le_bytes());
        let checksum = fnv1a64(&bytes[HEADER_LEN..]);
        bytes[HEADER_LEN - 8..HEADER_LEN].copy_from_slice(&checksum.to_le_bytes());
        let error = match Index::from_bytes(bytes) {
            Ok(_) => panic!("analyzer id 3 must be refused"),
            Err(error) => error,
        };
        assert_eq!(error, rustsearch_core::Error::UnsupportedAnalyzer(3));
    }

    #[test]
    fn a_built_index_round_trips_through_the_core() {
        use rustsearch_core::{Index, SearchOptions};
        let html = r#"<html><head><title>Walking</title></head><body><main>
            <p>Rust walks a directory tree with walkdir.</p></main></body></html>"#;
        let (title, text) = extract_page(html);
        let tokens: Vec<Arc<str>> = Analyzer::None
            .analyze(&text)
            .into_iter()
            .map(Arc::from)
            .collect();
        let mut postings: HashMap<Arc<str>, Postings> = HashMap::new();
        for (ordinal, token) in tokens.iter().enumerate() {
            post(&mut postings, token, 0, ordinal as u32, false);
        }
        let docs = vec![Doc {
            url: "/".into(),
            title,
            text,
            tokens: tokens.clone(),
            title_tokens: Vec::new(),
            positions: Vec::new(),
            hash: 0,
            facets: Vec::new(),
        }];
        let bytes = serialize(
            &docs,
            postings,
            HashMap::new(),
            tokens.len() as u64,
            0,
            Analyzer::None,
            false,
            0,
        )
        .unwrap();
        let index = Index::from_bytes(bytes).unwrap();
        let hits = index.search("walkdir", SearchOptions::default());
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "/");
        assert!(hits[0].snippet.contains("walkdir"));
        let fuzzy = index.search(
            "walkdxr",
            SearchOptions {
                fuzzy: true,
                ..Default::default()
            },
        );
        assert_eq!(fuzzy.len(), 1, "one substitution should find walkdir");
        assert!(index
            .try_search(
                &"x".repeat(rustsearch_core::index::MAX_QUERY_BYTES + 1),
                SearchOptions::default()
            )
            .is_err());
        assert!(index.content_hash(0).is_some());
        assert!(index.content_hash(1).is_none());
    }
}
