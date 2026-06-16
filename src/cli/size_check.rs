//! In-build artefact size-check — the Rust port of the iOS BugseeAgent's
//! `prepare_size_check` / `_evaluate_size_check` / `run_size_check`.
//!
//! When `BUGSEE_SIZE_CHECK_ENABLED` is set and at least one threshold is
//! configured, the post-action fetches the previous build's `artifact_size`
//! from `/v2/apps/<token>/builds/baseline` and compares it to the freshly
//! packaged `.ipa`. Growth past a WARNING threshold prints a `warning:` line;
//! growth past a FAIL threshold prints an `error:` line and exits non-zero
//! (`ExitCode::SizeCheckFailed`) — the one place the post-action deliberately
//! fails the build.
//!
//! The baseline is fetched BEFORE the build is registered so the lookup can
//! never pick up the in-flight build as its own baseline (the server's baseline
//! filter requires `status='ready'`, but propagation is async — fetching
//! pre-register sidesteps the race entirely).
//!
//! Every infrastructure problem (master switch off, no thresholds, no
//! package_id, baseline lookup failure, first build) degrades to "skip" — the
//! size-check never fails a build on infra trouble, only on real growth.

use std::collections::HashMap;

use serde_json::Value;

use crate::upload::build_info::builds_url;
use crate::upload::http;

/// Configured growth thresholds. Each gate is independently optional; a `0` /
/// unset / malformed value disables that gate (mirrors the agent + the Android
/// plugin). All-`None` means "no active gate" → skip.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Thresholds {
    pub warning_pct: Option<f64>,
    pub fail_pct: Option<f64>,
    pub warning_bytes: Option<u64>,
    pub fail_bytes: Option<u64>,
}

impl Thresholds {
    fn is_empty(&self) -> bool {
        self.warning_pct.is_none()
            && self.fail_pct.is_none()
            && self.warning_bytes.is_none()
            && self.fail_bytes.is_none()
    }
}

/// The previous build's size baseline for this `(app, package_id, format,
/// configuration)`.
#[derive(Debug, Clone)]
pub struct Baseline {
    pub artifact_size: u64,
    pub version: Option<String>,
    pub build: Option<String>,
}

/// A prepared check: the active thresholds + the resolved baseline. Produced by
/// [`prepare`] before registration; consumed by [`Prepared::decide`] after.
#[derive(Debug, Clone)]
pub struct Prepared {
    thresholds: Thresholds,
    baseline: Baseline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Warn,
    Fail,
}

fn env_truthy(env: &HashMap<String, String>, key: &str) -> bool {
    match env.get(key).map(|s| s.trim().to_ascii_lowercase()) {
        Some(v) => matches!(v.as_str(), "1" | "true" | "yes" | "on"),
        None => false,
    }
}

/// Parse a positive, finite float env var; `None` when unset / empty / malformed
/// / `<= 0` / non-finite. Mirrors the agent's `parse_float` (which rejects NaN
/// and ±Inf so a threshold can't be silently un-triggerable).
fn parse_pos_float(env: &HashMap<String, String>, key: &str) -> Option<f64> {
    let raw = env.get(key)?;
    if raw.is_empty() {
        return None;
    }
    let v: f64 = raw.trim().parse().ok()?;
    if v.is_finite() && v > 0.0 {
        Some(v)
    } else {
        None
    }
}

/// Parse a positive integer env var; `None` when unset / empty / malformed /
/// `<= 0`. Mirrors the agent's `parse_int`.
fn parse_pos_int(env: &HashMap<String, String>, key: &str) -> Option<u64> {
    let raw = env.get(key)?;
    if raw.is_empty() {
        return None;
    }
    // The agent parses with Python `int()` (base 10, sign allowed); a negative
    // or zero value disables the gate. `i64` then filter > 0 matches that.
    let v: i64 = raw.trim().parse().ok()?;
    if v > 0 {
        Some(v as u64)
    } else {
        None
    }
}

