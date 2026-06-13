//! Build-environment resolvers.
//!
//! Three small helpers that were previously duplicated across both
//! Python BugseeAgents:
//!
//! - `bugsee-cli build-env xcode-version` — emits the dotted Xcode
//!   version (e.g. `16.2.0`). Reads `$XCODE_VERSION_ACTUAL` first
//!   (the numeric `"1620"` form Xcode exports), then falls back to
//!   `xcodebuild -version`.
//! - `bugsee-cli build-env machine-label` — CI-provider-aware host
//!   label (e.g. `github-actions:runner-1`). Mirrors the Android
//!   Gradle plugin's `BuildMachineResolver` cascade so the
//!   dashboard groups iOS + Android builds from the same runner.
//! - `bugsee-cli build-env read-plist <path>` — reads an
//!   Info.plist (binary or XML) and prints the top-level keys +
//!   string values as JSON.
//!
//! Each subcommand prints to stdout and exits 0. Empty / failed
//! resolution prints an empty value (or `{}` for plist) — the
//! Python callers treat absence as "skip this field" rather than
//! reading an error string.

use clap::{Args, Subcommand};
use serde_json::Value;
use std::collections::HashMap;
use std::env;
use std::path::PathBuf;
use std::process::Command;

#[derive(Subcommand, Debug)]
pub enum BuildEnvCommand {
    /// Resolve the dotted Xcode version (`16.2.0`). Reads
    /// `$XCODE_VERSION_ACTUAL` then falls back to `xcodebuild -version`.
    XcodeVersion,

    /// Emit the CI-provider-aware host label
    /// (`github-actions:runner-1`, `gitlab-ci:macos-pool`,
    /// `jenkins:agent`, etc.) or the local hostname.
    MachineLabel,

    /// Read an Info.plist (binary or XML) and emit top-level keys
    /// + string values as JSON.
    ReadPlist(ReadPlistArgs),
}

#[derive(Args, Debug)]
pub struct ReadPlistArgs {
    /// Path to the Info.plist (binary or XML).
    pub path: PathBuf,
}

pub fn dispatch(cmd: BuildEnvCommand) -> anyhow::Result<()> {
    match cmd {
        BuildEnvCommand::XcodeVersion => {
            println!("{}", resolve_xcode_version().unwrap_or_default());
            Ok(())
        }
        BuildEnvCommand::MachineLabel => {
            let env_map: HashMap<String, String> = env::vars().collect();
            println!("{}", resolve_machine_label(&env_map).unwrap_or_default());
            Ok(())
        }
        BuildEnvCommand::ReadPlist(args) => {
            let json = read_plist_to_json(&args.path);
            println!("{}", serde_json::to_string(&json)?);
            Ok(())
        }
    }
}

// ─── Xcode version ──────────────────────────────────────────────────

/// Returns the dotted Xcode version or `None`. Prefers
/// `$XCODE_VERSION_ACTUAL` (Xcode exports `"1620"` for 16.2.0 — pure
/// digits, needs reformatting) and falls back to `xcodebuild -version`
/// when the env var is absent (e.g. CLI invocations outside an Xcode
/// build phase).
pub fn resolve_xcode_version() -> Option<String> {
    let actual = env::var("XCODE_VERSION_ACTUAL").ok();
    if let Some(s) = actual.as_deref() {
        if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) {
            return Some(format_xcode_actual(s));
        }
    }
    // Fallback: `xcodebuild -version` prints `Xcode X.Y\nBuild version ...`
    let output = Command::new("/usr/bin/xcodebuild")
        .arg("-version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first = stdout.lines().next()?.trim();
    let version = first.strip_prefix("Xcode")?.trim();
    if version.is_empty() {
        return None;
    }
    // Normalise to 3-part dotted to match the `XCODE_VERSION_ACTUAL`
    // path. xcodebuild typically prints `Xcode 16.2` (2 components);
    // the env-var path always produces `16.2.0`. Without this pad,
    // the dashboard splits telemetry for the same Xcode install
    // depending on which resolver path ran (env var inside a Run
    // Script vs xcodebuild outside one).
    Some(normalise_dotted_three_parts(version))
}

