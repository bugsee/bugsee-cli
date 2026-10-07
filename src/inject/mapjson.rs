//! Streaming reads and rewrites of source-map JSON.
//!
//! A source map is routinely tens of MB, mostly one huge `mappings` string and
//! an array of whole source files (`sourcesContent`). Parsing it into a
//! `serde_json::Value` held ~2x the file on the heap just to look at, or change,
//! a handful of top-level keys. Everything here walks the document once with
//! [`struson`] instead, so memory stays at the reader's small buffer plus the
//! one member name / id string being looked at — whatever the map's size.
//!
//! Syntax is still validated end to end: a skipped value is checked like a read
//! one, and trailing garbage after the top-level value is an error.

use std::io::{Read, Write};

use struson::reader::{JsonReader, JsonStreamReader, ReaderError, ReaderSettings, ValueType};
use struson::writer::{JsonStreamWriter, JsonWriter};

/// Why a streaming pass over a map failed.
#[derive(Debug)]
pub enum MapJsonError {
    /// The input is not (this shape of) valid JSON.
    Json(String),
    /// Writing the rewritten copy failed.
    Io(std::io::Error),
}

impl std::fmt::Display for MapJsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MapJsonError::Json(m) => f.write_str(m),
            MapJsonError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl From<ReaderError> for MapJsonError {
    fn from(e: ReaderError) -> Self {
        match e {
            // Invalid UTF-8 inside a string surfaces as an I/O `InvalidData` error;
            // keep it one (as `read_to_string` reported it) rather than "bad JSON".
            ReaderError::IoError { error, .. } => MapJsonError::Io(error),
            other => MapJsonError::Json(other.to_string()),
        }
    }
}

impl From<struson::reader::TransferError> for MapJsonError {
    fn from(e: struson::reader::TransferError) -> Self {
        match e {
            struson::reader::TransferError::ReaderError(r) => r.into(),
            struson::reader::TransferError::WriterError(w) => MapJsonError::Io(w),
        }
    }
}

impl From<std::io::Error> for MapJsonError {
    fn from(e: std::io::Error) -> Self {
        MapJsonError::Io(e)
    }
}

type Res<T> = std::result::Result<T, MapJsonError>;

/// A reader that does NOT apply struson's default "restrict number values" (it rejects
/// an exponent beyond ±99 and any number over 100 characters). That guard protects code
/// that PARSES numbers into big integers; this module never parses one, it only skips or
/// copies the number's text. Left on, valid JSON such as `1e100` made a copy fail, and
/// `--strip-sources-content` then fell back to uploading the unstripped map: the very
/// source the flag exists to keep off the wire.
fn reader<R: Read>(input: R) -> JsonStreamReader<R> {
    JsonStreamReader::new_custom(
        input,
        ReaderSettings {
            restrict_number_values: false,
            ..Default::default()
        },
    )
}

/// What [`top_level_strings`] found.
pub struct TopLevel {
    /// Whether the document's top-level value is an object.
    pub is_object: bool,
    /// One entry per requested key, in order: its value when it is a string,
    /// else `None` (absent, or any other JSON type). With a duplicated key the
    /// LAST occurrence wins, as it does in a parsed `serde_json::Value`.
    pub values: Vec<Option<String>>,
}

/// Read the string values of the given top-level keys, validating the whole
/// document. A top-level value that is not an object yields `is_object: false`
/// and no values (it is still syntax-checked).
pub fn top_level_strings<R: Read>(input: R, keys: &[&str]) -> Res<TopLevel> {
    let mut r = reader(input);
    let mut values: Vec<Option<String>> = vec![None; keys.len()];
    let is_object = r.peek()? == ValueType::Object;
    if is_object {
        r.begin_object()?;
        while r.has_next()? {
            let idx = {
                let name = r.next_name()?;
                keys.iter().position(|k| *k == name)
            };
            match idx {
                Some(i) if r.peek()? == ValueType::String => values[i] = Some(r.next_string()?),
                Some(i) => {
                    values[i] = None;
                    r.skip_value()?;
                }
                None => r.skip_value()?,
            }
        }
        r.end_object()?;
    } else {
        r.skip_value()?;
    }
    r.consume_trailing_whitespace()?;
    Ok(TopLevel { is_object, values })
}