/// Read the `BUGSEE_SIZE_CHECK_*` threshold env vars.
pub fn resolve_thresholds(env: &HashMap<String, String>) -> Thresholds {
    Thresholds {
        warning_pct: parse_pos_float(env, "BUGSEE_SIZE_CHECK_WARNING_PCT"),
        fail_pct: parse_pos_float(env, "BUGSEE_SIZE_CHECK_FAIL_PCT"),
        warning_bytes: parse_pos_int(env, "BUGSEE_SIZE_CHECK_WARNING_BYTES"),
        fail_bytes: parse_pos_int(env, "BUGSEE_SIZE_CHECK_FAIL_BYTES"),
    }
}

/// Compact byte renderer mirroring the front-end's (and the Android plugin's),
/// so users read one set of numbers across the stack. Negative values keep their
/// sign; the unit is chosen by magnitude.
pub fn format_bytes(n: i64) -> String {
    let abs_n = n.unsigned_abs();
    if abs_n < 1024 {
        format!("{n} B")
    } else if abs_n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.0)
    } else if abs_n < 1024 * 1024 * 1024 {
        format!("{:.1} MB", n as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", n as f64 / (1024.0 * 1024.0 * 1024.0))
    }
}

/// Percent renderer: whole numbers print without a decimal, else one decimal
/// place. Mirrors the agent's `_format_pct`.
pub fn format_pct(p: f64) -> String {
    if p == p.trunc() {
        format!("{}%", p as i64)
    } else {
        format!("{p:.1}%")
    }
}

/// Pure threshold evaluation. Returns `(verdict, triggered_by, delta_bytes,
/// delta_pct)`. A shrunk or flat artefact always PASSes (the feature is a growth
/// alarm). FAIL wins over WARN; percent is checked before bytes within a
/// severity so the log names the more familiar number. Port of
/// `_evaluate_size_check`.
fn evaluate(
    local_size: u64,
    baseline_size: u64,
    t: &Thresholds,
) -> (Verdict, Option<String>, i64, f64) {
    if baseline_size == 0 {
        return (Verdict::Pass, None, 0, 0.0);
    }
    let delta_bytes = local_size as i64 - baseline_size as i64;
    let delta_pct = (delta_bytes as f64 / baseline_size as f64) * 100.0;
    if delta_bytes <= 0 {
        return (Verdict::Pass, None, delta_bytes, delta_pct);
    }
    let delta_bytes_u = delta_bytes as u64;

    if let Some(fp) = t.fail_pct {
        if delta_pct >= fp {
            return (
                Verdict::Fail,
                Some(format!("fail threshold {}", format_pct(fp))),
                delta_bytes,
                delta_pct,
            );
        }
    }
    if let Some(fb) = t.fail_bytes {
        if delta_bytes_u >= fb {
            return (
                Verdict::Fail,
                Some(format!("fail threshold {}", format_bytes(fb as i64))),
                delta_bytes,
                delta_pct,
            );
        }
    }
    if let Some(wp) = t.warning_pct {
        if delta_pct >= wp {
            return (
                Verdict::Warn,
                Some(format!("warning threshold {}", format_pct(wp))),
                delta_bytes,
                delta_pct,
            );
        }
    }
    if let Some(wb) = t.warning_bytes {
        if delta_bytes_u >= wb {
            return (
                Verdict::Warn,
                Some(format!("warning threshold {}", format_bytes(wb as i64))),
                delta_bytes,
                delta_pct,
            );
        }
    }
    (Verdict::Pass, None, delta_bytes, delta_pct)
}

