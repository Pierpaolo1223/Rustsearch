//! The optional title postings (v4, F3): a second postings list per dictionary term,
//! over the words in the page title.
//!
//! The dictionary stays the single place a term is named: an entry is 14 bytes and
//! its postings are the **body** list. This section adds, for the terms that occur
//! in a title, where their title list lives:
//!
//! ```text
//! u32 count                                  terms with title postings, ≤ 2M
//! entry[count], sorted by term_index, 16 bytes each:
//!   u32 term_index                           into the term dictionary
//!   u32 title_df                             postings in this list (= read_list's count)
//!   u32 postings_off                         from the section start, after the table
//!   u32 postings_len                         exact: read_list consumes it whole
//! postings blobs, in entry order
//! ```
//!
//! `title_df` is stored rather than prefixed to the blob because the postings are
//! ranked with the same `read_list` the body uses, and that decoder takes the count
//! up front. Four bytes per title term buys a reader that never has to look at the
//! blob to know its length, and the count is checked against `doc_count` before it
//! is used.
//!
//! Only terms with a non-empty list have an entry, so the common case — a title term
//! that also appears in the body — costs 16 bytes and no dictionary growth.
use crate::error::Error;
use crate::format::u32_at;

/// Bytes of one title entry.
pub const TITLE_ENTRY: usize = 16;
/// A corrupt header cannot make the reader accept an implausible table.
pub const MAX_TITLE_TERMS: u32 = 2_000_000;

/// A bounds-checked view of the `titlepostings` section.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TitlePostings<'a> {
    data: &'a [u8],
    count: usize,
    table: &'a [u8],
}

