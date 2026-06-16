//! `.xcactivitylog` build-timings decoder — the Rust port of the iOS
//! BugseeAgent's SLF0 timings parser (`tools.bundle/BugseeAgent`).
//!
//! Xcode writes one gzipped `.xcactivitylog` per build/archive under
//! `<DerivedData>/Logs/Build/`. Each is a gzip of an `SLF0` token stream — a
//! self-describing serialization of the build's `IDEActivityLogSection` tree.
//! We decode it into two products that mirror the Android Gradle plugin's
//! `TimingsPayloadSerializer`, so the back-end + front-end render both
//! platforms from one schema:
//!
//!   - **inline summary** (`build_metadata.timings`): `total_ms` (wall-clock
//!     SPAN), a `top_tasks` list of the slowest `Build target ` groupings, and
//!     per-category OCCUPANCY rollups (`native_ms` / `resources_ms` /
//!     `packaging_ms` / `other_ms`). Occupancy is an interval UNION over the
//!     command-invocation sections, so it can never exceed `total_ms` (the fix
//!     for a naive per-category SUM that double-counts Swift batch windows).
//!     iOS never emits `managed_code_ms` — Swift / Obj-C / C / C++ all compile
//!     into the Mach-O and land in `native`.
//!   - **per-target Gantt detail blob** (the `timings.json` bundle entry): one
//!     row per `Build target ` grouping — `{schema_version, build_started_at_ms,
//!     wall_clock_ms, tasks:[{path, category, start_ms, end_ms}]}`. Written RAW
//!     (NOT gzipped) into the build-info bundle: the worker re-gzips each entry
//!     internally before validation, exactly as it does for `dependencies.json`.
//!
//! The decode is total over arbitrary bytes: a malformed / truncated stream
//! degrades to "no timings", never a panic — losing timings is acceptable;
//! failing an already-signed build is not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use flate2::read::MultiGzDecoder;
use regex::RegexSet;
use serde_json::{json, Map, Value};
use std::io::Read;
use std::sync::OnceLock;

/// CFAbsoluteTime is seconds since 2001-01-01 00:00:00 UTC; add this to convert
/// to the Unix epoch (matches the agent's `_CF_ABSOLUTE_TIME_EPOCH_OFFSET`).
const CF_ABSOLUTE_TIME_EPOCH_OFFSET: f64 = 978_307_200.0;

/// Hard cap on the decompressed `.xcactivitylog` we will materialise (20 MiB) —
/// bounds memory on a pathological log; a truncated tail loses only late
/// sections (the class table + root section live at the head).
const MAX_DECOMPRESSED: usize = 20 * 1024 * 1024;

/// Cap on the `top_tasks` list (slowest `Build target ` groupings). Mirrors the
/// agent's `_XCACTIVITYLOG_TOP_N` and the Android plugin default.
const TOP_N: usize = 10;

/// Wire-format schema version for the per-target timeline DETAIL blob. Kept in
/// lockstep with the Android plugin's `TimingsPayloadSerializer.SCHEMA_VERSION`
/// and the back-end's `_SUPPORTED_SCHEMA_VERSIONS` — all currently 1.
const TIMELINE_SCHEMA_VERSION: i64 = 1;

/// Cap on task records emitted in the timeline blob (Android's
/// `TimingsPayloadSerializer.MAX_TASKS`). When exceeded we keep the SLOWEST
/// targets, then re-sort the kept slice chronologically.
const TIMELINE_MAX_TASKS: usize = 10_000;

/// Title prefix marking the per-target grouping sections that become both the
/// Gantt rows and the `top_tasks` entries. The mega-wrappers (root `Build
/// <scheme>`, `Prepare build`, `Run post-actions`, `Prepare packages`) do NOT
/// carry this prefix and are excluded — they span the whole build.
const TARGET_PREFIX: &str = "Build target ";

const CLASS_SECTION: &str = "IDEActivityLogSection";
const CLASS_COMMAND: &str = "IDEActivityLogCommandInvocationSection";

// ─── Categories ─────────────────────────────────────────────────────

/// Cross-platform category buckets. Wire names match the Android Gradle
/// plugin's (`native_ms` / `resources_ms` / `packaging_ms` / `other_ms`) so the
/// back-end renders both platforms from one schema. iOS never emits
/// `managed_code` (that is reserved for JVM-bytecode pipelines on Android).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Category {
    Native,
    Resources,
    Packaging,
    Other,
}

impl Category {
    fn as_str(self) -> &'static str {
        match self {
            Category::Native => "native",
            Category::Resources => "resources",
            Category::Packaging => "packaging",
            Category::Other => "other",
        }
    }
}

// ─── Lazy regex sets (compile once per process) ─────────────────────
//
// Precedence is native → resources → packaging — each set checked in order,
// the first matching group wins (see `classify_section_title`). Within a group
// any pattern matching is enough, so a `RegexSet` (which reports "any match")
// is the right primitive. Every pattern is `^`-anchored, mirroring Python's
// `re.compile(...).match(title)` (anchored at start, not end).

fn native_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            // Per-source compile events: "Compile Foo.swift (arm64)", etc.
            r"(?i)^Compile \S+\.(swift|m|mm|c|cpp|cxx|cc)\b",
            r"^CompileSwiftSources\b",
            r"^CompileC\b",
            r"^CompileSwift\b",
            r"^Planning Swift module\b",
            r"^SwiftDriver\b",
            r"^Emit(?:ting)? [Ss]wift [Mm]odule\b",
            r"^Emitting module for\b",
            r"^SwiftMergeGeneratedHeaders\b",
            r"^SwiftVerifyEmittedModuleInterface\b",
            r"^Generate Swift Constant Values\b",
            r"^Compiling Clang module\b",
            r"^Precompile module\b",
            r"^Explicitly Built\b",
            r"^Discovering version info for swiftc\b",
            r"(?i)^Extract app intents metadata\b",
        ])
        .expect("known-valid native category regex set")
    })
}

fn resources_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            r"(?i)^Compile asset catalog",
            r"^CompileAssetCatalog\b",
            r"^CompileStoryboard\b",
            r"^CompileXIB\b",
            r"^CompileXCStrings\b",
            // `LinkStoryboards` must be caught here, before packaging's generic
            // `^Link\b` claims it — the native→resources→packaging precedence is
            // what keeps this working.
            r"^LinkStoryboards\b",
            r"^CompileStrings\b",
            r"^ProcessInfoPlistFile\b",
            r"^CpResource\b",
            r"^CopyPlistFile\b",
            r"^CopyStringsFile\b",
            r"^CopyTiffFile\b",
            r"^CopyPNGFile\b",
            r"^GenerateAssetSymbols\b",
        ])
        .expect("known-valid resources category regex set")
    })
}

fn packaging_set() -> &'static RegexSet {
    static SET: OnceLock<RegexSet> = OnceLock::new();
    SET.get_or_init(|| {
        RegexSet::new([
            r"^Link\b",
            r"^Ld\b",
            r"^CodeSign\b",
            r"^Sign \b",
            r"^SignManifestFile\b",
            r"^Strip\b",
            r"^Touch\b",
            r"^Embed\b",
            r"^ProcessProductPackaging\b",
            r"^RegisterExecutionPolicyException\b",
            r"^Validate\b",
            r"^GenerateDSYMFile\b",
            r"^CreateUniversalBinary\b",
            // Swift stdlib embedding — `swift-stdlib-tool` copies the runtime
            // dylibs into the bundle. Conceptually packaging, not compilation.
            r"^Copy Swift standard libraries\b",
        ])
        .expect("known-valid packaging category regex set")
    })
}

/// Map a command-invocation title to a category bucket. Returns `None` only for
/// an empty title. Precedence-ordered: native → resources → packaging → other.
fn classify_section_title(title: &str) -> Option<Category> {
    if title.is_empty() {
        return None;
    }
    // `Compiling Clang module <name>` is a real native compile (explicit module
    // build); guard it ahead of the sets so it never falls through to `other`.
    if title.starts_with("Compiling Clang module") {
        return Some(Category::Native);
    }
    if native_set().is_match(title) {
        return Some(Category::Native);
    }
    if resources_set().is_match(title) {
        return Some(Category::Resources);
    }
    if packaging_set().is_match(title) {
        return Some(Category::Packaging);
    }
    Some(Category::Other)
}

// ─── PII scrubbing for emitted titles ───────────────────────────────

