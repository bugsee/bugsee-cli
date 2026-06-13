//! iOS dependency-graph collector.
//!
//! Replaces the duplicate Python parsers that lived in both the iOS
//! SDK's `tools.bundle/BugseeAgent` and the fastlane plugin's
//! `BugseeAgent`. Both Python sides now shell to `bugsee-cli ios-deps
//! collect` and consume the JSON output, leaving the Python sides as
//! thin wrappers that pass the result to their existing
//! `_build_dependencies_payload` wire-shape formatters.
//!
//! ## Sources scanned
//!
//! - **CocoaPods** — `Podfile.lock` (carries a real graph; subspecs
//!   declared in DEPENDENCIES are direct, transitive subspecs of an
//!   umbrella pod are not).
//! - **Swift Package Manager** — `Package.resolved` (both Xcode-managed
//!   `{"object": {"pins": ...}}` shape and SPM CLI v2 `{"pins": ...}`).
//! - **Carthage** — `Cartfile.resolved` (`github` / `git` / `binary`
//!   lines). No graph info, all entries direct.
//! - **Vendored frameworks** — `otool -L` on the linked product binary
//!   when supplied. `file`-type entries for `@rpath/...` and
//!   `@executable_path/...` Mach-O references, system dylibs filtered.
//!
//! ## Wire shape
//!
//! Output JSON to stdout:
//! ```json
//! {
//!   "entries":     [DepEntry, ...],
//!   "scope_label": "all",
//!   "truncated":   false
//! }
//! ```
//!
//! `DepEntry`:
//! ```json
//! {
//!   "id":      "library::SocketRocket",     // <type>:<group>:<name>
//!   "group":   "",
//!   "name":    "SocketRocket",
//!   "version": "0.7.1",                     // null if unknown
//!   "direct":  true,
//!   "scope":   null,
//!   "type":    "library" | "file",
//!   "parents": ["library::Foo", ...]
//! }
//! ```
//!
//! Field shape MUST stay byte-compatible with the Python
//! implementations the migration retires — both have pinned tests for
//! the exact output format, including the `<type>:<group>:<name>` id
//! triple (single colons; empty group yields the double-colon
//! `library::Name` form by coincidence).

use clap::{Args, Subcommand};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default truncation cap. Matches the Android Gradle plugin's
/// `DependencyPayloadSerializer.MAX_ENTRIES` and the Python
/// `DEPENDENCIES_MAX_COUNT` — a cross-platform contract that the
/// fastlane reader and the worker both pin.
pub const DEPENDENCIES_MAX_COUNT: usize = 5000;

/// `bugsee-cli ios-deps` argument shape.
#[derive(Subcommand, Debug)]
pub enum IosDepsCommand {
    /// Scan a project root for iOS dep manifests + an optional linked
    /// binary, merge into one entry list, output JSON to stdout.
    Collect(CollectArgs),
}

#[derive(Args, Debug)]
pub struct CollectArgs {
    /// Project root. Lockfiles are searched for at this path and at
    /// each ancestor up to 6 levels (matches the Python
    /// `_find_first_above` contract).
    #[arg(long)]
    pub project_root: PathBuf,

    /// Optional linked product binary for the vendored-framework
    /// scan. When given, runs `otool -L` and emits `file`-type
    /// entries for `@rpath/...` and `@executable_path/...` Mach-O
    /// references. iOS-only.
    #[arg(long)]
    pub product_binary: Option<PathBuf>,

    /// Truncation cap on the merged entry list. Defaults to 5000.
    #[arg(long, default_value_t = DEPENDENCIES_MAX_COUNT)]
    pub max_entries: usize,
}

/// Per-entry shape. Matches the Python wire format the existing
/// `_build_dependencies_payload` consumes.
///
/// The optional `url` field is populated from Package.resolved's
/// `location` field for SPM entries — it's the key OSV's
/// SwiftURL ecosystem uses for vulnerability lookups. Dedup
/// prefers entries that carry `url` over those that don't (same
/// package via CocoaPods vs SPM → SPM wins because it has the
/// upstream URL).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DepEntry {
    pub id: String,
    pub group: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub direct: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(rename = "type")]
    pub type_: String,
    pub parents: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// Top-level CLI output shape. Mirrors the Python
/// `_collect_all_dependencies` return tuple
/// `(entries, scope_label, truncated)`.
#[derive(Debug, Clone, Serialize)]
pub struct CollectResult {
    pub entries: Vec<DepEntry>,
    pub scope_label: String,
    pub truncated: bool,
}

/// Canonical id format. Single colons; an empty `group` yields the
/// `<type>::<name>` form by coincidence, which is exactly what the
/// viewer's `identityOf` and the worker's `_identity` functions
/// produce.
pub fn make_dep_id(type_: &str, group: &str, name: &str) -> String {
    format!("{}:{}:{}", type_, group, name)
}

/// Strip the `(version)` or `(~> constraint)` suffix from a
/// CocoaPods reference line.
fn strip_pod_version_paren(s: &str) -> String {
    match s.find('(') {
        None => s.trim().to_string(),
        Some(idx) => s[..idx].trim().to_string(),
    }
}

/// Walk up from `start_dir` (or the documented relative path) looking
/// for `filename`. Caps the climb at 6 levels — matches the Python
/// `_find_first_above(max_levels=6)` contract.
pub fn find_first_above(start_dir: &Path, filename: &str) -> Option<PathBuf> {
    if start_dir.as_os_str().is_empty() {
        return None;
    }
    let mut current = start_dir
        .canonicalize()
        .unwrap_or_else(|_| start_dir.to_path_buf());
    for _ in 0..6 {
        let candidate = current.join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }
        let parent = match current.parent() {
            Some(p) if p != current => p.to_path_buf(),
            _ => return None,
        };
        current = parent;
    }
    None
}

