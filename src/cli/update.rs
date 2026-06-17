//! `bugsee-cli update` — self-update the running binary in place.
//!
//! Resolves the newest published version WITHIN THE SAME MAJOR as the running
//! binary (minor/patch are non-breaking by our release policy; a major bump is
//! never auto-adopted), downloads + SHA-256-verifies that release's artefact for
//! the host triple, and atomically replaces the current executable.
//!
//! ## Discovery contract
//!
//! `https://download.bugsee.com/cli/v<major>.x/version.txt` holds the latest
//! bare `X.Y.Z` within major `<major>` (maintained by the release mirror,
//! `mirror-to-s3.yml`). The SAME per-major pointer is what the Android Gradle
//! plugin and the iOS BugseeAgents read for THEIR auto-update, so the
//! "newest non-breaking version" cap is computed identically across every
//! producer — one contract, four implementations.

use clap::Args;
use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{config_invalid, input_invalid};

const DOWNLOAD_BASE: &str = "https://download.bugsee.com/cli";

/// Download root for self-update. Defaults to the public mirror; overridable via
/// `BUGSEE_CLI_UPDATE_BASE_URL` for an air-gapped internal mirror (and for the
/// end-to-end tests, which point it at a local mock server).
fn download_base() -> String {
    std::env::var("BUGSEE_CLI_UPDATE_BASE_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DOWNLOAD_BASE.to_string())
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Check whether a newer compatible version is available and report it,
    /// WITHOUT downloading or replacing anything. Exits 0 either way.
    #[arg(long)]
    pub check: bool,

    /// Update to this exact version instead of the resolved latest-in-major
    /// (e.g. `0.5.1`). A version in a DIFFERENT major than the running binary
    /// is allowed but warned about — it may contain breaking changes.
    #[arg(long, value_name = "X.Y.Z")]
    pub version: Option<String>,

    /// Re-install even when already on the target version (repairs a corrupt
    /// or partial install).
    #[arg(long)]
    pub force: bool,

    /// Check at most once per this interval — e.g. `12h`, `30m`, `1d`, `3600s`
    /// (bare number = seconds). The last-check time is recorded next to the
    /// binary and the command no-ops while still fresh. `--max-age` ALSO implies
    /// best-effort: any check/download failure is logged and exits 0, so a
    /// caller (the Gradle plugin, the iOS BugseeAgents) can run this on every
    /// build/invocation without ever failing it.
    #[arg(long, value_name = "DURATION")]
    pub max_age: Option<String>,
}

pub async fn dispatch(args: UpdateArgs) -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");

    // `--max-age` turns this into a throttled, best-effort call: the consumers
    // (Gradle plugin, iOS BugseeAgents) run `update --max-age 12h` on every
    // build, and the CLI itself owns BOTH the throttle and the never-fail
    // posture so that logic lives in exactly one place.
    let max_age = match args.max_age.as_deref() {
        Some(s) => Some(parse_duration(s)?),
        None => None,
    };

    if let Some(ttl) = max_age {
        // Throttle gate: if we checked recently, do nothing (no network).
        if let Some(last) = read_last_check() {
            let now = now_epoch();
            if is_fresh(last, now, ttl.as_secs()) {
                tracing::debug!(
                    "bugsee-cli update check throttled ({}s since last check < {}s)",
                    now.saturating_sub(last),
                    ttl.as_secs()
                );
                report(current, current, UpdateAction::Throttled);
                return Ok(());
            }
        }
    }

    let outcome = run(&args).await;

    if max_age.is_some() {
        // We got past the throttle gate, so a check was ATTEMPTED — record the
        // time regardless of success so a down/absent endpoint can't be hammered
        // every build (strict "at most one attempt per interval").
        write_last_check(now_epoch());
        if let Err(e) = outcome {
            tracing::warn!("bugsee-cli update (best-effort) did not complete: {e}");
            report(current, current, UpdateAction::Skipped);
            return Ok(());
        }
    }
    outcome
}

