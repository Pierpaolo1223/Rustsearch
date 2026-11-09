//! The analyzer: tokenize, drop stopwords, stem. One function, shared by the
//! builder and the query engine, so an index and the queries against it can never
//! disagree about what a term is.
//!
//! The analyzer is a property of the artifact, not of the process: it is stored in
//! the header's flags (see [`crate::format`]). A stemmed index must be queried with
//! the same stemmer, because `"indexes"` is stored as `"index"`.
use alloc::string::String;
use alloc::vec::Vec;

use crate::stemmer;
use crate::stopwords::{
    might_be_stopword, FINGERPRINTS_EN, FINGERPRINTS_IT, STOPWORDS_EN, STOPWORDS_IT,
};
use crate::tokenize::tokenize;

/// How text is normalized before it reaches the dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Analyzer {
    /// Unicode lowercase and alphanumeric runs, nothing else. The v1 behaviour.
    #[default]
    None,
    /// English Snowball stemmer plus the English stopword list.
    English,
    /// Italian Snowball stemmer plus the Italian stopword list.
    Italian,
}

impl Analyzer {
    /// The id stored in the header's low flag bits.
    pub const fn id(self) -> u32 {
        match self {
            Analyzer::None => 0,
            Analyzer::English => 1,
            Analyzer::Italian => 2,
        }
    }

    /// Parse a header id. `None` means the artifact was written by a newer build.
    pub fn from_id(id: u32) -> Option<Self> {
        match id {
            0 => Some(Analyzer::None),
            1 => Some(Analyzer::English),
            2 => Some(Analyzer::Italian),
            _ => None,
        }
    }

