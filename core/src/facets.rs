//! The optional `facets` section (v4) and the query-level filter.
//!
//! A facet is a build-time taxonomy: a field name (`lang`, `section`, `tags`) and,
//! per value, the sorted list of documents that carry it. The artifact knows the
//! values; the query only picks among them, so a filter never has to be trusted
//! with a string it did not come from.
//!
//! ```text
//! facets:
//!   u16 field_count
//!   u32 field_offsets[field_count]          // from the section start, name order
//!   field:
//!     u16 name_len, name bytes
//!     u16 value_count
//!     u32 value_offsets[value_count]        // from the section start, value order
//!     value:
//!       u16 value_len, value bytes
//!       u32 doc_count
//!       doc_count varints: the first id absolute, then positive deltas
//! ```
//!
//! The offset tables are the same idiom as the term dictionary and the document
//! records: a field is found by binary search on its name, a value by binary search
//! on its name inside the field's table, and a doc list by decoding `doc_count`
//! varints. Nothing is scanned linearly and nothing is decoded twice. Because every
//! entry starts at an offset, a value's byte range is `its list start .. the next
//! value's offset` (or the field, or the section end), which is what lets the
//! validator require that each list ends exactly where it should.
//!
//! Values are opaque bytes from the site's build: a quote, `<` or a newline is
//! stored and returned unchanged. The renderer sets them with `textContent`.
use alloc::vec::Vec;

use crate::error::Error;
use crate::varint::read_u32;

/// Filters the engine accepts for one query: fields, values per field and bytes in
/// total. A UI sends a handful; anything larger is a bug, not a query.
pub const MAX_FILTER_FIELDS: usize = 8;
/// Values per field (they are OR-ed inside the field).
pub const MAX_FILTER_VALUES: usize = 8;
/// Total bytes of field names and values in one query, like [`crate::index::MAX_QUERY_BYTES`].
pub const MAX_FILTER_BYTES: usize = 512;

/// What the builder may write, enforced again by the reader.
pub const MAX_FACET_FIELDS: usize = 64;
/// Values per field, including each document's tags.
pub const MAX_FACET_VALUES: usize = 512;
/// `section`, `lang`, `tags`, `series`, `level`… all fit.
pub const MAX_FACET_NAME_BYTES: usize = 32;
/// Long enough for a path segment or a tag, short enough not to bloat the section.
pub const MAX_FACET_VALUE_BYTES: usize = 64;

/// One `field=value` constraint. Values in the same field are OR-ed, fields are
/// AND-ed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filter<'a> {
    pub field: &'a str,
    pub value: &'a str,
}

impl<'a> Filter<'a> {
    pub const fn new(field: &'a str, value: &'a str) -> Self {
        Self { field, value }
    }
}

/// A bounds-checked view of the `facets` section.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Facets<'a> {
    data: &'a [u8],
    count: usize,
    table: &'a [u8],
}

impl<'a> Facets<'a> {
    pub(crate) fn new(data: &'a [u8]) -> Result<Self, Error> {
        if data.len() < 2 {
            return Err(Error::BadFacets);
        }
        let count = u16::from_le_bytes([data[0], data[1]]) as usize;
        if count == 0 || count > MAX_FACET_FIELDS {
            return Err(Error::BadFacets);
        }
        let table_len = count.checked_mul(4).ok_or(Error::BadFacets)?;
        let table = data
            .get(2..2usize.checked_add(table_len).ok_or(Error::BadFacets)?)
            .ok_or(Error::BadFacets)?;
        Ok(Self { data, count, table })
    }

    fn field_off(&self, field: usize) -> Option<usize> {
        if field >= self.count {
            return None;
        }
        let bytes = self.table.get(field * 4..field * 4 + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    }

    fn field_end(&self, field: usize) -> Option<usize> {
        match self.field_off(field + 1) {
            Some(end) => Some(end),
            None if field + 1 == self.count => Some(self.data.len()),
            None => None,
        }
    }

    /// The name of field `field`, or `None` when the section is malformed.
    pub(crate) fn field_name(&self, field: usize) -> Option<&'a str> {
        let off = self.field_off(field)?;
        let len = u16::from_le_bytes([*self.data.get(off)?, *self.data.get(off + 1)?]) as usize;
        let end = off.checked_add(2)?.checked_add(len)?;
        if end > self.field_end(field)? {
            return None;
        }
        core::str::from_utf8(self.data.get(off + 2..end)?).ok()
    }