/// Locate an Xcode-managed `Package.resolved` by scanning `root` for
/// sibling `*.xcodeproj` and `*.xcworkspace` directories and probing
/// the nested SPM path inside each. Mirrors the Python BugseeAgent's
/// `_spm_resolved_paths` (tools.bundle/BugseeAgent:3578–3595) which
/// is what both Python implementations relied on before the migration.
///
/// `find_first_above` only walks UP and joins the relative path to
/// each ancestor, so it never descends into a sibling `*.xcodeproj` —
/// that's the regression this helper closes. Returns the first
/// matching `Package.resolved`, with `.xcworkspace` preferred over
/// `.xcodeproj` (Xcode prefers a workspace when both exist).
pub fn find_xcode_spm_resolved(root: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(root).ok()?;
    let mut xcodeproj: Vec<PathBuf> = Vec::new();
    let mut xcworkspace: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let p = entry.path();
        if !p.is_dir() {
            continue;
        }
        match p.extension().and_then(|e| e.to_str()) {
            Some("xcodeproj") => xcodeproj.push(p),
            Some("xcworkspace") => xcworkspace.push(p),
            _ => {}
        }
    }
    let nested_in_proj = "project.xcworkspace/xcshareddata/swiftpm/Package.resolved";
    let nested_in_ws = "xcshareddata/swiftpm/Package.resolved";
    for ws in &xcworkspace {
        let candidate = ws.join(nested_in_ws);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    for proj in &xcodeproj {
        let candidate = proj.join(nested_in_proj);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ─── Podfile.lock parser ────────────────────────────────────────────

/// Parse a Podfile.lock. Returns the list of top-level pods + parent
/// edges built from the children references inside the PODS section.
pub fn parse_podfile_lock(path: &Path) -> Vec<DepEntry> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // Split into the PODS: and DEPENDENCIES: sections; everything
    // past is ignored.
    let mut pods_lines: Vec<&str> = Vec::new();
    let mut deps_lines: Vec<&str> = Vec::new();
    let mut current: Option<&mut Vec<&str>> = None;
    for line in content.lines() {
        let trimmed = line.trim_end();
        if is_section_header(trimmed) {
            let header = trimmed.trim_end_matches(':').trim();
            current = match header {
                "PODS" => Some(&mut pods_lines),
                "DEPENDENCIES" => Some(&mut deps_lines),
                _ => None,
            };
            continue;
        }
        if let Some(ref mut buf) = current {
            buf.push(line);
        }
    }

    // PODS: parse — top-level pods and their immediate children.
    let mut pods: HashMap<String, PodInfo> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut current_pod: Option<String> = None;
    for line in &pods_lines {
        if line.trim().is_empty() {
            continue;
        }
        // Child line: `    - Name [(constraint)]` (4-space indent).
        if let Some(child) = parse_child_line(line) {
            if let Some(ref pod) = current_pod {
                let info = pods.get_mut(pod).expect("current_pod set ⇒ entry exists");
                let child_ref = strip_pod_version_paren(child);
                info.children.push(child_ref);
            }
            continue;
        }
        // Pod line: `  - Name (Version):` (2-space indent).
        if let Some(pod_body) = parse_pod_line(line) {
            let (name, version) = parse_name_version(pod_body);
            pods.insert(
                name.clone(),
                PodInfo {
                    version,
                    children: Vec::new(),
                },
            );
            order.push(name.clone());
            current_pod = Some(name);
        }
    }

    // DEPENDENCIES: bare names of pods the user declared.
    let mut direct_names: HashSet<String> = HashSet::new();
    for line in &deps_lines {
        if line.trim().is_empty() {
            continue;
        }
        if let Some(dep_body) = parse_dep_line(line) {
            direct_names.insert(strip_pod_version_paren(dep_body));
        }
    }

    // Reverse the children map to build the parents lookup.
    let mut parents_by_name: HashMap<String, Vec<String>> = HashMap::new();
    for name in &order {
        let info = &pods[name];
        let owner_id = make_dep_id("library", "", name);
        for child in &info.children {
            parents_by_name
                .entry(child.clone())
                .or_default()
                .push(owner_id.clone());
        }
    }

    // Emit entries in insertion order.
    let mut out = Vec::with_capacity(order.len());
    for name in &order {
        let info = &pods[name];
        out.push(DepEntry {
            id: make_dep_id("library", "", name),
            group: String::new(),
            name: name.clone(),
            version: info.version.clone(),
            direct: direct_names.contains(name),
            scope: None,
            type_: "library".to_string(),
            parents: parents_by_name.get(name).cloned().unwrap_or_default(),
            // CocoaPods lockfile carries no upstream URL — pods are
            // resolved through the CocoaPods spec repo by name.
            // Absence here is the signal that SPM's `location` (if
            // the same package is also pulled via SPM) should win
            // on dedup so the OSV vuln lookup has a URL key.
            url: None,
        });
    }
    out
}

#[derive(Debug)]
struct PodInfo {
    version: Option<String>,
    children: Vec<String>,
}

fn is_section_header(line: &str) -> bool {
    // Matches `^[A-Z][A-Z _]+:\s*$` from the Python regex.
    if !line.ends_with(':') {
        return false;
    }
    let head = &line[..line.len() - 1];
    if head.is_empty() {
        return false;
    }
    let mut chars = head.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_uppercase() {
        return false;
    }
    chars.all(|c| c.is_ascii_uppercase() || c == ' ' || c == '_')
}

/// `  - <body>` with optional trailing colon. Returns the body slice.
fn parse_pod_line(line: &str) -> Option<&str> {
    let body = line.strip_prefix("  - ")?;
    Some(body.strip_suffix(':').unwrap_or(body))
}

/// `    - <body>`.
fn parse_child_line(line: &str) -> Option<&str> {
    line.strip_prefix("    - ")
}

/// `  - <body>` in DEPENDENCIES section.
fn parse_dep_line(line: &str) -> Option<&str> {
    line.strip_prefix("  - ")
}

/// `Name (Version)` → ("Name", Some("Version")). If no parens,
/// returns (body, None).
fn parse_name_version(body: &str) -> (String, Option<String>) {
    if let Some(open) = body.find('(') {
        if let Some(close_rel) = body[open..].find(')') {
            let close = open + close_rel;
            let name = body[..open].trim().to_string();
            let version = body[open + 1..close].trim().to_string();
            return (name, Some(version));
        }
    }
    (body.trim().to_string(), None)
}

// ─── Package.resolved parser ────────────────────────────────────────

/// Parse Package.resolved (both Xcode-managed and SPM CLI v2 shapes).
pub fn parse_package_resolved(path: &Path) -> Vec<DepEntry> {
    let bytes = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let v: serde_json::Value = match serde_json::from_str(&bytes) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    // v1 (Xcode-managed): {"object": {"pins": [...]}}
    // v2 (SPM CLI):       {"pins": [...]}
    let pins = v
        .pointer("/pins")
        .or_else(|| v.pointer("/object/pins"))
        .and_then(|x| x.as_array())
        .cloned()
        .unwrap_or_default();

    let mut out = Vec::new();
    for pin in pins.iter() {
        // identity (lowercased) preferred over package (capitalised)
        // when both are present — stable across Xcode versions
        // writing the same package set.
        let name = pin
            .pointer("/identity")
            .and_then(|s| s.as_str())
            .or_else(|| pin.pointer("/package").and_then(|s| s.as_str()))
            .map(|s| s.to_string());
        let name = match name {
            Some(n) if !n.is_empty() => n,
            _ => continue,
        };

        // version > branch > revision (most specific human-readable
        // value wins).
        let version = pin
            .pointer("/state/version")
            .and_then(|s| s.as_str())
            .or_else(|| pin.pointer("/state/branch").and_then(|s| s.as_str()))
            .or_else(|| pin.pointer("/state/revision").and_then(|s| s.as_str()))
            .map(|s| s.to_string());

        // Upstream URL — v1 uses `repositoryURL`, v2 uses
        // `location`. Both forms encode the same thing (the SPM
        // pin's source location); we surface either. OSV's
        // SwiftURL ecosystem keys vulnerability lookups off this
        // field, so a missing URL silently excludes the package
        // from the vuln scan. Dedup pref prioritises url-bearing
        // entries over url-less ones (see merge_dep_entries).
        let url = pin
            .pointer("/location")
            .and_then(|s| s.as_str())
            .or_else(|| pin.pointer("/repositoryURL").and_then(|s| s.as_str()))
            .map(|s| s.to_string());

        out.push(DepEntry {
            id: make_dep_id("library", "", &name),
            group: String::new(),
            name,
            version,
            direct: true, // SPM doesn't carry graph info in the resolved file.
            scope: None,
            type_: "library".to_string(),
            parents: Vec::new(),
            url,
        });
    }
    out
}

// ─── Cartfile.resolved parser ───────────────────────────────────────

/// Parse a Cartfile.resolved. `github "repo" "version"` /
/// `git "url" "version"` / `binary "url" "version"`.
pub fn parse_cartfile_resolved(path: &Path) -> Vec<DepEntry> {
    let content = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let line_re = regex::Regex::new(
        r#"^\s*(\S+)\s+["']([^"']+)["']\s+["']([^"']+)["']\s*$"#,
    )
    .unwrap();

    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some(captures) = line_re.captures(line) else {
            continue;
        };
        let name = captures.get(2).map(|m| m.as_str()).unwrap_or("").to_string();
        let version = captures
            .get(3)
            .map(|m| m.as_str().to_string());
        out.push(DepEntry {
            id: make_dep_id("library", "", &name),
            group: String::new(),
            name,
            version,
            direct: true,
            scope: None,
            type_: "library".to_string(),
            parents: Vec::new(),
            // Cartfile.resolved doesn't carry an upstream URL for
            // the github / binary forms (the repo path IS the
            // identifier); leave None.
            url: None,
        });
    }
    out
}