    /// The name `rustsearch build --stemmer` accepts.
    pub const fn name(self) -> &'static str {
        match self {
            Analyzer::None => "none",
            Analyzer::English => "english",
            Analyzer::Italian => "italian",
        }
    }

    /// Parse a command-line or configuration value.
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "off" => Some(Analyzer::None),
            "en" | "eng" | "english" => Some(Analyzer::English),
            "it" | "ita" | "italian" => Some(Analyzer::Italian),
            _ => None,
        }
    }

    /// True when `word` is a stopword for this analyzer. `word` must already be
    /// tokenized (lowercase, alphanumeric).
    ///
    /// The fingerprint table rejects almost every word with one hash and one load;
    /// only a hit pays the binary search over the list, which decides.
    pub fn is_stopword(self, word: &str) -> bool {
        let (list, table): (&[&str], &[u32; crate::stopwords::TABLE_SLOTS]) = match self {
            Analyzer::None => return false,
            Analyzer::English => (&STOPWORDS_EN, &FINGERPRINTS_EN),
            Analyzer::Italian => (&STOPWORDS_IT, &FINGERPRINTS_IT),
        };
        if !might_be_stopword(table, word) {
            return false;
        }
        list.binary_search(&word).is_ok()
    }

    /// The dictionary form of one token.
    pub fn stem(self, word: &str) -> String {
        stemmer::stem(self, word)
    }

    /// Analyze a document: tokenize, drop stopwords, stem. Order and duplicates are
    /// preserved, because the builder counts term frequencies.
    pub fn analyze(self, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for token in tokenize(text) {
            if let Some(term) = self.term(&token) {
                out.push(term);
            }
        }
        out
    }

    /// Analyze a document keeping the token ordinal: the index of the token in the
    /// token stream, **stopwords included**, so a phrase that drops a word keeps its
    /// gap (`"index of the shard"` requires three tokens between the two terms that
    /// are indexed). F6's positions are exactly these ordinals.
    pub fn analyze_positional(self, text: &str) -> Vec<(String, u32)> {
        let mut out = Vec::new();
        for (ordinal, token) in tokenize(text).into_iter().enumerate() {
            let Some(term) = self.term(&token) else {
                continue;
            };
            out.push((term, u32::try_from(ordinal).unwrap_or(u32::MAX)));
        }
        out
    }

    /// Normalize one already-tokenized token, or `None` when it is a stopword.
    fn term(self, token: &str) -> Option<String> {
        if self.is_stopword(token) {
            return None;
        }
        Some(self.stem(token))
    }

    /// Analyze a query and deduplicate it, keeping the first spelling.
    ///
    /// A repeated query word adds no ranking signal and doubles the postings work,
    /// so `"rust rust tokio"` becomes `["rust", "tokio"]`. Stopwords are dropped
    /// here as well, which is why a query made only of them returns nothing.
    pub fn query_terms(self, query: &str, max_terms: usize) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for term in self.analyze(query) {
            if out.iter().any(|seen| seen == &term) {
                continue;
            }
            out.push(term);
            if out.len() >= max_terms {
                break;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_the_old_tokenizer() {
        let analyzer = Analyzer::None;
        assert_eq!(
            analyzer.analyze("The Indexes Are Here"),
            ["the", "indexes", "are", "here"]
        );
        assert!(!analyzer.is_stopword("the"));
    }

    #[test]
    fn english_drops_stopwords_and_stems() {
        let analyzer = Analyzer::English;
        assert_eq!(
            analyzer.analyze("The indexes are indexing"),
            ["index", "index"]
        );
        assert_eq!(
            analyzer.query_terms("the indexes of a running process", 8),
            ["index", "run", "process"]
        );
        assert!(analyzer.is_stopword("the"));
        assert!(!analyzer.is_stopword("index"));
    }

    #[test]
    fn italian_drops_stopwords_and_stems() {
        let analyzer = Analyzer::Italian;
        assert_eq!(
            analyzer.analyze("Gli indici delle pagine"),
            ["indic", "pagin"]
        );
        assert_eq!(
            analyzer.query_terms("la ricerca delle pagine", 8),
            ["ricerc", "pagin"]
        );
    }

    #[test]
    fn numbers_and_identifiers_survive_analysis() {
        assert_eq!(
            Analyzer::English.analyze("version 2 or v3"),
            ["version", "2", "v3"]
        );
        assert_eq!(
            Analyzer::Italian.analyze("versione 2 oppure v3"),
            ["version", "2", "v3"]
        );
    }

    #[test]
    fn positional_analysis_keeps_the_gaps_of_dropped_stopwords() {
        // "the" is a stopword, but it still occupies ordinal 0: the phrase
        // "the index" and the word "index" are not the same query.
        assert_eq!(
            Analyzer::English.analyze_positional("the index of the shard"),
            [(String::from("index"), 1), (String::from("shard"), 4)]
        );
        assert_eq!(
            Analyzer::English.analyze_positional("index"),
            [(String::from("index"), 0)]
        );
    }

    #[test]
    fn a_query_of_only_stopwords_is_empty() {
        assert!(Analyzer::English.query_terms("the and of", 8).is_empty());
    }

    #[test]
    fn the_fingerprint_table_never_hides_a_real_stopword() {
        for analyzer in [Analyzer::English, Analyzer::Italian] {
            let list: &[&str] = match analyzer {
                Analyzer::Italian => &STOPWORDS_IT,
                _ => &STOPWORDS_EN,
            };
            for word in list {
                assert!(
                    analyzer.is_stopword(word),
                    "{word:?} is in the list but the table missed it"
                );
            }
        }
        // The common case must not fall through to the list: none of these words is
        // in either list, so the table alone has to reject them.
        for word in ["index", "walkdir", "pagine", "rust", "zzz", "q"] {
            assert!(!Analyzer::English.is_stopword(word), "{word:?}");
            assert!(!Analyzer::Italian.is_stopword(word), "{word:?}");
        }
        for word in ["index", "storage", "immutable"] {
            assert!(
                !crate::stopwords::might_be_stopword(&FINGERPRINTS_EN, word),
                "{word:?} should be rejected by the table, not by the list"
            );
        }
    }

    #[test]
    fn names_round_trip_their_ids() {
        for analyzer in [Analyzer::None, Analyzer::English, Analyzer::Italian] {
            assert_eq!(Analyzer::parse(analyzer.name()), Some(analyzer));
            assert_eq!(Analyzer::from_id(analyzer.id()), Some(analyzer));
        }
        assert_eq!(Analyzer::parse("IT"), Some(Analyzer::Italian));
        assert_eq!(Analyzer::parse("klingon"), None);
        assert_eq!(Analyzer::from_id(9), None);
    }
}