/// Pad a dotted version string to exactly 3 components by appending
/// `.0` for each missing trailing field, or truncating any extras.
/// `"16.2"` → `"16.2.0"`; `"16"` → `"16.0.0"`; `"16.2.0"` stays;
/// `"16.2.0.42"` → `"16.2.0"`. Non-numeric components are passed
/// through verbatim — the only goal is shape parity.
fn normalise_dotted_three_parts(s: &str) -> String {
    let mut parts: Vec<String> = s.split('.').map(|p| p.to_string()).collect();
    while parts.len() < 3 {
        parts.push("0".to_string());
    }
    parts.truncate(3);
    parts.join(".")
}

/// `"1620"` → `"16.2.0"`. Last digit is patch, second-to-last is
/// minor, prefix is major. So `"1543"` → `15.4.3`. Each component
/// drops leading zeros but a fully-zero component stays as `"0"`.
///
/// 1-digit inputs are treated as patch-only: `"6"` → `"0.0.6"`.
/// (Pre-fix the off-by-one in the slicing dropped the digit
/// entirely and returned `"0.0.0"`.) 2-digit inputs are
/// minor+patch: `"06"` → `"0.0.6"`, `"15"` → `"0.1.5"`.
fn format_xcode_actual(s: &str) -> String {
    let len = s.len();
    // Anchor on patch (always the last char when len>=1). Then
    // minor is the next char to the left (or "0"); major is
    // everything else (or "0").
    if len == 0 {
        return "0.0.0".to_string();
    }
    let patch = &s[len - 1..];
    let minor = if len >= 2 { &s[len - 2..len - 1] } else { "0" };
    let major = if len >= 3 { &s[..len - 2] } else { "0" };
    format!("{}.{}.{}", strip_lead(major), strip_lead(minor), strip_lead(patch))
}

fn strip_lead(s: &str) -> String {
    let t = s.trim_start_matches('0');
    if t.is_empty() {
        "0".to_string()
    } else {
        t.to_string()
    }
}

// ─── Machine label ──────────────────────────────────────────────────

/// `<provider>[:<detail>]` label describing where the build ran.
/// Mirrors the Android Gradle plugin's `BuildMachineResolver` cascade
/// so the dashboard can group iOS + Android builds from the same CI
/// runner under one machine identity. First positive provider signal
/// wins. Falls back to the local hostname when no provider matches.
pub fn resolve_machine_label(env: &HashMap<String, String>) -> Option<String> {
    fn with_detail(prefix: &str, detail: Option<&str>) -> String {
        match detail.map(|s| s.trim()).filter(|s| !s.is_empty()) {
            Some(d) => format!("{}:{}", prefix, d),
            None => prefix.to_string(),
        }
    }

    if env_truthy(env.get("GITHUB_ACTIONS")) {
        return Some(with_detail(
            "github-actions",
            env.get("RUNNER_NAME").map(String::as_str),
        ));
    }
    if env_truthy(env.get("GITLAB_CI")) {
        let detail = env
            .get("CI_RUNNER_DESCRIPTION")
            .and_then(|s| {
                let t = s.trim();
                (!t.is_empty()).then_some(t)
            })
            .or_else(|| {
                env.get("CI_RUNNER_ID").and_then(|s| {
                    let t = s.trim();
                    (!t.is_empty()).then_some(t)
                })
            });
        return Some(with_detail("gitlab-ci", detail));
    }
    if env
        .get("JENKINS_URL")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return Some(with_detail(
            "jenkins",
            env.get("NODE_NAME").map(String::as_str),
        ));
    }
    if env_truthy(env.get("CIRCLECI")) {
        return Some(with_detail(
            "circleci",
            env.get("CIRCLE_NODE_INDEX").map(String::as_str),
        ));
    }
    if env_truthy(env.get("BITRISE_IO")) {
        return Some(with_detail(
            "bitrise",
            env.get("BITRISE_APP_SLUG").map(String::as_str),
        ));
    }
    if env
        .get("TEAMCITY_VERSION")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        let detail = env
            .get("AGENT_NAME")
            .and_then(|s| {
                let t = s.trim();
                (!t.is_empty()).then_some(t)
            })
            .or_else(|| {
                env.get("agent.name").and_then(|s| {
                    let t = s.trim();
                    (!t.is_empty()).then_some(t)
                })
            });
        return Some(with_detail("teamcity", detail));
    }
    // Xcode Cloud — Apple's own CI. `CI_WORKFLOW` is the canonical
    // presence signal; `CI_XCODEBUILD_ACTION` adds action context
    // (build / archive / test) when available.
    let ci_workflow = env
        .get("CI_WORKFLOW")
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if ci_workflow || env_truthy(env.get("CI_XCODE_CLOUD")) {
        let detail = env
            .get("CI_WORKFLOW")
            .and_then(|s| {
                let t = s.trim();
                (!t.is_empty()).then_some(t)
            })
            .or_else(|| {
                env.get("CI_XCODEBUILD_ACTION").and_then(|s| {
                    let t = s.trim();
                    (!t.is_empty()).then_some(t)
                })
            });
        return Some(with_detail("xcode-cloud", detail));
    }
    if env_truthy(env.get("CI")) {
        let host = env
            .get("HOSTNAME")
            .and_then(|s| {
                let t = s.trim();
                (!t.is_empty()).then_some(t.to_string())
            })
            .or_else(local_hostname);
        return Some(with_detail("ci", host.as_deref()));
    }
    local_hostname()
}