async fn run(args: &UpdateArgs) -> anyhow::Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    let triple = host_triple().ok_or_else(|| {
        config_invalid(format!(
            "no published bugsee-cli build for this host ({}/{}); self-update is unavailable — \
             install via your package manager instead",
            std::env::consts::OS,
            std::env::consts::ARCH,
        ))
    })?;

    let base = download_base();

    // Resolve the target version.
    let target = match args.version.as_deref() {
        Some(explicit) => {
            let explicit = validate_version(explicit)?;
            if major_of(&explicit) != major_of(current) {
                tracing::warn!(
                    "requested {explicit} crosses a major boundary from {current} — it may \
                     contain breaking changes (the same-major cap only applies to the default, \
                     version-less update)"
                );
            }
            explicit
        }
        None => {
            let channel = channel_url(&base, current);
            tracing::info!(
                "checking {channel} for the latest {}.x release",
                major_of(current)
            );
            let latest = validate_version(fetch_text(&channel).await?.trim())?;
            match decide_target(current, &latest) {
                None => {
                    report(current, &latest, UpdateAction::UpToDate);
                    return Ok(());
                }
                Some(t) => t,
            }
        }
    };

    if args.check {
        report(current, &target, UpdateAction::Available);
        return Ok(());
    }
    if target == current && !args.force {
        report(current, &target, UpdateAction::UpToDate);
        return Ok(());
    }

    tracing::info!("downloading bugsee-cli {target} for {triple}");
    let tmp = tempfile::tempdir()?;
    let new_binary = install(&base, &target, triple, tmp.path()).await?;

    // Atomically replace the running executable. `self_replace` handles the
    // Windows "can't overwrite a running .exe" case (rename-then-stage) and the
    // Unix in-place rename uniformly.
    self_replace::self_replace(&new_binary).map_err(|e| {
        // A permission error here is the common case (binary owned by root, e.g.
        // a system/Homebrew install). Surface it actionably.
        input_invalid(format!(
            "could not replace the running binary at {:?}: {e} — re-run with sufficient \
             permissions, or update via your package manager",
            std::env::current_exe().ok()
        ))
    })?;

    report(current, &target, UpdateAction::Updated);
    Ok(())
}

/// The host's Rust target triple, matching what `bugsee-cli` is published for.
/// `None` for hosts with no published build (Linux musl, Windows ARM64, …).
/// Resolved from the compile-time target via `std::env::consts`.
pub(crate) fn host_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("windows", "x86_64") => Some("x86_64-pc-windows-msvc"),
        _ => None,
    }
}

/// `cli/v<major>.x/version.txt` — the per-major "latest" pointer for `current`.
fn channel_url(base: &str, current: &str) -> String {
    format!("{base}/v{}.x/version.txt", major_of(current))
}

/// `(archive_url, sha256_url)` for a `(version, triple)`.
fn artifact_urls(base: &str, version: &str, triple: &str) -> (String, String) {
    let ext = if triple.contains("windows") {
        "zip"
    } else {
        "tar.xz"
    };
    let art = format!("{base}/v{version}/bugsee-cli-{triple}.{ext}");
    let sha = format!("{art}.sha256");
    (art, sha)
}

/// Decide the update target given the running version and the channel's latest.
/// Returns `Some(latest)` ONLY when `latest` is the SAME major as `current` AND
/// strictly newer — the non-breaking-update rule. Pure (no I/O) so the cap
/// semantics are unit-tested directly.
pub(crate) fn decide_target(current: &str, latest: &str) -> Option<String> {
    if major_of(latest) == major_of(current) && cmp_versions(latest, current) == Ordering::Greater {
        Some(latest.to_string())
    } else {
        None
    }
}

/// Validate a version string is a clean `X.Y.Z[-pre]` — rejects anything that
/// could smuggle a path segment into the download URL (the version is
/// interpolated into `cli/v<version>/`). Returns the trimmed value.
fn validate_version(v: &str) -> anyhow::Result<String> {
    let v = v.trim();
    let core_ok = !v.is_empty()
        && v.split(['.', '-', '+'])
            .next()
            .is_some_and(|s| !s.is_empty())
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+'));
    // Must have at least MAJOR.MINOR.PATCH numeric core.
    let has_three = {
        let core = v.split(['-', '+']).next().unwrap_or("");
        core.split('.').count() >= 3
            && core
                .split('.')
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
    };
    if core_ok && has_three {
        Ok(v.to_string())
    } else {
        Err(input_invalid(format!("not a valid X.Y.Z version: {v:?}")))
    }
}

/// Leading-digit numeric component vector (`"0.5.0-rc1"` → `[0,5,0,1]`),
/// matching the Gradle plugin's / BugseeAgents' comparison so the cap is
/// computed identically in all four implementations.
fn components(v: &str) -> Vec<u64> {
    v.split(['.', '-', '+'])
        .map(|seg| {
            seg.chars()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .parse()
                .unwrap_or(0)
        })
        .collect()
}

