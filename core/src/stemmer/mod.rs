//! Snowball stemmers for English and Italian, vendored for `no_std`.
//!
//! The generated algorithm modules come from the Snowball compiler through
//! `rust-stemmers` 1.2 (MIT / BSD-3-Clause, © Jakob Demler and CurrySoftware);
//! the only edits are the `alloc` imports and the `core::cmp::min` call, because
//! `rust-stemmers` itself requires `std` and pulls in `serde`. Shipping two
//! languages instead of all eighteen is what keeps the WASM module small.
//!
//! A stemmer is only ever selected through [`Analyzer`](crate::analyze::Analyzer),
//! and the chosen analyzer is stored in the artifact's header so a query is
//! always analyzed with the same tables as the text.
mod among;
mod english;
mod italian;
mod snowball_env;

use alloc::string::String;

pub use among::Among;
use snowball_env::SnowballEnv;

use crate::analyze::Analyzer;

/// The stem of `word`, or `word` itself when no analyzer is selected.
///
/// The input must already be lowercased (the tokenizer does that). An empty stem
/// is treated as no stem: a term nobody could type would only add noise.
pub fn stem(analyzer: Analyzer, word: &str) -> String {
    let algorithm: fn(&mut SnowballEnv) -> bool = match analyzer {
        Analyzer::None => return String::from(word),
        Analyzer::English => english::stem,
        Analyzer::Italian => italian::stem,
    };
    let mut env = SnowballEnv::create(word);
    algorithm(&mut env);
    let stemmed = env.get_current().into_owned();
    if stemmed.is_empty() {
        String::from(word)
    } else {
        stemmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_collapses_inflections() {
        assert_eq!(stem(Analyzer::English, "indexes"), "index");
        assert_eq!(stem(Analyzer::English, "indexing"), "index");
        assert_eq!(stem(Analyzer::English, "running"), "run");
        assert_eq!(stem(Analyzer::English, "fruitlessly"), "fruitless");
        assert_eq!(stem(Analyzer::English, "walkdir"), "walkdir");
    }

    #[test]
    fn italian_collapses_inflections() {
        assert_eq!(stem(Analyzer::Italian, "indici"), "indic");
        assert_eq!(stem(Analyzer::Italian, "cercando"), "cerc");
        assert_eq!(stem(Analyzer::Italian, "pagine"), "pagin");
    }

    #[test]
    fn no_analyzer_is_the_identity() {
        for word in ["running", "indici", "walkdir", ""] {
            assert_eq!(stem(Analyzer::None, word), word);
        }
    }
}