    /// Every field name, in section order.
    pub(crate) fn names(&self) -> impl Iterator<Item = &'a str> + '_ {
        (0..self.count).filter_map(|field| self.field_name(field))
    }

    /// Index of the field named `name`, by binary search.
    pub(crate) fn field_index(&self, name: &str) -> Option<usize> {
        let (mut low, mut high) = (0usize, self.count);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.field_name(mid) {
                Some(candidate) if candidate < name => low = mid + 1,
                Some(_) => high = mid,
                None => return None,
            }
        }
        (low < self.count && self.field_name(low) == Some(name)).then_some(low)
    }

    fn values_off(&self, field: usize) -> Option<(usize, usize)> {
        let off = self.field_off(field)?;
        let name_len =
            u16::from_le_bytes([*self.data.get(off)?, *self.data.get(off + 1)?]) as usize;
        let count_off = off.checked_add(2)?.checked_add(name_len)?;
        let count = u16::from_le_bytes([
            *self.data.get(count_off)?,
            *self.data.get(count_off.checked_add(1)?)?,
        ]) as usize;
        if count == 0 || count > MAX_FACET_VALUES {
            return None;
        }
        let table = count_off.checked_add(2)?;
        table.checked_add(count.checked_mul(4)?)?;
        Some((table, count))
    }

    pub(crate) fn values(&self, field: usize) -> Option<usize> {
        self.values_off(field).map(|(_, count)| count)
    }

    fn value_off(&self, field: usize, value: usize) -> Option<usize> {
        let (table, count) = self.values_off(field)?;
        if value >= count {
            return None;
        }
        let bytes = self.data.get(table + value * 4..table + value * 4 + 4)?;
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    }

    fn value_end(&self, field: usize, value: usize) -> Option<usize> {
        match self.value_off(field, value + 1) {
            Some(end) => Some(end),
            None if value + 1 == self.values(field)? => self.field_end(field),
            None => None,
        }
    }

    /// The name of `value` inside `field`, or `None` when it is malformed.
    pub(crate) fn value_name(&self, field: usize, value: usize) -> Option<&'a str> {
        let off = self.value_off(field, value)?;
        let len = u16::from_le_bytes([*self.data.get(off)?, *self.data.get(off + 1)?]) as usize;
        let end = off.checked_add(2)?.checked_add(len)?;
        if end > self.value_end(field, value)? {
            return None;
        }
        core::str::from_utf8(self.data.get(off + 2..end)?).ok()
    }

    /// Index of `name` inside `field`, by binary search.
    pub(crate) fn value_index(&self, field: usize, name: &str) -> Option<usize> {
        let count = self.values(field)?;
        let (mut low, mut high) = (0usize, count);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.value_name(field, mid) {
                Some(candidate) if candidate < name => low = mid + 1,
                Some(_) => high = mid,
                None => return None,
            }
        }
        (low < count && self.value_name(field, low) == Some(name)).then_some(low)
    }

    /// `(doc_count, varint bytes)` of one value's doc list.
    fn doc_list(&self, field: usize, value: usize) -> Option<(u32, &'a [u8])> {
        let off = self.value_off(field, value)?;
        let len = u16::from_le_bytes([*self.data.get(off)?, *self.data.get(off + 1)?]) as usize;
        let count_off = off.checked_add(2)?.checked_add(len)?;
        let end = self.value_end(field, value)?;
        if count_off.checked_add(4)? > end {
            return None;
        }
        let bytes = self.data.get(count_off..count_off + 4)?;
        let count = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        Some((count, self.data.get(count_off + 4..end)?))
    }

    /// The number of documents in one value's list.
    pub(crate) fn doc_count(&self, field: usize, value: usize) -> Option<u32> {
        self.doc_list(field, value).map(|(count, _)| count)
    }

    /// Decode one value's doc list into `out`, in ascending order.
    ///
    /// `out` is appended to, not cleared: a caller unions the lists of a field and
    /// sorts once.
    pub(crate) fn collect_docs(
        &self,
        field: usize,
        value: usize,
        out: &mut Vec<u32>,
    ) -> Result<(), Error> {
        let (count, bytes) = self.doc_list(field, value).ok_or(Error::BadFacets)?;
        let mut pos = 0usize;
        let mut doc: u32 = 0;
        for index in 0..count {
            let delta = read_u32(bytes, &mut pos).ok_or(Error::BadFacets)?;
            doc = if index == 0 {
                delta
            } else {
                doc.checked_add(delta).ok_or(Error::BadFacets)?
            };
            if delta == 0 && index > 0 {
                return Err(Error::BadFacets);
            }
            out.push(doc);
        }
        Ok(())
    }

    /// Check the whole section once, at load: offsets monotonic and inside the
    /// section, names non-empty and strictly sorted, doc lists ascending and below
    /// `doc_count`, every list ending exactly at the next entry.
    pub(crate) fn validate(&self, doc_count: u32) -> Result<(), Error> {
        let mut previous: Option<&str> = None;
        let mut previous_off = 2 + self.count * 4;
        for field in 0..self.count {
            let name = self.field_name(field).ok_or(Error::BadFacets)?;
            if name.is_empty() || name.len() > MAX_FACET_NAME_BYTES {
                return Err(Error::BadFacets);
            }
            if let Some(previous) = previous {
                if previous >= name {
                    return Err(Error::BadFacets);
                }
            }
            previous = Some(name);
            let off = self.field_off(field).ok_or(Error::BadFacets)?;
            if off < previous_off {
                return Err(Error::BadFacets);
            }
            let (table, values) = self.values_off(field).ok_or(Error::BadFacets)?;
            if table < off {
                return Err(Error::BadFacets);
            }
            previous_off = table;
            let mut previous_value: Option<&str> = None;
            for value in 0..values {
                let name = self.value_name(field, value).ok_or(Error::BadFacets)?;
                if name.is_empty() || name.len() > MAX_FACET_VALUE_BYTES {
                    return Err(Error::BadFacets);
                }
                if let Some(previous) = previous_value {
                    if previous >= name {
                        return Err(Error::BadFacets);
                    }
                }
                previous_value = Some(name);
                let off = self.value_off(field, value).ok_or(Error::BadFacets)?;
                if off < previous_off {
                    return Err(Error::BadFacets);
                }
                previous_off = off;
                let (count, bytes) = self.doc_list(field, value).ok_or(Error::BadFacets)?;
                if count == 0 || count > doc_count {
                    return Err(Error::BadFacets);
                }
                let mut pos = 0usize;
                let mut doc: u32 = 0;
                for index in 0..count {
                    let delta = read_u32(bytes, &mut pos).ok_or(Error::BadFacets)?;
                    let next = if index == 0 {
                        delta
                    } else {
                        doc.checked_add(delta).ok_or(Error::BadFacets)?
                    };
                    if index > 0 && delta == 0 {
                        return Err(Error::BadFacets);
                    }
                    if next >= doc_count {
                        return Err(Error::BadFacets);
                    }
                    doc = next;
                }
                // The list has to end on the entry boundary: a short list would
                // leave the next value's bytes unreachable, a long one would eat
                // into them.
                if pos != bytes.len() {
                    return Err(Error::BadFacets);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::{String, ToString};
    use alloc::vec;

    /// One field: its name and its `(value, doc ids)` pairs.
    type Field = (String, Vec<(String, Vec<u32>)>);

    /// Build a section by hand, mirroring `rustsearch-index`'s encoder.
    struct Builder {
        fields: Vec<Field>,
    }

    impl Builder {
        fn new() -> Self {
            Self { fields: Vec::new() }
        }

        fn field(&mut self, name: &str, values: &[(&str, &[u32])]) -> &mut Self {
            self.fields.push((
                name.to_string(),
                values
                    .iter()
                    .map(|(value, docs)| (value.to_string(), docs.to_vec()))
                    .collect(),
            ));
            self
        }

        fn build(&self) -> Vec<u8> {
            let header = 2 + self.fields.len() * 4;
            let mut entries: Vec<u8> = Vec::new();
            let mut field_offsets: Vec<u32> = Vec::new();
            for (name, values) in &self.fields {
                field_offsets.push((header + entries.len()) as u32);
                entries.extend_from_slice(&(name.len() as u16).to_le_bytes());
                entries.extend_from_slice(name.as_bytes());
                entries.extend_from_slice(&(values.len() as u16).to_le_bytes());
                let table = entries.len();
                entries.resize(table + values.len() * 4, 0);
                let mut value_offsets: Vec<u32> = Vec::new();
                for (value, docs) in values {
                    value_offsets.push((header + entries.len()) as u32);
                    entries.extend_from_slice(&(value.len() as u16).to_le_bytes());
                    entries.extend_from_slice(value.as_bytes());
                    entries.extend_from_slice(&(docs.len() as u32).to_le_bytes());
                    let mut previous = 0u32;
                    for (index, doc) in docs.iter().enumerate() {
                        let delta = if index == 0 { *doc } else { doc - previous };
                        crate::varint::write_u32(&mut entries, delta);
                        previous = *doc;
                    }
                }
                for (index, offset) in value_offsets.iter().enumerate() {
                    entries[table + index * 4..table + index * 4 + 4]
                        .copy_from_slice(&offset.to_le_bytes());
                }
            }
            let mut out = Vec::with_capacity(header + entries.len());
            out.extend_from_slice(&(self.fields.len() as u16).to_le_bytes());
            for offset in field_offsets {
                out.extend_from_slice(&offset.to_le_bytes());
            }
            out.extend_from_slice(&entries);
            out
        }
    }

    fn sample() -> Vec<u8> {
        Builder::new()
            .field("lang", &[("en", &[0, 2, 5]), ("it", &[1, 3])])
            .field("section", &[("guide", &[2, 5]), ("reference", &[0, 1, 3])])
            .build()
    }

    #[test]
    fn reads_fields_values_and_doc_lists() {
        let bytes = sample();
        let facets = Facets::new(&bytes).unwrap();
        facets.validate(6).unwrap();
        assert_eq!(facets.names().count(), 2);
        assert_eq!(facets.names().collect::<Vec<_>>(), vec!["lang", "section"]);
        assert_eq!(facets.field_index("section"), Some(1));
        assert_eq!(facets.field_index("mood"), None);
        assert_eq!(facets.value_index(0, "it"), Some(1));
        assert_eq!(facets.value_index(0, "de"), None);
        let mut docs = Vec::new();
        facets.collect_docs(0, 0, &mut docs).unwrap();
        assert_eq!(docs, vec![0, 2, 5]);
        assert_eq!(facets.doc_count(1, 1), Some(3));
    }

    #[test]
    fn validates_against_the_document_count() {
        let bytes = sample();
        let facets = Facets::new(&bytes).unwrap();
        assert!(facets.validate(6).is_ok());
        // The lists reach document 5: an artifact with fewer documents is corrupt.
        assert_eq!(facets.validate(5), Err(Error::BadFacets));
    }

    #[test]
    fn rejects_out_of_order_names_and_offsets() {
        let mut bytes = sample();
        let facets = Facets::new(&bytes).unwrap();
        facets.validate(6).unwrap();
        // Point the second field at the first field's entry: not monotonic.
        let second = 2 + 4;
        let first_off = Facets::new(&bytes).unwrap().field_off(0).unwrap() as u32;
        bytes[second..second + 4].copy_from_slice(&first_off.to_le_bytes());
        let broken = Facets::new(&bytes).unwrap();
        assert_eq!(broken.validate(6), Err(Error::BadFacets));
    }

    #[test]
    fn rejects_a_value_offset_that_points_back_into_the_table() {
        let mut bytes = sample();
        let facets = Facets::new(&bytes).unwrap();
        // The `lang` field's first value offset lives right after its name and
        // value count; rewrite it to point at the field table itself.
        let field_off = facets.field_off(0).unwrap();
        let name_len = u16::from_le_bytes([bytes[field_off], bytes[field_off + 1]]) as usize;
        let value_off = field_off + 2 + name_len + 2;
        bytes[value_off..value_off + 4].copy_from_slice(&(field_off as u32).to_le_bytes());
        let broken = Facets::new(&bytes).unwrap();
        assert_eq!(broken.validate(6), Err(Error::BadFacets));
    }

    #[test]
    fn a_truncated_doc_list_is_rejected_not_read_past() {
        let mut bytes = sample();
        bytes.pop();
        let facets = Facets::new(&bytes).unwrap();
        assert_eq!(facets.validate(6), Err(Error::BadFacets));
    }

    #[test]
    fn values_are_opaque_bytes_on_the_way_back() {
        let bytes = Builder::new()
            .field("tag", &[("a \"quote\" <b>\n", &[0])])
            .build();
        let facets = Facets::new(&bytes).unwrap();
        facets.validate(1).unwrap();
        assert_eq!(facets.value_name(0, 0), Some("a \"quote\" <b>\n"));
    }
}
