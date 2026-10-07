//! Unity IL2CPP LineNumberMappings bundle discovery and packing.
//!
//! Bundle layout (one ZIP uploaded as `format: il2cpp-linemap`):
//!   LineNumberMappings.json  (required)
//!   MethodMap.tsv            (optional sibling)
//!   il2cppFileRoot.txt       (optional sibling)
//!   manifest.json            (written by the CLI)

use std::fs;
use std::path::{Path, PathBuf};

use struson::reader::{JsonReader, JsonStreamReader, ReaderError, ValueType};
use walkdir::WalkDir;

use super::suffix::ExtraSuffixes;
use crate::error::{config_invalid, input_invalid, input_not_found, Error};

pub const LINE_NUMBER_MAPPINGS: &str = "LineNumberMappings.json";
pub const METHOD_MAP: &str = "MethodMap.tsv";
pub const FILE_ROOT: &str = "il2cppFileRoot.txt";
pub const MANIFEST: &str = "manifest.json";
/// Optional top-level key the mappings document may carry; parsers ignore it
/// (docs/unity-il2cpp-linenumber-mappings.md, section 2.1).
const DEBUG_ID_SENTINEL: &str = "__debug-id__";

/// A discovered IL2CPP line-map directory (or a direct path to the JSON).
#[derive(Debug, Clone)]
pub struct LinemapBundle {
    pub json_path: PathBuf,
    pub method_map: Option<PathBuf>,
    pub file_root: Option<PathBuf>,
}

/// Discover `LineNumberMappings.json` under `paths` (file or directory walk).
/// A file whose name ends in one of `extra` is taken as the mappings JSON too;
/// its `MethodMap.tsv` / `il2cppFileRoot.txt` siblings are looked up the same
/// way.
pub fn discover(paths: &[PathBuf], extra: &ExtraSuffixes) -> Vec<LinemapBundle> {
    let mut found = Vec::new();
    for path in paths {
        if path.is_file() {
            if is_linemap_json(path) || extra.matches_path(path) {
                if let Some(bundle) = bundle_from_json(path) {
                    found.push(bundle);
                }
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_file() && (is_linemap_json(p) || extra.matches_path(p)) {
                if let Some(bundle) = bundle_from_json(p) {
                    found.push(bundle);
                }
            }
        }
    }
    found.sort_by(|a, b| a.json_path.cmp(&b.json_path));
    found.dedup_by(|a, b| a.json_path == b.json_path);
    found
}

fn is_linemap_json(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(LINE_NUMBER_MAPPINGS)
}

fn bundle_from_json(json_path: &Path) -> Option<LinemapBundle> {
    let dir = json_path.parent()?.to_path_buf();
    let method_map = {
        let p = dir.join(METHOD_MAP);
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };
    let file_root = {
        let p = dir.join(FILE_ROOT);
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };
    Some(LinemapBundle {
        json_path: json_path.to_path_buf(),
        method_map,
        file_root,
    })
}

/// What a validated `LineNumberMappings.json` holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MappingsStats {
    /// Generated C++ files (top-level keys).
    pub cpp_files: usize,
    /// (C++ file, C# file) pairs.
    pub cs_files: usize,
    /// `cpp_line -> cs_line` entries.
    pub lines: usize,
}

