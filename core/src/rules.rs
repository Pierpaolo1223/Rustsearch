//! Build-time synonyms and editorial curation, parsed from a line-based file.
//!
//! Two small languages, both deliberately boring to parse and to review:
//!
//! ```text
//! # synonyms.syn — read by `build --synonyms`
//! deploy => publish, ship
//! install => setup
//!
//! # curate.txt — read by `search --curate`
//! install | pin /docs/install.html
//! install | boost /docs/cli.html 1.5
//! ```
//!
//! **Why synonyms are build-time.** The expansion is applied to the index: a page
//! that says *ship* also carries a posting for *deploy*, so the query needs no rules at
//! all and a static client keeps working unchanged. The cost is artifact bytes and a
//! slightly different `df`, and the price is paid once per build instead of on every
//! query.
//!
//! **Why curation is runtime.** A pin is an editorial decision ("this is the install
//! page"), and the person who makes it should not have to rebuild a million-document
//! artifact to change it. A sidecar read by the caller also means the artifact stays a
//! pure function of the site: two sites with different editors can share one index.
//!
//! **What neither can do.** A synonym can only add terms that exist in the dictionary;
//! a pin can only reorder hits the engine already returned. Neither invents a match, so
//! the response never contains a document BM25 did not find — the property that keeps
//! the feature from becoming a way to smuggle results in.
//!
//! Both parsers are total functions of their input: every line either contributes or
//! returns an error with its number, and the caps below bound what a hostile file can
//! make the caller allocate.
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;

use crate::index::Hit;

/// A synonym term (source or target) is one word; a rule longer than this is refused
/// rather than silently truncated.
pub const MAX_TERM_BYTES: usize = 64;
/// Distinct source terms one file may define.
pub const MAX_SYNONYM_TERMS: usize = 2_000;
/// Targets one source term may map to.
pub const MAX_SYNONYMS_PER_TERM: usize = 8;
/// Distinct queries one curation file may name.
pub const MAX_CURATION_QUERIES: usize = 2_000;
/// Rules one query may carry (pins and boosts together).
pub const MAX_RULES_PER_QUERY: usize = 32;
/// Pins one query may carry: more than this is an editorial accident, not a page.
pub const MAX_PINS_PER_QUERY: usize = 5;
/// A query key longer than this is not a query this engine would match exactly.
pub const MAX_QUERY_BYTES: usize = 200;
/// A curated URL is a path on this site; a scheme, a host or a `//` prefix is refused.
pub const MAX_URL_BYTES: usize = 512;
/// The lowest and highest boost factor accepted. A factor of 0 would hide a document
/// from a query that matched it, which is a different feature (and a dangerous one).
pub const MIN_BOOST: f32 = 0.1;
pub const MAX_BOOST: f32 = 10.0;

/// Why a rule file was refused. The line number is part of the value: a rule file is
/// edited by hand, and "line 7" is the only useful thing to say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuleError {
    pub line: usize,
    pub message: &'static str,
}

impl fmt::Display for RuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "line {}: {}", self.line, self.message)
    }
}

#[cfg(feature = "std")]
impl std::error::Error for RuleError {}

/// `source -> targets`, sorted by source so a lookup is a binary search and two builds
/// of one file are byte-identical.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synonyms {
    entries: Vec<(String, Vec<String>)>,
}

impl Synonyms {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Every `(source, targets)` pair, sorted.
    pub fn entries(&self) -> &[(String, Vec<String>)] {
        &self.entries
    }

    /// The targets of an analyzed term, or an empty slice.
    pub fn expand(&self, term: &str) -> &[String] {
        match self
            .entries
            .binary_search_by(|(source, _)| source.as_str().cmp(term))
        {
            Ok(index) => self
                .entries
                .get(index)
                .map(|(_, targets)| targets.as_slice())
                .unwrap_or_default(),
            Err(_) => &[],
        }
    }