/// Copy `input` to `output`, dropping every `sourcesContent` — at the top level
/// AND inside the (nestable) `sections[i].map` of an indexed map. Returns
/// whether anything was dropped. Everything else is copied verbatim, in its
/// original order, without being held in memory.
///
/// Errors with [`MapJsonError::Json`] when the input is not a JSON object.
pub fn strip_sources_content<R: Read, W: Write>(input: R, output: W) -> Res<bool> {
    let mut r = reader(input);
    let mut w = JsonStreamWriter::new(output);
    if r.peek()? != ValueType::Object {
        return Err(MapJsonError::Json("not a JSON object".into()));
    }
    let removed = strip_object(&mut r, &mut w)?;
    r.consume_trailing_whitespace()?;
    w.finish_document()?;
    Ok(removed)
}

fn strip_object<R: JsonReader, W: JsonWriter>(r: &mut R, w: &mut W) -> Res<bool> {
    let mut removed = false;
    r.begin_object()?;
    w.begin_object()?;
    while r.has_next()? {
        let name = r.next_name()?.to_owned();
        if name == "sourcesContent" {
            r.skip_value()?;
            removed = true;
            continue;
        }
        w.name(&name)?;
        if name == "sections" && r.peek()? == ValueType::Array {
            r.begin_array()?;
            w.begin_array()?;
            while r.has_next()? {
                if r.peek()? == ValueType::Object {
                    removed |= strip_section(r, w)?;
                } else {
                    r.transfer_to(w)?;
                }
            }
            r.end_array()?;
            w.end_array()?;
        } else {
            r.transfer_to(w)?;
        }
    }
    r.end_object()?;
    w.end_object()?;
    Ok(removed)
}

/// One `sections[i]` object: only its `map` (when an object) is searched.
fn strip_section<R: JsonReader, W: JsonWriter>(r: &mut R, w: &mut W) -> Res<bool> {
    let mut removed = false;
    r.begin_object()?;
    w.begin_object()?;
    while r.has_next()? {
        let name = r.next_name()?.to_owned();
        w.name(&name)?;
        if name == "map" && r.peek()? == ValueType::Object {
            removed |= strip_object(r, w)?;
        } else {
            r.transfer_to(w)?;
        }
    }
    r.end_object()?;
    w.end_object()?;
    Ok(removed)
}

