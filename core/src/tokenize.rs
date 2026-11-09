//! The tokenizer: the one function the builder and the query engine must agree on.
//!
//! It is deliberately simple: split on anything that is not alphanumeric, lowercase
//! with the Unicode rules, drop tokens longer than [`MAX_TOKEN_BYTES`]. Stemming and
//! stopwords are a language decision, so they live one level up in
//! [`crate::analyze::Analyzer`]; the tokenizer itself is language-neutral and stays
//! the same for every artifact.
//!
//! A token is a [`Cow`] rather than a `String`: a run that is already lowercase ASCII
//! borrows the page text, which is the common case and removes one allocation per
//! token from a build (1.55M of them on the reference corpus). A run with uppercase
//! or non-ASCII characters is folded into an owned string, because its lowercase form
//! can differ in length (`İ` is two chars).
use alloc::borrow::Cow;
use alloc::string::String;
use alloc::vec::Vec;

/// Tokens longer than this are not indexed (a data URI or a minified hash, not a word).
pub const MAX_TOKEN_BYTES: usize = 64;

/// Tokenize `text`, preserving order and duplicates (the builder counts term frequencies).
pub fn tokenize(text: &str) -> Vec<Cow<'_, str>> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (at, ch) in text.char_indices() {
        if ch.is_alphanumeric() {
            if start.is_none() {
                start = Some(at);
            }
        } else if let Some(begin) = start.take() {
            push(&mut out, &text[begin..at]);
        }
    }
    if let Some(begin) = start {
        push(&mut out, &text[begin..]);
    }
    out
}

/// Push one run, lowering it only when it is not already lowercase.
fn push<'a>(out: &mut Vec<Cow<'a, str>>, run: &'a str) {
    if run.is_ascii() {
        // ASCII lowercasing never changes the byte length, so the cap can be checked
        // before the (only) allocation the run may need.
        if run.len() > MAX_TOKEN_BYTES {
            return;
        }
        if run.bytes().any(|byte| byte.is_ascii_uppercase()) {
            out.push(Cow::Owned(run.to_ascii_lowercase()));
        } else {
            out.push(Cow::Borrowed(run));
        }
        return;
    }
    let lowered: String = run.chars().flat_map(char::to_lowercase).collect();
    if lowered.len() > MAX_TOKEN_BYTES {
        return;
    }
    out.push(Cow::Owned(lowered));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(text: &str) -> Vec<String> {
        tokenize(text).into_iter().map(Cow::into_owned).collect()
    }

    #[test]
    fn splits_on_punctuation_and_lowercases() {
        assert_eq!(plain("Hello, WORLD! 42"), ["hello", "world", "42"]);
        assert_eq!(
            plain("snake_case-kebab.dot"),
            ["snake", "case", "kebab", "dot"]
        );
        assert_eq!(plain("Café déjà vu"), ["café", "déjà", "vu"]);
    }

    #[test]
    fn an_already_lowercase_run_is_borrowed() {
        let text = "immutable index";
        let tokens = tokenize(text);
        assert_eq!(plain(text), ["immutable", "index"]);
        for token in &tokens {
            assert!(
                matches!(token, Cow::Borrowed(_)),
                "{token:?} should borrow the page text"
            );
        }
        // Uppercase and non-ASCII runs are folded into owned strings instead.
        assert!(matches!(tokenize("Index")[0], Cow::Owned(_)));
        assert!(matches!(tokenize("café")[0], Cow::Owned(_)));
    }

    #[test]
    fn keeps_unicode_case_folding_short() {
        // `İ`.to_lowercase() is two chars; the token is still valid UTF-8.
        let tokens = tokenize("İstanbul");
        assert_eq!(tokens.len(), 1);
        assert!(tokens[0].starts_with('i'));
    }

    #[test]
    fn drops_a_token_that_would_exceed_the_cap() {
        let long = "a".repeat(MAX_TOKEN_BYTES + 1);
        assert!(tokenize(&long).is_empty());
        assert_eq!(tokenize(&"a".repeat(MAX_TOKEN_BYTES)).len(), 1);
        // The cap counts the lowercased bytes, not the source bytes: `İ` is two
        // bytes in and three out, so 21 of them (63 lowered bytes) survive and 22
        // (66) do not.
        assert_eq!(tokenize(&"İ".repeat(21)).len(), 1);
        assert_eq!(tokenize(&"İ".repeat(21))[0].len(), 63);
        assert!(tokenize(&"İ".repeat(22)).is_empty());
    }
}
