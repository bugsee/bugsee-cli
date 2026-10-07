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

/// Differential tests: the streaming implementations must agree with the
/// `serde_json::Value`-tree logic they replaced, over many generated documents
/// (whitespace, escapes, surrogate pairs, exponent numbers, nested sections).
#[cfg(test)]
mod differential {
    use super::*;
    use serde_json::Value;

    /// Deterministic xorshift: reproducible failures without a `rand` dependency.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
        fn pick<'a>(&mut self, xs: &[&'a str]) -> &'a str {
            xs[self.below(xs.len() as u64) as usize]
        }
        fn ws(&mut self) -> &'static str {
            ["", "", " ", "\n", "\t ", "\r\n  "][self.below(6) as usize]
        }
    }

    const KEYS: &[&str] = &[
        "version",
        "sources",
        "sourcesContent",
        "mappings",
        "debug_id",
        "debugId",
        "uuid",
        "sections",
        "map",
        "names",
        "file",
        "offset",
        "x\\u00e9y",
        "k",
    ];
    const STRINGS: &[&str] = &[
        r#""""#,
        r#""a""#,
        r#""h\u00e9llo""#,
        "\"h\u{e9}llo \u{2603}\"",
        r#""esc \n \" \\ \/ \t""#,
        r#""\ud83d\ude00""#,
        r#""AAAA;AACA,SAAS""#,
        r#""11111111-1111-1111-1111-111111111111""#,
    ];
    const NUMBERS: &[&str] = &[
        "0",
        "-1",
        "3",
        "1.50",
        "2e10",
        "-0.5E-3",
        "100000000000000000000",
        "1e100",
        "-2.5E-150",
        "99999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999999",
    ];

    fn scalar(r: &mut Rng) -> String {
        match r.below(5) {
            0 => "null".into(),
            1 => "true".into(),
            2 => "false".into(),
            3 => r.pick(NUMBERS).into(),
            _ => r.pick(STRINGS).into(),
        }
    }

    fn value(r: &mut Rng, depth: u32) -> String {
        if depth == 0 || r.below(3) == 0 {
            return scalar(r);
        }
        if r.below(2) == 0 {
            let n = r.below(4);
            let items: Vec<String> = (0..n).map(|_| value(r, depth - 1)).collect();
            let sep = format!("{},{}", r.ws(), r.ws());
            format!("[{}{}{}]", r.ws(), items.join(&sep), r.ws())
        } else {
            object(r, depth - 1, false)
        }
    }

    /// An object over the map vocabulary. `maplike` biases towards the keys the
    /// transforms care about and recurses into `sections[].map`. Keys are unique
    /// within an object (duplicates have their own targeted test).
    fn object(r: &mut Rng, depth: u32, maplike: bool) -> String {
        let n = r.below(6);
        let mut used = std::collections::HashSet::new();
        let mut members = Vec::new();
        for _ in 0..n {
            let key = r.pick(KEYS);
            if !used.insert(key) {
                continue;
            }
            let v = match key {
                "sections" if depth > 0 => {
                    let m = r.below(3);
                    let secs: Vec<String> = (0..m)
                        .map(|_| {
                            let mut parts = vec![format!(r#""offset":{}"#, value(r, 1))];
                            if r.below(4) != 0 {
                                parts.push(format!(r#""map":{}"#, object(r, depth - 1, true)));
                            }
                            if r.below(5) == 0 {
                                parts.push(format!(r#""url":{}"#, scalar(r)));
                            }
                            format!("{{{}}}", parts.join(","))
                        })
                        .collect();
                    format!("[{}]", secs.join(","))
                }
                "sourcesContent" if maplike || r.below(2) == 0 => {
                    format!(r#"["{}","src2"]"#, "x".repeat(r.below(200) as usize))
                }
                _ => value(r, depth.saturating_sub(1)),
            };
            members.push(format!("{}\"{}\"{}:{}{}", r.ws(), key, r.ws(), r.ws(), v));
        }
        format!("{{{}{}}}", members.join(","), r.ws())
    }

    fn doc(r: &mut Rng) -> String {
        format!("{}{}{}", r.ws(), object(r, 3, true), r.ws())
    }

    // --- the reference (old, tree-based) behaviour -------------------------------

    fn ref_strip(map: &mut serde_json::Map<String, Value>) -> bool {
        let mut removed = map.remove("sourcesContent").is_some();
        if let Some(Value::Array(sections)) = map.get_mut("sections") {
            for section in sections {
                if let Some(Value::Object(inner)) = section.get_mut("map") {
                    removed |= ref_strip(inner);
                }
            }
        }
        removed
    }

    fn ref_ids(v: &Value, keys: &[&str]) -> Vec<Option<String>> {
        keys.iter()
            .map(|k| v.get(*k).and_then(Value::as_str).map(str::to_string))
            .collect()
    }

    const CASES: u64 = 600;

    #[test]
    fn strip_sources_content_agrees_with_the_tree_implementation() {
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        let mut removed_some = 0;
        for case in 0..CASES {
            let text = doc(&mut r);
            let mut expected: Value = serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("generator produced invalid JSON ({e}): {text}"));
            let expect_removed = ref_strip(expected.as_object_mut().unwrap());
            let mut out = Vec::new();
            let removed = strip_sources_content(text.as_bytes(), &mut out)
                .unwrap_or_else(|e| panic!("case {case}: {e}\n{text}"));
            let got: Value = serde_json::from_slice(&out).unwrap_or_else(|e| {
                panic!(
                    "case {case}: output not JSON ({e}): {}",
                    String::from_utf8_lossy(&out)
                )
            });
            assert_eq!(got, expected, "case {case}\ninput:  {text}");
            assert_eq!(removed, expect_removed, "case {case}\ninput: {text}");
            removed_some += removed as u32;
        }
        assert!(
            removed_some > 50,
            "the generator must exercise stripping ({removed_some})"
        );
    }

    #[test]
    fn top_level_strings_agrees_with_the_tree_implementation() {
        let keys = ["debug_id", "debugId", "uuid"];
        let mut r = Rng(0xD1B5_4A32_D192_ED03);
        for case in 0..CASES {
            let text = doc(&mut r);
            let v: Value = serde_json::from_str(&text).unwrap();
            let got = top_level_strings(text.as_bytes(), &keys).unwrap();
            assert!(got.is_object);
            assert_eq!(got.values, ref_ids(&v, &keys), "case {case}\ninput: {text}");
        }
    }

    #[test]
    fn set_debug_ids_agrees_with_the_tree_implementation() {
        let mut r = Rng(0xA076_1D64_78BD_642F);
        for case in 0..CASES {
            let text = doc(&mut r);
            let mut expected: Value = serde_json::from_str(&text).unwrap();
            let obj = expected.as_object_mut().unwrap();
            obj.insert("debug_id".into(), Value::String("new-id".into()));
            obj.insert("debugId".into(), Value::String("new-id".into()));
            let mut out = Vec::new();
            set_debug_ids(text.as_bytes(), &mut out, "new-id").unwrap();
            let got: Value = serde_json::from_slice(&out).unwrap();
            assert_eq!(got, expected, "case {case}\ninput: {text}");
            // Idempotent: applying it to its own output changes nothing semantically.
            let mut again = Vec::new();
            set_debug_ids(out.as_slice(), &mut again, "new-id").unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&again).unwrap(), expected);
        }
    }

    #[test]
    fn duplicate_keys_resolve_like_a_parsed_value() {
        // serde_json keeps the LAST of a duplicated key; so must the stream.
        let text = r#"{"debug_id":"a","debug_id":"b","uuid":"u","uuid":7,"sourcesContent":["s"],"sourcesContent":["t"]}"#;
        let v: Value = serde_json::from_str(text).unwrap();
        let keys = ["debug_id", "uuid"];
        assert_eq!(
            top_level_strings(text.as_bytes(), &keys).unwrap().values,
            ref_ids(&v, &keys)
        );
        let mut out = Vec::new();
        assert!(strip_sources_content(text.as_bytes(), &mut out).unwrap());
        assert!(!String::from_utf8(out).unwrap().contains("sourcesContent"));
    }

    #[test]
    fn truncated_or_trailing_garbage_documents_are_rejected_everywhere() {
        let mut r = Rng(0x1234_5678_9ABC_DEF1);
        for case in 0..200 {
            let text = doc(&mut r);
            let text = text.trim_end();
            // Any strict prefix of a top-level object is invalid JSON.
            let cut = 1 + r.below(text.len() as u64 - 1) as usize;
            let mut cut = cut;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            let bad = &text[..cut];
            assert!(
                serde_json::from_str::<Value>(bad).is_err(),
                "case {case}: {bad}"
            );
            assert!(
                top_level_strings(bad.as_bytes(), &["uuid"]).is_err(),
                "case {case}: {bad}"
            );
            assert!(
                strip_sources_content(bad.as_bytes(), Vec::new()).is_err(),
                "case {case}: {bad}"
            );
            assert!(
                set_debug_ids(bad.as_bytes(), Vec::new(), "x").is_err(),
                "case {case}: {bad}"
            );
            // Trailing garbage after a complete document.
            let junk = format!("{text} x");
            assert!(
                top_level_strings(junk.as_bytes(), &["uuid"]).is_err(),
                "case {case}"
            );
            assert!(
                strip_sources_content(junk.as_bytes(), Vec::new()).is_err(),
                "case {case}"
            );
        }
    }

    #[test]
    fn a_large_map_streams_through_with_its_big_values_intact() {
        // Bigger than any internal buffer: a 3 MB mappings string must pass through
        // `transfer_to` byte for byte while a 3 MB sourcesContent entry is dropped.
        let mappings = "AAAA;".repeat(600_000);
        let content = "z".repeat(3_000_000);
        let text = format!(
            r#"{{"version":3,"sourcesContent":["{content}"],"mappings":"{mappings}","debug_id":"d"}}"#
        );
        let mut out = Vec::new();
        assert!(strip_sources_content(text.as_bytes(), &mut out).unwrap());
        let got: Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(got["mappings"].as_str().unwrap(), mappings);
        assert!(got.get("sourcesContent").is_none());
        assert_eq!(got["debug_id"], "d");
    }

    /// Numbers no double can hold, exponents beyond +-99 and very long integers are valid JSON
    /// grammar. They are skipped when reading ids and COPIED VERBATIM by the rewrites: the
    /// stream never parses a number, so it must not refuse one (a refusal made
    /// `--strip-sources-content` give up and upload the sources it was asked to drop).
    #[test]
    fn out_of_range_and_huge_numbers_copy_verbatim() {
        let long = "7".repeat(150);
        for n in ["1e999999", "1e100", "-3E-120", long.as_str()] {
            let doc = format!(r#"{{"n":{n},"sourcesContent":["x"],"debug_id":"d"}}"#);
            assert_eq!(
                top_level_strings(doc.as_bytes(), &["debug_id"])
                    .unwrap()
                    .values,
                [Some("d".to_string())]
            );
            let mut out = Vec::new();
            assert!(
                strip_sources_content(doc.as_bytes(), &mut out).unwrap(),
                "{n}"
            );
            assert_eq!(
                String::from_utf8(out).unwrap(),
                format!(r#"{{"n":{n},"debug_id":"d"}}"#)
            );
            let mut ids = Vec::new();
            set_debug_ids(doc.as_bytes(), &mut ids, "x").unwrap();
            assert!(
                String::from_utf8(ids)
                    .unwrap()
                    .contains(&format!(r#""n":{n}"#)),
                "{n}"
            );
        }
    }

    /// 200 000 randomly corrupted documents (byte replaced / deleted / inserted):
    /// the stream accepts exactly what `serde_json` accepts, and everything it
    /// copies out re-parses — it never launders invalid JSON into an upload.
    #[test]
    fn random_corruption_is_accepted_exactly_when_serde_accepts_it() {
        let base = br#"{"version":3,"file":"a\u00e9.js","sources":["a.ts","b\"q"],"sourcesContent":["x\ny","\ud83d\ude00"],"names":["n1"],"mappings":"AAAA;AACA","x":[1,2.5e3,-0,true,null,{"k":"v"}],"y":[1e100,-2.5E-150]}"#;
        let mut r = Rng(0x1234_5678_9ABC_DEF1);
        for case in 0..200_000 {
            let mut b = base.to_vec();
            for _ in 0..1 + r.below(2) {
                let at = r.below(b.len() as u64) as usize;
                match r.below(3) {
                    0 => b[at] = r.next() as u8,
                    1 => {
                        b.remove(at);
                    }
                    _ => b.insert(at, r.next() as u8),
                }
            }
            let serde_res = serde_json::from_slice::<Value>(&b);
            // The one deliberate difference: a number no f64 holds is rejected by serde
            // ("number out of range") but is valid JSON the stream copies verbatim.
            let range_only = serde_res
                .as_ref()
                .err()
                .is_some_and(|e| e.to_string().contains("number out of range"));
            let mut out = Vec::new();
            let streamed_ok = strip_sources_content(b.as_slice(), &mut out).is_ok();
            assert!(
                streamed_ok == serde_res.is_ok() || (streamed_ok && range_only),
                "case {case} disagrees on {:?}",
                String::from_utf8_lossy(&b)
            );
            if streamed_ok && !range_only {
                assert!(
                    serde_json::from_slice::<Value>(&out).is_ok(),
                    "case {case}: output is not valid JSON: {}",
                    String::from_utf8_lossy(&out)
                );
            }
        }
    }
}