/// Copy `input` to `output` with the top-level `debug_id` and `debugId` set to
/// `debug_id` (any existing ones are dropped and the pair is written last).
/// Everything else is copied verbatim and in order.
///
/// Errors with [`MapJsonError::Json`] when the input is not a JSON object.
pub fn set_debug_ids<R: Read, W: Write>(input: R, output: W, debug_id: &str) -> Res<()> {
    let mut r = reader(input);
    let mut w = JsonStreamWriter::new(output);
    if r.peek()? != ValueType::Object {
        return Err(MapJsonError::Json("not a JSON object".into()));
    }
    r.begin_object()?;
    w.begin_object()?;
    while r.has_next()? {
        let name = r.next_name()?.to_owned();
        if name == "debug_id" || name == "debugId" {
            r.skip_value()?;
            continue;
        }
        w.name(&name)?;
        r.transfer_to(&mut w)?;
    }
    r.end_object()?;
    w.name("debug_id")?;
    w.string_value(debug_id)?;
    w.name("debugId")?;
    w.string_value(debug_id)?;
    w.end_object()?;
    r.consume_trailing_whitespace()?;
    w.finish_document()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> Res<(bool, String)> {
        let mut out = Vec::new();
        let removed = strip_sources_content(s.as_bytes(), &mut out)?;
        Ok((removed, String::from_utf8(out).unwrap()))
    }

    #[test]
    fn top_level_strings_picks_string_values_last_wins_and_ignores_other_types() {
        let t = top_level_strings(
            br#"{"debug_id": 5, "debugId": "camel", "uuid": "a", "uuid": "b", "x": {"debug_id": "nested"}}"#.as_slice(),
            &["debug_id", "debugId", "uuid"],
        )
        .unwrap();
        assert!(t.is_object);
        assert_eq!(
            t.values,
            [None, Some("camel".into()), Some("b".into())],
            "non-string -> None, duplicates -> last, nested keys ignored"
        );
        // A later non-string occurrence replaces an earlier string, as in a Value.
        let t = top_level_strings(br#"{"uuid": "a", "uuid": 1}"#.as_slice(), &["uuid"]).unwrap();
        assert_eq!(t.values, [None]);
    }

    #[test]
    fn top_level_strings_non_object_and_invalid_json() {
        for ok in ["[]", "3", "\"s\"", "null"] {
            let t = top_level_strings(ok.as_bytes(), &["uuid"]).unwrap();
            assert!(!t.is_object, "{ok}");
            assert_eq!(t.values, [None]);
        }
        for bad in [
            "",
            "{",
            r#"{"a":}"#,
            r#"{"a":1} x"#,
            r#"{"a":1}{"b":2}"#,
            "[1,]",
        ] {
            assert!(
                matches!(
                    top_level_strings(bad.as_bytes(), &["a"]),
                    Err(MapJsonError::Json(_))
                ),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn strip_removes_top_level_and_nested_section_sources_in_order() {
        let (removed, out) = strip(
            r#"{"version":3,"sourcesContent":["a"],"sections":[{"offset":{"line":0,"column":0},"map":{"sources":["x"],"sourcesContent":["secret"],"sections":[{"map":{"sourcesContent":["deep"],"names":[]}}]}},7],"mappings":"AAAA"}"#,
        )
        .unwrap();
        assert!(removed);
        assert_eq!(
            out,
            r#"{"version":3,"sections":[{"offset":{"line":0,"column":0},"map":{"sources":["x"],"sections":[{"map":{"names":[]}}]}},7],"mappings":"AAAA"}"#
        );
    }

    #[test]
    fn strip_reports_nothing_removed_and_ignores_non_object_sections() {
        let (removed, out) =
            strip(r#"{"sections":"nope","map":{"sourcesContent":[1]},"a":1.50}"#).unwrap();
        assert!(!removed, "only sections[].map is searched");
        assert!(
            out.contains(r#""a":1.50"#),
            "numbers keep their text: {out}"
        );
    }

    /// Numbers are copied as TEXT, however unusual: a large exponent or a very long integer
    /// is valid JSON, and refusing to copy it must never make a strip give up (which would
    /// upload the sources it was asked to drop).
    #[test]
    fn unusual_numbers_are_copied_verbatim_and_sources_are_still_stripped() {
        let long_int = "9".repeat(101);
        for n in ["1e100", "1e-100", "-2.5E+150", "1e999999", long_int.as_str(), "0.000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000000001"] {
            let doc = format!(r#"{{"n":{n},"sourcesContent":["secret"],"mappings":"AA"}}"#);
            let (removed, out) = strip(&doc).unwrap_or_else(|e| panic!("{n}: {e}"));
            assert!(removed, "{n}: the sources must still be stripped");
            assert_eq!(out, format!(r#"{{"n":{n},"mappings":"AA"}}"#), "{n}");
            let mut ids = Vec::new();
            set_debug_ids(doc.as_bytes(), &mut ids, "id").unwrap_or_else(|e| panic!("{n}: {e}"));
            assert!(String::from_utf8(ids).unwrap().contains(&format!(r#""n":{n}"#)), "{n}");
            assert_eq!(
                top_level_strings(doc.as_bytes(), &["mappings"]).unwrap().values,
                [Some("AA".to_string())]
            );
        }
    }

    #[test]
    fn strip_rejects_non_objects_and_bad_json() {
        assert!(matches!(strip("[]"), Err(MapJsonError::Json(_))));
        assert!(matches!(strip("{"), Err(MapJsonError::Json(_))));
        assert!(matches!(
            strip(r#"{"a":1} junk"#),
            Err(MapJsonError::Json(_))
        ));
    }

    #[test]
    fn set_debug_ids_replaces_existing_pair_and_keeps_the_rest() {
        let mut out = Vec::new();
        set_debug_ids(
            br#"{"debug_id":"old","version":3,"debugId":"older","mappings":"AA"}"#.as_slice(),
            &mut out,
            "new-id",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(out).unwrap(),
            r#"{"version":3,"mappings":"AA","debug_id":"new-id","debugId":"new-id"}"#
        );
        assert!(matches!(
            set_debug_ids(b"[]".as_slice(), Vec::new(), "x"),
            Err(MapJsonError::Json(_))
        ));
    }
}