fn user_home_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"/Users/[A-Za-z0-9._\-]+").expect("known-valid user-home regex")
    })
}

fn private_var_folders_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"/private/var/folders/\S+").expect("known-valid private-var regex")
    })
}

/// `os.path.basename`-equivalent: the segment after the last `/`. Trailing-slash
/// paths yield `""` (matching Python), bare names yield themselves.
fn os_basename(s: &str) -> &str {
    s.rsplit('/').next().unwrap_or(s)
}

/// Strip PII (username, machine-local paths) from an xcactivitylog section
/// title before it ships in `top_tasks` / the Gantt blob.
///
/// Order matches the agent: replace `/Users/<name>` with `<home>` (drops the
/// username), collapse `/private/var/folders/...` to its basename, then reduce
/// any other absolute-path token (at a whitespace boundary) to its basename.
/// `<home>/...` survives the third pass because it no longer starts with `/`.
fn sanitize_section_title_for_emission(title: &str) -> String {
    if title.is_empty() {
        return String::new();
    }
    let step1 = user_home_re().replace_all(title, "<home>");
    let step2 = private_var_folders_re()
        .replace_all(&step1, |c: &regex::Captures| os_basename(&c[0]).to_string());
    // Third pass: any absolute path at a token boundary (start-of-string or
    // after whitespace) → basename. The Rust regex crate has no lookbehind, so
    // we capture the boundary and re-emit it. `<home>` tokens don't start with
    // `/`, so they are never matched here.
    let abs_re = abs_path_re();
    let out = abs_re.replace_all(&step2, |c: &regex::Captures| {
        let boundary = &c[1];
        let path = &c[2];
        let base = os_basename(path);
        let base = if base.is_empty() { path } else { base };
        format!("{boundary}{base}")
    });
    out.into_owned()
}

fn abs_path_re() -> &'static regex::Regex {
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    // `(^|\s)` emulates Python's `(?<!\S)` (path is at start or preceded by
    // whitespace); `(/\S+)` is the absolute-path run.
    RE.get_or_init(|| regex::Regex::new(r"(^|\s)(/\S+)").expect("known-valid absolute-path regex"))
}

// ─── SLF0 tokenizer ─────────────────────────────────────────────────

/// One SLF0 token. We keep only the values the section extractor reads:
/// `ClassDef` names (the class table), `ClassRef` indices, `Str` payloads
/// (titles), and `Double` values (timestamps). `Int` / `ArrayCount` / `Blob`
/// carry no value here — only their TYPE participates in the section shape
/// check.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Str(String),
    ClassDef(String),
    Blob,
    Int,
    Double(Option<f64>),
    ClassRef(Option<i64>),
    ArrayCount,
    Null,
}

#[inline]
fn is_delim(b: u8) -> bool {
    matches!(b, b'"' | b'%' | b'*' | b'#' | b'^' | b'@' | b'(' | b'-')
}

/// `bytes.decode('ascii', 'replace')`: each byte becomes exactly one char —
/// valid ASCII bytes pass through, the rest become U+FFFD. This keeps the
/// decoded length equal to the byte length (relied on by the `^` length check).
fn ascii_lossy(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' })
        .collect()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 16 ASCII hex chars → 8 bytes → little-endian IEEE-754 double. Mirrors
/// `struct.unpack('<d', bytes.fromhex(p))`. Any non-hex byte → `None`.
fn hex_bytes_to_f64_le(p: &[u8]) -> Option<f64> {
    if p.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (k, slot) in out.iter_mut().enumerate() {
        let hi = hex_val(p[2 * k])?;
        let lo = hex_val(p[2 * k + 1])?;
        *slot = (hi << 4) | lo;
    }
    Some(f64::from_le_bytes(out))
}

/// Tokenize a DECOMPRESSED `SLF0` stream into a flat token list. The leading
/// 4-byte `SLF0` header is skipped. Returns `(tokens, desync_count)`; a non-zero
/// desync hints the grammar drifted (malformed length / number payload), but
/// parsing continues best-effort. Port of the agent's `_tokenize_slf`.
fn tokenize_slf(data: &[u8]) -> (Vec<Token>, usize) {
    let mut tokens = Vec::new();
    let mut desync = 0usize;
    let n = data.len();
    let mut i = 4.min(n); // skip the 4-byte 'SLF0' header
    let mut payload: Vec<u8> = Vec::new();
    while i < n {
        let b = data[i];
        if is_delim(b) {
            i += 1;
            match b {
                b'"' | b'%' | b'*' => {
                    // DECIMAL length, then that many raw bytes (UTF-8 lossy).
                    // Python's `int(p, 10)` strips surrounding whitespace, so
                    // inter-token filler (real logs and the synthetic fixtures
                    // pad with spaces) is tolerated; `.trim()` matches that.
                    let p = ascii_lossy(&payload);
                    let ln: usize = if p.is_empty() {
                        0
                    } else {
                        match p.trim().parse::<usize>() {
                            Ok(v) => v,
                            Err(_) => {
                                desync += 1;
                                0
                            }
                        }
                    };
                    let start = i;
                    let end = start.saturating_add(ln).min(n);
                    let value = String::from_utf8_lossy(&data[start..end]).into_owned();
                    i = start.saturating_add(ln);
                    match b {
                        b'"' => tokens.push(Token::Str(value)),
                        b'%' => tokens.push(Token::ClassDef(value)),
                        _ => tokens.push(Token::Blob),
                    }
                }
                b'^' => {
                    // 16 hex chars, little-endian double. Wrong length → None
                    // (no desync); malformed hex → None + desync.
                    if payload.len() == 16 {
                        match hex_bytes_to_f64_le(&payload) {
                            Some(f) => tokens.push(Token::Double(Some(f))),
                            None => {
                                tokens.push(Token::Double(None));
                                desync += 1;
                            }
                        }
                    } else {
                        tokens.push(Token::Double(None));
                    }
                }
                _ => {
                    // '#', '@', '(' carry a HEX payload; '-' carries none. We
                    // need the value only for '@' (class-table reference).
                    // `int(p, 16)` strips whitespace (see the decimal branch);
                    // `.trim()` matches it so inter-token filler before an `@`
                    // class-ref still resolves.
                    let p = ascii_lossy(&payload);
                    let parsed: Option<i64> = if p.is_empty() {
                        Some(0)
                    } else {
                        match i64::from_str_radix(p.trim(), 16) {
                            Ok(v) => Some(v),
                            Err(_) => {
                                desync += 1;
                                None
                            }
                        }
                    };
                    match b {
                        b'#' => tokens.push(Token::Int),
                        b'@' => tokens.push(Token::ClassRef(parsed)),
                        b'(' => tokens.push(Token::ArrayCount),
                        _ => tokens.push(Token::Null),
                    }
                }
            }
            payload.clear();
        } else {
            payload.push(b);
            i += 1;
        }
    }
    (tokens, desync)
}

// ─── Section extraction ─────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SectionClass {
    Section,
    Command,
}

#[derive(Debug, Clone)]
struct Section {
    cls: SectionClass,
    title: String,
    start_cf: f64,
    end_cf: f64,
}

fn class_of<'a>(table: &[&'a str], idx: i64) -> Option<&'a str> {
    if idx >= 1 && (idx as usize) <= table.len() {
        Some(table[(idx - 1) as usize])
    } else {
        None
    }
}