impl Prepared {
    /// Evaluate the freshly built artefact against the prepared baseline and
    /// return the verdict plus the fully-formatted log line. PASS → the bare
    /// summary; WARN → `warning: <summary> — exceeds <gate>`; FAIL → `<summary>
    /// — exceeds <gate>` (the caller wraps the FAIL line in
    /// `Error::SizeCheckFailed`, and `main` prepends `error: `). Port of
    /// `run_size_check`'s formatting.
    pub fn decide(&self, local_size: u64) -> (Verdict, String) {
        let (verdict, triggered_by, delta_bytes, delta_pct) =
            evaluate(local_size, self.baseline.artifact_size, &self.thresholds);

        let baseline_label = match (&self.baseline.version, &self.baseline.build) {
            (Some(v), Some(b)) if !v.is_empty() && !b.is_empty() => format!("version {v} ({b})"),
            (Some(v), _) if !v.is_empty() => format!("version {v}"),
            (_, Some(b)) if !b.is_empty() => format!("({b})"),
            _ => "previous build".to_string(),
        };

        // Positive deltas get an explicit `+`; negative ones already carry `-`.
        let pct = format_pct(delta_pct);
        let bytes = format_bytes(delta_bytes);
        let delta_pct_signed = if delta_bytes >= 0 {
            format!("+{pct}")
        } else {
            pct
        };
        let delta_bytes_signed = if delta_bytes >= 0 {
            format!("+{bytes}")
        } else {
            bytes
        };

        let summary = format!(
            "Bugsee size check: {} → {} ({}, {}) vs {}",
            format_bytes(self.baseline.artifact_size as i64),
            format_bytes(local_size as i64),
            delta_pct_signed,
            delta_bytes_signed,
            baseline_label,
        );

        match verdict {
            Verdict::Pass => (Verdict::Pass, summary),
            Verdict::Warn => (
                Verdict::Warn,
                format!(
                    "warning: {summary} — exceeds {}",
                    triggered_by.unwrap_or_default()
                ),
            ),
            Verdict::Fail => (
                Verdict::Fail,
                format!("{summary} — exceeds {}", triggered_by.unwrap_or_default()),
            ),
        }
    }
}

/// GET the baseline for this `(package_id, format=ipa, build_configuration)`.
/// Returns `None` for every non-success outcome (no baseline yet, network /
/// auth / 4xx / 5xx, malformed payload, legacy build with no captured size) —
/// the caller treats `None` as skip and never fails the build on infra trouble.
/// Port of `_fetch_baseline`.
async fn fetch_baseline(
    endpoint: &str,
    app_token: &str,
    package_id: &str,
    build_configuration: &str,
) -> Option<Baseline> {
    let url = format!("{}/baseline", builds_url(endpoint, app_token));
    let mut query: Vec<(&str, &str)> = vec![("package_id", package_id), ("format", "ipa")];
    if !build_configuration.is_empty() {
        query.push(("build_configuration", build_configuration));
    }

    let client = http::build_client().ok()?;
    let resp = match client.get(&url).query(&query).send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::info!(error = %e, "Bugsee: size-check baseline lookup failed; skipping check");
            return None;
        }
    };
    if !resp.status().is_success() {
        tracing::info!(
            status = resp.status().as_u16(),
            "Bugsee: size-check baseline lookup non-2xx; skipping check"
        );
        return None;
    }
    let text = resp.text().await.ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;

    // Tolerate the `{ ok, result: {...} }` envelope and the flat shape.
    let result = parsed
        .get("result")
        .filter(|r| r.is_object())
        .unwrap_or(&parsed);
    let build = result.get("build")?;
    if !build.is_object() {
        // `{ build: null }` — no eligible baseline yet (first build).
        return None;
    }
    // `artifact_size` must be a positive number; a legacy build without one is
    // treated as no baseline (don't compare against a different methodology).
    // `as_f64()` on a JSON number is always finite, so `<= 0.0` is exhaustive.
    let artifact_size = build.get("artifact_size").and_then(Value::as_f64)?;
    if artifact_size <= 0.0 {
        return None;
    }
    let str_field = |k: &str| {
        build
            .get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Some(Baseline {
        artifact_size: artifact_size as u64,
        version: str_field("version"),
        build: str_field("build"),
    })
}