    /// Parse the file format above. Comments start with `#`; blank lines are skipped.
    ///
    /// A duplicated source merges its targets (first occurrence wins for order, and a
    /// target already listed is not added twice), so generating a file by concatenation
    /// cannot silently drop a rule.
    pub fn parse(text: &str) -> Result<Self, RuleError> {
        let mut entries: Vec<(String, Vec<String>)> = Vec::new();
        for (number, raw) in text.lines().enumerate() {
            let line = strip_comment(raw);
            if line.is_empty() {
                continue;
            }
            let line_number = number + 1;
            let (source, targets) = line.split_once("=>").ok_or(RuleError {
                line: line_number,
                message: "expected `source => target, target`",
            })?;
            let source = normalize_term(source).ok_or(RuleError {
                line: line_number,
                message: "a synonym source must be one word of at most 64 bytes",
            })?;
            let mut wanted: Vec<String> = Vec::new();
            for target in targets.split(',') {
                let target = normalize_term(target).ok_or(RuleError {
                    line: line_number,
                    message: "a synonym target must be one word of at most 64 bytes",
                })?;
                if target != source && !wanted.contains(&target) {
                    wanted.push(target);
                }
            }
            if wanted.is_empty() {
                return Err(RuleError {
                    line: line_number,
                    message: "a synonym rule needs at least one target",
                });
            }
            if wanted.len() > MAX_SYNONYMS_PER_TERM {
                return Err(RuleError {
                    line: line_number,
                    message: "at most 8 targets per source term",
                });
            }
            match entries.binary_search_by(|(existing, _)| existing.cmp(&source)) {
                Ok(index) => {
                    let Some((_, list)) = entries.get_mut(index) else {
                        continue;
                    };
                    for target in wanted {
                        if !list.contains(&target) {
                            list.push(target);
                        }
                    }
                    if list.len() > MAX_SYNONYMS_PER_TERM {
                        return Err(RuleError {
                            line: line_number,
                            message: "at most 8 targets per source term (merged duplicates)",
                        });
                    }
                }
                Err(index) => {
                    if entries.len() >= MAX_SYNONYM_TERMS {
                        return Err(RuleError {
                            line: line_number,
                            message: "at most 2,000 synonym terms",
                        });
                    }
                    entries.insert(index, (source, wanted));
                }
            }
        }
        Ok(Self { entries })
    }
}

/// One editorial rule.
#[derive(Debug, Clone, PartialEq)]
pub enum Rule {
    /// Move this URL to the front, in the order the pins appear.
    Pin(String),
    /// Multiply this URL's score.
    Boost(String, f32),
}

/// `query -> rules`, with the query normalized the way the engine compares it.
#[derive(Debug, Clone, Default)]
pub struct Curations {
    entries: Vec<(String, Vec<Rule>)>,
}

impl Curations {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn rules_for(&self, query: &str) -> &[Rule] {
        let key = normalize_query(query);
        match self.entries.iter().find(|(candidate, _)| *candidate == key) {
            Some((_, rules)) => rules.as_slice(),
            None => &[],
        }
    }