/// Extract every section instance as a `Section`. A section starts at an `@`-ref
/// to one of the two section classes, immediately followed by `# " " " ^ ^`
/// then a subSections field (`(` or `-`). Sections with a missing / non-finite /
/// inverted timestamp pair are dropped. Port of `_extract_slf_sections`.
fn extract_slf_sections(tokens: &[Token]) -> Vec<Section> {
    let class_table: Vec<&str> = tokens
        .iter()
        .filter_map(|t| match t {
            Token::ClassDef(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    let ntok = tokens.len();
    let mut sections = Vec::new();
    // Need indices up to k+7, so k ranges over 0..(ntok-7).
    for k in 0..ntok.saturating_sub(7) {
        let cls_ref = match tokens[k] {
            Token::ClassRef(Some(v)) => v,
            _ => continue,
        };
        let cls = match class_of(&class_table, cls_ref) {
            Some(CLASS_SECTION) => SectionClass::Section,
            Some(CLASS_COMMAND) => SectionClass::Command,
            _ => continue,
        };
        // Shape: # " " " ^ ^ then ( or - .
        let shape_ok = matches!(tokens[k + 1], Token::Int)
            && matches!(tokens[k + 2], Token::Str(_))
            && matches!(tokens[k + 3], Token::Str(_))
            && matches!(tokens[k + 4], Token::Str(_))
            && matches!(tokens[k + 5], Token::Double(_))
            && matches!(tokens[k + 6], Token::Double(_))
            && matches!(tokens[k + 7], Token::ArrayCount | Token::Null);
        if !shape_ok {
            continue;
        }
        let title = match &tokens[k + 3] {
            Token::Str(s) => s.clone(),
            _ => continue,
        };
        let start_cf = match tokens[k + 5] {
            Token::Double(Some(v)) => v,
            _ => continue,
        };
        let end_cf = match tokens[k + 6] {
            Token::Double(Some(v)) => v,
            _ => continue,
        };
        if !(start_cf.is_finite() && end_cf.is_finite()) {
            continue;
        }
        if end_cf < start_cf {
            continue;
        }
        sections.push(Section {
            cls,
            title,
            start_cf,
            end_cf,
        });
    }
    sections
}

// ─── Interval math + rounding ───────────────────────────────────────

/// Total length (seconds) of the UNION of `[start, end]` intervals — the
/// OCCUPANCY metric. Overlapping windows merge, so the result never exceeds
/// wall-clock. Port of `_interval_union_seconds`.
fn interval_union_seconds(intervals: &[(f64, f64)]) -> f64 {
    let mut ordered: Vec<(f64, f64)> = intervals.to_vec();
    // All values are finite (callers filter non-finite sections), so the
    // partial_cmp unwrap is sound; fall back to Equal defensively.
    ordered.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let mut total = 0.0;
    let mut cur: Option<(f64, f64)> = None;
    for (s, e) in ordered {
        match cur {
            None => cur = Some((s, e)),
            Some((cs, ce)) => {
                if s <= ce {
                    if e > ce {
                        cur = Some((cs, e));
                    }
                } else {
                    total += ce - cs;
                    cur = Some((s, e));
                }
            }
        }
    }
    if let Some((cs, ce)) = cur {
        total += ce - cs;
    }
    total
}

/// `int(round(x))` with Python 3 semantics (round half to EVEN). The CLI must
/// agree with the agent's millisecond values byte-for-byte; `f64::round` rounds
/// half away from zero and would diverge on exact `.5` boundaries.
fn py_round_to_i64(x: f64) -> i64 {
    x.round_ties_even() as i64
}

/// Dominant category for a target window: the category with the greatest
/// command OCCUPANCY among command-invocations CONTAINED within the window.
/// Falls back to `Native` when no classifiable command is contained. Tie-break
/// keeps the category whose commands appear FIRST in `commands` order (matching
/// Python's insertion-ordered dict + strict `>`). Port of
/// `_dominant_category_for_window`.
fn dominant_category_for_window(
    commands: &[(Category, f64, f64)],
    win_start: f64,
    win_end: f64,
) -> Category {
    // Preserve first-encounter order of distinct categories.
    let mut by_cat: Vec<(Category, Vec<(f64, f64)>)> = Vec::new();
    for &(cat, s, e) in commands {
        if s >= win_start && e <= win_end {
            if let Some(slot) = by_cat.iter_mut().find(|(c, _)| *c == cat) {
                slot.1.push((s, e));
            } else {
                by_cat.push((cat, vec![(s, e)]));
            }
        }
    }
    if by_cat.is_empty() {
        return Category::Native;
    }
    let mut best_cat = Category::Native;
    let mut best_occ = -1.0_f64;
    for (cat, intervals) in &by_cat {
        let occ = interval_union_seconds(intervals);
        if occ > best_occ {
            best_occ = occ;
            best_cat = *cat;
        }
    }
    best_cat
}

// ─── Timeline blob ──────────────────────────────────────────────────

/// One resolved `Build target ` grouping: emitted path (prefix stripped),
/// dominant category, and the CFAbsoluteTime window.
struct TargetRow {
    path: String,
    category: Category,
    start_cf: f64,
    end_cf: f64,
}

/// Build the Gantt-chart DETAIL blob from the per-target grouping records.
/// Offsets are relative to `build_start_cf` (the whole build's earliest start).
/// Returns `None` when there are no target groupings. Port of
/// `_build_timeline_blob`.
fn build_timeline_blob(targets: &[TargetRow], build_start_cf: f64) -> Option<Value> {
    if targets.is_empty() {
        return None;
    }

    let build_started_at_ms =
        py_round_to_i64((build_start_cf + CF_ABSOLUTE_TIME_EPOCH_OFFSET) * 1000.0);
    let latest_end_cf = targets
        .iter()
        .map(|t| t.end_cf)
        .fold(f64::NEG_INFINITY, f64::max);
    let wall_clock_ms = py_round_to_i64((latest_end_cf - build_start_cf) * 1000.0).max(0);

    // Truncation keeps the SLOWEST targets, then re-sorts chronologically.
    let kept: Vec<&TargetRow> = if targets.len() > TIMELINE_MAX_TASKS {
        eprintln!(
            "Bugsee: build-timings detail blob truncated ({} targets -> {} slowest kept)",
            targets.len(),
            TIMELINE_MAX_TASKS
        );
        let mut by_dur: Vec<&TargetRow> = targets.iter().collect();
        by_dur.sort_by(|a, b| {
            (b.end_cf - b.start_cf)
                .partial_cmp(&(a.end_cf - a.start_cf))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        by_dur.truncate(TIMELINE_MAX_TASKS);
        by_dur
    } else {
        targets.iter().collect()
    };

    let mut chronological = kept;
    chronological.sort_by(|a, b| {
        a.start_cf
            .partial_cmp(&b.start_cf)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let tasks: Vec<Value> = chronological
        .iter()
        .map(|t| {
            let path: String = sanitize_section_title_for_emission(&t.path)
                .chars()
                .take(255)
                .collect();
            json!({
                "path": path,
                "category": t.category.as_str(),
                "start_ms": py_round_to_i64((t.start_cf - build_start_cf) * 1000.0),
                "end_ms": py_round_to_i64((t.end_cf - build_start_cf) * 1000.0),
            })
        })
        .collect();

    Some(json!({
        "schema_version": TIMELINE_SCHEMA_VERSION,
        "build_started_at_ms": build_started_at_ms,
        "wall_clock_ms": wall_clock_ms,
        "tasks": tasks,
    }))
}

// ─── Top-level parse ────────────────────────────────────────────────

/// Per-category occupancy rollup, in fixed emission order. iOS never fills
/// `managed_code`; it stays at 0 for parity with Android and is dropped by the
/// resolver's `value > 0` gate.
struct CategorySums {
    managed_code: i64,
    native: i64,
    resources: i64,
    packaging: i64,
    other: i64,
}

impl CategorySums {
    fn iter(&self) -> [(&'static str, i64); 5] {
        [
            ("managed_code", self.managed_code),
            ("native", self.native),
            ("resources", self.resources),
            ("packaging", self.packaging),
            ("other", self.other),
        ]
    }
}

struct Parsed {
    total_ms: i64,
    top_tasks: Vec<Value>,
    category_sums: CategorySums,
    timeline: Option<Value>,
}

/// Decode an already-decompressed `SLF0` stream into the parsed timing data.
/// Returns `None` when the stream tokenizes into no section at all. Port of the
/// body of `_parse_xcactivitylog` (after decompression).
fn parse_stream(data: &[u8]) -> Option<Parsed> {
    let (tokens, _desync) = tokenize_slf(data);
    let sections = extract_slf_sections(&tokens);
    if sections.is_empty() {
        return None;
    }

    // total_ms = wall-clock SPAN over ALL sections.
    let build_start_cf = sections
        .iter()
        .map(|s| s.start_cf)
        .fold(f64::INFINITY, f64::min);
    let build_end_cf = sections
        .iter()
        .map(|s| s.end_cf)
        .fold(f64::NEG_INFINITY, f64::max);
    // Individual timestamps passed the per-section `is_finite()` filter, but a
    // corrupt log with absurd values (e.g. `1e308` and `-1e308`) can still make
    // the span × 1000 overflow to `inf`. Python raises `OverflowError` in
    // `int(round(inf))` here, caught by `resolve_build_timings`' broad except →
    // timings dropped entirely. Match that (an `inf as i64` would otherwise
    // saturate to a garbage `i64::MAX` ms). Guarding the SPAN is sufficient: every
    // downstream duration is bounded by `[0, span]` and `build_started_at_ms` by
    // a finite `build_start_cf`.
    let span_ms = (build_end_cf - build_start_cf) * 1000.0;
    if !span_ms.is_finite() {
        return None;
    }
    let total_ms = py_round_to_i64(span_ms).max(0);

    // Classify each COMMAND-invocation once; reused for the category chips and
    // each target's dominant category.
    let mut commands: Vec<(Category, f64, f64)> = Vec::new();
    for sec in &sections {
        if sec.cls != SectionClass::Command {
            continue;
        }
        if let Some(bucket) = classify_section_title(&sec.title) {
            commands.push((bucket, sec.start_cf, sec.end_cf));
        }
    }

    // Per-category OCCUPANCY (interval union per bucket), in ms.
    let bucket_occ = |want: Category| -> i64 {
        let intervals: Vec<(f64, f64)> = commands
            .iter()
            .filter(|(c, _, _)| *c == want)
            .map(|(_, s, e)| (*s, *e))
            .collect();
        if intervals.is_empty() {
            0
        } else {
            py_round_to_i64(interval_union_seconds(&intervals) * 1000.0)
        }
    };
    let category_sums = CategorySums {
        managed_code: 0,
        native: bucket_occ(Category::Native),
        resources: bucket_occ(Category::Resources),
        packaging: bucket_occ(Category::Packaging),
        other: bucket_occ(Category::Other),
    };

    // `Build target ` groupings → Gantt rows + top_tasks. The mega-wrappers are
    // `IDEActivityLogSection` too but lack the prefix, so they're excluded.
    let mut targets: Vec<TargetRow> = Vec::new();
    for sec in &sections {
        if sec.cls != SectionClass::Section {
            continue;
        }
        if !sec.title.starts_with(TARGET_PREFIX) {
            continue;
        }
        let category = dominant_category_for_window(&commands, sec.start_cf, sec.end_cf);
        let stripped = &sec.title[TARGET_PREFIX.len()..];
        let path = if stripped.is_empty() {
            sec.title.clone()
        } else {
            stripped.to_string()
        };
        targets.push(TargetRow {
            path,
            category,
            start_cf: sec.start_cf,
            end_cf: sec.end_cf,
        });
    }

    // top_tasks = slowest target groupings, longest first, capped at TOP_N.
    let mut by_dur: Vec<&TargetRow> = targets.iter().collect();
    by_dur.sort_by(|a, b| {
        (b.end_cf - b.start_cf)
            .partial_cmp(&(a.end_cf - a.start_cf))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut top_tasks: Vec<Value> = Vec::new();
    for t in by_dur.into_iter().take(TOP_N) {
        let dur = py_round_to_i64((t.end_cf - t.start_cf) * 1000.0);
        if dur < 1 {
            // sub-millisecond targets are noise
            continue;
        }
        let name: String = sanitize_section_title_for_emission(&t.path)
            .chars()
            .take(255)
            .collect();
        top_tasks.push(json!({ "name": name, "duration_ms": dur }));
    }

    let timeline = build_timeline_blob(&targets, build_start_cf);

    Some(Parsed {
        total_ms,
        top_tasks,
        category_sums,
        timeline,
    })
}

/// Decompress + parse an `.xcactivitylog`. Returns `None` when the log can't be
/// opened / decompressed / tokenized into any section. Port of
/// `_parse_xcactivitylog`.
fn parse_xcactivitylog(log_path: &Path) -> Option<Parsed> {
    let file = std::fs::File::open(log_path).ok()?;
    let mut decoder = MultiGzDecoder::new(file);
    // Read up to MAX+1 so an over-cap log is detected, then clamp to MAX.
    let mut data = Vec::new();
    let mut limited = (&mut decoder).take((MAX_DECOMPRESSED + 1) as u64);
    limited.read_to_end(&mut data).ok()?;
    if data.len() > MAX_DECOMPRESSED {
        data.truncate(MAX_DECOMPRESSED);
    }
    parse_stream(&data)
}

// ─── DerivedData log discovery ──────────────────────────────────────

/// Walk up from `$OBJROOT` to the first ancestor with a `Logs/Build/`
/// subdirectory — Xcode's per-project DerivedData root. Bounded by an explicit
/// step cap so a malformed `$OBJROOT` never walks to `/`. Port of
/// `_find_derived_data_root`.
fn find_derived_data_root(obj_root: &str) -> Option<PathBuf> {
    if obj_root.is_empty() {
        return None;
    }
    let mut current = PathBuf::from(obj_root);
    for _ in 0..10 {
        if current.join("Logs").join("Build").is_dir() {
            return Some(current);
        }
        match current.parent() {
            Some(parent) if parent != current => current = parent.to_path_buf(),
            _ => return None,
        }
    }
    None
}

/// Return the newest `.xcactivitylog` under the discovered `Logs/Build/`, or
/// `None`. "Newest" is by mtime, tie-broken by filename descending (so
/// same-second mtimes on HFS+ / rsync caches stay deterministic). Port of
/// `_find_latest_xcactivitylog`.
fn find_latest_xcactivitylog(obj_root: &str) -> Option<PathBuf> {
    let dd_root = find_derived_data_root(obj_root)?;
    let log_dir = dd_root.join("Logs").join("Build");
    let mut entries: Vec<(std::time::SystemTime, String, PathBuf)> = Vec::new();
    for entry in std::fs::read_dir(&log_dir).ok()? {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("xcactivitylog") {
            continue;
        }
        let mtime = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let name = entry.file_name().to_string_lossy().into_owned();
        entries.push((mtime, name, path));
    }
    if entries.is_empty() {
        return None;
    }
    // Descending by (mtime, filename).
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Some(entries.into_iter().next().unwrap().2)
}

// ─── Public surface ─────────────────────────────────────────────────

/// The two timing products for a build. Either may be `None` independently;
/// both `None` means no timing source was available at all.
#[derive(Debug, Default)]
pub struct BuildTimings {
    /// The inline `build_metadata.timings` sub-object (`total_ms`, `top_tasks`,
    /// `<bucket>_ms`), or `None` when nothing was extractable.
    pub summary: Option<Value>,
    /// The per-target Gantt DETAIL blob — written RAW (NOT gzipped) as the
    /// `timings.json` bundle entry. `None` when there is no `Build target `
    /// grouping to chart.
    pub timeline: Option<Value>,
}

/// Resolve the build's timing data from the Xcode build-setting environment
/// (`$OBJROOT` → newest `.xcactivitylog` under the DerivedData `Logs/Build/`).
///
/// Never panics: any parser failure (malformed / truncated SLF stream,
/// unexpected arithmetic) degrades to `BuildTimings::default()` so the build's
/// other publish steps proceed. Port of `resolve_build_timings` (the broad
/// `try/except` becomes a `catch_unwind` + `Option`-returning internals).
pub fn resolve(env: &HashMap<String, String>) -> BuildTimings {
    std::panic::catch_unwind(|| resolve_impl(env)).unwrap_or_else(|_| {
        eprintln!(
            "Bugsee: build-timings extraction panicked — omitting timings from build_metadata"
        );
        BuildTimings::default()
    })
}

fn resolve_impl(env: &HashMap<String, String>) -> BuildTimings {
    let obj_root = env.get("OBJROOT").map(String::as_str).unwrap_or("");
    let log_path = match find_latest_xcactivitylog(obj_root) {
        Some(p) => p,
        None => return BuildTimings::default(),
    };
    let parsed = match parse_xcactivitylog(&log_path) {
        Some(p) => p,
        None => return BuildTimings::default(),
    };

    // Inline summary. Emit zero-valued category sums only when positive, so "no
    // data" stays distinguishable from "genuinely zero time in this category".
    let mut summary = Map::new();
    if parsed.total_ms > 0 {
        summary.insert("total_ms".into(), json!(parsed.total_ms));
    }
    if !parsed.top_tasks.is_empty() {
        summary.insert("top_tasks".into(), Value::Array(parsed.top_tasks));
    }
    for (bucket, value) in parsed.category_sums.iter() {
        if value > 0 {
            summary.insert(format!("{bucket}_ms"), json!(value));
        }
    }
    let summary = if summary.is_empty() {
        None
    } else {
        Some(Value::Object(summary))
    };

    // Timeline DETAIL blob — only when it carries at least one task, so the
    // caller never bundles an empty blob. Mirrors the deps "summary + blob"
    // split.
    let timeline = parsed.timeline.filter(|t| {
        t.get("tasks")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false)
    });

    BuildTimings { summary, timeline }
}

/// SLF0 synthetic-log encoder (port of the Python test's `_SLFWriter`). Lives
/// at module scope and is `pub(crate)` so sibling modules' integration tests
/// (`xcode.rs`'s post-action test) can drive the full `resolve()` wiring without
/// re-implementing the encoder.
#[cfg(test)]
pub(crate) mod fixtures {
    use super::{CLASS_COMMAND, CLASS_SECTION};
    use std::path::{Path, PathBuf};

    pub(crate) struct SlfWriter {
        buf: Vec<u8>,
        classes: Vec<String>,
    }

    impl SlfWriter {
        pub(crate) fn new() -> Self {
            Self {
                buf: b"SLF0".to_vec(),
                classes: Vec::new(),
            }
        }

        pub(crate) fn string(&mut self, s: &str) -> &mut Self {
            let raw = s.as_bytes();
            self.buf.extend_from_slice(raw.len().to_string().as_bytes());
            self.buf.push(b'"');
            self.buf.extend_from_slice(raw);
            self
        }

        pub(crate) fn class_def(&mut self, name: &str) -> i64 {
            let raw = name.as_bytes();
            self.buf.extend_from_slice(raw.len().to_string().as_bytes());
            self.buf.push(b'%');
            self.buf.extend_from_slice(raw);
            self.classes.push(name.to_string());
            self.classes.len() as i64
        }

        pub(crate) fn blob(&mut self, raw: &[u8]) -> &mut Self {
            self.buf.extend_from_slice(raw.len().to_string().as_bytes());
            self.buf.push(b'*');
            self.buf.extend_from_slice(raw);
            self
        }

        pub(crate) fn int(&mut self, v: i64) -> &mut Self {
            self.buf.extend_from_slice(format!("{v:x}").as_bytes());
            self.buf.push(b'#');
            self
        }

        pub(crate) fn double(&mut self, v: f64) -> &mut Self {
            let hex: String = v.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
            self.buf.extend_from_slice(hex.as_bytes());
            self.buf.push(b'^');
            self
        }

        pub(crate) fn class_ref(&mut self, idx: i64) -> &mut Self {
            self.buf.extend_from_slice(format!("{idx:x}").as_bytes());
            self.buf.push(b'@');
            self
        }

        pub(crate) fn array(&mut self, count: i64) -> &mut Self {
            self.buf.extend_from_slice(format!("{count:x}").as_bytes());
            self.buf.push(b'(');
            self
        }

        pub(crate) fn null(&mut self) -> &mut Self {
            self.buf.push(b'-');
            self
        }

        pub(crate) fn raw(&mut self, raw: &[u8]) -> &mut Self {
            self.buf.extend_from_slice(raw);
            self
        }

        pub(crate) fn bytes(&self) -> Vec<u8> {
            self.buf.clone()
        }
    }

    pub(crate) fn section_header(
        w: &mut SlfWriter,
        class_idx: i64,
        title: &str,
        start_cf: f64,
        end_cf: f64,
        n_children: i64,
    ) {
        w.class_ref(class_idx);
        w.int(1); // sectionType
        w.string("com.apple.dt.IDE.BuildLogSection"); // domainType
        w.string(title); // title (the field we extract)
        w.string(""); // signature
        w.double(start_cf); // timeStarted
        w.double(end_cf); // timeStopped
        if n_children > 0 {
            w.array(n_children);
        } else {
            w.null();
        }
    }

    pub(crate) const T0: f64 = 8.0e8;

    /// Assemble a synthetic SLF0 stream. `commands` and `targets` are
    /// `(title, start, end)`; `extra` is `(class_name, title, start, end)`.
    pub(crate) fn build_log(
        commands: &[(&str, f64, f64)],
        targets: &[(&str, f64, f64)],
        extra: &[(&str, &str, f64, f64)],
        with_blob: bool,
    ) -> Vec<u8> {
        let mut w = SlfWriter::new();
        let sec_idx = w.class_def(CLASS_SECTION);
        let cmd_idx = w.class_def(CLASS_COMMAND);
        // A non-section class def so a 1-based off-by-one would mis-resolve.
        w.class_def("IDEActivityLogUnitTestSection");

        if with_blob {
            // Body carries stray delimiter bytes — proves the tokenizer
            // consumes the length in bytes and stays in sync.
            w.blob(br##"{"k":"^\"#(value"}"##);
        }

        for (title, s, e) in targets {
            section_header(&mut w, sec_idx, title, *s, *e, 0);
            w.raw(b"  ");
        }
        for (cls_name, title, s, e) in extra {
            let idx = if *cls_name == CLASS_SECTION {
                sec_idx
            } else {
                cmd_idx
            };
            section_header(&mut w, idx, title, *s, *e, 0);
            w.raw(b"  ");
        }
        for (title, s, e) in commands {
            section_header(&mut w, cmd_idx, title, *s, *e, 0);
            w.raw(b"  ");
        }
        w.bytes()
    }

    pub(crate) fn write_gzipped_log(dir: &Path, data: &[u8]) -> PathBuf {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use std::io::Write;
        let path = dir.join("build.xcactivitylog");
        let f = std::fs::File::create(&path).unwrap();
        let mut enc = GzEncoder::new(f, Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap();
        path
    }

    /// Write a gzipped synthetic `.xcactivitylog` for the given sections into
    /// `dir`, returning its path. Used by `xcode.rs`'s post-action integration
    /// test to exercise the timings wiring end to end.
    pub(crate) fn write_synthetic_log(
        dir: &Path,
        commands: &[(&str, f64, f64)],
        targets: &[(&str, f64, f64)],
    ) -> PathBuf {
        write_gzipped_log(dir, &build_log(commands, targets, &[], false))
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn parse(commands: &[(&str, f64, f64)], targets: &[(&str, f64, f64)]) -> Parsed {
        parse_stream(&build_log(commands, targets, &[], false)).expect("parse")
    }

    // ── Tokenizer ──────────────────────────────────────────────────

    #[test]
    fn string_length_is_decimal_not_hex() {
        // 12 bytes — decimal "12". If treated as hex (0x12=18) the stream
        // would over-read and desync.
        let mut w = SlfWriter::new();
        w.string("abcdefghijkl"); // len 12
        let (tokens, desync) = tokenize_slf(&w.bytes());
        assert_eq!(desync, 0);
        assert_eq!(tokens, vec![Token::Str("abcdefghijkl".into())]);
    }

    #[test]
    fn blob_consumed_in_bytes_no_desync() {
        let mut w = SlfWriter::new();
        w.blob(br##"{"k":"^\"#(value"}"##);
        w.string("after");
        let (tokens, desync) = tokenize_slf(&w.bytes());
        assert_eq!(desync, 0);
        // The blob body's stray delimiters did NOT desync — `after` survives.
        assert_eq!(tokens.last(), Some(&Token::Str("after".into())));
    }

    #[test]
    fn double_decodes_little_endian() {
        let mut w = SlfWriter::new();
        w.double(1234.5);
        let (tokens, _d) = tokenize_slf(&w.bytes());
        match tokens.as_slice() {
            [Token::Double(Some(v))] => assert!((v - 1234.5).abs() < 1e-9),
            other => panic!("expected one double, got {other:?}"),
        }
    }

    #[test]
    fn int_is_hex() {
        // `ff#` → 255. We don't keep the int VALUE, but the type must parse and
        // not desync.
        let mut data = b"SLF0".to_vec();
        data.extend_from_slice(b"ff#");
        let (tokens, desync) = tokenize_slf(&data);
        assert_eq!(desync, 0);
        assert_eq!(tokens, vec![Token::Int]);
    }

    #[test]
    fn null_token() {
        let mut data = b"SLF0".to_vec();
        data.push(b'-');
        let (tokens, desync) = tokenize_slf(&data);
        assert_eq!(desync, 0);
        assert_eq!(tokens, vec![Token::Null]);
    }

    #[test]
    fn malformed_double_yields_none_not_crash() {
        // 16 non-hex chars before `^` → Double(None) + desync, no panic.
        let mut data = b"SLF0".to_vec();
        data.extend_from_slice(b"zzzzzzzzzzzzzzzz^");
        let (tokens, desync) = tokenize_slf(&data);
        assert_eq!(tokens, vec![Token::Double(None)]);
        assert_eq!(desync, 1);
    }

    #[test]
    fn short_double_payload_is_none_without_desync() {
        // Wrong length (not 16) → None, but NOT counted as desync (mirrors the
        // agent: only a 16-char malformed-hex payload desyncs).
        let mut data = b"SLF0".to_vec();
        data.extend_from_slice(b"abc^");
        let (tokens, desync) = tokenize_slf(&data);
        assert_eq!(tokens, vec![Token::Double(None)]);
        assert_eq!(desync, 0);
    }

    // ── Section extraction ──────────────────────────────────────────

    #[test]
    fn resolves_class_refs_1_based() {
        let parsed = parse(
            &[("Compile Foo.swift (arm64)", T0, T0 + 1.0)],
            &[("Build target Alpha", T0, T0 + 2.0)],
        );
        // The command + target were both resolved through the 1-based table.
        assert!(parsed.total_ms > 0);
        assert_eq!(parsed.top_tasks.len(), 1);
    }

    #[test]
    fn inverted_timestamps_dropped() {
        // end < start → section dropped → no sections → None.
        let data = build_log(&[("Ld X", T0 + 5.0, T0 + 1.0)], &[], &[], false);
        assert!(parse_stream(&data).is_none());
    }

    #[test]
    fn non_finite_timestamps_dropped() {
        let data = build_log(&[("Ld X", f64::NAN, T0 + 1.0)], &[], &[], false);
        assert!(parse_stream(&data).is_none());
    }

    #[test]
    fn huge_finite_span_drops_timings_no_saturation() {
        // A corrupt log with finite-but-absurd timestamps: the span × 1000
        // overflows to `inf`. Python raises OverflowError → no timings; the Rust
        // must drop timings too, NOT emit a saturated `i64::MAX` total_ms.
        let data = build_log(&[("Compile A.swift", T0, 1.0e308)], &[], &[], false);
        assert!(
            parse_stream(&data).is_none(),
            "an overflowing span must drop timings, not saturate"
        );
    }

    #[test]
    fn non_section_class_refs_ignored() {
        // A perfectly section-SHAPED token sequence whose `@` ref points at a
        // class that is NEITHER section class (here index 3, the unit-test
        // class) must be ignored — it does not become a section.
        let mut w = SlfWriter::new();
        w.class_def(CLASS_SECTION); // index 1
        w.class_def(CLASS_COMMAND); // index 2
        w.class_def("IDEActivityLogUnitTestSection"); // index 3
        section_header(&mut w, 3, "RunTests", T0, T0 + 1.0, 0);
        // Only the non-section class instance exists → nothing parsed.
        assert!(parse_stream(&w.bytes()).is_none());
    }

    // ── Interval union ──────────────────────────────────────────────

    #[test]
    fn interval_union_empty() {
        assert_eq!(interval_union_seconds(&[]), 0.0);
    }

    #[test]
    fn interval_union_disjoint_sums() {
        assert!((interval_union_seconds(&[(0.0, 1.0), (2.0, 4.0)]) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn interval_union_fully_overlapping_is_not_sum() {
        // Two identical 3s windows → union is 3s, not 6s.
        assert!((interval_union_seconds(&[(0.0, 3.0), (0.0, 3.0)]) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn interval_union_partial_overlap_merges() {
        assert!((interval_union_seconds(&[(0.0, 2.0), (1.0, 3.0)]) - 3.0).abs() < 1e-9);
    }

    #[test]
    fn interval_union_nested_absorbed() {
        assert!((interval_union_seconds(&[(0.0, 10.0), (2.0, 3.0)]) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn interval_union_unsorted_input() {
        assert!((interval_union_seconds(&[(5.0, 6.0), (0.0, 1.0), (2.0, 3.0)]) - 3.0).abs() < 1e-9);
    }

    // ── Classifier ──────────────────────────────────────────────────

    #[test]
    fn swift_compile_is_native() {
        assert_eq!(
            classify_section_title("Compile Foo.swift (arm64)"),
            Some(Category::Native)
        );
    }

    #[test]
    fn compile_c_is_native() {
        assert_eq!(
            classify_section_title("CompileC bar.o"),
            Some(Category::Native)
        );
    }

    #[test]
    fn compile_swift_sources_is_native() {
        assert_eq!(
            classify_section_title("CompileSwiftSources normal arm64"),
            Some(Category::Native)
        );
    }

    #[test]
    fn compiling_clang_module_is_native() {
        assert_eq!(
            classify_section_title("Compiling Clang module Foundation"),
            Some(Category::Native)
        );
    }

    #[test]
    fn ld_is_packaging() {
        assert_eq!(
            classify_section_title("Ld MyApp normal"),
            Some(Category::Packaging)
        );
    }

    #[test]
    fn link_is_packaging() {
        assert_eq!(
            classify_section_title("Link MyApp"),
            Some(Category::Packaging)
        );
    }

    #[test]
    fn link_storyboards_is_resources() {
        // Must be caught by resources BEFORE packaging's generic `^Link\b`.
        assert_eq!(
            classify_section_title("LinkStoryboards"),
            Some(Category::Resources)
        );
    }

    #[test]
    fn validate_is_packaging() {
        assert_eq!(
            classify_section_title("Validate MyApp.app"),
            Some(Category::Packaging)
        );
    }

    #[test]
    fn compile_asset_catalogs_is_resources() {
        assert_eq!(
            classify_section_title("Compile asset catalog Assets.xcassets"),
            Some(Category::Resources)
        );
    }

    #[test]
    fn process_info_plist_is_resources() {
        assert_eq!(
            classify_section_title("ProcessInfoPlistFile Info.plist"),
            Some(Category::Resources)
        );
    }

    #[test]
    fn unknown_phase_is_other() {
        assert_eq!(
            classify_section_title("Resolve Package Graph"),
            Some(Category::Other)
        );
    }

    #[test]
    fn empty_title_returns_none() {
        assert_eq!(classify_section_title(""), None);
    }

    // ── Sanitizer ───────────────────────────────────────────────────

    #[test]
    fn user_home_path_stripped() {
        assert_eq!(
            sanitize_section_title_for_emission(
                "Compile /Users/alice/Projects/MyApp/Sources/Foo.swift"
            ),
            "Compile <home>/Projects/MyApp/Sources/Foo.swift"
        );
    }

    #[test]
    fn user_home_path_stripped_when_followed_by_non_path_char() {
        // The username class stops at the first non-username char, preserving
        // the surrounding sentence shape.
        assert_eq!(
            sanitize_section_title_for_emission("see /Users/alice)"),
            "see <home>)"
        );
    }

    #[test]
    fn pii_free_title_unchanged() {
        assert_eq!(
            sanitize_section_title_for_emission("Compile Foo.swift (arm64)"),
            "Compile Foo.swift (arm64)"
        );
    }

    #[test]
    fn private_var_folders_path_cleaned() {
        let out = sanitize_section_title_for_emission(
            "WriteAuxiliaryFile /private/var/folders/xy/abc/T/Script.sh",
        );
        assert_eq!(out, "WriteAuxiliaryFile Script.sh");
    }

    #[test]
    fn generic_absolute_path_collapsed_to_basename() {
        assert_eq!(
            sanitize_section_title_for_emission("Strip /Applications/Xcode.app/Contents/usr"),
            "Strip usr"
        );
    }

    #[test]
    fn empty_passes_through() {
        assert_eq!(sanitize_section_title_for_emission(""), "");
    }

    // ── Dominant category ───────────────────────────────────────────

    #[test]
    fn dominant_greatest_occupancy_wins() {
        let cmds = [
            (Category::Native, T0, T0 + 1.0),
            (Category::Packaging, T0 + 1.0, T0 + 5.0),
        ];
        assert_eq!(
            dominant_category_for_window(&cmds, T0, T0 + 6.0),
            Category::Packaging
        );
    }

    #[test]
    fn dominant_occupancy_not_sum_decides() {
        // Native: three identical 2s windows (union 2s). Packaging: one 3s
        // window. Occupancy (union) picks packaging, a naive SUM would pick
        // native (6s).
        let cmds = [
            (Category::Native, T0, T0 + 2.0),
            (Category::Native, T0, T0 + 2.0),
            (Category::Native, T0, T0 + 2.0),
            (Category::Packaging, T0 + 2.0, T0 + 5.0),
        ];
        assert_eq!(
            dominant_category_for_window(&cmds, T0, T0 + 6.0),
            Category::Packaging
        );
    }

    #[test]
    fn dominant_only_contained_commands_count() {
        // A long packaging command straddles the window end → not counted.
        let cmds = [
            (Category::Native, T0, T0 + 1.0),
            (Category::Packaging, T0 + 1.0, T0 + 100.0),
        ];
        assert_eq!(
            dominant_category_for_window(&cmds, T0, T0 + 2.0),
            Category::Native
        );
    }

    #[test]
    fn dominant_boundary_inclusive() {
        // Commands ending/starting exactly at the window edge ARE contained.
        let cmds = [(Category::Packaging, T0, T0 + 2.0)];
        assert_eq!(
            dominant_category_for_window(&cmds, T0, T0 + 2.0),
            Category::Packaging
        );
    }

    #[test]
    fn dominant_no_contained_falls_back_to_native() {
        let cmds = [(Category::Packaging, T0 + 10.0, T0 + 11.0)];
        assert_eq!(
            dominant_category_for_window(&cmds, T0, T0 + 1.0),
            Category::Native
        );
    }

    #[test]
    fn dominant_tie_break_keeps_first_seen() {
        // Two categories with equal occupancy → the one appearing first in
        // `commands` order wins.
        let cmds = [
            (Category::Resources, T0, T0 + 2.0),
            (Category::Packaging, T0 + 2.0, T0 + 4.0),
        ];
        assert_eq!(
            dominant_category_for_window(&cmds, T0, T0 + 4.0),
            Category::Resources
        );
    }

    // ── Parse pipeline ──────────────────────────────────────────────

    #[test]
    fn total_ms_is_span_over_all_sections() {
        let parsed = parse(
            &[("Compile Foo.swift", T0, T0 + 3.0)],
            &[("Build target Alpha", T0, T0 + 5.0)],
        );
        // Span = 5s = 5000ms.
        assert_eq!(parsed.total_ms, 5000);
    }

    #[test]
    fn category_occupancy_is_union_not_sum() {
        // Three identical 2s native windows → occupancy 2000ms, not 6000ms.
        let parsed = parse(
            &[
                ("Compile A.swift", T0, T0 + 2.0),
                ("Compile B.swift", T0, T0 + 2.0),
                ("Compile C.swift", T0, T0 + 2.0),
            ],
            &[("Build target Alpha", T0, T0 + 2.0)],
        );
        assert_eq!(parsed.category_sums.native, 2000);
        assert!(parsed.category_sums.native <= parsed.total_ms);
    }

    #[test]
    fn per_category_occupancy_independent() {
        let parsed = parse(
            &[
                ("Compile A.swift", T0, T0 + 2.0),
                ("Ld App", T0 + 2.0, T0 + 5.0),
            ],
            &[("Build target Alpha", T0, T0 + 5.0)],
        );
        assert_eq!(parsed.category_sums.native, 2000);
        assert_eq!(parsed.category_sums.packaging, 3000);
        assert_eq!(parsed.category_sums.resources, 0);
        assert_eq!(parsed.category_sums.managed_code, 0);
    }

    #[test]
    fn gantt_emits_only_build_target_groupings() {
        let parsed = parse(
            &[("Compile Foo.swift", T0, T0 + 3.0)],
            &[
                ("Build BugseeApp", T0, T0 + 5.0), // mega-wrapper (no prefix)
                ("Build target Alpha", T0, T0 + 4.0),
                ("Build target Beta", T0 + 1.0, T0 + 5.0),
            ],
        );
        let timeline = parsed.timeline.expect("timeline");
        let paths: Vec<&str> = timeline["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["path"].as_str().unwrap())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["Alpha", "Beta"]);
        assert!(!paths.contains(&"BugseeApp"));
    }

    #[test]
    fn gantt_offsets_relative_to_build_start() {
        let parsed = parse(&[], &[("Build target Beta", T0 + 1.0, T0 + 5.0)]);
        let timeline = parsed.timeline.expect("timeline");
        let task = &timeline["tasks"][0];
        // build_start = T0 (earliest section = the target itself at T0+1? no —
        // only one section, so build_start = T0+1). Offsets are relative to it.
        assert_eq!(task["start_ms"], json!(0));
        assert_eq!(task["end_ms"], json!(4000));
        // build_started_at_ms is anchored to the build's start in Unix-epoch ms.
        let expected = py_round_to_i64((T0 + 1.0 + CF_ABSOLUTE_TIME_EPOCH_OFFSET) * 1000.0);
        assert_eq!(timeline["build_started_at_ms"], json!(expected));
    }

    #[test]
    fn gantt_sorted_by_start_ms_ascending() {
        let parsed = parse(
            &[],
            &[
                ("Build target Late", T0 + 3.0, T0 + 4.0),
                ("Build target Early", T0, T0 + 1.0),
            ],
        );
        let timeline = parsed.timeline.expect("timeline");
        let tasks = timeline["tasks"].as_array().unwrap();
        let starts: Vec<i64> = tasks
            .iter()
            .map(|t| t["start_ms"].as_i64().unwrap())
            .collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted);
        assert_eq!(tasks[0]["path"], json!("Early"));
    }

    #[test]
    fn dominant_category_assignment_in_timeline() {
        let parsed = parse(
            &[("Ld Alpha", T0, T0 + 3.0)],
            &[("Build target Alpha", T0, T0 + 4.0)],
        );
        let timeline = parsed.timeline.expect("timeline");
        assert_eq!(timeline["tasks"][0]["category"], json!("packaging"));
    }

    #[test]
    fn dominant_category_falls_back_to_native_in_timeline() {
        // Target with no contained classifiable command → native bar.
        let parsed = parse(&[], &[("Build target Empty", T0, T0 + 2.0)]);
        let timeline = parsed.timeline.expect("timeline");
        assert_eq!(timeline["tasks"][0]["category"], json!("native"));
    }

    /// Byte-for-byte parity with the Python BugseeAgent reference parser. The
    /// expected values were captured by running `agent._parse_xcactivitylog` on
    /// this exact fixture (fractional times exercise rounding + occupancy):
    /// `{category_sums:{native:3000,packaging:3000,resources:500,other:0,
    /// managed_code:0}, total_ms:7500, top_tasks:[{Beta,5500},{Alpha,4250}],
    /// timeline:{build_started_at_ms:1778307200000, wall_clock_ms:6500,
    /// tasks:[{native,Alpha,0,4250},{packaging,Beta,1000,6500}]}}`.
    #[test]
    fn parity_with_python_reference_fixture() {
        let parsed = parse(
            &[
                ("Compile Foo.swift (arm64)", T0, T0 + 2.5), // native
                ("Compile Bar.swift (arm64)", T0 + 0.5, T0 + 3.0), // native (overlaps)
                ("Ld Beta", T0 + 3.0, T0 + 6.0),             // packaging
                ("Compile asset catalog", T0 + 1.0, T0 + 1.5), // resources
            ],
            &[
                ("Build BugseeApp", T0, T0 + 7.5), // mega-wrapper (excluded from tasks)
                ("Build target Alpha", T0, T0 + 4.25),
                ("Build target Beta", T0 + 1.0, T0 + 6.5),
            ],
        );

        // total_ms = wall-clock span over ALL sections (the 7.5s wrapper).
        assert_eq!(parsed.total_ms, 7500);
        // Per-category OCCUPANCY (interval union): native union of [0,2.5] and
        // [0.5,3.0] = 3.0s; packaging Ld = 3.0s; resources asset = 0.5s.
        assert_eq!(parsed.category_sums.native, 3000);
        assert_eq!(parsed.category_sums.packaging, 3000);
        assert_eq!(parsed.category_sums.resources, 500);
        assert_eq!(parsed.category_sums.other, 0);
        assert_eq!(parsed.category_sums.managed_code, 0);

        // top_tasks: slowest target first (Beta 5500ms, Alpha 4250ms).
        let tt: Vec<(&str, i64)> = parsed
            .top_tasks
            .iter()
            .map(|t| {
                (
                    t["name"].as_str().unwrap(),
                    t["duration_ms"].as_i64().unwrap(),
                )
            })
            .collect();
        assert_eq!(tt, vec![("Beta", 5500), ("Alpha", 4250)]);

        // Timeline: mega-wrapper excluded; Alpha dominant=native, Beta=packaging.
        let tl = parsed.timeline.expect("timeline");
        assert_eq!(tl["schema_version"], json!(1));
        assert_eq!(tl["build_started_at_ms"], json!(1_778_307_200_000_i64));
        assert_eq!(tl["wall_clock_ms"], json!(6500));
        let tasks = tl["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 2);
        // Sorted by start_ms: Alpha (0) then Beta (1000).
        assert_eq!(
            tasks[0],
            json!({"path":"Alpha","category":"native","start_ms":0,"end_ms":4250})
        );
        assert_eq!(
            tasks[1],
            json!({"path":"Beta","category":"packaging","start_ms":1000,"end_ms":6500})
        );
    }

    #[test]
    fn top_tasks_are_targets_slowest_first() {
        let parsed = parse(
            &[],
            &[
                ("Build target Slow", T0, T0 + 9.0),
                ("Build target Fast", T0, T0 + 1.0),
            ],
        );
        let names: Vec<&str> = parsed
            .top_tasks
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Slow", "Fast"]);
        assert_eq!(parsed.top_tasks[0]["duration_ms"], json!(9000));
    }

    #[test]
    fn top_tasks_excludes_zero_duration_targets() {
        let parsed = parse(
            &[],
            &[
                ("Build target Real", T0, T0 + 2.0),
                ("Build target Instant", T0, T0), // 0ms → noise, dropped
            ],
        );
        let names: Vec<&str> = parsed
            .top_tasks
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["Real"]);
    }

    #[test]
    fn no_targets_yields_no_timeline() {
        let parsed = parse(&[("Compile Foo.swift", T0, T0 + 1.0)], &[]);
        assert!(parsed.timeline.is_none());
        assert!(parsed.top_tasks.is_empty());
    }

    #[test]
    fn blob_in_stream_does_not_break_parse() {
        let data = build_log(
            &[("Compile Foo.swift", T0, T0 + 1.0)],
            &[("Build target Alpha", T0, T0 + 2.0)],
            &[],
            true, // inject a `*` blob with stray delimiters up front
        );
        let parsed = parse_stream(&data).expect("parse survives the blob");
        assert_eq!(parsed.top_tasks.len(), 1);
        assert_eq!(parsed.top_tasks[0]["name"], json!("Alpha"));
    }

    // ── resolve() (fs-backed) ───────────────────────────────────────

    #[test]
    fn resolve_none_when_objroot_missing() {
        let env = HashMap::new();
        let t = resolve(&env);
        assert!(t.summary.is_none());
        assert!(t.timeline.is_none());
    }

    #[test]
    fn resolve_none_when_no_log_found() {
        let td = tempfile::tempdir().unwrap();
        let mut env = HashMap::new();
        env.insert("OBJROOT".into(), td.path().to_string_lossy().into_owned());
        let t = resolve(&env);
        assert!(t.summary.is_none() && t.timeline.is_none());
    }

    #[test]
    fn resolve_swallows_bad_gzip() {
        // A `.xcactivitylog` that is not valid gzip → parse returns None →
        // (None, None), no panic.
        let td = tempfile::tempdir().unwrap();
        let logs_build = td.path().join("Logs").join("Build");
        std::fs::create_dir_all(&logs_build).unwrap();
        std::fs::write(logs_build.join("fake.xcactivitylog"), b"not real gzip").unwrap();
        let obj_root = td.path().join("Build").join("Intermediates.noindex");
        std::fs::create_dir_all(&obj_root).unwrap();
        let mut env = HashMap::new();
        env.insert("OBJROOT".into(), obj_root.to_string_lossy().into_owned());
        let t = resolve(&env);
        assert!(t.summary.is_none() && t.timeline.is_none());
    }

    #[test]
    fn resolve_full_synthetic_fixture() {
        let td = tempfile::tempdir().unwrap();
        let logs_build = td.path().join("Logs").join("Build");
        std::fs::create_dir_all(&logs_build).unwrap();
        let targets = [
            ("Build BugseeSwiftUIDev", T0, T0 + 5.0), // mega-wrapper
            ("Build target Alpha", T0, T0 + 4.0),
            ("Build target Beta", T0 + 1.0, T0 + 5.0),
        ];
        let commands = [
            ("Compile Foo.swift (arm64)", T0, T0 + 3.0), // native
            ("Ld Beta", T0 + 1.0, T0 + 4.0),             // packaging
        ];
        let data = build_log(&commands, &targets, &[], false);
        // Write into the discovered Logs/Build dir.
        let log_path = write_gzipped_log(&logs_build, &data);
        assert!(log_path.exists());

        let obj_root = td
            .path()
            .join("Build")
            .join("Intermediates.noindex")
            .join("ArchiveIntermediates")
            .join("MyApp")
            .join("IntermediateBuildFilesPath");
        std::fs::create_dir_all(&obj_root).unwrap();

        let mut env = HashMap::new();
        env.insert("OBJROOT".into(), obj_root.to_string_lossy().into_owned());
        let t = resolve(&env);

        let summary = t.summary.expect("summary");
        assert!(summary["total_ms"].as_i64().unwrap() > 0);
        assert!(summary.get("top_tasks").is_some());
        // Per-category occupancy emitted as `<bucket>_ms`, never exceeding total.
        let native_ms = summary["native_ms"].as_i64().unwrap();
        assert!(native_ms <= summary["total_ms"].as_i64().unwrap());
        // managed_code is zero on iOS → omitted.
        assert!(summary.get("managed_code_ms").is_none());
        // top_tasks are TARGETS (prefix stripped), not the mega-wrapper.
        let top_names: Vec<&str> = summary["top_tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert!(top_names.contains(&"Alpha"));
        assert!(top_names.contains(&"Beta"));
        assert!(!top_names.contains(&"BugseeSwiftUIDev"));

        // The Gantt blob excludes the mega-wrapper and is RAW (not gzipped).
        let timeline = t.timeline.expect("timeline");
        assert_eq!(timeline["schema_version"], json!(1));
        let mut paths: Vec<&str> = timeline["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["path"].as_str().unwrap())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["Alpha", "Beta"]);
    }

    // ── Rounding parity ─────────────────────────────────────────────

    #[test]
    fn py_round_is_half_to_even() {
        // Python 3 round(): ties go to the nearest EVEN integer.
        assert_eq!(py_round_to_i64(0.5), 0);
        assert_eq!(py_round_to_i64(1.5), 2);
        assert_eq!(py_round_to_i64(2.5), 2);
        assert_eq!(py_round_to_i64(3.5), 4);
        assert_eq!(py_round_to_i64(-0.5), 0);
        assert_eq!(py_round_to_i64(-1.5), -2);
        // Non-ties round normally.
        assert_eq!(py_round_to_i64(2.4), 2);
        assert_eq!(py_round_to_i64(2.6), 3);
    }

    // ── DerivedData discovery ───────────────────────────────────────

    #[test]
    fn find_derived_data_root_walks_up() {
        let td = tempfile::tempdir().unwrap();
        let dd = td.path();
        std::fs::create_dir_all(dd.join("Logs").join("Build")).unwrap();
        let deep = dd
            .join("Build")
            .join("Intermediates.noindex")
            .join("Archive");
        std::fs::create_dir_all(&deep).unwrap();
        let found = find_derived_data_root(&deep.to_string_lossy()).unwrap();
        assert_eq!(found, dd);
    }

    #[test]
    fn find_derived_data_root_none_when_absent() {
        let td = tempfile::tempdir().unwrap();
        assert!(find_derived_data_root(&td.path().to_string_lossy()).is_none());
    }

    #[test]
    fn find_latest_picks_newest_by_name_on_mtime_tie() {
        let td = tempfile::tempdir().unwrap();
        let logs_build = td.path().join("Logs").join("Build");
        std::fs::create_dir_all(&logs_build).unwrap();
        // Two logs; descending-by-name breaks an mtime tie deterministically.
        std::fs::write(logs_build.join("A-aaaa.xcactivitylog"), b"x").unwrap();
        std::fs::write(logs_build.join("Z-zzzz.xcactivitylog"), b"y").unwrap();
        // A non-log file must be ignored.
        std::fs::write(logs_build.join("readme.txt"), b"z").unwrap();
        let found = find_latest_xcactivitylog(&td.path().to_string_lossy()).unwrap();
        assert!(found
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("Z-"));
    }
}