fn major_of(v: &str) -> u64 {
    components(v).first().copied().unwrap_or(0)
}

fn cmp_versions(a: &str, b: &str) -> Ordering {
    let (a, b) = (components(a), components(b));
    for i in 0..a.len().max(b.len()) {
        let ai = a.get(i).copied().unwrap_or(0);
        let bi = b.get(i).copied().unwrap_or(0);
        match ai.cmp(&bi) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

#[derive(Clone, Copy)]
enum UpdateAction {
    Updated,
    UpToDate,
    Available,
    /// `--max-age` gate hit: still fresh, no check performed.
    Throttled,
    /// `--max-age` best-effort: a check/download failure was swallowed.
    Skipped,
}

impl UpdateAction {
    fn as_str(self) -> &'static str {
        match self {
            UpdateAction::Updated => "updated",
            UpdateAction::UpToDate => "up-to-date",
            UpdateAction::Available => "available",
            UpdateAction::Throttled => "throttled",
            UpdateAction::Skipped => "skipped",
        }
    }
}

/// Human progress on stderr (tracing), one machine-readable JSON line on stdout
/// (per the stdout-is-structured convention).
fn report(current: &str, target: &str, action: UpdateAction) {
    match action {
        UpdateAction::Updated => tracing::info!("updated {current} → {target}"),
        UpdateAction::UpToDate => tracing::info!("already up to date ({current})"),
        UpdateAction::Available => {
            tracing::info!("update available: {current} → {target} (run without --check to apply)")
        }
        UpdateAction::Throttled => tracing::debug!("update check throttled ({current})"),
        UpdateAction::Skipped => tracing::debug!("update check skipped ({current})"),
    }
    println!(
        "{}",
        serde_json::json!({
            "current": current,
            "target": target,
            "action": action.as_str(),
            "updated": matches!(action, UpdateAction::Updated),
        })
    );
}

/// Parse a coarse duration: `Ns` / `Nm` / `Nh` / `Nd`, or a bare `N` (seconds).
fn parse_duration(s: &str) -> anyhow::Result<std::time::Duration> {
    let s = s.trim();
    let (num, mult): (&str, u64) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    let n: u64 = num
        .trim()
        .parse()
        .map_err(|_| input_invalid(format!("invalid --max-age duration: {s:?}")))?;
    Ok(std::time::Duration::from_secs(n.saturating_mul(mult)))
}

/// `true` when `last` is a sane past timestamp newer than `now - ttl`. A zero or
/// future `last` is treated as stale (forces a re-check) rather than trusted.
fn is_fresh(last: u64, now: u64, ttl_secs: u64) -> bool {
    last > 0 && last <= now && now.saturating_sub(last) < ttl_secs
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The last-check timestamp file lives NEXT TO the binary, so each cached/managed
/// install throttles independently (and `self_replace` leaves sibling files
/// intact across updates). `None` if the exe path can't be resolved.
fn check_state_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join(".bugsee-cli-update-check"))
}