    pub fn parse(text: &str) -> Result<Self, RuleError> {
        let mut entries: Vec<(String, Vec<Rule>)> = Vec::new();
        for (number, raw) in text.lines().enumerate() {
            let line = strip_comment(raw);
            if line.is_empty() {
                continue;
            }
            let line_number = number + 1;
            let (query, action) = line.split_once('|').ok_or(RuleError {
                line: line_number,
                message: "expected `query | pin /path` or `query | boost /path 1.5`",
            })?;
            let query = normalize_query(query);
            if query.is_empty() || query.len() > MAX_QUERY_BYTES {
                return Err(RuleError {
                    line: line_number,
                    message: "a curated query must be 1..=200 bytes",
                });
            }
            let rule = parse_action(action.trim(), line_number)?;
            if let Rule::Pin(url) = &rule {
                if count_pins(&entries, &query) >= MAX_PINS_PER_QUERY {
                    return Err(RuleError {
                        line: line_number,
                        message: "at most 5 pins per query",
                    });
                }
                if entries.iter().any(|(q, rules)| {
                    *q == query
                        && rules
                            .iter()
                            .any(|existing| existing == &Rule::Pin(url.clone()))
                }) {
                    // The same pin twice is a copy-paste, not an order: ignore it.
                    continue;
                }
            }
            match entries.iter_mut().find(|(existing, _)| *existing == query) {
                Some((_, rules)) => {
                    if rules.len() >= MAX_RULES_PER_QUERY {
                        return Err(RuleError {
                            line: line_number,
                            message: "at most 32 rules per query",
                        });
                    }
                    // A pin goes after the previous pins and before the boosts, so the
                    // front of the list is exactly the editorial order.
                    match rule {
                        Rule::Pin(url) => {
                            let at = rules
                                .iter()
                                .position(|rule| matches!(rule, Rule::Boost(_, _)))
                                .unwrap_or(rules.len());
                            rules.insert(at, Rule::Pin(url));
                        }
                        boost => rules.push(boost),
                    }
                }
                None => {
                    if entries.len() >= MAX_CURATION_QUERIES {
                        return Err(RuleError {
                            line: line_number,
                            message: "at most 2,000 curated queries",
                        });
                    }
                    entries.push((query, vec![rule]));
                }
            }
        }
        Ok(Self { entries })
    }

    /// Apply the rules of `query` to the hits the engine already returned.
    ///
    /// Boosts multiply, then the list is sorted by score (stable, so equal scores keep
    /// their BM25 order), then pins are moved to the front in the order they were
    /// listed. A pin whose URL is not in `hits` is ignored: the engine did not find
    /// that document for this query, and a curation is not allowed to invent it.
    ///
    /// This is why the function takes the hits after truncation: a pin reorders what
    /// the caller was going to show, it does not widen the match.
    pub fn apply(&self, query: &str, hits: &mut Vec<Hit>) {
        let key = normalize_query(query);
        let Some((_, rules)) = self.entries.iter().find(|(candidate, _)| *candidate == key) else {
            return;
        };
        let mut boosted = false;
        for rule in rules {
            if let Rule::Boost(url, factor) = rule {
                for hit in hits.iter_mut() {
                    if same_path(&hit.url, url) {
                        hit.score *= factor;
                        boosted = true;
                    }
                }
            }
        }
        if boosted {
            hits.sort_by(|left, right| {
                right
                    .score
                    .partial_cmp(&left.score)
                    .unwrap_or(core::cmp::Ordering::Equal)
            });
        }
        let pins: Vec<&String> = rules
            .iter()
            .filter_map(|rule| match rule {
                Rule::Pin(url) => Some(url),
                Rule::Boost(_, _) => None,
            })
            .collect();
        if pins.is_empty() {
            return;
        }
        let mut ordered: Vec<Hit> = Vec::with_capacity(hits.len());
        for url in pins {
            if let Some(position) = hits.iter().position(|hit| same_path(&hit.url, url)) {
                ordered.push(hits.remove(position));
            }
        }
        ordered.append(hits);
        *hits = ordered;
    }
}

/// A rule names a path on this site (`/docs/install.html`); the artifact carries the
/// same path with or without the leading slash, depending on the `--base-url` it was
/// built with (`demo/site/x.html` versus `/demo/site/x.html`). The slash is the only
/// difference allowed: a rule that named another origin was already refused by
/// `validate_url`, and a prefix match would let `/docs` pin everything under it.
fn same_path(hit_url: &str, rule_url: &str) -> bool {
    hit_url.strip_prefix('/').unwrap_or(hit_url) == rule_url.strip_prefix('/').unwrap_or(rule_url)
}