/// Check that `path` is the mappings document the symbolicator reads, WITHOUT holding it in
/// memory (these files run to tens of MB): `{ cpp_path: { cs_path: { cpp_line: cs_line } } }`,
/// where every line number is a non-negative integer that fits a `u32`.
///
/// The CLI packs this file as-is, so without a check a truncated or wrong file (an interrupted
/// Unity build, the wrong artefact, a merge-conflicted checkout) uploads "successfully" and
/// every IL2CPP crash then fails to symbolicate with nothing pointing back at the upload.
/// A document with no entries at all is accepted (a project can have none) but flagged.
///
/// Malformed content is `InputInvalid` (exit 11), naming the file and the JSON path of the
/// problem; an I/O failure reading it stays an I/O error.
pub fn validate_mappings(path: &Path) -> anyhow::Result<MappingsStats> {
    let file = fs::File::open(path).map_err(Error::Io)?;
    let bad = |detail: &str| {
        input_invalid(format!(
            "{} is not a valid IL2CPP line-number map ({detail}); expected \
             {{cpp_path: {{cs_path: {{cpp_line: cs_line}}}}}} with integer line numbers: {}",
            LINE_NUMBER_MAPPINGS,
            path.display()
        ))
    };
    let from_reader = |e: ReaderError| match e {
        // Invalid UTF-8 is damaged content, like any other malformed byte; only a real I/O
        // failure is an I/O error.
        ReaderError::IoError { error, .. } if error.kind() != std::io::ErrorKind::InvalidData => {
            anyhow::Error::from(Error::Io(error))
        }
        other => bad(&other.to_string()),
    };
    // The reader's default number guard (no exponent beyond +-99, no token over 100
    // characters) stays ON: a line number is at most 10 digits, so anything the guard
    // refuses is invalid here anyway, and it stops a huge number from being buffered.
    let mut r = JsonStreamReader::new(std::io::BufReader::new(file));
    let mut stats = MappingsStats {
        cpp_files: 0,
        cs_files: 0,
        lines: 0,
    };
    let expect = |r: &mut JsonStreamReader<_>, want: ValueType, what: &str| -> anyhow::Result<()> {
        let got = r.peek().map_err(from_reader)?;
        if got == want {
            Ok(())
        } else {
            Err(bad(&format!("{what} must be {want}, found {got}")))
        }
    };

    expect(&mut r, ValueType::Object, "the top-level value")?;
    r.begin_object().map_err(from_reader)?;
    while r.has_next().map_err(from_reader)? {
        let cpp = r.next_name().map_err(from_reader)?.to_owned();
        if cpp == DEBUG_ID_SENTINEL {
            // The documented optional sentinel (the build's debug id): parsers ignore it,
            // whatever it holds. Still read through, so syntax errors in it are caught.
            r.skip_value().map_err(from_reader)?;
            continue;
        }
        expect(
            &mut r,
            ValueType::Object,
            &format!("the entry for \"{cpp}\""),
        )?;
        r.begin_object().map_err(from_reader)?;
        stats.cpp_files += 1;
        while r.has_next().map_err(from_reader)? {
            let cs = r.next_name().map_err(from_reader)?.to_owned();
            expect(
                &mut r,
                ValueType::Object,
                &format!("the entry for \"{cpp}\" -> \"{cs}\""),
            )?;
            r.begin_object().map_err(from_reader)?;
            stats.cs_files += 1;
            while r.has_next().map_err(from_reader)? {
                let line = r.next_name().map_err(from_reader)?.to_owned();
                if line.is_empty()
                    || !line.bytes().all(|b| b.is_ascii_digit())
                    || line.parse::<u32>().is_err()
                {
                    return Err(bad(&format!(
                        "\"{line}\" under \"{cpp}\" is not a C++ line number"
                    )));
                }
                expect(
                    &mut r,
                    ValueType::Number,
                    &format!("the C# line for {cpp}:{line}"),
                )?;
                let n = r.next_number_as_string().map_err(from_reader)?;
                if n.parse::<u32>().is_err() {
                    let shown: String = n.chars().take(32).collect();
                    return Err(bad(&format!(
                        "{cpp}:{line} maps to {shown}, not a non-negative integer line"
                    )));
                }
                stats.lines += 1;
            }
            r.end_object().map_err(from_reader)?;
        }
        r.end_object().map_err(from_reader)?;
    }
    r.end_object().map_err(from_reader)?;
    r.consume_trailing_whitespace().map_err(from_reader)?;
    if stats.lines == 0 {
        tracing::warn!(path = %path.display(), "the line-number map has no entries");
    }
    Ok(stats)
}

/// Parse `--uuid` values: comma-separated and/or repeated spellings.
pub fn parse_uuids(raw: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for item in raw {
        for part in item.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    if out.is_empty() {
        // Same class as `--type elf` missing `--uuid` (exit 20 / config_invalid):
        // caller-supplied identity is configuration, not a malformed input file.
        return Err(config_invalid(
            "--uuid is required for --type il2cpp-linemap (one or more IL2CPP \
             module build-ids / Mach-O UUIDs; comma-separate for multi-ABI)",
        ));
    }
    Ok(out)
}

/// Build a manifest.json body for the upload ZIP.
///
/// `items` lists only entries that are actually packed (plus the required
/// LineNumberMappings.json and this manifest itself is not listed).
pub fn packed_item_names(has_method_map: bool, has_file_root: bool) -> Vec<&'static str> {
    let mut items = vec![LINE_NUMBER_MAPPINGS];
    if has_method_map {
        items.push(METHOD_MAP);
    }
    if has_file_root {
        items.push(FILE_ROOT);
    }
    items
}