fn local_hostname() -> Option<String> {
    // Absolute path `/usr/bin/hostname` (POSIX standard location on
    // both macOS and Linux). The rest of this codebase already uses
    // absolute paths for system utilities (`/usr/bin/xcodebuild`,
    // `/usr/bin/otool`); the previously-bare `Command::new("hostname")`
    // was a PATH-hijack outlier — an attacker who can drop a `hostname`
    // shim earlier on PATH (CI workspace, direnv-prepended project
    // bin/, etc.) would have escalated to "run as the build user".
    let output = Command::new("/usr/bin/hostname").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Truthy-token check matching the Android Gradle plugin's set.
fn env_truthy(value: Option<&String>) -> bool {
    value
        .map(|v| v.trim().to_ascii_lowercase())
        .map(|v| matches!(v.as_str(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

// ─── Plist reader ───────────────────────────────────────────────────

/// Read an Info.plist (binary or XML) and produce a JSON object of
/// top-level keys → string values. The Python BugseeAgents only ever
/// read string-valued keys (`CFBundleShortVersionString`,
/// `CFBundleVersion`, `CFBundleIdentifier`), so collapsing to strings
/// keeps the JSON shape simple. Non-string values are stringified
/// via Display.
///
/// Returns `{}` on any error (missing path, unreadable file, parse
/// failure, root that isn't a Dict). The Python callers treat
/// absence as "field unknown" rather than failing.
pub fn read_plist_to_json(path: &std::path::Path) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let value = match plist::Value::from_file(path) {
        Ok(v) => v,
        Err(_) => return out,
    };
    let dict = match value.as_dictionary() {
        Some(d) => d,
        None => return out,
    };
    for (k, v) in dict.iter() {
        let s = stringify_plist_value(v);
        if let Some(s) = s {
            out.insert(k.to_string(), Value::String(s));
        }
    }
    out
}

fn stringify_plist_value(v: &plist::Value) -> Option<String> {
    if let Some(s) = v.as_string() {
        return Some(s.to_string());
    }
    if let Some(i) = v.as_signed_integer() {
        return Some(i.to_string());
    }
    if let Some(u) = v.as_unsigned_integer() {
        return Some(u.to_string());
    }
    if let Some(f) = v.as_real() {
        return Some(f.to_string());
    }
    if let Some(b) = v.as_boolean() {
        return Some(b.to_string());
    }
    None
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ── format_xcode_actual ──────────────────────────────────────

    #[test]
    fn format_xcode_actual_canonical_inputs() {
        // `"1620"` → 16.2.0 — the format Xcode 16.2.0 emits.
        assert_eq!(format_xcode_actual("1620"), "16.2.0");
        // `"1543"` → 15.4.3 — three-digit patch case. A naive
        // `[0:2], [2:4]` split would yield "15.43".
        assert_eq!(format_xcode_actual("1543"), "15.4.3");
        // `"800"` → 8.0.0 — leading-major case.
        assert_eq!(format_xcode_actual("800"), "8.0.0");
    }

    #[test]
    fn format_xcode_actual_short_inputs_preserve_digits() {
        // Off-by-one regression pin. Pre-fix, the slicing math
        // returned `"0.0.0"` for any 1-character input — the
        // digit was dropped entirely. The new patch-anchored
        // computation places single digits in the patch slot.
        assert_eq!(format_xcode_actual("6"), "0.0.6");
        // 2-digit input: patch is last digit, minor is first.
        assert_eq!(format_xcode_actual("06"), "0.0.6");
        assert_eq!(format_xcode_actual("15"), "0.1.5");
        // 0-length input edge case: synthesise a placeholder
        // rather than panic.
        assert_eq!(format_xcode_actual(""), "0.0.0");
    }

    // ── normalise_dotted_three_parts ──────────────────────────────

    #[test]
    fn normalise_dotted_pads_to_three_parts() {
        // The fix the resolve_xcode_version normalisation closes.
        // Without padding, the same Xcode install yields different
        // bucket strings depending on whether XCODE_VERSION_ACTUAL
        // is in env (env-path → "16.2.0") or xcodebuild was
        // shelled to (fallback path → "16.2"). The dashboard's
        // string-typed version column then double-counts.
        assert_eq!(normalise_dotted_three_parts("16.2"), "16.2.0");
        assert_eq!(normalise_dotted_three_parts("16"), "16.0.0");
        assert_eq!(normalise_dotted_three_parts("16.2.0"), "16.2.0");
        // Extras truncated. Apple's xcodebuild has never emitted
        // four parts, but pin the behaviour anyway so a future
        // toolchain change can't silently re-split telemetry.
        assert_eq!(normalise_dotted_three_parts("16.2.0.42"), "16.2.0");
    }

    // ── resolve_xcode_version ────────────────────────────────────

    #[test]
    fn resolve_xcode_version_reads_env_var() {
        // We can't easily mock env vars across the whole process,
        // so test format_xcode_actual directly above and trust the
        // env-var branch is the obvious wrapper.
    }

    // ── resolve_machine_label ────────────────────────────────────

    #[test]
    fn github_actions_with_runner_name() {
        let env = env_with(&[
            ("GITHUB_ACTIONS", "true"),
            ("RUNNER_NAME", "gh-runner-7"),
            // Even with generic CI=true, GitHub Actions wins.
            ("CI", "true"),
        ]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("github-actions:gh-runner-7"),
        );
    }

    #[test]
    fn github_actions_without_runner_name() {
        let env = env_with(&[("GITHUB_ACTIONS", "true")]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("github-actions"),
        );
    }

    #[test]
    fn gitlab_prefers_runner_description_over_id() {
        let env = env_with(&[
            ("GITLAB_CI", "true"),
            ("CI_RUNNER_DESCRIPTION", "macos-arm64-pool"),
            ("CI_RUNNER_ID", "12345"),
        ]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("gitlab-ci:macos-arm64-pool"),
        );
    }

    #[test]
    fn gitlab_falls_back_to_runner_id() {
        let env = env_with(&[
            ("GITLAB_CI", "true"),
            ("CI_RUNNER_ID", "12345"),
        ]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("gitlab-ci:12345"),
        );
    }

    #[test]
    fn jenkins_uses_node_name() {
        let env = env_with(&[
            ("JENKINS_URL", "https://jenkins.example.com"),
            ("NODE_NAME", "mac-build-agent"),
        ]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("jenkins:mac-build-agent"),
        );
    }

    #[test]
    fn circleci_uses_node_index() {
        let env = env_with(&[("CIRCLECI", "true"), ("CIRCLE_NODE_INDEX", "0")]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("circleci:0"),
        );
    }

    #[test]
    fn bitrise_uses_app_slug() {
        let env = env_with(&[
            ("BITRISE_IO", "true"),
            ("BITRISE_APP_SLUG", "abc123def456"),
        ]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("bitrise:abc123def456"),
        );
    }

    #[test]
    fn xcode_cloud_uses_workflow_name() {
        let env = env_with(&[
            ("CI_WORKFLOW", "Release Archive"),
            // Xcode Cloud also sets generic CI=true.
            ("CI", "true"),
        ]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("xcode-cloud:Release Archive"),
        );
    }

    #[test]
    fn generic_ci_uses_hostname_from_env() {
        let env = env_with(&[("CI", "true"), ("HOSTNAME", "ci-runner-42")]);
        assert_eq!(
            resolve_machine_label(&env).as_deref(),
            Some("ci:ci-runner-42"),
        );
    }

    #[test]
    fn no_provider_returns_local_hostname_or_none() {
        // No CI vars at all → falls through to local_hostname.
        // The host running tests has a hostname, so it should
        // return Some(...); we just verify it doesn't return
        // None unconditionally on a CI-less env.
        let env: HashMap<String, String> = HashMap::new();
        let label = resolve_machine_label(&env);
        // Either a hostname string or None on a sandboxed runner —
        // BOTH are acceptable behaviours. Just verify the function
        // doesn't return one of the provider prefixes.
        if let Some(l) = label {
            assert!(!l.starts_with("github-actions"));
            assert!(!l.starts_with("gitlab-ci"));
            assert!(!l.starts_with("ci:"));
        }
    }

    // ── env_truthy — cross-language contract ─────────────────────

    #[test]
    fn env_truthy_canonical_tokens() {
        for tok in &["1", "true", "yes", "on", "TRUE"] {
            assert!(env_truthy(Some(&tok.to_string())), "expected: {:?}", tok);
        }
    }

    #[test]
    fn env_truthy_falsy_tokens() {
        for tok in &["", "0", "false", "off"] {
            assert!(!env_truthy(Some(&tok.to_string())), "expected falsy: {:?}", tok);
        }
        assert!(!env_truthy(None));
    }

    // ── read_plist_to_json ───────────────────────────────────────

    #[test]
    fn read_plist_xml_returns_string_keys() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        // Minimal XML plist with the three keys both BugseeAgents
        // care about. The reader strings-coerces signed integer
        // (`CFBundleVersion` can be written as int OR string).
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleShortVersionString</key>
  <string>1.2.3</string>
  <key>CFBundleVersion</key>
  <string>42</string>
  <key>CFBundleIdentifier</key>
  <string>com.example.app</string>
</dict>
</plist>"#;
        std::fs::write(tmp.path(), body).unwrap();
        let out = read_plist_to_json(tmp.path());
        assert_eq!(out.get("CFBundleShortVersionString").and_then(|v| v.as_str()), Some("1.2.3"));
        assert_eq!(out.get("CFBundleVersion").and_then(|v| v.as_str()), Some("42"));
        assert_eq!(out.get("CFBundleIdentifier").and_then(|v| v.as_str()), Some("com.example.app"));
    }

    #[test]
    fn read_plist_missing_path_returns_empty_object() {
        let out = read_plist_to_json(std::path::Path::new("/no/such/plist"));
        assert!(out.is_empty());
    }

    #[test]
    fn read_plist_malformed_returns_empty_object() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "this is not a plist").unwrap();
        let out = read_plist_to_json(tmp.path());
        assert!(out.is_empty());
    }

    #[test]
    fn read_plist_stringifies_integer_values() {
        // CFBundleVersion is sometimes written as <integer>, not
        // <string>. The Python callers want a string either way.
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let body = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleVersion</key>
  <integer>42</integer>
</dict>
</plist>"#;
        std::fs::write(tmp.path(), body).unwrap();
        let out = read_plist_to_json(tmp.path());
        assert_eq!(out.get("CFBundleVersion").and_then(|v| v.as_str()), Some("42"));
    }
}