fn count_pins(entries: &[(String, Vec<Rule>)], query: &str) -> usize {
    entries
        .iter()
        .filter(|(candidate, _)| candidate == query)
        .map(|(_, rules)| {
            rules
                .iter()
                .filter(|rule| matches!(rule, Rule::Pin(_)))
                .count()
        })
        .sum()
}

fn parse_action(action: &str, line: usize) -> Result<Rule, RuleError> {
    if let Some(url) = action.strip_prefix("pin ") {
        return Ok(Rule::Pin(validate_url(url).ok_or(RuleError {
            line,
            message: "a pin takes one path on this site, e.g. /docs/install.html",
        })?));
    }
    if let Some(rest) = action.strip_prefix("boost ") {
        let (url, factor) = rest.rsplit_once(' ').ok_or(RuleError {
            line,
            message: "a boost takes `path factor`, e.g. /docs/cli.html 1.5",
        })?;
        let url = validate_url(url).ok_or(RuleError {
            line,
            message: "a boost takes one path on this site",
        })?;
        let factor: f32 = factor.parse().map_err(|_| RuleError {
            line,
            message: "a boost factor must be a number",
        })?;
        if !factor.is_finite() || !(MIN_BOOST..=MAX_BOOST).contains(&factor) {
            return Err(RuleError {
                line,
                message: "a boost factor must be between 0.1 and 10",
            });
        }
        return Ok(Rule::Boost(url, factor));
    }
    Err(RuleError {
        line,
        message: "expected `pin /path` or `boost /path factor`",
    })
}

/// A curated URL is same-origin by construction: a leading `/`, not `//` (which is a
/// host), no scheme, no control characters, no spaces. A curation file that could name
/// another origin is a phishing vector the engine will not carry.
fn validate_url(raw: &str) -> Option<String> {
    let url = raw.trim();
    if url.len() < 2 || url.len() > MAX_URL_BYTES {
        return None;
    }
    if !url.starts_with('/') || url.starts_with("//") {
        return None;
    }
    if url.chars().any(|ch| {
        ch.is_control() || ch.is_whitespace() || matches!(ch, '\\' | '"' | '\'' | '<' | '>')
    }) {
        return None;
    }
    Some(url.to_string())
}

fn strip_comment(line: &str) -> &str {
    let without = match line.find('#') {
        Some(at) => line.get(..at).unwrap_or(""),
        None => line,
    };
    without.trim()
}

/// A term is lowered and must be one word: the mapping is between dictionary terms,
/// and a phrase would need the query parser's rules rather than a rewrite.
fn normalize_term(raw: &str) -> Option<String> {
    let term = raw.trim();
    if term.is_empty() || term.len() > MAX_TERM_BYTES {
        return None;
    }
    if term.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
        return None;
    }
    Some(term.to_lowercase())
}