pub fn manifest_json(uuids: &[String], target: Option<&str>, items: &[&str]) -> String {
    serde_json::json!({
        "format": "il2cpp-linemap",
        "target": target.unwrap_or("unknown"),
        "images": uuids.iter().map(|u| serde_json::json!({
            "arch": "unknown",
            "uuid": u,
        })).collect::<Vec<_>>(),
        "items": items.iter().map(|e| serde_json::json!({ "entry": e })).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Require that at least one bundle was found.
pub fn require_bundles(paths: &[PathBuf], bundles: &[LinemapBundle]) -> anyhow::Result<()> {
    if bundles.is_empty() {
        return Err(input_not_found(format!(
            "no {} found under: {}",
            LINE_NUMBER_MAPPINGS,
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// Read optional file-root override or sibling file contents (for logging).
pub fn read_file_root(
    bundle: &LinemapBundle,
    override_path: Option<&Path>,
) -> anyhow::Result<Option<String>> {
    if let Some(p) = override_path {
        let s = fs::read_to_string(p).map_err(|e| {
            input_not_found(format!(
                "--il2cpp-root path unreadable ({}): {}",
                p.display(),
                e
            ))
        })?;
        return Ok(Some(s.trim().to_string()));
    }
    if let Some(ref p) = bundle.file_root {
        let s = fs::read_to_string(p).unwrap_or_default();
        return Ok(Some(s.trim().to_string()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_json_and_siblings() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/il2cpp-linemap/android");
        let found = discover(&[root], &ExtraSuffixes::default());
        assert_eq!(found.len(), 1);
        assert!(found[0].method_map.is_some());
        assert!(found[0].file_root.is_some());
    }

    #[test]
    fn extra_suffix_discovers_renamed_json_with_its_siblings() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("linemaps");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Game.linemap.json"), b"{}").unwrap();
        fs::write(dir.join(METHOD_MAP), b"").unwrap();
        fs::write(dir.join("unrelated.json"), b"{}").unwrap();
        let root = tmp.path().to_path_buf();

        assert!(
            discover(std::slice::from_ref(&root), &ExtraSuffixes::default()).is_empty(),
            "without the suffix the renamed JSON is not a linemap"
        );
        let extra = ExtraSuffixes::parse(&[".linemap.json".to_string()]).unwrap();
        let found = discover(&[root], &extra);
        assert_eq!(found.len(), 1);
        assert!(found[0].json_path.ends_with("Game.linemap.json"));
        assert!(found[0].method_map.is_some(), "siblings resolve next to it");
    }

    // ── validate_mappings ──────────────────────────────────────────

    fn fixture(platform: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/il2cpp-linemap")
            .join(platform)
            .join(LINE_NUMBER_MAPPINGS)
    }

    fn write_json(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(LINE_NUMBER_MAPPINGS);
        fs::write(&p, bytes).unwrap();
        (dir, p)
    }

    fn invalid(p: &Path) -> String {
        match validate_mappings(p).unwrap_err().downcast::<Error>() {
            Ok(Error::InputInvalid(m)) => m,
            other => panic!("expected InputInvalid, got {other:?}"),
        }
    }

    #[test]
    fn the_committed_fixtures_are_valid() {
        for platform in ["android", "ios"] {
            let stats = validate_mappings(&fixture(platform)).unwrap();
            assert!(
                stats.cpp_files >= 1 && stats.cs_files >= 1 && stats.lines >= 1,
                "{stats:?}"
            );
        }
        let stats = validate_mappings(&fixture("android")).unwrap();
        assert_eq!(stats.cpp_files, 2);
        assert_eq!(stats.cs_files, 3);
        assert_eq!(stats.lines, 7);
    }

    /// The optional `__debug-id__` sentinel is ignored whatever it holds; it is not an entry.
    #[test]
    fn the_debug_id_sentinel_is_ignored_but_still_syntax_checked() {
        for sentinel in [r#""3f2a-uuid""#, "7", "null", r#"{"x":[1,2]}"#] {
            let doc = format!(r#"{{"__debug-id__":{sentinel},"a.cpp":{{"A.cs":{{"1":2}}}}}}"#);
            let (_d, p) = write_json(doc.as_bytes());
            assert_eq!(
                validate_mappings(&p).unwrap(),
                MappingsStats {
                    cpp_files: 1,
                    cs_files: 1,
                    lines: 1
                },
                "{sentinel}"
            );
        }
        let (_d, p) = write_json(br#"{"__debug-id__":"unterminated,"a.cpp":{}}"#);
        assert!(validate_mappings(&p).is_err());
        // Only that exact key is special: another scalar entry is still a shape error.
        let (_d, p) = write_json(br#"{"__other__":"x","a.cpp":{"A.cs":{"1":2}}}"#);
        assert!(invalid(&p).contains("__other__"));
    }

    #[test]
    fn an_empty_object_is_accepted_and_counts_nothing() {
        let (_d, p) = write_json(b"{}");
        assert_eq!(
            validate_mappings(&p).unwrap(),
            MappingsStats {
                cpp_files: 0,
                cs_files: 0,
                lines: 0
            }
        );
        let (_d, p) = write_json(br#"{"a.cpp":{"A.cs":{}}}"#);
        assert_eq!(validate_mappings(&p).unwrap().cs_files, 1);
    }

    /// Everything that is not the documented shape is `InputInvalid`, with a message that
    /// names the file and the problem.
    #[test]
    fn malformed_or_misshapen_documents_are_rejected() {
        let good = br#"{"a.cpp":{"A.cs":{"1":2}}}"#;
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("empty file", Vec::new(), "LineNumberMappings.json"),
            (
                "whitespace only",
                b"  \n".to_vec(),
                "LineNumberMappings.json",
            ),
            (
                "truncated",
                good[..good.len() - 3].to_vec(),
                "LineNumberMappings.json",
            ),
            (
                "trailing junk",
                [&good[..], b" x"].concat(),
                "LineNumberMappings.json",
            ),
            (
                "two documents",
                [&good[..], &good[..]].concat(),
                "LineNumberMappings.json",
            ),
            (
                "byte-order mark",
                [&b"\xef\xbb\xbf"[..], &good[..]].concat(),
                "LineNumberMappings.json",
            ),
            (
                "binary",
                vec![0xff, 0xfe, 0x00, 0x01],
                "LineNumberMappings.json",
            ),
            ("an array", b"[]".to_vec(), "top-level value must be"),
            ("a string", br#""x""#.to_vec(), "top-level value must be"),
            ("cpp entry is a number", br#"{"a.cpp":3}"#.to_vec(), "a.cpp"),
            (
                "cs entry is an array",
                br#"{"a.cpp":{"A.cs":[1]}}"#.to_vec(),
                "A.cs",
            ),
            (
                "one level short",
                br#"{"a.cpp":{"1":2}}"#.to_vec(),
                "must be Object",
            ),
            (
                "string line value",
                br#"{"a.cpp":{"A.cs":{"1":"2"}}}"#.to_vec(),
                "C# line",
            ),
            (
                "null line value",
                br#"{"a.cpp":{"A.cs":{"1":null}}}"#.to_vec(),
                "C# line",
            ),
            (
                "non-numeric cpp line",
                br#"{"a.cpp":{"A.cs":{"one":2}}}"#.to_vec(),
                "not a C++ line",
            ),
            (
                "empty cpp line key",
                br#"{"a.cpp":{"A.cs":{"":2}}}"#.to_vec(),
                "not a C++ line",
            ),
            (
                "negative cpp line",
                br#"{"a.cpp":{"A.cs":{"-1":2}}}"#.to_vec(),
                "not a C++ line",
            ),
            (
                "cpp line over u32",
                br#"{"a.cpp":{"A.cs":{"4294967296":2}}}"#.to_vec(),
                "not a C++ line",
            ),
            (
                "negative cs line",
                br#"{"a.cpp":{"A.cs":{"1":-2}}}"#.to_vec(),
                "non-negative integer",
            ),
            (
                "fractional cs line",
                br#"{"a.cpp":{"A.cs":{"1":2.5}}}"#.to_vec(),
                "non-negative integer",
            ),
            (
                "exponent cs line",
                br#"{"a.cpp":{"A.cs":{"1":1e3}}}"#.to_vec(),
                "non-negative integer",
            ),
            (
                "cs line over u32",
                br#"{"a.cpp":{"A.cs":{"1":4294967296}}}"#.to_vec(),
                "non-negative integer",
            ),
            (
                "deeply nested",
                format!("{}1{}", r#"{"a":"#.repeat(50_000), "}".repeat(50_000)).into_bytes(),
                "LineNumberMappings.json",
            ),
        ];
        for (label, bytes, needle) in cases {
            let (_d, p) = write_json(&bytes);
            let msg = invalid(&p);
            assert!(msg.contains(needle), "{label}: {msg}");
            assert!(
                msg.contains(&p.display().to_string()),
                "{label}: names the file: {msg}"
            );
        }
    }

    /// No prefix of a real document is itself a valid one, and none crashes the check.
    #[test]
    fn every_truncation_of_a_real_map_is_rejected() {
        let whole = fs::read(fixture("ios")).unwrap();
        validate_mappings(&fixture("ios")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(LINE_NUMBER_MAPPINGS);
        let trimmed = whole.trim_ascii_end();
        for cut in 0..trimmed.len() {
            fs::write(&p, &trimmed[..cut]).unwrap();
            assert!(
                validate_mappings(&p).is_err(),
                "a {cut}-byte prefix was accepted"
            );
        }
    }

    /// A leaf "number" of thousands of digits (a wrong file or a partial write) is refused by
    /// the reader's number guard before it is buffered, and the message stays short.
    #[test]
    fn a_huge_number_is_rejected_cheaply_with_a_short_message() {
        for body in ["9".repeat(50_000), format!("1e{}", "9".repeat(40))] {
            let doc = format!(r#"{{"a.cpp":{{"A.cs":{{"1":{body}}}}}}}"#);
            let (_d, p) = write_json(doc.as_bytes());
            let msg = invalid(&p);
            assert!(
                msg.len() < 1_000,
                "the error must not echo the number: {} bytes",
                msg.len()
            );
        }
    }

    /// A map of tens of MB is checked as a stream: this one has 300 000 entries.
    #[test]
    fn a_large_map_validates() {
        let mut doc = String::from("{");
        for f in 0..30 {
            if f > 0 {
                doc.push(',');
            }
            doc.push_str(&format!(r#""f{f}.cpp":{{"F{f}.cs":{{"#));
            for l in 0..10_000 {
                if l > 0 {
                    doc.push(',');
                }
                doc.push_str(&format!(r#""{l}":{}"#, l / 2));
            }
            doc.push_str("}}");
        }
        doc.push('}');
        let (_d, p) = write_json(doc.as_bytes());
        let stats = validate_mappings(&p).unwrap();
        assert_eq!((stats.cpp_files, stats.lines), (30, 300_000));
    }

    #[test]
    fn a_missing_file_is_an_io_error_not_invalid_content() {
        let dir = tempfile::tempdir().unwrap();
        let err = validate_mappings(&dir.path().join("nope.json")).unwrap_err();
        assert!(matches!(err.downcast::<Error>(), Ok(Error::Io(_))));
    }

    #[test]
    fn parse_uuids_comma_and_multi() {
        let got = parse_uuids(&["aaa,bbb".to_string(), "ccc".to_string()]).unwrap();
        assert_eq!(got, vec!["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn parse_uuids_empty_errors() {
        assert!(parse_uuids(&[]).is_err());
        assert!(parse_uuids(&["".to_string(), "  ".to_string()]).is_err());
    }

    #[test]
    fn manifest_items_only_list_packed_siblings() {
        let json = manifest_json(
            &["u1".into()],
            Some("android"),
            &packed_item_names(false, true),
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entries: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["entry"].as_str().unwrap())
            .collect();
        assert_eq!(
            entries,
            vec!["LineNumberMappings.json", "il2cppFileRoot.txt"]
        );
        assert!(!entries.contains(&"MethodMap.tsv"));
    }
}