impl<'a> TitlePostings<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Result<Self, Error> {
        let count = u32_at(data, 0).ok_or(Error::BadCount)? as usize;
        if count == 0 || count as u32 > MAX_TITLE_TERMS {
            return Err(Error::BadCount);
        }
        let table_len = count.checked_mul(TITLE_ENTRY).ok_or(Error::BadCount)?;
        let table = data
            .get(4..4usize.checked_add(table_len).ok_or(Error::BadCount)?)
            .ok_or(Error::OutOfBounds)?;
        Ok(Self { data, count, table })
    }

    /// The length of the section this view was built over, for `validate`.
    pub(crate) fn raw_len(&self) -> usize {
        self.data.len()
    }

    /// `(term_index, title_df, postings_off, postings_len)` of entry `index`.
    fn entry(&self, index: usize) -> Result<(u32, u32, u32, u32), Error> {
        if index >= self.count {
            return Err(Error::OutOfBounds);
        }
        let at = index * TITLE_ENTRY;
        Ok((
            u32_at(self.table, at).ok_or(Error::OutOfBounds)?,
            u32_at(self.table, at + 4).ok_or(Error::OutOfBounds)?,
            u32_at(self.table, at + 8).ok_or(Error::OutOfBounds)?,
            u32_at(self.table, at + 12).ok_or(Error::OutOfBounds)?,
        ))
    }

    fn term_index(&self, index: usize) -> Result<u32, Error> {
        u32_at(self.table, index * TITLE_ENTRY).ok_or(Error::OutOfBounds)
    }

    /// The title list of a dictionary term: `None` when the term never occurs in a
    /// title. Binary search on the term index, which is what keeps this table usable
    /// without a second dictionary.
    pub(crate) fn lookup(&self, term_index: usize) -> Result<Option<(u32, &'a [u8])>, Error> {
        let needle = term_index as u32;
        let (mut low, mut high) = (0usize, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            if self.term_index(mid)? < needle {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        if low >= self.count || self.term_index(low)? != needle {
            return Ok(None);
        }
        let (_, df, off, len) = self.entry(low)?;
        let start = off as usize;
        let end = start.checked_add(len as usize).ok_or(Error::OutOfBounds)?;
        let bytes = self.data.get(start..end).ok_or(Error::OutOfBounds)?;
        Ok(Some((df, bytes)))
    }

    /// Check the whole table at load: term indices strictly increasing and inside the
    /// dictionary, counts plausible, blobs after the table, non-overlapping and
    /// ascending. Whether a blob *decodes* is checked when the term is queried, the
    /// same way a body posting list is.
    pub(crate) fn validate(
        &self,
        term_count: u32,
        doc_count: u32,
        section_len: usize,
    ) -> Result<(), Error> {
        let table_end = 4usize + self.count * TITLE_ENTRY;
        let mut previous_index: Option<u32> = None;
        let mut previous_end = table_end;
        for index in 0..self.count {
            let (term_index, df, off, len) = self.entry(index)?;
            if term_index >= term_count || df == 0 || df > doc_count {
                return Err(Error::BadTitlePostings);
            }
            if let Some(previous) = previous_index {
                if previous >= term_index {
                    return Err(Error::BadTitlePostings);
                }
            }
            previous_index = Some(term_index);
            let off = off as usize;
            let end = off
                .checked_add(len as usize)
                .ok_or(Error::BadTitlePostings)?;
            if off < previous_end || end > section_len {
                return Err(Error::BadTitlePostings);
            }
            previous_end = end;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Build a section by hand, mirroring `rustsearch-index`'s encoder.
    fn section_of(entries: &[(u32, u32, Vec<u8>)]) -> Vec<u8> {
        let table = 4 + entries.len() * TITLE_ENTRY;
        let mut out = Vec::new();
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        out.resize(table, 0);
        let mut offset = table;
        for (index, (term_index, df, blob)) in entries.iter().enumerate() {
            let at = 4 + index * TITLE_ENTRY;
            out[at..at + 4].copy_from_slice(&term_index.to_le_bytes());
            out[at + 4..at + 8].copy_from_slice(&df.to_le_bytes());
            out[at + 8..at + 12].copy_from_slice(&(offset as u32).to_le_bytes());
            out[at + 12..at + 16].copy_from_slice(&(blob.len() as u32).to_le_bytes());
            out.extend_from_slice(blob);
            offset += blob.len();
        }
        out
    }

    /// One all-one posting block for `docs`, as `write_list` would emit it.
    fn list(docs: &[u32]) -> Vec<u8> {
        let mut bytes = Vec::new();
        crate::postings::write_list(docs, &vec![1u32; docs.len()], &mut bytes).unwrap();
        bytes
    }

    #[test]
    fn finds_a_terms_title_list_and_reports_a_miss() {
        let bytes = section_of(&[(3, 2, list(&[1, 7])), (9, 1, list(&[4]))]);
        let title = TitlePostings::new(&bytes).unwrap();
        title.validate(10, 8, bytes.len()).unwrap();
        let (df, blob) = title.lookup(3).unwrap().unwrap();
        assert_eq!(df, 2);
        assert_eq!(blob, &list(&[1, 7])[..]);
        assert!(title.lookup(2).unwrap().is_none());
        assert!(title.lookup(12).unwrap().is_none(), "past the last entry");
        assert_eq!(title.lookup(9).unwrap().unwrap().0, 1);
    }

    #[test]
    fn rejects_a_table_that_is_not_sorted_by_term_index() {
        let bytes = section_of(&[(9, 1, list(&[4])), (3, 2, list(&[1, 7]))]);
        let title = TitlePostings::new(&bytes).unwrap();
        assert_eq!(
            title.validate(10, 8, bytes.len()),
            Err(Error::BadTitlePostings)
        );
    }

    #[test]
    fn rejects_a_term_index_past_the_dictionary_and_an_impossible_count() {
        let bytes = section_of(&[(10, 1, list(&[4]))]);
        assert_eq!(
            TitlePostings::new(&bytes)
                .unwrap()
                .validate(10, 8, bytes.len()),
            Err(Error::BadTitlePostings)
        );
        let bytes = section_of(&[(1, 9, list(&[4]))]);
        assert_eq!(
            TitlePostings::new(&bytes)
                .unwrap()
                .validate(10, 8, bytes.len()),
            Err(Error::BadTitlePostings)
        );
        let bytes = section_of(&[(1, 0, Vec::new())]);
        assert_eq!(
            TitlePostings::new(&bytes)
                .unwrap()
                .validate(10, 8, bytes.len()),
            Err(Error::BadTitlePostings)
        );
    }

    #[test]
    fn rejects_a_blob_before_the_table_or_past_the_section() {
        let mut bytes = section_of(&[(1, 1, list(&[4]))]);
        // Point the first blob back at the count word.
        bytes[4 + 8..4 + 12].copy_from_slice(&0u32.to_le_bytes());
        assert_eq!(
            TitlePostings::new(&bytes)
                .unwrap()
                .validate(10, 8, bytes.len()),
            Err(Error::BadTitlePostings)
        );

        let mut bytes = section_of(&[(1, 1, list(&[4]))]);
        let len = bytes.len() as u32;
        bytes[4 + 12..4 + 16].copy_from_slice(&(len + 1).to_le_bytes());
        assert_eq!(
            TitlePostings::new(&bytes)
                .unwrap()
                .validate(10, 8, bytes.len()),
            Err(Error::BadTitlePostings)
        );
    }

    #[test]
    fn rejects_an_empty_or_truncated_section() {
        assert!(TitlePostings::new(&[]).is_err());
        assert!(TitlePostings::new(&[0, 0, 0, 0]).is_err(), "count 0");
        let bytes = section_of(&[(1, 1, list(&[4]))]);
        assert!(TitlePostings::new(&bytes[..8]).is_err(), "truncated table");
    }
}