// ─── Vendored frameworks (otool -L) ────────────────────────────────

const SYSTEM_DYLIB_PREFIXES: &[&str] = &[
    "/usr/lib/",
    "/System/Library/",
    "/Library/Frameworks/",
];

/// Run `otool -L` on the linked product binary and emit `file`-type
/// entries for each embedded framework reference.
pub fn parse_vendored_frameworks(binary_path: &Path) -> Vec<DepEntry> {
    if !binary_path.is_file() {
        return Vec::new();
    }
    let output = match Command::new("/usr/bin/otool")
        .args(["-L", binary_path.to_string_lossy().as_ref()])
        .output()
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return Vec::new(),
    };
    let stdout = String::from_utf8_lossy(&output);
    let framework_re = regex::Regex::new(r"/([^/]+\.framework)/").unwrap();
    let line_re = regex::Regex::new(r"^\s*([^\s]+)\s+\(compatibility version").unwrap();

    let mut seen: HashSet<String> = HashSet::new();
    let mut entries = Vec::new();
    for raw in stdout.lines() {
        let Some(captures) = line_re.captures(raw) else {
            continue;
        };
        let load_path = captures.get(1).map(|m| m.as_str()).unwrap_or("");
        if SYSTEM_DYLIB_PREFIXES.iter().any(|p| load_path.starts_with(p)) {
            continue;
        }
        if !(load_path.starts_with("@rpath/")
            || load_path.starts_with("@executable_path/")
            || load_path.starts_with("@loader_path/"))
        {
            continue;
        }
        let name = framework_re
            .captures(load_path)
            .and_then(|c| c.get(1).map(|m| m.as_str().to_string()))
            .unwrap_or_else(|| {
                Path::new(load_path)
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string()
            });
        if seen.contains(&name) {
            continue;
        }
        seen.insert(name.clone());
        entries.push(DepEntry {
            id: make_dep_id("file", "", &name),
            group: String::new(),
            name,
            version: None,
            direct: true,
            scope: None,
            type_: "file".to_string(),
            parents: Vec::new(),
            // Vendored frameworks are local binary blobs — no
            // upstream URL to surface.
            url: None,
        });
    }
    entries
}

// ─── Merger ─────────────────────────────────────────────────────────

