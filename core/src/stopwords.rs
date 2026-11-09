//! Stopword lists for the languages with a stemmer.
//!
//! Both tables are sorted by UTF-8 bytes, which is `str`'s `Ord`, so
//! [`Analyzer::is_stopword`](crate::analyze::Analyzer::is_stopword) can binary
//! search them. A unit test fails if a list stops being sorted, because that would
//! silently make half the entries unreachable.
//!
//! The lists are deliberately short and language-neutral-ish: articles, pronouns,
//! auxiliaries, prepositions and a handful of conjunctions. Removing more hurts
//! precision more than it helps, and BM25's idf already discounts common words.

/// Open-addressed fingerprints built from the list at compile time.
///
/// A stopword check runs once per token, so the binary search over 134 `&str` was
/// the second-biggest analyzer cost after the stemmer (~154 ms of a 1.82 s build on
/// the 10k corpus, 10.0M checks/s). The table turns the common case — a word that is
/// *not* a stopword — into one hash and one load; a fingerprint hit (31 useful
/// bits of the same hash, so about 1 in 2 billion for a word not in the list) falls
/// back to the binary search, which stays the authority.
///
/// The 32 bits compared are the high 32 bits of the FNV-1a of the word, so two
/// *different* words that land in one slot stay distinct unless those bits collide.
/// The list itself is unchanged: the table is an index into it, not a copy.
pub const TABLE_SLOTS: usize = 256;

const fn const_fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        index += 1;
    }
    hash
}

const fn fingerprints(words: &[&str]) -> [u32; TABLE_SLOTS] {
    let mut table = [0u32; TABLE_SLOTS];
    let mut index = 0;
    while index < words.len() {
        let hash = const_fnv1a64(words[index].as_bytes());
        // Bit 0 is forced on, because 0 marks an empty slot.
        let fingerprint = ((hash >> 8) as u32) | 1;
        let mut slot = (hash & (TABLE_SLOTS as u64 - 1)) as usize;
        while table[slot] != 0 && table[slot] != fingerprint {
            slot = (slot + 1) & (TABLE_SLOTS - 1);
        }
        table[slot] = fingerprint;
        index += 1;
    }
    table
}

pub const FINGERPRINTS_EN: [u32; TABLE_SLOTS] = fingerprints(&STOPWORDS_EN);
pub const FINGERPRINTS_IT: [u32; TABLE_SLOTS] = fingerprints(&STOPWORDS_IT);

/// True when `word` may be in the list the table was built from: a hit means "run
/// the binary search", and a miss means "it cannot be there".
pub fn might_be_stopword(table: &[u32; TABLE_SLOTS], word: &str) -> bool {
    let hash = const_fnv1a64(word.as_bytes());
    let mine = ((hash >> 8) as u32) | 1;
    let mut slot = (hash & (TABLE_SLOTS as u64 - 1)) as usize;
    loop {
        let stored = table[slot];
        if stored == 0 {
            return false;
        }
        if stored == mine {
            return true;
        }
        slot = (slot + 1) & (TABLE_SLOTS - 1);
    }
}

pub const STOPWORDS_EN: [&str; 134] = [
    "a",
    "about",
    "above",
    "after",
    "again",
    "against",
    "all",
    "also",
    "am",
    "an",
    "and",
    "any",
    "are",
    "as",
    "at",
    "be",
    "because",
    "been",
    "before",
    "being",
    "below",
    "between",
    "both",
    "but",
    "by",
    "can",
    "cannot",
    "could",
    "did",
    "do",
    "does",
    "doing",
    "done",
    "down",
    "during",
    "each",
    "few",
    "for",
    "from",
    "further",
    "had",
    "has",
    "have",
    "having",
    "he",
    "her",
    "here",
    "hers",
    "herself",
    "him",
    "himself",
    "his",
    "how",
    "i",
    "if",
    "in",
    "into",
    "is",
    "it",
    "its",
    "itself",
    "just",
    "me",
    "more",
    "most",
    "my",
    "myself",
    "no",
    "nor",
    "not",
    "now",
    "of",
    "off",
    "on",
    "once",
    "only",
    "or",
    "other",
    "our",
    "ours",
    "ourselves",
    "out",
    "over",
    "own",
    "same",
    "she",
    "should",
    "so",
    "some",
    "such",
    "than",
    "that",
    "the",
    "their",
    "theirs",
    "them",
    "themselves",
    "then",
    "there",
    "these",
    "they",
    "this",
    "those",
    "through",
    "to",
    "too",
    "under",
    "until",
    "up",
    "upon",
    "very",
    "via",
    "was",
    "we",
    "were",
    "what",
    "when",
    "where",
    "which",
    "while",
    "who",
    "whom",
    "whose",
    "why",
    "will",
    "with",
    "within",
    "without",
    "would",
    "you",
    "your",
    "yours",
    "yourself",
    "yourselves",
];

pub const STOPWORDS_IT: [&str; 160] = [
    "a", "abbiamo", "ad", "agli", "ai", "al", "alcune", "alcuni", "alla", "alle", "allo", "altra",
    "altre", "altri", "altro", "anche", "ancora", "avere", "avete", "aveva", "avevano", "che",
    "chi", "ci", "come", "con", "contro", "cosa", "cosi", "così", "cui", "da", "dagli", "dai",
    "dal", "dalla", "dalle", "dallo", "degli", "dei", "del", "della", "delle", "dello", "di",
    "dopo", "dove", "e", "ed", "era", "erano", "essere", "fa", "fare", "fatto", "fra", "già",
    "gli", "ha", "hai", "hanno", "ho", "i", "il", "in", "io", "la", "le", "lei", "lo", "loro",
    "lui", "ma", "mi", "mia", "mie", "miei", "mio", "molto", "ne", "negli", "nei", "nel", "nell",
    "nella", "nelle", "nello", "noi", "non", "o", "od", "ogni", "oppure", "per", "perche",
    "perché", "piu", "più", "prima", "puo", "può", "qualche", "qualcosa", "quale", "quali",
    "quando", "quanto", "quella", "quelle", "quelli", "quello", "questa", "queste", "questi",
    "questo", "quindi", "se", "sei", "sempre", "senza", "si", "sia", "siamo", "siete", "sono",
    "stata", "state", "stati", "stato", "su", "sua", "sue", "sugli", "sui", "sul", "sulla",
    "sulle", "sullo", "suo", "ti", "tra", "tu", "tua", "tue", "tuo", "tuoi", "tutta", "tutte",
    "tutti", "tutto", "un", "una", "uno", "vi", "voi", "vostra", "vostre", "vostri", "vostro", "è",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_lists_are_sorted_by_utf8_bytes() {
        for list in [&STOPWORDS_EN[..], &STOPWORDS_IT[..]] {
            assert!(
                list.windows(2).all(|pair| pair[0] < pair[1]),
                "stopword list is not sorted, binary search would miss entries"
            );
        }
    }
}