/// The query key: lowercase and one space between words, which is how the engine's
/// analysis sees a query it has already tokenized.
fn normalize_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for word in query.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        for ch in word.chars() {
            if ch.is_control() {
                continue;
            }
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        }
    }
    out.truncate(MAX_QUERY_BYTES);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;

    fn hit(url: &str, score: f32) -> Hit {
        Hit {
            doc_id: 0,
            score,
            url: url.to_string(),
            title: String::new(),
            snippet: String::new(),
            highlights: Vec::new(),
            matches: 1,
        }
    }

    #[test]
    fn synonyms_parse_and_merge() {
        let synonyms = Synonyms::parse(
            "# comment\ndeploy => publish, ship\n\nDeploy => publish\nrelease => ship\n",
        )
        .unwrap();
        assert_eq!(synonyms.len(), 2);
        assert_eq!(synonyms.expand("deploy"), ["publish", "ship"]);
        assert_eq!(synonyms.expand("release"), ["ship"]);
        assert!(synonyms.expand("missing").is_empty());
    }

    #[test]
    fn a_synonym_rule_that_is_not_one_word_is_refused_with_its_line() {
        let error = Synonyms::parse("ok => fine\nbad name => x").unwrap_err();
        assert_eq!(error.line, 2);
        let error = Synonyms::parse("lonely").unwrap_err();
        assert_eq!(error.line, 1);
        let error = Synonyms::parse("a => ").unwrap_err();
        assert_eq!(error.line, 1);
    }

    #[test]
    fn curation_parses_pins_in_order_and_boosts() {
        let curations = Curations::parse(
            "install | pin /docs/install.html\ninstall | boost /docs/cli.html 1.5\ninstall | pin /docs/quickstart.html\n",
        )
        .unwrap();
        let rules = curations.rules_for("  INSTALL ");
        assert_eq!(
            rules,
            [
                Rule::Pin("/docs/install.html".to_string()),
                Rule::Pin("/docs/quickstart.html".to_string()),
                Rule::Boost("/docs/cli.html".to_string(), 1.5)
            ]
        );
    }

    #[test]
    fn a_pin_moves_a_hit_and_a_boost_changes_the_order_without_inventing_matches() {
        let curations = Curations::parse("install | pin /c\ninstall | boost /b 4\n").unwrap();
        let mut hits = vec![hit("/a", 6.0), hit("/b", 2.0), hit("/c", 1.0)];
        curations.apply("install", &mut hits);
        assert_eq!(hits[0].url, "/c", "the pin comes first");
        assert_eq!(hits[1].url, "/b", "the boost moved /b above /a");
        assert_eq!(hits[2].url, "/a");
        assert_eq!(hits.len(), 3, "curation never adds a document");

        // A pin for a document the engine did not return is ignored.
        let curations = Curations::parse("install | pin /missing\n").unwrap();
        let mut hits = vec![hit("/a", 1.0)];
        curations.apply("install", &mut hits);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "/a");
    }

    #[test]
    fn a_rule_matches_a_hit_url_with_or_without_the_leading_slash() {
        // The demo builds with `--base-url demo/site`, so its hits are relative while
        // the rule file names site paths: the pin and the boost must still apply.
        let curations = Curations::parse(
            "install | pin /docs/install.html\ninstall | boost /docs/cli.html 4\n",
        )
        .unwrap();
        let mut hits = vec![hit("docs/cli.html", 2.0), hit("docs/install.html", 1.0)];
        curations.apply("install", &mut hits);
        assert_eq!(
            hits[0].url, "docs/install.html",
            "the pin still comes first"
        );
        assert_eq!(hits[1].url, "docs/cli.html");
        assert_eq!(hits[1].score, 8.0, "the boost matched the relative URL");

        // Exactly one leading slash is ignored, and a prefix is not a path.
        assert!(same_path("docs/a.html", "/docs/a.html"));
        assert!(!same_path("docs/a.html", "/docs"));
        assert!(!same_path("other/a.html", "/docs/a.html"));
    }

    #[test]
    fn a_curated_url_cannot_name_another_origin() {
        for bad in [
            "install | pin https://evil.example/x",
            "install | pin //evil.example/x",
            "install | pin /ok /second",
            "install | boost /ok 0",
            "install | boost /ok 99",
            "install | boost /ok NaN",
        ] {
            assert!(Curations::parse(bad).is_err(), "{bad} must be refused");
        }
        assert!(Curations::parse("install | boost /ok 0.5").is_ok());
    }

    #[test]
    fn caps_stop_a_hostile_file() {
        let mut file = String::new();
        for index in 0..6 {
            file.push_str(&format!("q | pin /p{index}\n"));
        }
        let error = Curations::parse(&file).unwrap_err();
        assert_eq!(error.message, "at most 5 pins per query");

        let mut file = String::new();
        for index in 0..9 {
            file.push_str(&format!("deploy => t{index}\n"));
        }
        assert!(Synonyms::parse(&file).is_err());
    }

    #[test]
    fn a_query_key_is_normalized_once() {
        assert_eq!(normalize_query("  Install\tThe  CLI "), "install the cli");
        assert_eq!(normalize_query("CAFÉ"), "café");
    }
}