/// Merge multiple source lists into one. Dedup prefers entries that
/// carry the `url` field over those that don't (OSV's SwiftURL
/// ecosystem needs the URL for vulnerability lookups, so dropping the
/// url-bearing variant in favour of a url-less one would silently
/// exclude that package from the vuln scan). Ties — both or neither
/// carry a url — keep the first seen, matching the manifest append
/// order (CocoaPods → SPM → Carthage → vendored).
///
/// Output is sorted by `id` for deterministic JSON output and for
/// stable diffs across consecutive runs of the same build.
///
/// Truncation: when the deduped set exceeds `max_entries`, drop
/// non-direct entries first (transitive deps are less actionable
/// than direct ones). Among entries with the same `direct` flag,
/// drop by id-ascending order so the truncation is deterministic.
///
/// Returns `(entries, truncated)`. After truncation, parent refs
/// pointing at evicted ids are stripped so consumers never need to
/// handle dangling refs.
pub fn merge_dep_entries(sources: Vec<Vec<DepEntry>>, max_entries: usize) -> (Vec<DepEntry>, bool) {
    // Dedup pass: url-preference, then first-seen. Track insertion
    // order via a parallel Vec<String> of ids so the dedup is
    // observable in tests.
    let mut by_id: HashMap<String, DepEntry> = HashMap::new();
    for source in sources {
        for entry in source {
            match by_id.get_mut(&entry.id) {
                None => {
                    by_id.insert(entry.id.clone(), entry);
                }
                Some(prev) => {
                    // Field-wise merge. Wholesale-replacing the
                    // existing record (the historic behaviour) loses
                    // first-seen `parents` / `direct` / `version`
                    // from the loser, because the parsers populate
                    // those asymmetrically: CocoaPods walks the PODS
                    // graph and emits `direct: false` +
                    // `parents: [...]` for transitive subspecs; SPM
                    // emits `direct: true` + `parents: []` because a
                    // `Package.resolved` only records resolved pins,
                    // not the dependency graph. If both sources
                    // surface the same id and SPM wins for url-
                    // preference, the CocoaPods graph evidence
                    // would silently disappear.
                    //
                    // Rules: copy `url` from incoming if previous
                    // had none. Promote `direct: true` (a true flag
                    // from any source is authoritative). Fill in
                    // `version` only when previous had none.
                    // Preserve previous `parents` — they're the more
                    // reliable graph signal.
                    if prev.url.is_none() && entry.url.is_some() {
                        prev.url = entry.url;
                    }
                    if entry.direct {
                        prev.direct = true;
                    }
                    if prev.version.is_none() && entry.version.is_some() {
                        prev.version = entry.version;
                    }
                    if prev.parents.is_empty() && !entry.parents.is_empty() {
                        prev.parents = entry.parents;
                    }
                }
            }
        }
    }
    if by_id.is_empty() {
        return (Vec::new(), false);
    }

    // Sort by id for deterministic output.
    let mut deduped: Vec<DepEntry> = by_id.into_values().collect();
    deduped.sort_by(|a, b| a.id.cmp(&b.id));

    let truncated = deduped.len() > max_entries;
    if truncated {
        // Direct-prefer truncation: keep direct entries first. Stable
        // sort floats direct entries to the front while preserving id-
        // ascending order within each group. Then truncate at the cap.
        deduped.sort_by(|a, b| match (a.direct, b.direct) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => std::cmp::Ordering::Equal, // stable sort preserves id order
        });
        deduped.truncate(max_entries);
        // Re-sort the kept set by id so the final output stays
        // id-ordered regardless of the truncation pass.
        deduped.sort_by(|a, b| a.id.cmp(&b.id));
    }

    // Self-consistency: filter parent refs pointing at evicted ids.
    let kept: HashSet<String> = deduped.iter().map(|e| e.id.clone()).collect();
    for e in deduped.iter_mut() {
        if !e.parents.is_empty() {
            e.parents.retain(|p| kept.contains(p));
        }
    }
    (deduped, truncated)
}

// ─── Orchestrator + CLI dispatch ────────────────────────────────────

/// Top-level orchestrator. Locates each source under `project_root`,
/// parses each, merges, returns the canonical result.
pub fn collect(project_root: &Path, product_binary: Option<&Path>, max_entries: usize) -> CollectResult {
    let podfile = find_first_above(project_root, "Podfile.lock");
    // SPM `Package.resolved` discovery has three shapes the iOS
    // toolchain produces:
    //   1. Pure-SPM package: directly at `<root>/Package.resolved`.
    //   2. Xcode-managed (project file): nested under
    //      `<root>/<App>.xcodeproj/project.xcworkspace/xcshareddata/swiftpm/Package.resolved`.
    //   3. Xcode-managed (workspace file): nested under
    //      `<root>/<App>.xcworkspace/xcshareddata/swiftpm/Package.resolved`.
    // `find_first_above` only walks upward joining the relative path
    // to ancestors, so it never finds the nested-in-xcodeproj shape.
    // Probe (2)+(3) via `find_xcode_spm_resolved` before the upward
    // search.
    let package_resolved = find_first_above(project_root, "Package.resolved")
        .or_else(|| find_xcode_spm_resolved(project_root))
        .or_else(|| find_first_above(project_root, "xcshareddata/swiftpm/Package.resolved"));
    let cartfile = find_first_above(project_root, "Cartfile.resolved");

    let pods = podfile.as_deref().map(parse_podfile_lock).unwrap_or_default();
    let spm = package_resolved
        .as_deref()
        .map(parse_package_resolved)
        .unwrap_or_default();
    let cart = cartfile
        .as_deref()
        .map(parse_cartfile_resolved)
        .unwrap_or_default();
    let vendored = product_binary
        .map(parse_vendored_frameworks)
        .unwrap_or_default();

    let (entries, truncated) = merge_dep_entries(vec![pods, spm, cart, vendored], max_entries);
    CollectResult {
        entries,
        scope_label: "all".to_string(),
        truncated,
    }
}

