//! JSON for the WASM and CLI boundaries, without serde.
//!
//! The shape is small and fixed, so a hand-written serializer is both smaller and
//! faster than deriving one. Strings are escaped; snippets are plain text, never
//! HTML, and the renderer is responsible for escaping before it inserts them into
//! the DOM.
use alloc::string::String;
use core::fmt::Write;

use crate::facets::Filter;
use crate::index::{Hit, Index, SearchReport};

/// Escape a string for a JSON string literal.
pub fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 2);
    out.push('"');
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            // A static-site deploy inlines this JSON in an HTML document, so the
            // characters that can end an HTML context are escaped even though JSON
            // does not require it. Parsing the escape back yields the same bytes,
            // which keeps `highlights` (byte ranges into the snippet) valid.
            '<' => out.push_str("\\u003c"),
            '>' => out.push_str("\\u003e"),
            '&' => out.push_str("\\u0026"),
            ch if ch == '\u{2028}' || ch == '\u{2029}' => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch if (ch as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", ch as u32);
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

/// `{"results":[...]}` — the contract shared by the wasm module and `--json`.
pub fn hits_to_json(hits: &[Hit]) -> String {
    let mut out = String::with_capacity(64 + hits.len() * 160);
    out.push_str("{\"results\":[");
    write_hits(&mut out, hits);
    out.push_str("]}");
    out
}

/// `{"results":[...],"filters":[...]}` — the same contract plus the constraint
/// the engine applied, so a UI can render the active taxonomy without keeping its
/// own copy of the query string. With no filters this is exactly [`hits_to_json`].
pub fn hits_to_json_with_filters(hits: &[Hit], filters: &[Filter<'_>]) -> String {
    if filters.is_empty() {
        return hits_to_json(hits);
    }
    let mut out = String::with_capacity(96 + hits.len() * 160 + filters.len() * 48);
    out.push_str("{\"results\":[");
    write_hits(&mut out, hits);
    out.push_str("],\"filters\":[");
    for (index, filter) in filters.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"field\":{},\"value\":{}}}",
            json_escape(filter.field),
            json_escape(filter.value)
        );
    }
    out.push_str("]}");
    out
}

/// `{"results":[...],"corrected":[{"from":..,"to":..}]}` — F8's answer.
///
/// `corrected` is present only when the engine actually changed a token, so a
/// query with no typo keeps the exact shape of [`hits_to_json`].
pub fn search_report_to_json(report: &SearchReport) -> String {
    let mut out = String::with_capacity(64 + report.hits.len() * 160);
    out.push_str("{\"results\":[");
    write_hits(&mut out, &report.hits);
    out.push(']');
    write_corrected(&mut out, &report.corrected);
    out.push('}');
    out
}

/// The same contract plus `filters`, the constraint the engine applied.
pub fn search_report_to_json_with_filters(report: &SearchReport, filters: &[Filter<'_>]) -> String {
    let mut out = String::with_capacity(96 + report.hits.len() * 160 + filters.len() * 48);
    out.push_str("{\"results\":[");
    write_hits(&mut out, &report.hits);
    out.push(']');
    write_corrected(&mut out, &report.corrected);
    write_filters(&mut out, filters);
    out.push('}');
    out
}

fn write_corrected(out: &mut String, corrected: &[crate::index::Correction]) {
    if corrected.is_empty() {
        return;
    }
    out.push_str(",\"corrected\":[");
    for (index, correction) in corrected.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"from\":{},\"to\":{}}}",
            json_escape(&correction.from),
            json_escape(&correction.to)
        );
    }
    out.push(']');
}

fn write_filters(out: &mut String, filters: &[Filter<'_>]) {
    if filters.is_empty() {
        return;
    }
    out.push_str(",\"filters\":[");
    for (index, filter) in filters.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"field\":{},\"value\":{}}}",
            json_escape(filter.field),
            json_escape(filter.value)
        );
    }
    out.push(']');
}