/// Resolve the size-check config and fetch the baseline. Returns `Some(Prepared)`
/// only when the check is active AND a baseline exists; `None` for every skip
/// condition (master switch off, no thresholds, no `package_id`, lookup failed /
/// first build). Run BEFORE the build is registered. Port of `prepare_size_check`.
pub async fn prepare(
    env: &HashMap<String, String>,
    endpoint: &str,
    app_token: &str,
    package_id: Option<&str>,
    build_configuration: &str,
) -> Option<Prepared> {
    if !env_truthy(env, "BUGSEE_SIZE_CHECK_ENABLED") {
        return None;
    }
    let thresholds = resolve_thresholds(env);
    if thresholds.is_empty() {
        tracing::info!("Bugsee: size-check enabled but no thresholds configured; skipping");
        return None;
    }
    let package_id = match package_id {
        Some(p) if !p.is_empty() => p,
        _ => {
            tracing::info!("Bugsee: size-check skipped — no package_id resolved from Info.plist");
            return None;
        }
    };
    let baseline = match fetch_baseline(endpoint, app_token, package_id, build_configuration).await
    {
        Some(b) => b,
        None => {
            tracing::info!("Bugsee: size-check skipped — no baseline available");
            return None;
        }
    };
    Some(Prepared {
        thresholds,
        baseline,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn thresholds(fail_pct: Option<f64>, fail_bytes: Option<u64>) -> Thresholds {
        Thresholds {
            warning_pct: None,
            fail_pct,
            warning_bytes: None,
            fail_bytes,
        }
    }

    fn prepared(t: Thresholds, baseline_size: u64) -> Prepared {
        Prepared {
            thresholds: t,
            baseline: Baseline {
                artifact_size: baseline_size,
                version: Some("1.0".into()),
                build: Some("42".into()),
            },
        }
    }

    // ── Threshold parsing ───────────────────────────────────────────

    #[test]
    fn thresholds_reject_zero_negative_nonfinite_malformed() {
        let env = env_of(&[
            ("BUGSEE_SIZE_CHECK_WARNING_PCT", "0"),    // zero → disabled
            ("BUGSEE_SIZE_CHECK_FAIL_PCT", "inf"),     // non-finite → disabled
            ("BUGSEE_SIZE_CHECK_WARNING_BYTES", "-5"), // negative → disabled
            ("BUGSEE_SIZE_CHECK_FAIL_BYTES", "abc"),   // malformed → disabled
        ]);
        let t = resolve_thresholds(&env);
        assert_eq!(t, Thresholds::default());
        assert!(t.is_empty());
    }

    #[test]
    fn thresholds_parse_valid_values() {
        let env = env_of(&[
            ("BUGSEE_SIZE_CHECK_WARNING_PCT", "5.5"),
            ("BUGSEE_SIZE_CHECK_FAIL_PCT", "10"),
            ("BUGSEE_SIZE_CHECK_WARNING_BYTES", "1048576"),
            ("BUGSEE_SIZE_CHECK_FAIL_BYTES", "5242880"),
        ]);
        let t = resolve_thresholds(&env);
        assert_eq!(t.warning_pct, Some(5.5));
        assert_eq!(t.fail_pct, Some(10.0));
        assert_eq!(t.warning_bytes, Some(1_048_576));
        assert_eq!(t.fail_bytes, Some(5_242_880));
        assert!(!t.is_empty());
    }

    // ── Formatting ──────────────────────────────────────────────────

    #[test]
    fn format_bytes_units_and_sign() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1536), "1.5 KB");
        assert_eq!(format_bytes(1024 * 1024 + 1024 * 512), "1.5 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.50 GB");
        assert_eq!(format_bytes(-2048), "-2.0 KB");
    }

    #[test]
    fn format_pct_whole_vs_fractional() {
        assert_eq!(format_pct(20.0), "20%");
        assert_eq!(format_pct(20.5), "20.5%");
        assert_eq!(format_pct(-5.0), "-5%");
    }

    // ── Evaluation ──────────────────────────────────────────────────

    #[test]
    fn shrunk_or_flat_always_passes() {
        let t = thresholds(Some(1.0), Some(1));
        assert_eq!(evaluate(900, 1000, &t).0, Verdict::Pass); // shrunk
        assert_eq!(evaluate(1000, 1000, &t).0, Verdict::Pass); // flat
    }

    #[test]
    fn zero_baseline_passes() {
        let t = thresholds(Some(1.0), Some(1));
        assert_eq!(evaluate(1_000_000, 0, &t).0, Verdict::Pass);
    }

    #[test]
    fn fail_pct_triggers_fail() {
        // +20% growth vs a 10% fail threshold.
        let t = thresholds(Some(10.0), None);
        let (v, label, db, dp) = evaluate(1200, 1000, &t);
        assert_eq!(v, Verdict::Fail);
        assert_eq!(label.as_deref(), Some("fail threshold 10%"));
        assert_eq!(db, 200);
        assert!((dp - 20.0).abs() < 1e-9);
    }

    #[test]
    fn fail_wins_over_warning() {
        let t = Thresholds {
            warning_pct: Some(5.0),
            fail_pct: Some(10.0),
            warning_bytes: None,
            fail_bytes: None,
        };
        assert_eq!(evaluate(1200, 1000, &t).0, Verdict::Fail);
    }

    #[test]
    fn warning_when_below_fail() {
        // +7% growth: above the 5% warning, below the 10% fail.
        let t = Thresholds {
            warning_pct: Some(5.0),
            fail_pct: Some(10.0),
            warning_bytes: None,
            fail_bytes: None,
        };
        let (v, label, _, _) = evaluate(1070, 1000, &t);
        assert_eq!(v, Verdict::Warn);
        assert_eq!(label.as_deref(), Some("warning threshold 5%"));
    }

    #[test]
    fn bytes_gate_triggers_when_no_pct() {
        let t = thresholds(None, Some(100));
        let (v, label, _, _) = evaluate(1150, 1000, &t);
        assert_eq!(v, Verdict::Fail);
        assert_eq!(label.as_deref(), Some("fail threshold 100 B"));
    }

    #[test]
    fn growth_below_all_thresholds_passes() {
        let t = thresholds(Some(50.0), Some(1_000_000));
        assert_eq!(evaluate(1010, 1000, &t).0, Verdict::Pass); // +1%, +10B
    }

    #[test]
    fn fail_bytes_precedes_warning_pct_when_both_fire() {
        // +50% (200B) growth: warning_pct(5%) AND fail_bytes(100B) both cross.
        // FAIL wins over WARN, and the gate ORDER (fail_bytes before any
        // warning) means the label names the FAIL bytes gate. Pins precedence.
        let t = Thresholds {
            warning_pct: Some(5.0),
            fail_pct: None,
            warning_bytes: None,
            fail_bytes: Some(100),
        };
        let (v, label, _, _) = evaluate(600, 400, &t);
        assert_eq!(v, Verdict::Fail);
        assert_eq!(label.as_deref(), Some("fail threshold 100 B"));
    }

    #[test]
    fn warning_pct_label_precedes_warning_bytes_when_both_fire() {
        // Both warning gates cross; percent is checked first, so the label must
        // name the pct gate (a reorder of the two WARN blocks would change this).
        let t = Thresholds {
            warning_pct: Some(5.0),
            fail_pct: None,
            warning_bytes: Some(100),
            fail_bytes: None,
        };
        let (v, label, _, _) = evaluate(600, 400, &t); // +50%, +200B
        assert_eq!(v, Verdict::Warn);
        assert_eq!(label.as_deref(), Some("warning threshold 5%"));
    }

    // ── decide() formatting ─────────────────────────────────────────

    #[test]
    fn decide_fail_line_has_no_error_prefix_but_names_gate() {
        let p = prepared(thresholds(Some(10.0), None), 1_000_000);
        let (v, line) = p.decide(1_300_000); // +30%
        assert_eq!(v, Verdict::Fail);
        // No `error:` prefix (main adds it); carries the signed deltas + gate.
        assert!(!line.starts_with("error:"), "line: {line}");
        assert!(line.contains("Bugsee size check:"));
        assert!(line.contains("(+30%, +"));
        assert!(line.contains("exceeds fail threshold 10%"));
        assert!(line.contains("vs version 1.0 (42)"));
    }

    #[test]
    fn decide_warn_line_has_warning_prefix() {
        let p = prepared(
            Thresholds {
                warning_pct: Some(5.0),
                fail_pct: Some(50.0),
                warning_bytes: None,
                fail_bytes: None,
            },
            1_000_000,
        );
        let (v, line) = p.decide(1_100_000); // +10%
        assert_eq!(v, Verdict::Warn);
        assert!(
            line.starts_with("warning: Bugsee size check:"),
            "line: {line}"
        );
        assert!(line.contains("exceeds warning threshold 5%"));
    }

    #[test]
    fn decide_pass_line_is_bare_summary() {
        let p = prepared(thresholds(Some(50.0), None), 1_000_000);
        let (v, line) = p.decide(900_000); // shrunk
        assert_eq!(v, Verdict::Pass);
        assert!(!line.starts_with("warning:") && !line.starts_with("error:"));
        // Negative delta renders without a leading '+'.
        assert!(line.contains("(-10%, -"), "line: {line}");
    }

    // ── prepare() (skip conditions + baseline fetch) ────────────────

    #[tokio::test]
    async fn prepare_skips_when_master_switch_off() {
        let env = env_of(&[("BUGSEE_SIZE_CHECK_FAIL_PCT", "10")]);
        // No BUGSEE_SIZE_CHECK_ENABLED → skip without any network.
        let p = prepare(&env, "http://127.0.0.1:1", "TKN", Some("com.x"), "Release").await;
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn prepare_skips_when_no_thresholds() {
        let env = env_of(&[("BUGSEE_SIZE_CHECK_ENABLED", "1")]);
        let p = prepare(&env, "http://127.0.0.1:1", "TKN", Some("com.x"), "Release").await;
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn prepare_skips_when_no_package_id() {
        let env = env_of(&[
            ("BUGSEE_SIZE_CHECK_ENABLED", "1"),
            ("BUGSEE_SIZE_CHECK_FAIL_PCT", "10"),
        ]);
        let p = prepare(&env, "http://127.0.0.1:1", "TKN", None, "Release").await;
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn prepare_fetches_baseline_with_query_and_returns_prepared() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/baseline"))
            .and(query_param("package_id", "com.example.app"))
            .and(query_param("format", "ipa"))
            .and(query_param("build_configuration", "Release"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true,
                "result": { "build": { "artifact_size": 1_000_000, "version": "1.0", "build": "42" } }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let env = env_of(&[
            ("BUGSEE_SIZE_CHECK_ENABLED", "1"),
            ("BUGSEE_SIZE_CHECK_FAIL_PCT", "10"),
        ]);
        let uri = server.uri();
        let p = prepare(&env, &uri, "TKN", Some("com.example.app"), "Release")
            .await
            .expect("prepared");
        // The fetched baseline drives a FAIL on +30% growth.
        let (v, _line) = p.decide(1_300_000);
        assert_eq!(v, Verdict::Fail);
    }

    #[tokio::test]
    async fn prepare_skips_on_null_build_first_build() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/baseline"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "build": null }
            })))
            .mount(&server)
            .await;
        let env = env_of(&[
            ("BUGSEE_SIZE_CHECK_ENABLED", "1"),
            ("BUGSEE_SIZE_CHECK_FAIL_PCT", "10"),
        ]);
        let uri = server.uri();
        let p = prepare(&env, &uri, "TKN", Some("com.example.app"), "Release").await;
        assert!(p.is_none(), "a null baseline build must skip the check");
    }

    #[tokio::test]
    async fn prepare_skips_on_http_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/baseline"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let env = env_of(&[
            ("BUGSEE_SIZE_CHECK_ENABLED", "1"),
            ("BUGSEE_SIZE_CHECK_FAIL_PCT", "10"),
        ]);
        let uri = server.uri();
        // A 5xx must NEVER fail the build — degrade to skip.
        let p = prepare(&env, &uri, "TKN", Some("com.example.app"), "Release").await;
        assert!(p.is_none());
    }

    #[tokio::test]
    async fn prepare_skips_when_baseline_has_no_artifact_size() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/baseline"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "ok": true, "result": { "build": { "version": "1.0" } }
            })))
            .mount(&server)
            .await;
        let env = env_of(&[
            ("BUGSEE_SIZE_CHECK_ENABLED", "1"),
            ("BUGSEE_SIZE_CHECK_FAIL_PCT", "10"),
        ]);
        let uri = server.uri();
        let p = prepare(&env, &uri, "TKN", Some("com.example.app"), "Release").await;
        assert!(
            p.is_none(),
            "a legacy build with no artifact_size must skip"
        );
    }
}