pub fn dispatch(cmd: IosDepsCommand) -> anyhow::Result<()> {
    match cmd {
        IosDepsCommand::Collect(args) => {
            let result = collect(
                &args.project_root,
                args.product_binary.as_deref(),
                args.max_entries,
            );
            let json = serde_json::to_string(&result)?;
            println!("{}", json);
            Ok(())
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn write_fixture(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        p
    }

    // ── make_dep_id ────────────────────────────────────────────────

    #[test]
    fn make_dep_id_pinned_format() {
        // `<type>:<group>:<name>` — single colons. Empty group
        // yields the `<type>::<name>` form by coincidence.
        assert_eq!(make_dep_id("library", "", "Foo"), "library::Foo");
        assert_eq!(make_dep_id("file", "", "Bar.framework"), "file::Bar.framework");
    }

    // ── Podfile.lock ───────────────────────────────────────────────

    const PODFILE_BRAINTREE: &str = "PODS:\n\
        \x20\x20- Braintree (5.26.0):\n\
        \x20\x20\x20\x20- Braintree/Card (= 5.26.0)\n\
        \x20\x20\x20\x20- Braintree/Core (= 5.26.0)\n\
        \x20\x20- Braintree/Card (5.26.0):\n\
        \x20\x20\x20\x20- Braintree/Core\n\
        \x20\x20- Braintree/Core (5.26.0)\n\
        \x20\x20- SocketRocket (0.7.1)\n\
        \n\
        DEPENDENCIES:\n\
        \x20\x20- Braintree\n\
        \x20\x20- SocketRocket (~> 0.7.0)\n\
        \n\
        SPEC CHECKSUMS:\n\
        \x20\x20Braintree: deadbeef0123456789abcdef\n\
        \x20\x20SocketRocket: feedface0123456789abcdef\n";

    #[test]
    fn podfile_lock_top_level_and_versions() {
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Podfile.lock", PODFILE_BRAINTREE);
        let entries = parse_podfile_lock(&path);
        let by_name: HashMap<&str, &DepEntry> =
            entries.iter().map(|e| (e.name.as_str(), e)).collect();
        assert_eq!(by_name["Braintree"].version.as_deref(), Some("5.26.0"));
        assert_eq!(by_name["SocketRocket"].version.as_deref(), Some("0.7.1"));
        assert!(by_name.contains_key("Braintree/Card"));
    }

    #[test]
    fn podfile_lock_direct_flag() {
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Podfile.lock", PODFILE_BRAINTREE);
        let entries = parse_podfile_lock(&path);
        let by_name: HashMap<&str, &DepEntry> =
            entries.iter().map(|e| (e.name.as_str(), e)).collect();
        // Braintree + SocketRocket are listed in DEPENDENCIES.
        assert!(by_name["Braintree"].direct);
        assert!(by_name["SocketRocket"].direct);
        // Subspecs pulled in transitively via the umbrella are
        // NOT direct.
        assert!(!by_name["Braintree/Card"].direct);
        assert!(!by_name["Braintree/Core"].direct);
    }

    #[test]
    fn podfile_lock_parent_edges() {
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Podfile.lock", PODFILE_BRAINTREE);
        let entries = parse_podfile_lock(&path);
        let by_name: HashMap<&str, &DepEntry> =
            entries.iter().map(|e| (e.name.as_str(), e)).collect();
        // Braintree/Card listed as child of Braintree.
        let card_parents = &by_name["Braintree/Card"].parents;
        assert!(card_parents.contains(&make_dep_id("library", "", "Braintree")));
        // Braintree/Core listed as child of BOTH Braintree AND
        // Braintree/Card.
        let core_parents = &by_name["Braintree/Core"].parents;
        assert!(core_parents.contains(&make_dep_id("library", "", "Braintree")));
        assert!(core_parents.contains(&make_dep_id("library", "", "Braintree/Card")));
        // SocketRocket has no parents (not a child of anyone).
        assert!(by_name["SocketRocket"].parents.is_empty());
    }

    // ── Package.resolved ───────────────────────────────────────────

    #[test]
    fn package_resolved_xcode_legacy_shape() {
        let body = r#"{
            "object": {
                "pins": [
                    {
                        "package": "Alamofire",
                        "repositoryURL": "https://github.com/Alamofire/Alamofire.git",
                        "state": {"version": "5.8.1"}
                    },
                    {
                        "package": "swift-collections",
                        "state": {"revision": "abc123def456"}
                    }
                ]
            },
            "version": 1
        }"#;
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Package.resolved", body);
        let entries = parse_package_resolved(&path);
        let by_name: HashMap<&str, &DepEntry> =
            entries.iter().map(|e| (e.name.as_str(), e)).collect();
        assert_eq!(by_name["Alamofire"].version.as_deref(), Some("5.8.1"));
        assert_eq!(
            by_name["swift-collections"].version.as_deref(),
            Some("abc123def456")
        );
        // All SPM pins are direct=true.
        assert!(entries.iter().all(|e| e.direct));
        // v1 carries the url under `repositoryURL` — populate `url`
        // from it. A regression that read only the v2 `location`
        // field would miss vuln-scan keys for every Xcode-managed
        // SPM project.
        assert_eq!(
            by_name["Alamofire"].url.as_deref(),
            Some("https://github.com/Alamofire/Alamofire.git"),
        );
        // No URL on the revision-locked pin → None.
        assert_eq!(by_name["swift-collections"].url, None);
    }

    #[test]
    fn package_resolved_v2_uses_location_for_url() {
        // SPM CLI v2 carries the url under `location`. The url is
        // load-bearing for OSV vuln scanning, so a regression that
        // dropped the field would silently lose vuln coverage on
        // every SPM project using the new format.
        let body = r#"{
            "pins": [
                {
                    "identity": "alamofire",
                    "kind": "remoteSourceControl",
                    "location": "https://github.com/Alamofire/Alamofire.git",
                    "state": {"version": "5.8.1"}
                }
            ],
            "version": 2
        }"#;
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Package.resolved", body);
        let entries = parse_package_resolved(&path);
        assert_eq!(
            entries[0].url.as_deref(),
            Some("https://github.com/Alamofire/Alamofire.git"),
        );
    }

    #[test]
    fn package_resolved_v2_prefers_identity_over_package() {
        let body = r#"{
            "pins": [
                {
                    "identity": "alamofire",
                    "package": "Alamofire",
                    "kind": "remoteSourceControl",
                    "state": {"version": "5.8.1"}
                }
            ],
            "version": 2
        }"#;
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Package.resolved", body);
        let entries = parse_package_resolved(&path);
        // identity (lowercased) wins over package (capitalised).
        assert_eq!(entries[0].name, "alamofire");
    }

    #[test]
    fn package_resolved_state_prefers_version_over_revision() {
        let body = r#"{
            "pins": [
                {
                    "identity": "alamofire",
                    "state": {
                        "version":  "5.8.1",
                        "revision": "f455c2975872ccd2d9c81594c658af65716e9b9a"
                    }
                }
            ],
            "version": 2
        }"#;
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Package.resolved", body);
        let entries = parse_package_resolved(&path);
        // version wins — same precedence the Python implementation
        // pins. A regression that picked revision would replace the
        // tagged "5.8.1" with a 40-char SHA on every tagged pin.
        assert_eq!(entries[0].version.as_deref(), Some("5.8.1"));
    }

    // ── Cartfile.resolved ──────────────────────────────────────────

    #[test]
    fn cartfile_resolved_basic() {
        let body = "\
            github \"ReactiveCocoa/ReactiveCocoa\" \"v2.3.1\"\n\
            git \"https://example.com/private.git\" \"1.0.0\"\n\
            binary \"https://example.com/MyBin.json\" \"1.0.0\"\n\
            # comment line — must be skipped\n";
        let tmp = TempDir::new().unwrap();
        let path = write_fixture(tmp.path(), "Cartfile.resolved", body);
        let entries = parse_cartfile_resolved(&path);
        assert_eq!(entries.len(), 3);
        let names: HashMap<&str, &str> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.version.as_deref().unwrap_or("")))
            .collect();
        assert_eq!(names["ReactiveCocoa/ReactiveCocoa"], "v2.3.1");
        assert_eq!(names["https://example.com/private.git"], "1.0.0");
        // All Carthage entries direct.
        assert!(entries.iter().all(|e| e.direct));
    }

    // ── Merger ─────────────────────────────────────────────────────

    fn dummy_entry(name: &str) -> DepEntry {
        DepEntry {
            id: make_dep_id("library", "", name),
            group: String::new(),
            name: name.to_string(),
            version: Some("1.0".to_string()),
            direct: true,
            scope: None,
            type_: "library".to_string(),
            parents: Vec::new(),
            url: None,
        }
    }

    fn dummy_entry_with_url(name: &str, url: &str) -> DepEntry {
        let mut e = dummy_entry(name);
        e.url = Some(url.to_string());
        e
    }

    fn dummy_transitive(name: &str) -> DepEntry {
        let mut e = dummy_entry(name);
        e.direct = false;
        e
    }

    #[test]
    fn merger_dedup_first_source_wins_when_no_url_advantage() {
        // When neither colliding entry has a url, dedup falls through
        // to first-seen — matches the legacy first-source-wins
        // contract the fastlane Python tests have pinned.
        let mut first = dummy_entry("A");
        first.version = Some("from-cocoapods".to_string());
        let mut second = dummy_entry("A");
        second.version = Some("from-spm".to_string());
        let (out, truncated) = merge_dep_entries(
            vec![vec![first, dummy_entry("B")], vec![second, dummy_entry("C")]],
            DEPENDENCIES_MAX_COUNT,
        );
        assert_eq!(out.len(), 3);
        let kept_a = out.iter().find(|e| e.name == "A").unwrap();
        // First source's A survives.
        assert_eq!(kept_a.version.as_deref(), Some("from-cocoapods"));
        assert!(!truncated);
    }

    #[test]
    fn merger_dedup_url_preference_wins_over_first_source() {
        // Cross-manager case the SDK relies on: same package surfaces
        // via CocoaPods (no url) AND SPM (with url). The SPM entry
        // MUST win because OSV vulnerability lookups key off the
        // url; dropping it would silently exclude this package from
        // every vuln scan.
        let cocoapods_a = dummy_entry("A"); // no url
        let spm_a = dummy_entry_with_url(
            "A", "https://github.com/example/A.git",
        );
        let (out, _) = merge_dep_entries(
            vec![vec![cocoapods_a], vec![spm_a]],
            DEPENDENCIES_MAX_COUNT,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].url.as_deref(),
            Some("https://github.com/example/A.git"),
        );
    }

    #[test]
    fn merger_dedup_both_with_url_first_wins() {
        // Edge case: both colliding entries have a url. First-seen
        // wins (no preference reason to replace).
        let first = dummy_entry_with_url("A", "https://first.example/A");
        let second = dummy_entry_with_url("A", "https://second.example/A");
        let (out, _) = merge_dep_entries(
            vec![vec![first], vec![second]],
            DEPENDENCIES_MAX_COUNT,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].url.as_deref(), Some("https://first.example/A"));
    }

    #[test]
    fn merger_url_preference_preserves_parents_and_demoted_direct_flag() {
        // The bite the field-wise merge fix closes. A CocoaPods
        // subspec (`Braintree/Core`) is a transitive dep — the
        // parser emits `direct: false` + `parents: ["library::Braintree"]`,
        // and no url. The same package surfaces via SPM as a
        // resolved pin — the parser emits `direct: true` (SPM
        // doesn't model transitivity) + `parents: []` + a url.
        //
        // Pre-fix wholesale-replacement would wipe the CocoaPods
        // graph evidence: kept entry would have `parents: []`. The
        // dashboard would render `Braintree/Core` as orphaned /
        // direct even though the lockfile clearly shows it's a
        // child of `Braintree`.
        //
        // Post-fix the previous entry's `parents` survive; the url
        // is grafted on; `direct` is OR-merged so an SPM-direct +
        // CocoaPods-transitive package ends up direct=true (it IS
        // directly reachable via SPM).
        let braintree_umbrella = dummy_entry("Braintree");
        let mut cocoapods_subspec = dummy_transitive("Braintree/Core");
        cocoapods_subspec.parents = vec![make_dep_id("library", "", "Braintree")];
        let spm_entry = dummy_entry_with_url(
            "Braintree/Core",
            "https://github.com/braintree/braintree_ios.git",
        );
        let (out, _) = merge_dep_entries(
            vec![vec![braintree_umbrella, cocoapods_subspec], vec![spm_entry]],
            DEPENDENCIES_MAX_COUNT,
        );
        let core = out.iter().find(|e| e.name == "Braintree/Core").expect("Core kept");
        assert_eq!(
            core.url.as_deref(),
            Some("https://github.com/braintree/braintree_ios.git"),
        );
        // Parents from CocoaPods survive. The parent edge is the
        // load-bearing graph signal — without it, reachability
        // analysis on the worker side breaks.
        assert_eq!(
            core.parents,
            vec![make_dep_id("library", "", "Braintree")],
            "CocoaPods parent edge must survive url-preference replacement",
        );
        // `direct` is OR-merged. CocoaPods said false (transitive),
        // SPM said true (resolved pin) → result is true.
        assert!(core.direct, "direct must OR-merge to true");
    }

    #[test]
    fn merger_url_preference_does_not_demote_direct_when_both_transitive() {
        // Reverse asymmetry: SPM-style first (direct=true), then a
        // CocoaPods transitive (direct=false) bringing parents. We
        // should pick up the parents but NOT demote direct from
        // true → false. (`direct: true` is monotonic — once any
        // source reaches the package directly, it stays direct.)
        let umbrella = dummy_entry("Umbrella");
        let spm_first = dummy_entry_with_url("Pkg", "https://example.com/pkg.git");
        let mut cocoapods_subspec = dummy_transitive("Pkg");
        cocoapods_subspec.parents = vec![make_dep_id("library", "", "Umbrella")];
        let (out, _) = merge_dep_entries(
            vec![vec![spm_first], vec![umbrella, cocoapods_subspec]],
            DEPENDENCIES_MAX_COUNT,
        );
        let pkg = out.iter().find(|e| e.name == "Pkg").expect("Pkg kept");
        assert!(pkg.direct, "first-source direct=true must not be demoted");
        // Empty previous parents → backfilled from incoming.
        assert_eq!(
            pkg.parents,
            vec![make_dep_id("library", "", "Umbrella")],
            "empty parents must be backfilled from incoming source",
        );
    }

    #[test]
    fn merger_url_preference_fills_in_missing_version() {
        // SPM emits `version: Some(...)` from `state.version`;
        // vendored-frameworks emit `version: None`. If a package
        // surfaces first as a vendored framework (no version, no
        // url) and later via SPM (with both), the merger should
        // accept SPM's version onto the existing record.
        let mut vendored = dummy_entry("Foo");
        vendored.version = None;
        let mut spm = dummy_entry_with_url("Foo", "https://example.com/foo.git");
        spm.version = Some("2.0".to_string());
        let (out, _) = merge_dep_entries(
            vec![vec![vendored], vec![spm]],
            DEPENDENCIES_MAX_COUNT,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].version.as_deref(), Some("2.0"));
        assert_eq!(
            out[0].url.as_deref(),
            Some("https://example.com/foo.git"),
        );
    }

    #[test]
    fn merger_dedup_url_bearing_entry_does_not_get_overwritten_by_url_less() {
        // Opposite order from the cross-manager case: SPM first,
        // CocoaPods second. The url-bearing entry MUST remain — a
        // regression that flipped the comparison would lose the
        // url every time the manifest order put SPM first.
        let spm_a = dummy_entry_with_url(
            "A", "https://github.com/example/A.git",
        );
        let cocoapods_a = dummy_entry("A"); // no url
        let (out, _) = merge_dep_entries(
            vec![vec![spm_a], vec![cocoapods_a]],
            DEPENDENCIES_MAX_COUNT,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0].url.as_deref(),
            Some("https://github.com/example/A.git"),
        );
    }

    #[test]
    fn merger_output_is_sorted_by_id_ascending() {
        // The output ordering pins determinism — consecutive runs
        // of the same build MUST produce identical diffs, so the
        // dashboard's deps diff view doesn't show false noise. Pin
        // ascending-id sort.
        let entries = vec![vec![
            dummy_entry("Zebra"),
            dummy_entry("Apple"),
            dummy_entry("Mango"),
        ]];
        let (out, _) = merge_dep_entries(entries, DEPENDENCIES_MAX_COUNT);
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "Mango", "Zebra"]);
    }

    #[test]
    fn merger_truncation_prefers_direct_entries() {
        // Cap exceeded. The truncation strategy floats `direct: true`
        // entries to the kept set first — users care more about
        // their direct deps than transitive ones, and dropping a
        // direct entry from the vuln scan is more impactful.
        let mut entries = Vec::new();
        // 3 direct entries.
        for i in 0..3 {
            entries.push(dummy_entry(&format!("D{}", i)));
        }
        // 7 transitive entries.
        for i in 0..7 {
            entries.push(dummy_transitive(&format!("T{}", i)));
        }
        let (out, truncated) = merge_dep_entries(vec![entries], 5);
        assert!(truncated);
        assert_eq!(out.len(), 5);
        // All 3 direct entries must be in the kept set; the
        // remaining 2 slots are transitive.
        let kept_names: HashSet<&str> = out.iter().map(|e| e.name.as_str()).collect();
        for i in 0..3 {
            let n = format!("D{}", i);
            assert!(
                kept_names.contains(n.as_str()),
                "direct entry D{} must survive truncation, but didn't",
                i,
            );
        }
        // Pin direct-count to catch a regression that lost the
        // direct-prefer behaviour.
        let direct_count = out.iter().filter(|e| e.direct).count();
        assert_eq!(direct_count, 3);
    }

    #[test]
    fn merger_truncated_output_still_id_sorted() {
        // After truncation, the final output must still be id-
        // sorted. A regression that skipped the post-truncation
        // re-sort would surface as a non-deterministic order in
        // the kept set — caught here.
        let mut entries = Vec::new();
        entries.push(dummy_entry("Zebra"));
        entries.push(dummy_entry("Apple"));
        entries.push(dummy_entry("Mango"));
        entries.push(dummy_entry("Banana"));
        let (out, truncated) = merge_dep_entries(vec![entries], 2);
        assert!(truncated);
        assert_eq!(out.len(), 2);
        // First two by id-ascending = Apple, Banana.
        let names: Vec<&str> = out.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["Apple", "Banana"]);
    }

    #[test]
    fn merger_truncation_strips_dangling_parent_refs_in_kept_set() {
        // Self-consistency: when a child entry survives truncation
        // but its parent was evicted, the dangling parent id MUST
        // be stripped from the child's parents list.
        let mut child = dummy_entry("Child");
        child.parents = vec![make_dep_id("library", "", "Evicted")];
        // No "Evicted" entry in the source — simulates the
        // post-truncation dangling-ref case directly.
        let (out, _) = merge_dep_entries(vec![vec![child]], DEPENDENCIES_MAX_COUNT);
        assert_eq!(out[0].parents, Vec::<String>::new());
    }

    #[test]
    fn merger_truncates_at_cap() {
        let mut big = Vec::new();
        for i in 0..(DEPENDENCIES_MAX_COUNT + 50) {
            big.push(dummy_entry(&format!("p{}", i)));
        }
        let (out, truncated) = merge_dep_entries(vec![big], DEPENDENCIES_MAX_COUNT);
        assert_eq!(out.len(), DEPENDENCIES_MAX_COUNT);
        assert!(truncated);
    }

    #[test]
    fn merger_filters_dangling_parent_refs() {
        // Child entry has a parent ref pointing at an evicted
        // (never present) entry. After merge, the dangling ref
        // must be filtered.
        let evicted_id = make_dep_id("library", "", "PARENT");
        let mut child = dummy_entry("CHILD");
        child.parents = vec![evicted_id];
        let (out, _) = merge_dep_entries(vec![vec![child]], DEPENDENCIES_MAX_COUNT);
        assert_eq!(out[0].parents, Vec::<String>::new());
    }

    // ── strip_pod_version_paren ────────────────────────────────────

    #[test]
    fn strip_pod_version_paren_handles_constraints() {
        assert_eq!(
            strip_pod_version_paren("Braintree/Card (= 5.26.0)"),
            "Braintree/Card"
        );
        assert_eq!(
            strip_pod_version_paren("Braintree/Card (~> 5.26)"),
            "Braintree/Card"
        );
        assert_eq!(strip_pod_version_paren("  SocketRocket  "), "SocketRocket");
    }

    // ── JSON wire shape ────────────────────────────────────────────

    // ── Xcode-managed SPM discovery ─────────────────────────────────

    fn write_spm_resolved_minimal(p: &Path) {
        let body = "{\n  \"originHash\" : \"\",\n  \"pins\" : [{\n    \"identity\" : \"alamofire\",\n    \"kind\" : \"remoteSourceControl\",\n    \"location\" : \"https://github.com/Alamofire/Alamofire.git\",\n    \"state\" : { \"revision\" : \"abc\", \"version\" : \"5.10.0\" }\n  }],\n  \"version\" : 3\n}\n";
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn discovery_finds_xcode_managed_spm_resolved_in_xcodeproj() {
        // The bug the discovery fix closes. Xcode-managed SPM
        // nests `Package.resolved` 5 levels deep under
        // `<App>.xcodeproj/project.xcworkspace/...`. `find_first_above`
        // walks UP, never sideways into siblings, so it would
        // silently miss this shape — the migration regressed it.
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path()
            .join("MyApp.xcodeproj")
            .join("project.xcworkspace")
            .join("xcshareddata")
            .join("swiftpm")
            .join("Package.resolved");
        write_spm_resolved_minimal(&nested);

        let result = collect(tmp.path(), None, DEPENDENCIES_MAX_COUNT);
        assert!(
            result.entries.iter().any(|e| e.name == "alamofire"),
            "Xcode-managed SPM Package.resolved must be discoverable; got {:?}",
            result.entries,
        );
    }

    #[test]
    fn discovery_finds_xcode_managed_spm_resolved_in_xcworkspace() {
        // Workspace-rooted projects (App.xcworkspace, no xcodeproj
        // wrapper) nest one level shallower. Same discovery
        // mechanism, different sibling extension.
        let tmp = TempDir::new().unwrap();
        let nested = tmp.path()
            .join("MyApp.xcworkspace")
            .join("xcshareddata")
            .join("swiftpm")
            .join("Package.resolved");
        write_spm_resolved_minimal(&nested);

        let result = collect(tmp.path(), None, DEPENDENCIES_MAX_COUNT);
        assert!(
            result.entries.iter().any(|e| e.name == "alamofire"),
            "Workspace-managed SPM Package.resolved must be discoverable",
        );
    }

    #[test]
    fn discovery_prefers_xcworkspace_over_xcodeproj_when_both_exist() {
        // Xcode prefers `.xcworkspace` over `.xcodeproj` when both
        // are present in the same dir. Mirror that here so the dep
        // graph the CLI reports matches what Xcode is actually
        // building from.
        let tmp = TempDir::new().unwrap();
        let in_proj = tmp.path()
            .join("MyApp.xcodeproj")
            .join("project.xcworkspace")
            .join("xcshareddata")
            .join("swiftpm")
            .join("Package.resolved");
        let in_ws = tmp.path()
            .join("MyApp.xcworkspace")
            .join("xcshareddata")
            .join("swiftpm")
            .join("Package.resolved");
        // Write distinct fixtures so we can tell which one won.
        std::fs::create_dir_all(in_proj.parent().unwrap()).unwrap();
        std::fs::create_dir_all(in_ws.parent().unwrap()).unwrap();
        std::fs::write(&in_proj,
            "{\"pins\":[{\"identity\":\"from-proj\",\"kind\":\"remoteSourceControl\",\
             \"location\":\"https://example/proj.git\",\
             \"state\":{\"version\":\"1.0\",\"revision\":\"a\"}}],\"version\":3}",
        ).unwrap();
        std::fs::write(&in_ws,
            "{\"pins\":[{\"identity\":\"from-ws\",\"kind\":\"remoteSourceControl\",\
             \"location\":\"https://example/ws.git\",\
             \"state\":{\"version\":\"2.0\",\"revision\":\"b\"}}],\"version\":3}",
        ).unwrap();

        let result = collect(tmp.path(), None, DEPENDENCIES_MAX_COUNT);
        assert!(
            result.entries.iter().any(|e| e.name == "from-ws"),
            "xcworkspace should win when both exist; got {:?}",
            result.entries,
        );
        assert!(
            !result.entries.iter().any(|e| e.name == "from-proj"),
            "xcodeproj nested Package.resolved should be skipped when xcworkspace exists",
        );
    }

    #[test]
    fn collect_result_serialises_to_expected_shape() {
        let r = CollectResult {
            entries: vec![DepEntry {
                id: "library::Foo".to_string(),
                group: String::new(),
                name: "Foo".to_string(),
                version: Some("1.0".to_string()),
                direct: true,
                scope: None,
                type_: "library".to_string(),
                parents: Vec::new(),
                url: None,
            }],
            scope_label: "all".to_string(),
            truncated: false,
        };
        let json = serde_json::to_string(&r).unwrap();
        // Field names are the load-bearing contract.
        assert!(json.contains("\"entries\""));
        assert!(json.contains("\"scope_label\":\"all\""));
        assert!(json.contains("\"truncated\":false"));
        assert!(json.contains("\"type\":\"library\""));
        assert!(json.contains("\"id\":\"library::Foo\""));
        // DepEntry's optional `scope` field (None) is omitted from the
        // JSON via skip_serializing_if. (CollectResult's `scope_label`
        // is a different field name and DOES appear.)
        assert!(!json.contains("\"scope\":"));
    }
}