fn read_last_check() -> Option<u64> {
    std::fs::read_to_string(check_state_path()?)
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_last_check(now: u64) {
    if let Some(p) = check_state_path() {
        let _ = std::fs::write(p, now.to_string()); // best-effort; a read-only dir just means no throttle
    }
}

async fn fetch_text(url: &str) -> anyhow::Result<String> {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

/// Download + SHA-256-verify + extract the release artefact for `(version,
/// triple)` into `dest`, returning the path to the extracted binary. Does NOT
/// touch the running executable — the caller does the in-place replace — so this
/// is unit-testable against a mock server without clobbering the test binary.
async fn install(base: &str, version: &str, triple: &str, dest: &Path) -> anyhow::Result<PathBuf> {
    let (art_url, sha_url) = artifact_urls(base, version, triple);
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .build()?;

    let bytes = client
        .get(&art_url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    let expected = parse_sha256_sidecar(&fetch_text(&sha_url).await?);
    let actual = sha256_hex(&bytes);
    if !expected.eq_ignore_ascii_case(&actual) {
        anyhow::bail!("SHA-256 mismatch for {art_url}: expected {expected}, got {actual}");
    }

    let archive = dest.join(art_url.rsplit('/').next().unwrap_or("bugsee-cli-archive"));
    std::fs::write(&archive, &bytes)?;

    // System `tar` extracts both `.tar.xz` (macOS/Linux) and `.zip` (Windows 10+
    // bsdtar), auto-detecting compression by content. `--strip-components=1`
    // drops the `bugsee-cli-<triple>/` wrapper so the binary lands in `dest`.
    let status = std::process::Command::new("tar")
        .args([
            "-xf",
            &archive.to_string_lossy(),
            "-C",
            &dest.to_string_lossy(),
            "--strip-components=1",
        ])
        .status()?;
    if !status.success() {
        anyhow::bail!("tar extraction failed for {archive:?} (exit {status})");
    }

    let bin = dest.join(if triple.contains("windows") {
        "bugsee-cli.exe"
    } else {
        "bugsee-cli"
    });
    if !bin.is_file() {
        anyhow::bail!("extraction completed but bugsee-cli not found in {dest:?}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin)?.permissions();
        perms.set_mode(perms.mode() | 0o755);
        std::fs::set_permissions(&bin, perms)?;
    }
    Ok(bin)
}

/// Parse a `sha256sum`-style sidecar (`<hex>  <name>` / `<hex> *<name>`) → hex.
fn parse_sha256_sidecar(text: &str) -> String {
    text.split_whitespace().next().unwrap_or("").to_string()
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── version math (cap semantics) ───────────────────────────────

    #[test]
    fn major_and_components_parse_leading_digits() {
        assert_eq!(major_of("0.5.0"), 0);
        assert_eq!(major_of("1.2.3"), 1);
        assert_eq!(major_of("10.0.0"), 10);
        // Leading-digit-only extraction per segment: a non-numeric prerelease
        // label contributes 0; a numeric one contributes its value.
        assert_eq!(components("0.5.0-rc1"), vec![0, 5, 0, 0]);
        assert_eq!(components("1.2.3-4"), vec![1, 2, 3, 4]);
    }

    #[test]
    fn cmp_versions_is_componentwise() {
        assert_eq!(cmp_versions("0.5.1", "0.5.0"), Ordering::Greater);
        assert_eq!(cmp_versions("0.6.0", "0.5.9"), Ordering::Greater);
        assert_eq!(cmp_versions("0.5.0", "0.5.0"), Ordering::Equal);
        assert_eq!(cmp_versions("0.5.0", "0.10.0"), Ordering::Less);
        // trailing-zero padding
        assert_eq!(cmp_versions("0.5", "0.5.0"), Ordering::Equal);
    }

    #[test]
    fn decide_target_only_advances_within_same_major() {
        // newer same-major → update
        assert_eq!(decide_target("0.5.0", "0.5.1").as_deref(), Some("0.5.1"));
        assert_eq!(decide_target("0.5.0", "0.6.0").as_deref(), Some("0.6.0"));
        // newer but DIFFERENT major → never (breaking)
        assert_eq!(decide_target("0.5.0", "1.0.0"), None);
        assert_eq!(decide_target("1.2.0", "2.0.0"), None);
        // same or older → no-op
        assert_eq!(decide_target("0.5.0", "0.5.0"), None);
        assert_eq!(decide_target("0.5.1", "0.5.0"), None);
        // an older major published as "latest" (shouldn't happen) → no downgrade
        assert_eq!(decide_target("1.0.0", "0.9.0"), None);
    }

    // ── URL construction ───────────────────────────────────────────

    #[test]
    fn channel_url_is_per_major() {
        assert_eq!(
            channel_url("https://d/cli", "0.5.0"),
            "https://d/cli/v0.x/version.txt"
        );
        assert_eq!(
            channel_url("https://d/cli", "3.7.2"),
            "https://d/cli/v3.x/version.txt"
        );
    }

    #[test]
    fn artifact_urls_pick_ext_by_triple() {
        let (a, s) = artifact_urls("https://d/cli", "0.5.0", "aarch64-apple-darwin");
        assert_eq!(
            a,
            "https://d/cli/v0.5.0/bugsee-cli-aarch64-apple-darwin.tar.xz"
        );
        assert_eq!(s, format!("{a}.sha256"));
        let (w, _) = artifact_urls("https://d/cli", "0.5.0", "x86_64-pc-windows-msvc");
        assert!(w.ends_with(".zip"));
    }

    // ── version validation (path-traversal guard) ──────────────────

    #[test]
    fn validate_version_accepts_clean_and_rejects_dirty() {
        assert!(validate_version("0.5.0").is_ok());
        assert!(validate_version("10.20.30").is_ok());
        assert!(validate_version(" 0.5.0 ").is_ok()); // trimmed
        assert!(validate_version("0.5.0-rc1").is_ok());
        for bad in [
            "0.5",
            "latest",
            "../0.5.0",
            "0.5.0/../../etc",
            "0.5.x",
            "",
            "v0.5.0",
        ] {
            assert!(validate_version(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn host_triple_is_known_on_this_host() {
        // The test host is one of the 5 supported triples (CI + dev machines).
        assert!(host_triple().is_some());
    }

    #[test]
    fn parse_sha256_sidecar_takes_first_token() {
        assert_eq!(
            parse_sha256_sidecar("abc123  bugsee-cli.tar.xz\n"),
            "abc123"
        );
        assert_eq!(parse_sha256_sidecar("deadBEEF *file"), "deadBEEF");
    }

    // ── throttle (--max-age) ───────────────────────────────────────

    #[test]
    fn parse_duration_understands_suffixes_and_bare_seconds() {
        assert_eq!(parse_duration("90").unwrap().as_secs(), 90); // bare = seconds
        assert_eq!(parse_duration("45s").unwrap().as_secs(), 45);
        assert_eq!(parse_duration("30m").unwrap().as_secs(), 1_800);
        assert_eq!(parse_duration("12h").unwrap().as_secs(), 43_200);
        assert_eq!(parse_duration("2d").unwrap().as_secs(), 172_800);
        assert!(parse_duration("12x").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn is_fresh_only_for_a_recent_past_timestamp() {
        let now = 1_000_000;
        let ttl = 100;
        assert!(is_fresh(now - 50, now, ttl), "50s ago, ttl 100 → fresh");
        assert!(!is_fresh(now - 150, now, ttl), "150s ago, ttl 100 → stale");
        assert!(
            !is_fresh(now - ttl, now, ttl),
            "exactly ttl ago → stale (strict <)"
        );
        assert!(!is_fresh(0, now, ttl), "zero/never → stale");
        assert!(
            !is_fresh(now + 10, now, ttl),
            "future timestamp → distrusted/stale"
        );
    }

    // ── install(): download + verify + extract against a mock server ─

    #[tokio::test]
    async fn install_downloads_verifies_and_extracts() {
        use wiremock::matchers::{method, path as wm_path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let triple = "aarch64-apple-darwin";
        let tmp = tempfile::tempdir().unwrap();

        // Build a real tar containing `bugsee-cli-<triple>/bugsee-cli` so that
        // `tar -xf --strip-components=1` yields `bugsee-cli`. A plain tar (no xz)
        // works — system tar auto-detects compression by content, so the `.tar.xz`
        // name is fine even though the bytes are an uncompressed tar.
        let staging = tmp.path().join("staging");
        let wrapper = staging.join(format!("bugsee-cli-{triple}"));
        std::fs::create_dir_all(&wrapper).unwrap();
        std::fs::write(wrapper.join("bugsee-cli"), b"#!/bin/sh\necho fake-cli\n").unwrap();
        let tar_path = tmp.path().join("artefact.tar.xz");
        let ok = std::process::Command::new("tar")
            .args([
                "-cf",
                &tar_path.to_string_lossy(),
                "-C",
                &staging.to_string_lossy(),
                &format!("bugsee-cli-{triple}"),
            ])
            .status()
            .unwrap()
            .success();
        assert!(ok, "failed to build test tar");
        let tar_bytes = std::fs::read(&tar_path).unwrap();
        let sha = sha256_hex(&tar_bytes);

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(wm_path(format!("/v0.5.0/bugsee-cli-{triple}.tar.xz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(tar_bytes.clone()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(wm_path(format!(
                "/v0.5.0/bugsee-cli-{triple}.tar.xz.sha256"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_string(format!("{sha}  x")))
            .mount(&server)
            .await;

        let dest = tmp.path().join("dest");
        std::fs::create_dir_all(&dest).unwrap();
        let bin = install(&server.uri(), "0.5.0", triple, &dest)
            .await
            .unwrap();
        assert_eq!(bin, dest.join("bugsee-cli"));
        assert!(bin.is_file());
        assert!(std::fs::read_to_string(&bin).unwrap().contains("fake-cli"));
    }

    #[tokio::test]
    async fn install_rejects_sha_mismatch() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("anything"))
            .mount(&server)
            .await;
        // The sha sidecar (same matcher) returns "anything" too → won't match the
        // real digest of the body, so install must bail.
        let tmp = tempfile::tempdir().unwrap();
        let err = install(&server.uri(), "0.5.0", "aarch64-apple-darwin", tmp.path())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("SHA-256 mismatch"), "got: {err}");
    }
}