/// `{"lang":{"en":812,"it":20},...}` — the taxonomy the build recorded.
///
/// An artifact with no facet section serialises to `{}`, so a client can render
/// its filter UI from the response instead of hard-coding the site's fields.
pub fn facets_to_json(index: &Index) -> String {
    let mut out = String::with_capacity(128);
    out.push('{');
    for (field_index, field) in index.facet_fields().iter().enumerate() {
        if field_index > 0 {
            out.push(',');
        }
        let _ = write!(out, "{}:{{", json_escape(field));
        for (value_index, (value, count)) in index
            .facet_values(field)
            .unwrap_or_default()
            .iter()
            .enumerate()
        {
            if value_index > 0 {
                out.push(',');
            }
            let _ = write!(out, "{}:{count}", json_escape(value));
        }
        out.push('}');
    }
    out.push('}');
    out
}

fn write_hits(out: &mut String, hits: &[Hit]) {
    for (index, hit) in hits.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        // `matches` is how many documents the hit stands for: 1 without grouping,
        // the chunk count of the page with it. It is additive: every other key keeps
        // its meaning and its place.
        let _ = write!(
            out,
            "{{\"docId\":{},\"score\":{:.4},\"matches\":{},\"url\":{},\"title\":{},\"snippet\":{},\"highlights\":[",
            hit.doc_id,
            hit.score,
            hit.matches,
            json_escape(&hit.url),
            json_escape(&hit.title),
            json_escape(&hit.snippet),
        );
        for (range_index, (start, end)) in hit.highlights.iter().enumerate() {
            if range_index > 0 {
                out.push(',');
            }
            let _ = write!(out, "[{start},{end}]");
        }
        out.push_str("]}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn escapes_the_dangerous_characters() {
        assert_eq!(json_escape("<b>&"), "\"\\u003cb\\u003e\\u0026\"");
        assert_eq!(json_escape("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(json_escape("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn an_empty_result_set_is_still_an_object() {
        assert_eq!(hits_to_json(&[]), "{\"results\":[]}");
    }

    #[test]
    fn filters_are_echoed_only_when_they_were_applied() {
        assert_eq!(hits_to_json_with_filters(&[], &[]), "{\"results\":[]}");
        let json = hits_to_json_with_filters(
            &[],
            &[Filter::new("lang", "en"), Filter::new("tag", "a\"b")],
        );
        assert!(json.starts_with("{\"results\":[],\"filters\":["));
        assert!(json.contains("{\"field\":\"lang\",\"value\":\"en\"}"));
        // A quote inside a value is escaped, so the response stays parseable.
        assert!(json.contains("{\"field\":\"tag\",\"value\":\"a\\\"b\"}"));
    }

    #[test]
    fn a_report_without_a_correction_keeps_the_old_shape() {
        let report = SearchReport::default();
        assert_eq!(search_report_to_json(&report), "{\"results\":[]}");
        assert_eq!(
            search_report_to_json_with_filters(&report, &[]),
            "{\"results\":[]}"
        );
    }

    #[test]
    fn a_correction_is_reported_next_to_the_results() {
        let report = SearchReport {
            corrected: vec![crate::index::Correction {
                from: "postgress".into(),
                to: "postgres".into(),
            }],
            ..SearchReport::default()
        };
        let json = search_report_to_json(&report);
        assert_eq!(
            json,
            "{\"results\":[],\"corrected\":[{\"from\":\"postgress\",\"to\":\"postgres\"}]}"
        );
        let filtered = search_report_to_json_with_filters(&report, &[Filter::new("lang", "en")]);
        assert!(filtered.contains("\"corrected\":["));
        assert!(filtered.contains("\"filters\":[{\"field\":\"lang\",\"value\":\"en\"}]"));
    }

    #[test]
    fn one_hit_has_the_documented_keys() {
        let hit = Hit {
            doc_id: 3,
            score: 1.5,
            url: "/a/".into(),
            title: "A".into(),
            snippet: "hello <b>".into(),
            highlights: vec![(6, 9)],
            matches: 4,
        };
        let json = hits_to_json(&[hit]);
        assert!(json.contains("\"docId\":3"));
        assert!(json.contains("\"score\":1.5000"));
        assert!(json.contains("\"matches\":4"));
        assert!(json.contains("\"highlights\":[[6,9]]"));
        assert!(json.contains("hello \\u003cb\\u003e"));
    }
}
