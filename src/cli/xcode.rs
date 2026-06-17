//! Xcode build-phase orchestration — the `xcode post-action` command.
//!
//! Run from an Xcode "Run Script" build phase (Post-actions stage), which
//! exports the build settings as environment variables. This command reads
//! that environment and sequences the build-publish ops the CLI already owns,
//! so the iOS SDK's `tools.bundle/BugseeAgent` Python script can eventually
//! delegate the whole flow to one binary instead of orchestrating it itself.
//!
//! ## Phase 1 scope (this module)
//!
//! The **build-info + dSYM** path end to end:
//!
//!   1. parse the Xcode env into a map,
//!   2. gate (Release-only / `BUGSEE_BUILD_INFO_*` flags) — mirrors the
//!      agent's `should_run_build_publish_flow`,
//!   3. locate the built `.app` (archive `Products/Applications/*.app` or
//!      `$TARGET_BUILD_DIR/$WRAPPER_NAME`) — mirrors `find_app`,
//!   4. read its `Info.plist` for bundle id / version / build,
//!   5. resolve VCS + machine + Xcode metadata via the reusable resolvers,
//!   6. collect iOS dependencies via `ios_deps::collect`,
//!   7. register the build + upload the build-info bundle via
//!      `build_info::run` (self-contained mode),
//!   8. discover + upload dSYMs via `debug_files::run_dsym_upload`.
//!
//! The behaviour mirrors `run_size_analysis_flow` in the iOS agent
//! (`ios/sdk/.../tools.bundle/BugseeAgent`), restricted to the Phase-1 subset.
//! The registration payload field shape is copied field-for-field from that
//! flow so the worker/appserver accept it identically.
//!
//! ## Deferred (NOT implemented in Phase 1)
//!
//!   - Build timings (`.xcactivitylog` decode) — `build_metadata.timings` and
//!     the `request_timings_upload` / `timings.json` sidecar.
//!   - `.app` → `.ipa` packaging + artefact upload (`request_artifact_upload`,
//!     `upload build`).
//!   - The in-build size-check (`prepare_size_check` / `run_size_check`).
//!   - Daemonization — Phase 1 runs in the foreground.
//!
//! See the `TODO(phase 2)` / `TODO(phase 3)` markers below for where each
//! deferred concern slots in.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use serde_json::{json, Map, Value};

use crate::cli::{
    build_env, debug_files, ios_deps, size_check, vcs_metadata, xcactivitylog, xcode_ipa,
};
use crate::compress::Strategy;
use crate::error::{config_invalid, Error};
use crate::upload::build;
use crate::upload::http::RetryPolicy;

const DEFAULT_ENDPOINT: &str = "https://api.bugsee.com";

/// Default truncation cap on the collected dep list. Matches the agent's
/// `_DEPS_MAX_COUNT` and `ios_deps::DEPENDENCIES_MAX_COUNT`.
const DEPS_MAX_COUNT: usize = ios_deps::DEPENDENCIES_MAX_COUNT;

/// `dependencies_summary.collection_config.scope` — the agent's
/// `_DEPS_COLLECTION_SCOPE`. `ios_deps::collect` returns this as its
/// `scope_label`; we propagate that value rather than hardcoding.
const DEPS_COLLECTION_SCOPE: &str = "all";

/// Deps blob `schema_version` — the agent's `_DEPS_SCHEMA_VERSION`.
const DEPS_SCHEMA_VERSION: i64 = 1;

/// The `post-action` flow is configured almost entirely through environment
/// variables (Xcode exports build settings as env vars, and the BUGSEE_* knobs
/// ride alongside them), so they belong in `--help` even though they aren't
/// clap flags. Truthy values everywhere: `1`, `true`, `yes`, `on` (case-insensitive).
const POST_ACTION_ENV_HELP: &str = "\
Environment variables (read from the Xcode build-phase environment). Every
toggle below also has an --enable-<x> / --disable-<x> flag and every threshold a
--size-check-*  flag (listed under Options above); a flag passed on the command
line overrides its environment variable.

Gating (whether the post-action does anything):
  BUGSEE_BUILD_INFO_ENABLED             Master switch for the whole flow [default: on].
  BUGSEE_BUILD_INFO_ALL_ACTIONS         Also run on plain Build actions, not just Archive [default: off].
  BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS  Run for non-Release configurations too [default: off → Release-only].
                                        (Legacy alias: BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS.)

Collection opt-outs:
  BUGSEE_DEPENDENCIES_ENABLED           Collect the dependency graph [default: on].
  BUGSEE_BUILD_INFO_TIMINGS_ENABLED     Decode build timings from .xcactivitylog [default: on].

Artefact upload (size analysis):
  BUGSEE_SIZE_ANALYSIS_ENABLED          Upload the packaged .ipa for server-side size analysis [default: off].
  BUGSEE_CHUNKED_UPLOAD                 Use the chunked transport for the artefact [default: off].

Size-check build gate (deliberately fails the build; only with --force-foreground):
  BUGSEE_SIZE_CHECK_ENABLED             Enable the in-build size-growth check [default: off].
  BUGSEE_SIZE_CHECK_WARNING_PCT         Warn if the .ipa grew >= this percent vs the previous build.
  BUGSEE_SIZE_CHECK_FAIL_PCT            Fail (exit 40) if it grew >= this percent.
  BUGSEE_SIZE_CHECK_WARNING_BYTES       Warn if it grew >= this many bytes.
  BUGSEE_SIZE_CHECK_FAIL_BYTES          Fail (exit 40) if it grew >= this many bytes.

The app token and endpoint come from --app-token / --endpoint (or BUGSEE_APP_TOKEN /
BUGSEE_ENDPOINT). In background mode the daemon's log goes to $PROJECT_TEMP_DIR/bugsee-cli.log.";

/// `bugsee-cli xcode` argument shape.
#[derive(Subcommand, Debug)]
pub enum XcodeCommand {
    /// Run the iOS build-publish flow from an Xcode Run-Script post-action (backgrounded by default).
    ///
    /// Reads the Xcode build settings from the process environment, gates on
    /// the `BUGSEE_BUILD_INFO_*` flags (Release-only by default), and — when
    /// admitted — registers the build, uploads the build-info bundle, and
    /// uploads dSYMs. A no-op (exit 0) when gated out: this runs as a
    /// post-action and must never fail an already-signed build.
    ///
    /// Runs in the BACKGROUND by default (double-forks into a detached daemon so
    /// the archive returns immediately; its output goes to
    /// `$PROJECT_TEMP_DIR/bugsee-cli.log`). Pass `--force-foreground` to run
    /// synchronously instead — the only mode in which a size-check FAIL can
    /// deliberately fail the build (exit 40).
    #[command(after_long_help = POST_ACTION_ENV_HELP)]
    PostAction {
        /// Run synchronously in the foreground instead of detaching into a
        /// background daemon. Required for CI gating: a size-check FAIL only
        /// propagates its non-zero exit when foregrounded.
        #[arg(long)]
        force_foreground: bool,

        #[command(flatten)]
        overrides: PostActionOverrides,
    },
}

/// CLI alternatives to the `BUGSEE_*` post-action knobs. Every toggle has an
/// `--enable-<x>` / `--disable-<x>` pair; the numeric size-check thresholds are
/// plain value flags. A flag passed on the command line overrides the matching
/// environment variable (within a pair the last one wins); an unset flag leaves
/// the env var / default in force. These are overlaid onto the collected
/// environment by [`apply_overrides`] before any gate runs, so the env-driven
/// gate logic stays the single source of truth.
#[derive(clap::Args, Debug, Default)]
pub struct PostActionOverrides {
    /// Run the build-publish flow (overrides BUGSEE_BUILD_INFO_ENABLED; default: on).
    #[arg(long = "enable-build-info", overrides_with = "disable_build_info")]
    enable_build_info: bool,
    /// Skip the entire build-publish flow (overrides BUGSEE_BUILD_INFO_ENABLED).
    #[arg(long = "disable-build-info", overrides_with = "enable_build_info")]
    disable_build_info: bool,

    /// Also run on plain Build actions, not just Archive (overrides BUGSEE_BUILD_INFO_ALL_ACTIONS; default: off).
    #[arg(long = "enable-all-actions", overrides_with = "disable_all_actions")]
    enable_all_actions: bool,
    /// Run only on Archive actions (overrides BUGSEE_BUILD_INFO_ALL_ACTIONS).
    #[arg(long = "disable-all-actions", overrides_with = "enable_all_actions")]
    disable_all_actions: bool,

    /// Run for non-Release configurations too (overrides BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS; default: off → Release-only).
    #[arg(
        long = "enable-all-configurations",
        overrides_with = "disable_all_configurations"
    )]
    enable_all_configurations: bool,
    /// Restrict to Release configurations (overrides BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS).
    #[arg(
        long = "disable-all-configurations",
        overrides_with = "enable_all_configurations"
    )]
    disable_all_configurations: bool,

    /// Collect the dependency graph (overrides BUGSEE_DEPENDENCIES_ENABLED; default: on).
    #[arg(long = "enable-dependencies", overrides_with = "disable_dependencies")]
    enable_dependencies: bool,
    /// Skip dependency-graph collection (overrides BUGSEE_DEPENDENCIES_ENABLED).
    #[arg(long = "disable-dependencies", overrides_with = "enable_dependencies")]
    disable_dependencies: bool,

    /// Decode build timings from the .xcactivitylog (overrides BUGSEE_BUILD_INFO_TIMINGS_ENABLED; default: on).
    #[arg(long = "enable-timings", overrides_with = "disable_timings")]
    enable_timings: bool,
    /// Skip build-timings decoding (overrides BUGSEE_BUILD_INFO_TIMINGS_ENABLED).
    #[arg(long = "disable-timings", overrides_with = "enable_timings")]
    disable_timings: bool,

    /// Upload the packaged .ipa for server-side size analysis (overrides BUGSEE_SIZE_ANALYSIS_ENABLED; default: off).
    #[arg(
        long = "enable-size-analysis",
        overrides_with = "disable_size_analysis"
    )]
    enable_size_analysis: bool,
    /// Do not upload the .ipa for size analysis (overrides BUGSEE_SIZE_ANALYSIS_ENABLED).
    #[arg(
        long = "disable-size-analysis",
        overrides_with = "enable_size_analysis"
    )]
    disable_size_analysis: bool,

    /// Use the chunked transport for the artefact upload (overrides BUGSEE_CHUNKED_UPLOAD; default: off).
    #[arg(
        long = "enable-chunked-upload",
        overrides_with = "disable_chunked_upload"
    )]
    enable_chunked_upload: bool,
    /// Use the single-PUT transport for the artefact upload (overrides BUGSEE_CHUNKED_UPLOAD).
    #[arg(
        long = "disable-chunked-upload",
        overrides_with = "enable_chunked_upload"
    )]
    disable_chunked_upload: bool,

    /// Enable the in-build size-growth check (overrides BUGSEE_SIZE_CHECK_ENABLED; default: off).
    #[arg(long = "enable-size-check", overrides_with = "disable_size_check")]
    enable_size_check: bool,
    /// Disable the in-build size-growth check (overrides BUGSEE_SIZE_CHECK_ENABLED).
    #[arg(long = "disable-size-check", overrides_with = "enable_size_check")]
    disable_size_check: bool,

    /// Warn if the .ipa grew >= this percent vs the previous build (overrides BUGSEE_SIZE_CHECK_WARNING_PCT).
    #[arg(long = "size-check-warning-pct", value_name = "PCT")]
    size_check_warning_pct: Option<f64>,
    /// Fail (exit 40) if the .ipa grew >= this percent (overrides BUGSEE_SIZE_CHECK_FAIL_PCT).
    #[arg(long = "size-check-fail-pct", value_name = "PCT")]
    size_check_fail_pct: Option<f64>,
    /// Warn if the .ipa grew >= this many bytes (overrides BUGSEE_SIZE_CHECK_WARNING_BYTES).
    #[arg(long = "size-check-warning-bytes", value_name = "BYTES")]
    size_check_warning_bytes: Option<i64>,
    /// Fail (exit 40) if the .ipa grew >= this many bytes (overrides BUGSEE_SIZE_CHECK_FAIL_BYTES).
    #[arg(long = "size-check-fail-bytes", value_name = "BYTES")]
    size_check_fail_bytes: Option<i64>,
}

/// Resolve an `--enable-x` / `--disable-x` flag pair to a tri-state. `clap`'s
/// `overrides_with` guarantees the two bools are never both `true` (the last
/// one on the command line wins), so this is unambiguous; `None` means neither
/// was passed (fall back to the env var / default).
fn resolve_toggle(enable: bool, disable: bool) -> Option<bool> {
    if enable {
        Some(true)
    } else if disable {
        Some(false)
    } else {
        None
    }
}

/// Overlay the explicit CLI toggle/threshold flags onto the collected
/// environment map. A flag that was passed wins over the corresponding
/// `BUGSEE_*` env var; an unset flag leaves the env value (or its default)
/// untouched. Bool flags write the canonical `"1"` / `"0"` token so the
/// existing `env_truthy*` parsing applies unchanged; numeric flags stringify
/// and are re-validated by the size-check threshold parsers (a non-positive
/// value disables its gate, exactly as the env path does).
fn apply_overrides(env: &mut HashMap<String, String>, o: &PostActionOverrides) {
    fn set_bool(env: &mut HashMap<String, String>, key: &str, v: Option<bool>) {
        if let Some(b) = v {
            env.insert(key.to_string(), if b { "1" } else { "0" }.to_string());
        }
    }
    fn set_num(env: &mut HashMap<String, String>, key: &str, v: Option<impl ToString>) {
        if let Some(n) = v {
            env.insert(key.to_string(), n.to_string());
        }
    }

    set_bool(
        env,
        "BUGSEE_BUILD_INFO_ENABLED",
        resolve_toggle(o.enable_build_info, o.disable_build_info),
    );
    set_bool(
        env,
        "BUGSEE_BUILD_INFO_ALL_ACTIONS",
        resolve_toggle(o.enable_all_actions, o.disable_all_actions),
    );
    // All-configurations is read by `should_run` as an OR of the canonical key
    // and the legacy `BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS` alias. When the
    // flag is explicit, write BOTH keys so a stray legacy env var can't defeat
    // an explicit `--disable-all-configurations`.
    if let Some(b) = resolve_toggle(o.enable_all_configurations, o.disable_all_configurations) {
        let tok = if b { "1" } else { "0" }.to_string();
        env.insert(
            "BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS".to_string(),
            tok.clone(),
        );
        env.insert("BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS".to_string(), tok);
    }
    set_bool(
        env,
        "BUGSEE_DEPENDENCIES_ENABLED",
        resolve_toggle(o.enable_dependencies, o.disable_dependencies),
    );
    set_bool(
        env,
        "BUGSEE_BUILD_INFO_TIMINGS_ENABLED",
        resolve_toggle(o.enable_timings, o.disable_timings),
    );
    set_bool(
        env,
        "BUGSEE_SIZE_ANALYSIS_ENABLED",
        resolve_toggle(o.enable_size_analysis, o.disable_size_analysis),
    );
    set_bool(
        env,
        "BUGSEE_CHUNKED_UPLOAD",
        resolve_toggle(o.enable_chunked_upload, o.disable_chunked_upload),
    );
    set_bool(
        env,
        "BUGSEE_SIZE_CHECK_ENABLED",
        resolve_toggle(o.enable_size_check, o.disable_size_check),
    );

    set_num(
        env,
        "BUGSEE_SIZE_CHECK_WARNING_PCT",
        o.size_check_warning_pct,
    );
    set_num(env, "BUGSEE_SIZE_CHECK_FAIL_PCT", o.size_check_fail_pct);
    set_num(
        env,
        "BUGSEE_SIZE_CHECK_WARNING_BYTES",
        o.size_check_warning_bytes,
    );
    set_num(env, "BUGSEE_SIZE_CHECK_FAIL_BYTES", o.size_check_fail_bytes);
}

/// CLI dispatch. The app token comes from the global `--app-token` /
/// `BUGSEE_APP_TOKEN` like the other upload commands.
pub async fn dispatch(
    cmd: XcodeCommand,
    endpoint: Option<String>,
    app_token: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        // `force_foreground` is consumed by `main` (it decides whether to
        // daemonize BEFORE the async runtime starts); by the time dispatch runs
        // the decision is already made, so it's irrelevant here. The `overrides`
        // are overlaid onto the env map so every downstream gate sees them as if
        // they were environment variables (CLI flag wins over a real env var).
        XcodeCommand::PostAction { overrides, .. } => {
            let mut env: HashMap<String, String> = std::env::vars().collect();
            apply_overrides(&mut env, &overrides);
            let endpoint = endpoint.unwrap_or_else(|| DEFAULT_ENDPOINT.to_string());
            run_post_action(&env, &endpoint, app_token.as_deref()).await
        }
    }
}

// ─── Gating ─────────────────────────────────────────────────────────
//
// Ports the iOS agent's `should_run_build_publish_flow`
// (tools.bundle/BugseeAgent). The decision is split out as a pure function
// over an env map so it is exhaustively unit-testable without a real Xcode
// environment.

/// Outcome of the gate. `Run` carries the resolved `.app` source so the caller
/// doesn't re-derive it; `Skip` carries a human-readable reason for the log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Gate {
    Run,
    Skip(String),
}

/// `True` for missing / unset / empty values (treated as "default on") AND for
/// conventional truthy tokens. Mirrors the agent's
/// `_env_truthy_default_true`: empty string is treated as missing (Xcode's
/// "Add Environment Variable" emits `KEY=""` when the value field is left
/// blank — treating that as "off" would silently disable a user who thought
/// they were accepting the default).
fn env_truthy_default_true(value: Option<&String>) -> bool {
    match value {
        None => true,
        Some(v) if v.trim().is_empty() => true,
        Some(v) => env_truthy(Some(v)),
    }
}

/// `True` when an env-var-style value is a conventional "on" token. Matches
/// the agent's `_env_truthy` and the Android Gradle plugin's set so a single
/// CI config snippet enables the feature on both platforms.
fn env_truthy(value: Option<&String>) -> bool {
    matches!(
        value.map(|v| v.trim().to_ascii_lowercase()).as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

fn trimmed<'a>(env: &'a HashMap<String, String>, key: &str) -> &'a str {
    env.get(key).map(|s| s.trim()).unwrap_or("")
}

/// Gate for the build-info flow. Mirrors `should_run_build_publish_flow`.
///
/// Two entry points: an **Archive** action (`ACTION == install` + a valid
/// `ARCHIVE_PATH`), permitted whenever build-info is enabled; or a **plain
/// Build** action, which requires the user to opt in via
/// `BUGSEE_BUILD_INFO_ALL_ACTIONS` AND have a valid `TARGET_BUILD_DIR`.
/// `BUGSEE_BUILD_INFO_ENABLED` defaults ON; `CONFIGURATION` must be
/// Release-prefixed unless `BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS` (or the
/// legacy `BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS` alias) is set.
pub(crate) fn should_run(env: &HashMap<String, String>) -> Gate {
    // Pre-flight FIRST. If we wouldn't have run anyway (no archive AND no
    // opted-in build-dir source) the misconfiguration warning would just be
    // log noise.
    let action = trimmed(env, "ACTION");
    let archive_path = trimmed(env, "ARCHIVE_PATH");
    let has_archive =
        action == "install" && !archive_path.is_empty() && Path::new(archive_path).is_dir();

    let all_actions_optin = env_truthy(env.get("BUGSEE_BUILD_INFO_ALL_ACTIONS"));
    let target_build_dir = trimmed(env, "TARGET_BUILD_DIR");
    let has_build_dir =
        all_actions_optin && !target_build_dir.is_empty() && Path::new(target_build_dir).is_dir();

    if !has_archive && !has_build_dir {
        let reason = if action != "install" {
            if all_actions_optin {
                "BUGSEE_BUILD_INFO_ALL_ACTIONS is set but TARGET_BUILD_DIR is missing — \
                 cannot locate the .app"
                    .to_string()
            } else {
                format!(
                    "build-info upload requires an Archive action (got ACTION={action:?}); \
                     set BUGSEE_BUILD_INFO_ALL_ACTIONS=1 to also register on plain Build actions"
                )
            }
        } else {
            format!(
                "build-info upload could not locate the .xcarchive (ARCHIVE_PATH={archive_path:?})"
            )
        };
        return Gate::Skip(reason);
    }

    // Now resolve the gating flags — only from this point on, when the user
    // actually has an archive in hand and the misconfiguration is actionable.
    let build_info_enabled = env_truthy_default_true(env.get("BUGSEE_BUILD_INFO_ENABLED"));
    if !build_info_enabled {
        // The agent also warns when BUGSEE_SIZE_ANALYSIS_ENABLED is set while
        // build-info is off. Size-analysis is a Phase-2 concern here, so the
        // warning is deferred — but the disable still gates us out.
        // TODO(phase 2): warn on the size-analysis-without-build-info misconfig.
        return Gate::Skip("BUGSEE_BUILD_INFO_ENABLED is disabled".to_string());
    }

    let config = trimmed(env, "CONFIGURATION");
    // Release-only by default. The legacy
    // `BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS` env var is honoured as an
    // alias during the transition.
    let all_configurations = env_truthy(env.get("BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS"))
        || env_truthy(env.get("BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS"));
    // Case-insensitive "starts-with-release" — custom types like
    // `ReleaseProduction` / `Release-AppStore` are conceptually release
    // builds.
    let config_norm = config.to_ascii_lowercase();
    let is_release = config_norm == "release" || config_norm.starts_with("release");
    if !is_release && !all_configurations {
        return Gate::Skip(format!(
            "build-info upload skipped for configuration {config:?} \
             (set BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS=1 to include non-Release configurations)"
        ));
    }

    Gate::Run
}

// ─── `.app` location ────────────────────────────────────────────────
//
// Ports the agent's `find_app` / `find_app_in_archive` / `find_app_in_build_dir`.

/// Resolve the `.app` produced by the build. Tries the Archive path first
/// (`ARCHIVE_PATH/Products/Applications/*.app`), then the Build-action path
/// (`TARGET_BUILD_DIR/WRAPPER_NAME`, then `EXECUTABLE_FOLDER_PATH`, then a
/// single-`.app` scan of the build dir).
pub(crate) fn find_app(env: &HashMap<String, String>) -> Option<PathBuf> {
    let archive_path = trimmed(env, "ARCHIVE_PATH");
    if !archive_path.is_empty() && Path::new(archive_path).is_dir() {
        return find_app_in_archive(Path::new(archive_path));
    }
    find_app_in_build_dir(env)
}

/// `<archive>/Products/Applications/*.app` — the canonical location. Returns
/// the first `.app` directory (sorted for determinism).
fn find_app_in_archive(archive_path: &Path) -> Option<PathBuf> {
    let apps_dir = archive_path.join("Products").join("Applications");
    if !apps_dir.is_dir() {
        return None;
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(&apps_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().and_then(|x| x.to_str()) == Some("app"))
        .collect();
    entries.sort();
    entries.into_iter().next()
}

/// `$TARGET_BUILD_DIR/$WRAPPER_NAME` (then `EXECUTABLE_FOLDER_PATH`, then a
/// single-`.app` scan). Returns `None` when nothing resolves to an existing
/// `.app` directory.
fn find_app_in_build_dir(env: &HashMap<String, String>) -> Option<PathBuf> {
    let target_build_dir = trimmed(env, "TARGET_BUILD_DIR");
    if target_build_dir.is_empty() || !Path::new(target_build_dir).is_dir() {
        return None;
    }
    let base = Path::new(target_build_dir);

    let wrapper = trimmed(env, "WRAPPER_NAME");
    if !wrapper.is_empty() && wrapper.ends_with(".app") {
        let candidate = base.join(wrapper);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    let exec_folder = trimmed(env, "EXECUTABLE_FOLDER_PATH");
    if !exec_folder.is_empty() && exec_folder.ends_with(".app") {
        let candidate = base.join(exec_folder);
        if candidate.is_dir() {
            return Some(candidate);
        }
    }

    // Last resort: a single `.app` in the build dir (exotic setups where
    // neither env var is populated).
    let mut matches: Vec<PathBuf> = std::fs::read_dir(base)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_dir() && p.extension().and_then(|x| x.to_str()) == Some("app"))
        .collect();
    matches.sort();
    if matches.len() == 1 {
        matches.into_iter().next()
    } else {
        None
    }
}

// ─── Bundle info ────────────────────────────────────────────────────

/// `(package_id, version, build)` read from `<app>/Info.plist`. Mirrors the
/// agent's `resolve_bundle_info_from_app`: any field may be absent. Uses the
/// reusable `build_env::read_plist_to_json` (binary or XML plist).
pub(crate) struct BundleInfo {
    pub package_id: Option<String>,
    pub version: Option<String>,
    pub build: Option<String>,
}

pub(crate) fn resolve_bundle_info(app_path: &Path) -> BundleInfo {
    let plist = app_path.join("Info.plist");
    let map = build_env::read_plist_to_json(&plist);
    let get = |key: &str| -> Option<String> {
        map.get(key)
            .and_then(Value::as_str)
            .map(|s| s.to_string())
            .filter(|s| !s.is_empty())
    };
    BundleInfo {
        package_id: get("CFBundleIdentifier"),
        version: get("CFBundleShortVersionString"),
        build: get("CFBundleVersion"),
    }
}

// ─── Registration payload ───────────────────────────────────────────
//
// Built field-for-field from `run_size_analysis_flow`'s `/builds` body
// (Phase-1 subset — no artefact, no timings, no size-check fields). The
// worker/appserver pin this shape, so the field names/structure must match.

/// Inline `dependencies_summary` block. Mirrors the agent's `_deps_summary`:
/// `total` / `direct` / `transitive` / `by_type` / `truncated` /
/// `collected_at` (ISO-8601 Z) / `collection_config`.
fn deps_summary(
    entries: &[ios_deps::DepEntry],
    truncated: bool,
    scope: &str,
    collected_at: &str,
) -> Value {
    let direct = entries.iter().filter(|e| e.direct).count();
    let library = entries.iter().filter(|e| e.type_ == "library").count();
    json!({
        "total": entries.len(),
        "direct": direct,
        "transitive": entries.len() - direct,
        "by_type": { "library": library, "project": 0, "file": 0 },
        "truncated": truncated,
        "collected_at": collected_at,
        "collection_config": collection_config(scope),
    })
}

fn collection_config(scope: &str) -> Value {
    json!({
        "scope": scope,
        "include_selected_reason": false,
        "max_count": DEPS_MAX_COUNT,
    })
}

/// The gzip-free deps blob (the agent gzips it; the build-info bundle PUTs the
/// raw `dependencies.json` and lets `build_info::run` zstd the ZIP entry).
/// Shape mirrors the agent's `_deps_blob_gz` inner object.
fn deps_blob(entries: &[ios_deps::DepEntry], truncated: bool, scope: &str) -> Value {
    json!({
        "schema_version": DEPS_SCHEMA_VERSION,
        "truncated": truncated,
        "collection_config": collection_config(scope),
        "dependencies": entries,
    })
}

/// Build the `/v2/apps/<token>/builds` registration body. Phase-1 subset of
/// `run_size_analysis_flow`'s payload. `deps_summary_value` is `Some(..)` only
/// when at least one dep was collected (matches the agent gating
/// `dependencies_summary` on a non-empty collection).
///
/// NOTE on `uuid`: the agent sends the main executable's Mach-O `LC_UUID`
/// (`get_main_executable_uuid`) so the back-end can join `crash.uuid → build`,
/// falling back to a random UUID. Phase 1 does NOT extract the executable
/// UUID here — `dsym::slices` could supply it, but that is a follow-up. We
/// omit `uuid` rather than send a random one (a random uuid provides no
/// crash-join value and would only confuse the dashboard). The build record
/// is still useful via `package_id` + `version` + `build`.
/// TODO(phase 2): extract the main executable LC_UUID and set `uuid`.
#[allow(clippy::too_many_arguments)]
fn build_registration_payload(
    env: &HashMap<String, String>,
    bundle: &BundleInfo,
    vcs: &vcs_metadata::VcsMetadata,
    machine: Option<&str>,
    xcode_version: Option<&str>,
    deps_summary_value: Option<Value>,
    timings_summary: Option<Value>,
    build_uuid: &str,
    artifact_size: u64,
    request_artifact_upload: bool,
) -> Value {
    let mut payload = Map::new();

    // `uuid` — the main executable's Mach-O `LC_UUID` (or a random fallback).
    // The iOS SDK reports the same `LC_UUID` with every crash, so this is the
    // crash↔build join key. Always present so a build record is joinable.
    payload.insert("uuid".into(), Value::String(build_uuid.to_string()));

    // `format` — the artefact format. The agent always sends `"ipa"` (iOS);
    // pin it here too so the back-end groups iOS builds identically.
    payload.insert("format".into(), Value::String("ipa".into()));

    if let Some(v) = &bundle.package_id {
        payload.insert("package_id".into(), Value::String(v.clone()));
    }
    if let Some(v) = &bundle.version {
        payload.insert("version".into(), Value::String(v.clone()));
    }
    if let Some(v) = &bundle.build {
        payload.insert("build".into(), Value::String(v.clone()));
    }

    let config = trimmed(env, "CONFIGURATION");
    if !config.is_empty() {
        payload.insert(
            "build_configuration".into(),
            Value::String(config.to_string()),
        );
    }

    // `artifact_size` — the packaged `.ipa` byte size. The server stores it as
    // the build's size and uses it as the next build's size-check baseline.
    // Omitted when packaging failed (size 0).
    if artifact_size > 0 {
        payload.insert("artifact_size".into(), Value::from(artifact_size));
    }

    // `request_artifact_upload` — whether the server should sign an artefact
    // endpoint. Set from `BUGSEE_SIZE_ANALYSIS_ENABLED` (opt-in). `build::run`
    // re-asserts this same flag, so the two never disagree.
    payload.insert(
        "request_artifact_upload".into(),
        Value::Bool(request_artifact_upload),
    );

    // VCS block — nested under `vcs`, omitted when the resolver produced no
    // fields so the server distinguishes "unknown" cleanly. `VcsMetadata`
    // serializes with `skip_serializing_if = Option::is_none`, so an
    // all-None resolution becomes `{}`; treat that as absent.
    if let Ok(vcs_value) = serde_json::to_value(vcs) {
        if vcs_value
            .as_object()
            .map(|m| !m.is_empty())
            .unwrap_or(false)
        {
            payload.insert("vcs".into(), vcs_value);
        }
    }

    // `build_metadata` sub-object. `plugin_version` is the producer's own
    // version (the CLI here); `build_system_version` is Xcode; `build_sdk_version`
    // is `$SDK_NAME` (e.g. `iphoneos18.5`).
    let mut metadata = Map::new();
    if let Some(m) = machine {
        metadata.insert("machine".into(), Value::String(m.to_string()));
    }
    metadata.insert(
        "plugin_version".into(),
        Value::String(plugin_version().to_string()),
    );
    if let Some(x) = xcode_version {
        metadata.insert("build_system_version".into(), Value::String(x.to_string()));
    }
    let sdk_name = trimmed(env, "SDK_NAME");
    if !sdk_name.is_empty() {
        metadata.insert(
            "build_sdk_version".into(),
            Value::String(sdk_name.to_string()),
        );
    }
    // `build_metadata.timings` — the inline build-timings summary decoded from
    // the `.xcactivitylog` (total_ms, top_tasks, per-category `<bucket>_ms`).
    // Omitted when no log / no timings were extractable.
    if let Some(timings) = timings_summary {
        metadata.insert("timings".into(), timings);
    }
    if !metadata.is_empty() {
        payload.insert("build_metadata".into(), Value::Object(metadata));
    }

    // Dependencies. The agent sets `dependencies_summary` +
    // `request_dependencies_upload` when a blob was produced. The build-info
    // bundle (Phase D) carries the deps as a ZIP entry, so
    // `request_build_info_upload` (injected by `build_info::run`) is the
    // actual transport — but we keep `request_dependencies_upload` set too so
    // the legacy per-blob path stays available for non-flagged orgs during the
    // soak, exactly as the agent does.
    if let Some(summary) = deps_summary_value {
        payload.insert("request_dependencies_upload".into(), Value::Bool(true));
        payload.insert("dependencies_summary".into(), summary);
    }

    Value::Object(payload)
}

/// Producer version recorded as `build_metadata.plugin_version`. The CLI's own
/// crate version — the equivalent of the agent's `resolve_agent_version`.
fn plugin_version() -> &'static str {
    concat!("bugsee-cli/", env!("CARGO_PKG_VERSION"))
}

// ─── Orchestration ──────────────────────────────────────────────────

/// Per-step OUTCOME of a post-action run, emitted as one JSON line on stdout so
/// a delegating bootstrapper (the iOS BugseeAgent) can write its cross-producer
/// handshake manifest accurately — `artifact_uploaded` in particular must
/// reflect whether bytes actually shipped, not mere intent.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct PostActionReport {
    pub build_registered: bool,
    pub artifact_uploaded: bool,
    pub dsym_uploaded: bool,
    pub deps_collected: bool,
    pub timings: bool,
    pub size_analysis: bool,
    /// `"pass"` / `"warn"` / `"fail"` / `"skip"`.
    pub size_check: &'static str,
    /// The `error:`-ready gate line when `size_check == "fail"`. Internal: not
    /// serialized into the stdout report.
    #[serde(skip)]
    pub size_check_fail_line: Option<String>,
}

/// Run the post-action flow. A thin wrapper over [`run_post_action_inner`]: it
/// emits the JSON result report on stdout and maps a size-check FAIL to the
/// terminal `ExitCode::SizeCheckFailed`. Returns `Ok(())` for both the "ran" and
/// the "gated out / soft-failed" cases — a post-action must never fail an
/// already-signed build — except for a hard config error (no app token) or a
/// deliberate size-check FAIL.
async fn run_post_action(
    env: &HashMap<String, String>,
    endpoint: &str,
    app_token: Option<&str>,
) -> anyhow::Result<()> {
    let report = match run_post_action_inner(env, endpoint, app_token).await? {
        Some(r) => r,
        // Gated out / no .app — nothing ran, so no report is emitted.
        None => return Ok(()),
    };

    // Machine-readable result line on STDOUT — the ONLY thing this command writes
    // to stdout (tracing goes to stderr). In the background daemon there is no
    // stdout reader, so it lands in the log harmlessly.
    println!("{}", serde_json::to_string(&report)?);

    // The one place the post-action deliberately fails the build.
    if let Some(line) = report.size_check_fail_line {
        return Err(Error::SizeCheckFailed(line).into());
    }
    Ok(())
}

/// The post-action orchestration. Returns `Ok(Some(report))` after a full run,
/// `Ok(None)` when gated out / no `.app` was found, or `Err` only for a hard
/// config error. Never emits the stdout report or the size-check Err itself —
/// that is the caller's ([`run_post_action`]) job, which keeps this body
/// testable in-process.
async fn run_post_action_inner(
    env: &HashMap<String, String>,
    endpoint: &str,
    app_token: Option<&str>,
) -> anyhow::Result<Option<PostActionReport>> {
    // 1+2. Gate.
    match should_run(env) {
        Gate::Skip(reason) => {
            tracing::info!("Bugsee: {reason}. Skipping build-info upload.");
            return Ok(None);
        }
        Gate::Run => {}
    }

    // App token is required to register the build. This is the one hard
    // config error — surface it so the user fixes their invocation.
    let app_token = app_token.ok_or_else(|| {
        config_invalid(
            "--app-token (or BUGSEE_APP_TOKEN) is required for `xcode post-action` \
             (it registers the build with Bugsee)",
        )
    })?;

    // 3. Locate the `.app`.
    let app_path = match find_app(env) {
        Some(p) => p,
        None => {
            tracing::info!(
                archive_path = trimmed(env, "ARCHIVE_PATH"),
                target_build_dir = trimmed(env, "TARGET_BUILD_DIR"),
                "Bugsee: no .app found. Skipping build-info upload."
            );
            return Ok(None);
        }
    };
    tracing::info!(app = %app_path.display(), "Bugsee: located .app");

    // 4. Read Info.plist.
    let bundle = resolve_bundle_info(&app_path);

    // 5. Resolve provenance. None of these raise — they return None / empty on
    // failure to keep the payload best-effort.
    let working_dir = env
        .get("SRCROOT")
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let vcs = vcs_metadata::resolve(env, &working_dir);
    let machine = build_env::resolve_machine_label(env);
    let xcode_version = build_env::resolve_xcode_version();

    // A temp dir holds the payload + the deps/timings sidecars + the packaged
    // .ipa for the build registration.
    let tmpdir = tempfile::tempdir()?;

    // 6. Collect deps — opt-out via `BUGSEE_DEPENDENCIES_ENABLED` (default on),
    // mirroring the agent's gate so privacy-conscious shops can disable it.
    // Stages dependencies.json (RAW compact JSON; `build::run` zstd-compresses it
    // into the build-info bundle, and the worker re-gzips each entry internally).
    let mut deps_path: Option<PathBuf> = None;
    let mut deps_summary_value: Option<Value> = None;
    if env_truthy_default_true(env.get("BUGSEE_DEPENDENCIES_ENABLED")) {
        // Project root from PROJECT_DIR / SRCROOT / SOURCE_ROOT, mirroring the
        // agent's `_collect_dependencies_impl`.
        let deps_root = env
            .get("PROJECT_DIR")
            .or_else(|| env.get("SRCROOT"))
            .or_else(|| env.get("SOURCE_ROOT"))
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
        // Resolve the product binary so the vendored-framework scan runs (the
        // agent passes `--product-binary`). Best-effort: only the Build-dir path
        // exposes `EXECUTABLE_NAME`.
        let product_binary = resolve_product_binary(env);
        let collected = ios_deps::collect(&deps_root, product_binary.as_deref(), DEPS_MAX_COUNT);
        if !collected.entries.is_empty() {
            let scope = if collected.scope_label.is_empty() {
                DEPS_COLLECTION_SCOPE
            } else {
                collected.scope_label.as_str()
            };
            let collected_at = iso8601_now();
            let summary = deps_summary(
                &collected.entries,
                collected.truncated,
                scope,
                &collected_at,
            );
            let blob = deps_blob(&collected.entries, collected.truncated, scope);
            let path = tmpdir.path().join("dependencies.json");
            // Compact JSON (no whitespace) — matches the agent's `_deps_blob_gz`
            // `separators=(',', ':')`.
            std::fs::write(&path, serde_json::to_vec(&blob)?)?;
            deps_path = Some(path);
            deps_summary_value = Some(summary);
        }
    } else {
        tracing::info!("Bugsee: dependency collection disabled (BUGSEE_DEPENDENCIES_ENABLED).");
    }

    // 6b. Decode build timings from the newest `.xcactivitylog` ($OBJROOT →
    // DerivedData Logs/Build) — opt-out via `BUGSEE_BUILD_INFO_TIMINGS_ENABLED`
    // (default on), mirroring the agent's gate. The inline summary rides in
    // `build_metadata.timings`; the per-target Gantt DETAIL blob becomes a RAW
    // `timings.json` sidecar. Never fails — a malformed log degrades to no
    // timings.
    let timings = if env_truthy_default_true(env.get("BUGSEE_BUILD_INFO_TIMINGS_ENABLED")) {
        xcactivitylog::resolve(env)
    } else {
        tracing::info!("Bugsee: build-timings disabled (BUGSEE_BUILD_INFO_TIMINGS_ENABLED).");
        xcactivitylog::BuildTimings::default()
    };
    let timings_summary = timings.summary;
    let mut timings_path: Option<PathBuf> = None;
    if let Some(timeline) = &timings.timeline {
        let path = tmpdir.path().join("timings.json");
        match serde_json::to_vec(timeline) {
            Ok(bytes) => match std::fs::write(&path, bytes) {
                Ok(()) => timings_path = Some(path),
                Err(e) => {
                    tracing::warn!(error = %e, "Bugsee: failed to stage timings.json (continuing)");
                }
            },
            Err(e) => {
                tracing::warn!(error = %e, "Bugsee: failed to serialize timings blob (continuing)");
            }
        }
    }

    // 6c. Package the `.app` into a synthetic `.ipa` (always — the size feeds
    // the build record + the size-check baseline). Size-analysis (opt-in)
    // controls whether the bytes are also SHIPPED; packaging happens regardless.
    let size_analysis_enabled = env_truthy(env.get("BUGSEE_SIZE_ANALYSIS_ENABLED"));
    let ipa_path = tmpdir
        .path()
        .join(format!("{}.ipa", ipa_stem(&app_path, &bundle)));
    let (artifact_size, packaged) = match xcode_ipa::package_app_as_ipa(&app_path, &ipa_path) {
        Ok(()) => {
            let size = std::fs::metadata(&ipa_path).map(|m| m.len()).unwrap_or(0);
            tracing::info!(artifact_size = size, "Bugsee: packaged synthetic .ipa");
            (size, true)
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                size_analysis_requested = size_analysis_enabled,
                "Bugsee: .ipa packaging failed — registering the build without an \
                 artefact (bytes will NOT ship even if size-analysis was requested)"
            );
            (0, false)
        }
    };
    // Only ship bytes when size-analysis is on AND we actually packaged.
    let request_artifact_upload = size_analysis_enabled && packaged;
    // Chunked artefact transport is opt-in and only meaningful when shipping.
    let chunked = request_artifact_upload && env_truthy(env.get("BUGSEE_CHUNKED_UPLOAD"));

    // Main-executable `LC_UUID` — the crash↔build join key. A random fallback
    // guarantees the build record always carries SOME uuid (just not joinable).
    let build_uuid = xcode_ipa::main_executable_uuid(&app_path)
        .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());

    // 6d. Fetch the size-check baseline BEFORE registering — the lookup must not
    // pick up the in-flight build as its own baseline. Returns None for every
    // skip condition (master switch off, no thresholds, no package_id, first
    // build, infra error); the actual comparison runs LAST (after dSYMs) so a
    // size FAIL never blocks symbol upload.
    let config = trimmed(env, "CONFIGURATION");
    let size_check_prep = size_check::prepare(
        env,
        endpoint,
        app_token,
        bundle.package_id.as_deref(),
        config,
    )
    .await;

    // 7. Register the build (always) and — when size-analysis is on — ship the
    // artefact. The build-info bundle (deps/timings) rides the SAME
    // registration. `build::run` registers even with no sidecars and no
    // artefact, so a build record ALWAYS lands (closing the no-deps gap).
    let payload = build_registration_payload(
        env,
        &bundle,
        &vcs,
        machine.as_deref(),
        xcode_version.as_deref(),
        deps_summary_value,
        timings_summary,
        &build_uuid,
        artifact_size,
        request_artifact_upload,
    );
    let payload_path = tmpdir.path().join("payload.json");
    std::fs::write(&payload_path, serde_json::to_vec(&payload)?)?;

    let params = build::Params {
        endpoint,
        app_token,
        payload_json: &payload_path,
        artifact: &ipa_path,
        request_artifact_upload,
        mapping: None,
        deps: deps_path.as_deref(),
        timings: timings_path.as_deref(),
        strategy: Strategy::default(),
        chunked,
        dry_run: false,
        out: None,
    };
    // Soft-fail: a single upload hiccup must not fail the build. Log and
    // continue to the dSYM step, exactly as the agent does. `build_registered`
    // tracks the OUTCOME (did the POST + any artefact PUT succeed) for the
    // result report.
    let build_registered = match build::run(params, RetryPolicy::default()).await {
        Ok(outcome) => {
            tracing::info!(
                ?outcome,
                request_artifact_upload,
                "Bugsee: build registered"
            );
            true
        }
        Err(e) => {
            tracing::warn!(error = %e, "Bugsee: build registration/upload failed (continuing)");
            false
        }
    };
    // Bytes actually shipped only when we requested it AND the run succeeded.
    let artifact_uploaded = request_artifact_upload && build_registered;
    let deps_collected = deps_path.is_some();
    let timings_collected = timings_path.is_some();

    // 8. Discover + upload dSYMs. The folder is `$DWARF_DSYM_FOLDER_PATH`
    // (every Run-Script env carries it) or, for an archive, `<archive>/dSYMs`.
    let dsym_uploaded = upload_dsyms(env, endpoint, app_token, &bundle).await;

    // 9. In-build size-check evaluation (LAST, so a FAIL doesn't skip any
    // upload). PASS/WARN print a line and continue; FAIL is recorded on the
    // report and turned into `ExitCode::SizeCheckFailed` by the caller — the one
    // place the post-action deliberately fails the build. Skipped when packaging
    // failed (no real local size to compare).
    let mut size_check = "skip";
    let mut size_check_fail_line: Option<String> = None;
    if let Some(prep) = size_check_prep.filter(|_| packaged) {
        let (verdict, line) = prep.decide(artifact_size);
        match verdict {
            size_check::Verdict::Pass => {
                size_check = "pass";
                eprintln!("{line}"); // stderr → visible in Xcode's Run-Script log
            }
            size_check::Verdict::Warn => {
                size_check = "warn";
                eprintln!("{line}");
            }
            size_check::Verdict::Fail => {
                size_check = "fail";
                size_check_fail_line = Some(line);
            }
        }
    }

    Ok(Some(PostActionReport {
        build_registered,
        artifact_uploaded,
        dsym_uploaded,
        deps_collected,
        timings: timings_collected,
        size_analysis: size_analysis_enabled,
        size_check,
        size_check_fail_line,
    }))
}

/// A filesystem-safe `.ipa` filename stem: the bundle id (reverse-DNS) when
/// present, else the `.app`'s basename. Any char outside `[A-Za-z0-9._-]`
/// becomes `_` so a malformed Info.plist can't smuggle path-traversal into the
/// temp filename. Mirrors the agent's `safe_stem` derivation.
fn ipa_stem(app_path: &Path, bundle: &BundleInfo) -> String {
    let raw = bundle
        .package_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| {
            app_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    // Drop any directory component first (basename), then whitelist-sanitize.
    let base = Path::new(&raw)
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or(raw);
    let sanitized: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "build".to_string()
    } else {
        sanitized
    }
}

/// Resolve the linked product binary (`<app>/EXECUTABLE_NAME`) for the
/// vendored-framework dep scan. Only the Build-dir path exposes a usable
/// `EXECUTABLE_NAME` next to the `.app`; returns `None` otherwise.
fn resolve_product_binary(env: &HashMap<String, String>) -> Option<PathBuf> {
    let app_path = find_app_in_build_dir(env)?;
    let exec_name = trimmed(env, "EXECUTABLE_NAME");
    if exec_name.is_empty() {
        return None;
    }
    let candidate = app_path.join(exec_name);
    candidate.is_file().then_some(candidate)
}

/// Discover the dSYM folder and run the reusable upload path. Soft-fails: a
/// missing folder or an upload error logs and returns `false` — never fails the
/// build. Returns `true` only when the upload completed (used by the result
/// report a delegating bootstrapper reads).
async fn upload_dsyms(
    env: &HashMap<String, String>,
    endpoint: &str,
    app_token: &str,
    bundle: &BundleInfo,
) -> bool {
    let folder = match resolve_dsym_folder(env) {
        Some(f) => f,
        None => {
            tracing::info!("Bugsee: no dSYM folder found (DWARF_DSYM_FOLDER_PATH / archive dSYMs). Skipping dSYM upload.");
            return false;
        }
    };
    tracing::info!(folder = %folder.display(), "Bugsee: uploading dSYMs");

    // `run_dsym_upload` wants version/build strings. The dSYM presigned
    // metadata records them; fall back to empty (the server tolerates it) when
    // the plist lacked them.
    let version = bundle.version.as_deref().unwrap_or("");
    let build = bundle.build.as_deref().unwrap_or("");

    match debug_files::run_dsym_upload(
        std::slice::from_ref(&folder),
        endpoint,
        app_token,
        version,
        build,
        Strategy::default(),
        /* force */ false,
        /* dry_run */ false,
    )
    .await
    {
        Ok(()) => {
            tracing::info!("Bugsee: dSYM upload complete");
            true
        }
        Err(e) => {
            // Includes the "no .dSYM bundles found" case — a soft skip here,
            // not a build failure.
            tracing::warn!(error = %e, "Bugsee: dSYM upload skipped/failed (continuing)");
            false
        }
    }
}

/// Resolve the dSYM folder to scan. Prefers `$DWARF_DSYM_FOLDER_PATH` (set in
/// every Run-Script env); falls back to `<archive>/dSYMs` for an archive
/// action. Returns `None` when neither resolves to an existing directory.
fn resolve_dsym_folder(env: &HashMap<String, String>) -> Option<PathBuf> {
    let explicit = trimmed(env, "DWARF_DSYM_FOLDER_PATH");
    if !explicit.is_empty() {
        let p = PathBuf::from(explicit);
        if p.is_dir() {
            return Some(p);
        }
    }
    let archive_path = trimmed(env, "ARCHIVE_PATH");
    if !archive_path.is_empty() {
        let p = Path::new(archive_path).join("dSYMs");
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// `time::strftime('%Y-%m-%dT%H:%M:%SZ')` equivalent — UTC, second precision,
/// no fractional seconds (matches the agent's `_deps_summary` `collected_at`).
fn iso8601_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_iso8601_utc(secs)
}

/// Format a Unix timestamp (seconds) as `YYYY-MM-DDTHH:MM:SSZ` in UTC.
/// Pure integer date math (civil-from-days) — no chrono dependency, and
/// deterministic for tests.
fn format_iso8601_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let secs_of_day = unix_secs % 86_400;
    let hour = secs_of_day / 3_600;
    let minute = (secs_of_day % 3_600) / 60;
    let second = secs_of_day % 60;

    // Howard Hinnant's civil_from_days algorithm.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, minute, second
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use wiremock::matchers::{body_partial_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use zip::ZipArchive;

    fn env_of(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    // ── CLI override flags ─────────────────────────────────────────

    /// Parse a `bugsee-cli xcode post-action <extra...>` invocation through the
    /// real top-level `Cli` and return its `PostActionOverrides`. Exercises the
    /// actual clap wiring (`overrides_with` last-wins, value parsing) rather
    /// than constructing the struct by hand.
    fn parse_overrides(extra: &[&str]) -> PostActionOverrides {
        use clap::Parser;
        let mut args: Vec<&str> = vec!["bugsee-cli", "xcode", "post-action"];
        args.extend_from_slice(extra);
        let cli = crate::cli::Cli::try_parse_from(args).expect("args should parse");
        match cli.command {
            crate::cli::Command::Xcode(XcodeCommand::PostAction { overrides, .. }) => overrides,
            other => panic!("expected post-action, got {other:?}"),
        }
    }

    #[test]
    fn resolve_toggle_is_tristate() {
        assert_eq!(resolve_toggle(true, false), Some(true));
        assert_eq!(resolve_toggle(false, true), Some(false));
        assert_eq!(resolve_toggle(false, false), None);
        // `overrides_with` prevents both-true at the clap layer; if it ever
        // occurred, enable wins (documented behaviour).
        assert_eq!(resolve_toggle(true, true), Some(true));
    }

    #[test]
    fn enable_disable_pair_resolves_and_last_one_wins() {
        let only_enable = parse_overrides(&["--enable-size-check"]);
        assert_eq!(
            resolve_toggle(
                only_enable.enable_size_check,
                only_enable.disable_size_check
            ),
            Some(true),
        );

        let only_disable = parse_overrides(&["--disable-size-check"]);
        assert_eq!(
            resolve_toggle(
                only_disable.enable_size_check,
                only_disable.disable_size_check
            ),
            Some(false),
        );

        // Both passed — clap's reciprocal `overrides_with` makes the LAST one win.
        let disable_last = parse_overrides(&["--enable-size-check", "--disable-size-check"]);
        assert_eq!(
            resolve_toggle(
                disable_last.enable_size_check,
                disable_last.disable_size_check
            ),
            Some(false),
            "disable passed last must win",
        );
        let enable_last = parse_overrides(&["--disable-size-check", "--enable-size-check"]);
        assert_eq!(
            resolve_toggle(
                enable_last.enable_size_check,
                enable_last.disable_size_check
            ),
            Some(true),
            "enable passed last must win",
        );

        // Neither → fall through to env/default.
        let none = parse_overrides(&[]);
        assert_eq!(
            resolve_toggle(none.enable_size_check, none.disable_size_check),
            None,
        );
    }

    #[test]
    fn apply_overrides_writes_canonical_bool_tokens() {
        // A default-off knob enabled + a default-on knob disabled.
        let o = parse_overrides(&["--enable-size-check", "--disable-build-info"]);
        let mut env = env_of(&[]);
        apply_overrides(&mut env, &o);
        assert_eq!(
            env.get("BUGSEE_SIZE_CHECK_ENABLED").map(String::as_str),
            Some("1"),
        );
        assert_eq!(
            env.get("BUGSEE_BUILD_INFO_ENABLED").map(String::as_str),
            Some("0"),
        );
        // A knob with no flag passed is never written.
        assert!(!env.contains_key("BUGSEE_DEPENDENCIES_ENABLED"));
        // The canonical tokens parse the way the gate logic expects: the
        // default-on parser must now read the disabled knob as false.
        assert!(!env_truthy_default_true(
            env.get("BUGSEE_BUILD_INFO_ENABLED")
        ));
        assert!(env_truthy(env.get("BUGSEE_SIZE_CHECK_ENABLED")));
    }

    #[test]
    fn every_toggle_flag_writes_its_exact_canonical_key_and_token() {
        // Regression guard for the FULL flag→env-key→token mapping. The keys are
        // inline string literals in both `apply_overrides` and each consumer, so
        // a typo on one side is only caught by a test that pins the exact string
        // `apply_overrides` writes. Each row asserts `--enable-X` writes "1" and
        // `--disable-X` writes "0" to the documented key — a typo or a "1"/"0"
        // swap on ANY knob fails this test loudly.
        let cases: &[(&str, &str, &str)] = &[
            (
                "--enable-build-info",
                "--disable-build-info",
                "BUGSEE_BUILD_INFO_ENABLED",
            ),
            (
                "--enable-all-actions",
                "--disable-all-actions",
                "BUGSEE_BUILD_INFO_ALL_ACTIONS",
            ),
            (
                "--enable-all-configurations",
                "--disable-all-configurations",
                "BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS",
            ),
            (
                "--enable-dependencies",
                "--disable-dependencies",
                "BUGSEE_DEPENDENCIES_ENABLED",
            ),
            (
                "--enable-timings",
                "--disable-timings",
                "BUGSEE_BUILD_INFO_TIMINGS_ENABLED",
            ),
            (
                "--enable-size-analysis",
                "--disable-size-analysis",
                "BUGSEE_SIZE_ANALYSIS_ENABLED",
            ),
            (
                "--enable-chunked-upload",
                "--disable-chunked-upload",
                "BUGSEE_CHUNKED_UPLOAD",
            ),
            (
                "--enable-size-check",
                "--disable-size-check",
                "BUGSEE_SIZE_CHECK_ENABLED",
            ),
        ];
        for (enable, disable, key) in cases {
            let mut env = env_of(&[]);
            apply_overrides(&mut env, &parse_overrides(&[enable]));
            assert_eq!(
                env.get(*key).map(String::as_str),
                Some("1"),
                "{enable} must write {key}=1",
            );

            let mut env = env_of(&[]);
            apply_overrides(&mut env, &parse_overrides(&[disable]));
            assert_eq!(
                env.get(*key).map(String::as_str),
                Some("0"),
                "{disable} must write {key}=0",
            );
        }
    }

    #[test]
    fn cli_flag_overrides_pre_existing_env_var() {
        // Real env says enabled; the --disable flag must win.
        let o = parse_overrides(&["--disable-size-check"]);
        let mut env = env_of(&[("BUGSEE_SIZE_CHECK_ENABLED", "1")]);
        apply_overrides(&mut env, &o);
        assert_eq!(
            env.get("BUGSEE_SIZE_CHECK_ENABLED").map(String::as_str),
            Some("0"),
        );
        assert!(!env_truthy(env.get("BUGSEE_SIZE_CHECK_ENABLED")));
    }

    #[test]
    fn all_configurations_flag_writes_both_canonical_and_legacy_keys() {
        // Disabling must defeat a stray legacy-alias env var (should_run ORs
        // the two keys), so the explicit flag has to clear BOTH.
        let o = parse_overrides(&["--disable-all-configurations"]);
        let mut env = env_of(&[("BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS", "1")]);
        apply_overrides(&mut env, &o);
        assert_eq!(
            env.get("BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS")
                .map(String::as_str),
            Some("0"),
        );
        assert_eq!(
            env.get("BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS")
                .map(String::as_str),
            Some("0"),
        );
        // The OR the gate computes is now false.
        assert!(
            !(env_truthy(env.get("BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS"))
                || env_truthy(env.get("BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS"))),
        );

        // Enabling sets both to "1".
        let o = parse_overrides(&["--enable-all-configurations"]);
        let mut env = env_of(&[]);
        apply_overrides(&mut env, &o);
        assert_eq!(
            env.get("BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS")
                .map(String::as_str),
            Some("1"),
        );
        assert_eq!(
            env.get("BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS")
                .map(String::as_str),
            Some("1"),
        );
    }

    #[test]
    fn numeric_thresholds_stringify_and_reparse_through_size_check() {
        // All four thresholds at once, each a DISTINCT value, so a key swap (e.g.
        // a warning value written to a fail key) surfaces as a wrong resolved
        // field — not just a missing one.
        let o = parse_overrides(&[
            "--size-check-warning-pct",
            "5",
            "--size-check-fail-pct",
            "12.5",
            "--size-check-warning-bytes",
            "1000",
            "--size-check-fail-bytes",
            "1048576",
        ]);
        let mut env = env_of(&[]);
        apply_overrides(&mut env, &o);
        // Exact env-key strings written by apply_overrides.
        assert_eq!(
            env.get("BUGSEE_SIZE_CHECK_WARNING_PCT").map(String::as_str),
            Some("5"),
        );
        assert_eq!(
            env.get("BUGSEE_SIZE_CHECK_FAIL_PCT").map(String::as_str),
            Some("12.5"),
        );
        assert_eq!(
            env.get("BUGSEE_SIZE_CHECK_WARNING_BYTES")
                .map(String::as_str),
            Some("1000"),
        );
        assert_eq!(
            env.get("BUGSEE_SIZE_CHECK_FAIL_BYTES").map(String::as_str),
            Some("1048576"),
        );
        // And all four round-trip through the real threshold parser, each to its
        // own field with its own value.
        let t = crate::cli::size_check::resolve_thresholds(&env);
        assert_eq!(t.warning_pct, Some(5.0));
        assert_eq!(t.fail_pct, Some(12.5));
        assert_eq!(t.warning_bytes, Some(1_000));
        assert_eq!(t.fail_bytes, Some(1_048_576));
    }

    #[test]
    fn zero_numeric_flag_disables_its_gate_like_env() {
        // Parity with the env path: a 0 threshold disables that gate
        // (parse_pos_* rejects `<= 0`) rather than erroring.
        let o = parse_overrides(&["--size-check-fail-pct", "0", "--size-check-fail-bytes", "0"]);
        let mut env = env_of(&[]);
        apply_overrides(&mut env, &o);
        let t = crate::cli::size_check::resolve_thresholds(&env);
        assert_eq!(t.fail_pct, None);
        assert_eq!(t.fail_bytes, None);
    }

    #[test]
    fn negative_numeric_flag_is_rejected_at_the_cli_layer() {
        // The stringly-typed env path treats a `<= 0` threshold as "disable".
        // On the CLI a negative value is a hard parse error instead — clap will
        // not accept a hyphen-led token as a numeric option value — which is
        // friendlier than silently disabling the gate on a typo.
        use clap::Parser;
        let r = crate::cli::Cli::try_parse_from([
            "bugsee-cli",
            "xcode",
            "post-action",
            "--size-check-fail-bytes",
            "-1",
        ]);
        assert!(r.is_err(), "negative byte threshold must be rejected");
    }

    #[test]
    fn no_flags_is_a_pure_noop_on_the_env_map() {
        let o = parse_overrides(&[]);
        let mut env = env_of(&[
            ("BUGSEE_BUILD_INFO_ENABLED", "weird-value"),
            ("ACTION", "install"),
        ]);
        let before = env.clone();
        apply_overrides(&mut env, &o);
        assert_eq!(env, before, "no flag passed → env map untouched");
    }

    #[test]
    fn disable_build_info_flag_gates_out_should_run() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let base = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
        ]);
        // Without the flag this is a Run.
        assert_eq!(should_run(&base), Gate::Run);
        // The --disable-build-info flag overlays "0" → Skip.
        let mut env = base.clone();
        apply_overrides(&mut env, &parse_overrides(&["--disable-build-info"]));
        assert!(matches!(should_run(&env), Gate::Skip(_)));
    }

    #[test]
    fn enable_all_configurations_flag_admits_a_debug_build() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let base = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Debug"),
        ]);
        // Debug is Release-only-gated out by default.
        assert!(matches!(should_run(&base), Gate::Skip(_)));
        // The flag lifts the Release-only restriction.
        let mut env = base.clone();
        apply_overrides(&mut env, &parse_overrides(&["--enable-all-configurations"]));
        assert_eq!(should_run(&env), Gate::Run);
    }

    // ── Gating ─────────────────────────────────────────────────────

    /// An Archive action against a real dir, Release config → Run.
    #[test]
    fn gate_release_archive_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
        ]);
        assert_eq!(should_run(&env), Gate::Run);
    }

    /// `Release-AppStore` and lowercase `release` are Release-prefixed → Run.
    #[test]
    fn gate_release_prefixed_configs_run() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        for cfg in ["Release-AppStore", "ReleaseProduction", "release"] {
            let env = env_of(&[
                ("ACTION", "install"),
                ("ARCHIVE_PATH", archive.to_str().unwrap()),
                ("CONFIGURATION", cfg),
            ]);
            assert_eq!(should_run(&env), Gate::Run, "config {cfg} should run");
        }
    }

    /// Debug config without the all-configs opt-in → Skip.
    #[test]
    fn gate_debug_skipped_without_all_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Debug"),
        ]);
        match should_run(&env) {
            Gate::Skip(r) => assert!(r.contains("Debug"), "reason should name the config: {r}"),
            Gate::Run => panic!("Debug must be skipped without all-configs opt-in"),
        }
    }

    /// Debug config WITH the all-configs opt-in → Run.
    #[test]
    fn gate_debug_runs_with_all_configs() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Debug"),
            ("BUGSEE_BUILD_INFO_ALL_CONFIGURATIONS", "1"),
        ]);
        assert_eq!(should_run(&env), Gate::Run);
    }

    /// Legacy `BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS` alias also admits Debug.
    #[test]
    fn gate_debug_runs_with_legacy_all_configs_alias() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Debug"),
            ("BUGSEE_SIZE_ANALYSIS_ALL_CONFIGURATIONS", "yes"),
        ]);
        assert_eq!(should_run(&env), Gate::Run);
    }

    /// build-info explicitly disabled → Skip even on a Release archive.
    #[test]
    fn gate_build_info_disabled_skips() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        for disable in ["0", "false", "no", "off"] {
            let env = env_of(&[
                ("ACTION", "install"),
                ("ARCHIVE_PATH", archive.to_str().unwrap()),
                ("CONFIGURATION", "Release"),
                ("BUGSEE_BUILD_INFO_ENABLED", disable),
            ]);
            match should_run(&env) {
                Gate::Skip(r) => assert!(r.contains("BUGSEE_BUILD_INFO_ENABLED")),
                Gate::Run => panic!("{disable:?} should disable build-info"),
            }
        }
    }

    /// Empty `BUGSEE_BUILD_INFO_ENABLED` is treated as "default on" → Run.
    #[test]
    fn gate_empty_build_info_enabled_is_default_on() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("BUGSEE_BUILD_INFO_ENABLED", ""),
        ]);
        assert_eq!(should_run(&env), Gate::Run);
    }

    /// No archive and not opted into all-actions → Skip (needs-archive reason).
    #[test]
    fn gate_no_archive_no_optin_skips() {
        let env = env_of(&[("ACTION", "build"), ("CONFIGURATION", "Release")]);
        match should_run(&env) {
            Gate::Skip(r) => assert!(r.contains("Archive"), "reason: {r}"),
            Gate::Run => panic!("plain build without opt-in must skip"),
        }
    }

    /// `install` action but `ARCHIVE_PATH` points nowhere → Skip
    /// (could-not-locate reason).
    #[test]
    fn gate_install_with_missing_archive_skips() {
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", "/no/such/archive.xcarchive"),
            ("CONFIGURATION", "Release"),
        ]);
        match should_run(&env) {
            Gate::Skip(r) => assert!(r.contains("xcarchive"), "reason: {r}"),
            Gate::Run => panic!("missing archive must skip"),
        }
    }

    /// Plain Build opted into all-actions with a real TARGET_BUILD_DIR → Run.
    #[test]
    fn gate_all_actions_with_build_dir_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[
            ("ACTION", "build"),
            ("TARGET_BUILD_DIR", tmp.path().to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("BUGSEE_BUILD_INFO_ALL_ACTIONS", "1"),
        ]);
        assert_eq!(should_run(&env), Gate::Run);
    }

    /// All-actions opt-in but TARGET_BUILD_DIR missing → Skip with the
    /// opt-in-specific reason.
    #[test]
    fn gate_all_actions_missing_build_dir_skips() {
        let env = env_of(&[
            ("ACTION", "build"),
            ("CONFIGURATION", "Release"),
            ("BUGSEE_BUILD_INFO_ALL_ACTIONS", "1"),
        ]);
        match should_run(&env) {
            Gate::Skip(r) => assert!(r.contains("TARGET_BUILD_DIR"), "reason: {r}"),
            Gate::Run => panic!("opt-in without build dir must skip"),
        }
    }

    // ── .app location ──────────────────────────────────────────────

    /// Archive layout: `<archive>/Products/Applications/MyApp.app`.
    #[test]
    fn find_app_in_archive_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        let app = archive
            .join("Products")
            .join("Applications")
            .join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        let env = env_of(&[("ARCHIVE_PATH", archive.to_str().unwrap())]);
        assert_eq!(find_app(&env), Some(app));
    }

    /// Build-dir layout: `$TARGET_BUILD_DIR/$WRAPPER_NAME`.
    #[test]
    fn find_app_in_build_dir_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        let env = env_of(&[
            ("TARGET_BUILD_DIR", tmp.path().to_str().unwrap()),
            ("WRAPPER_NAME", "MyApp.app"),
        ]);
        assert_eq!(find_app(&env), Some(app));
    }

    /// Build-dir single-`.app` scan when WRAPPER_NAME is absent.
    #[test]
    fn find_app_single_app_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("Only.app");
        std::fs::create_dir_all(&app).unwrap();
        let env = env_of(&[("TARGET_BUILD_DIR", tmp.path().to_str().unwrap())]);
        assert_eq!(find_app(&env), Some(app));
    }

    /// No `.app` anywhere → None.
    #[test]
    fn find_app_none_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let env = env_of(&[("TARGET_BUILD_DIR", tmp.path().to_str().unwrap())]);
        assert_eq!(find_app(&env), None);
    }

    /// Two `.app` dirs in the build dir with no WRAPPER_NAME is ambiguous →
    /// None (picking either would risk the wrong app). Guards the
    /// single-`.app`-only scan condition.
    #[test]
    fn find_app_ambiguous_multiple_apps_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("First.app")).unwrap();
        std::fs::create_dir_all(tmp.path().join("Second.app")).unwrap();
        let env = env_of(&[("TARGET_BUILD_DIR", tmp.path().to_str().unwrap())]);
        assert_eq!(find_app(&env), None);
    }

    /// A WRAPPER_NAME that exists as a dir but is NOT `.app`-suffixed must not
    /// be accepted; with a real sibling `.app` present, the single-`.app`
    /// scan still resolves it. Guards the `ends_with(".app")` WRAPPER guard.
    #[test]
    fn find_app_ignores_non_app_wrapper() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("NotAnApp")).unwrap();
        let real = tmp.path().join("Real.app");
        std::fs::create_dir_all(&real).unwrap();
        let env = env_of(&[
            ("TARGET_BUILD_DIR", tmp.path().to_str().unwrap()),
            ("WRAPPER_NAME", "NotAnApp"),
        ]);
        // WRAPPER_NAME is rejected (no `.app`); the single-`.app` scan finds Real.app.
        assert_eq!(find_app(&env), Some(real));
    }

    // ── env_truthy tokens ──────────────────────────────────────────

    /// Every conventional "on" token (incl. `on`, uppercased, padded) is
    /// truthy; everything else is falsy. The all-actions opt-in and the
    /// all-configs flags route through this, so each token must be honoured.
    #[test]
    fn env_truthy_token_set() {
        for t in ["1", "true", "TRUE", "yes", "Yes", "on", "ON", " on "] {
            assert!(env_truthy(Some(&t.to_string())), "{t:?} should be truthy");
        }
        for f in ["0", "false", "no", "off", "", "  ", "enabled", "2"] {
            assert!(!env_truthy(Some(&f.to_string())), "{f:?} should be falsy");
        }
        assert!(!env_truthy(None));
    }

    // ── dSYM folder resolution ─────────────────────────────────────

    /// Explicit DWARF_DSYM_FOLDER_PATH wins when it points at a real dir.
    #[test]
    fn resolve_dsym_folder_prefers_explicit() {
        let tmp = tempfile::tempdir().unwrap();
        let explicit = tmp.path().join("explicit-dsyms");
        std::fs::create_dir_all(&explicit).unwrap();
        let env = env_of(&[("DWARF_DSYM_FOLDER_PATH", explicit.to_str().unwrap())]);
        assert_eq!(resolve_dsym_folder(&env), Some(explicit));
    }

    /// With no explicit var, the archive's `dSYMs` subfolder is used. Guards
    /// the `<archive>/dSYMs` fallback path name.
    #[test]
    fn resolve_dsym_folder_archive_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        let dsyms = archive.join("dSYMs");
        std::fs::create_dir_all(&dsyms).unwrap();
        let env = env_of(&[("ARCHIVE_PATH", archive.to_str().unwrap())]);
        assert_eq!(resolve_dsym_folder(&env), Some(dsyms));
    }

    /// Neither source resolves → None.
    #[test]
    fn resolve_dsym_folder_none() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap(); // no dSYMs subdir
        let env = env_of(&[("ARCHIVE_PATH", archive.to_str().unwrap())]);
        assert_eq!(resolve_dsym_folder(&env), None);
    }

    // ── plist → fields ─────────────────────────────────────────────

    /// Info.plist (XML) maps to package_id / version / build.
    #[test]
    fn resolve_bundle_info_maps_plist_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.example.myapp</string>
  <key>CFBundleShortVersionString</key><string>2.3.4</string>
  <key>CFBundleVersion</key><string>987</string>
</dict>
</plist>"#;
        std::fs::write(app.join("Info.plist"), plist).unwrap();

        let info = resolve_bundle_info(&app);
        assert_eq!(info.package_id.as_deref(), Some("com.example.myapp"));
        assert_eq!(info.version.as_deref(), Some("2.3.4"));
        assert_eq!(info.build.as_deref(), Some("987"));
    }

    /// Missing Info.plist → all None (no panic).
    #[test]
    fn resolve_bundle_info_missing_plist_is_all_none() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        let info = resolve_bundle_info(&app);
        assert!(info.package_id.is_none());
        assert!(info.version.is_none());
        assert!(info.build.is_none());
    }

    /// An empty plist string value normalizes to None (not Some("")), so the
    /// payload omits the field rather than sending a blank one. Guards the
    /// `filter(|s| !s.is_empty())` in resolve_bundle_info.
    #[test]
    fn resolve_bundle_info_empty_value_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let app = tmp.path().join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        let plist = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string></string>
  <key>CFBundleShortVersionString</key><string>1.0</string>
</dict>
</plist>"#;
        std::fs::write(app.join("Info.plist"), plist).unwrap();
        let info = resolve_bundle_info(&app);
        assert!(
            info.package_id.is_none(),
            "empty CFBundleIdentifier must be None, got {:?}",
            info.package_id
        );
        assert_eq!(info.version.as_deref(), Some("1.0"));
        assert!(info.build.is_none());
    }

    // ── payload builder ────────────────────────────────────────────

    fn dep(id: &str, direct: bool) -> ios_deps::DepEntry {
        ios_deps::DepEntry {
            id: id.to_string(),
            group: String::new(),
            name: id.to_string(),
            version: Some("1.0".into()),
            direct,
            scope: None,
            type_: "library".into(),
            parents: Vec::new(),
            url: None,
        }
    }

    /// The registration payload carries every Phase-1 field with the
    /// agent-pinned shape: package_id/version/build/build_configuration/format,
    /// nested vcs, build_metadata.{machine,plugin_version,build_system_version,
    /// build_sdk_version}, request_artifact_upload=false, and the deps trio.
    #[test]
    fn payload_builder_shape_matches_agent() {
        let env = env_of(&[("CONFIGURATION", "Release"), ("SDK_NAME", "iphoneos18.5")]);
        let bundle = BundleInfo {
            package_id: Some("com.example.app".into()),
            version: Some("1.2.3".into()),
            build: Some("42".into()),
        };
        let vcs = vcs_metadata::VcsMetadata {
            provider: Some("github"),
            commit_sha: Some("deadbeef".into()),
            branch: Some("main".into()),
            ..Default::default()
        };
        // Asymmetric direct/transitive split (3 direct, 1 transitive) so a
        // swap of the two summary fields is observable.
        let entries = vec![
            dep("library::Alamofire", true),
            dep("library::SnapKit", true),
            dep("library::Kingfisher", true),
            dep("library::SwiftyJSON", false),
        ];
        let summary = deps_summary(&entries, false, "all", "2026-06-16T00:00:00Z");

        let timings = serde_json::json!({
            "total_ms": 12345,
            "native_ms": 9000,
            "top_tasks": [{ "name": "Alpha", "duration_ms": 4000 }],
        });
        let payload = build_registration_payload(
            &env,
            &bundle,
            &vcs,
            Some("github-actions:runner-1"),
            Some("16.2"),
            Some(summary),
            Some(timings),
            "0123456789abcdef0123456789abcdef",
            4096,
            true,
        );
        let obj = payload.as_object().unwrap();

        assert_eq!(obj.get("format").and_then(Value::as_str), Some("ipa"));
        // uuid (crash↔build join key) + artifact_size are present.
        assert_eq!(
            obj.get("uuid").and_then(Value::as_str),
            Some("0123456789abcdef0123456789abcdef")
        );
        assert_eq!(obj.get("artifact_size").and_then(Value::as_u64), Some(4096));
        assert_eq!(
            obj.get("package_id").and_then(Value::as_str),
            Some("com.example.app")
        );
        assert_eq!(obj.get("version").and_then(Value::as_str), Some("1.2.3"));
        assert_eq!(obj.get("build").and_then(Value::as_str), Some("42"));
        assert_eq!(
            obj.get("build_configuration").and_then(Value::as_str),
            Some("Release")
        );
        // request_artifact_upload reflects the flag we passed (size-analysis on).
        assert_eq!(
            obj.get("request_artifact_upload").and_then(Value::as_bool),
            Some(true)
        );
        assert_eq!(
            obj.get("request_dependencies_upload")
                .and_then(Value::as_bool),
            Some(true)
        );

        // Nested vcs.
        let vcs_obj = obj.get("vcs").and_then(Value::as_object).unwrap();
        assert_eq!(
            vcs_obj.get("provider").and_then(Value::as_str),
            Some("github")
        );
        assert_eq!(
            vcs_obj.get("commit_sha").and_then(Value::as_str),
            Some("deadbeef")
        );
        assert_eq!(vcs_obj.get("branch").and_then(Value::as_str), Some("main"));

        // build_metadata.
        let meta = obj
            .get("build_metadata")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(
            meta.get("machine").and_then(Value::as_str),
            Some("github-actions:runner-1")
        );
        assert_eq!(
            meta.get("build_system_version").and_then(Value::as_str),
            Some("16.2")
        );
        assert_eq!(
            meta.get("build_sdk_version").and_then(Value::as_str),
            Some("iphoneos18.5")
        );
        assert!(meta
            .get("plugin_version")
            .and_then(Value::as_str)
            .unwrap()
            .starts_with("bugsee-cli/"));

        // build_metadata.timings — the inline build-timings summary, nested
        // under build_metadata (not at the payload root).
        let timings = meta.get("timings").and_then(Value::as_object).unwrap();
        assert_eq!(timings.get("total_ms").and_then(Value::as_i64), Some(12345));
        assert_eq!(timings.get("native_ms").and_then(Value::as_i64), Some(9000));
        assert_eq!(
            timings
                .get("top_tasks")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );

        // dependencies_summary.
        let dsum = obj
            .get("dependencies_summary")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(dsum.get("total").and_then(Value::as_u64), Some(4));
        // Direct (3) and transitive (1) are distinct so a swap is caught.
        assert_eq!(dsum.get("direct").and_then(Value::as_u64), Some(3));
        assert_eq!(dsum.get("transitive").and_then(Value::as_u64), Some(1));
        assert_eq!(dsum.get("truncated").and_then(Value::as_bool), Some(false));
        // `collected_at` must be the value we passed (not hardcoded / dropped).
        assert_eq!(
            dsum.get("collected_at").and_then(Value::as_str),
            Some("2026-06-16T00:00:00Z")
        );
        let by_type = dsum.get("by_type").and_then(Value::as_object).unwrap();
        assert_eq!(by_type.get("library").and_then(Value::as_u64), Some(4));
        // `project` / `file` are always 0 on iOS — pin them so a stray
        // non-zero (or a swapped key) is caught.
        assert_eq!(by_type.get("project").and_then(Value::as_u64), Some(0));
        assert_eq!(by_type.get("file").and_then(Value::as_u64), Some(0));
        let cc = dsum
            .get("collection_config")
            .and_then(Value::as_object)
            .unwrap();
        assert_eq!(cc.get("scope").and_then(Value::as_str), Some("all"));
        assert_eq!(cc.get("max_count").and_then(Value::as_u64), Some(5000));
        // The worker pins this field; it is always false on iOS.
        assert_eq!(
            cc.get("include_selected_reason").and_then(Value::as_bool),
            Some(false)
        );
    }

    /// An all-None VCS resolution omits the `vcs` key entirely (server
    /// distinguishes "unknown" from "{}").
    #[test]
    fn payload_omits_empty_vcs() {
        let env = env_of(&[("CONFIGURATION", "Release")]);
        let bundle = BundleInfo {
            package_id: Some("com.example.app".into()),
            version: None,
            build: None,
        };
        let vcs = vcs_metadata::VcsMetadata::default();
        let payload =
            build_registration_payload(&env, &bundle, &vcs, None, None, None, None, "ff", 0, false);
        let obj = payload.as_object().unwrap();
        assert!(!obj.contains_key("vcs"), "empty vcs must be omitted");
        // No deps summary passed → keys absent.
        assert!(!obj.contains_key("dependencies_summary"));
        assert!(!obj.contains_key("request_dependencies_upload"));
    }

    // ── ISO-8601 formatter ─────────────────────────────────────────

    #[test]
    fn iso8601_known_timestamps() {
        // 2021-01-01T00:00:00Z = 1609459200
        assert_eq!(format_iso8601_utc(1_609_459_200), "2021-01-01T00:00:00Z");
        // Epoch.
        assert_eq!(format_iso8601_utc(0), "1970-01-01T00:00:00Z");
        // A leap-year date with time-of-day: 2024-02-29T13:37:42Z = 1709213862
        assert_eq!(format_iso8601_utc(1_709_213_862), "2024-02-29T13:37:42Z");
    }

    // ── End-to-end (wiremock) ──────────────────────────────────────

    /// Full `post-action` against a mock server: a Release archive with a
    /// Podfile.lock registers the build (asserting the body) and PUTs the
    /// build-info bundle (asserting the dependencies.json entry). The dSYM
    /// folder is empty so that step no-ops.
    #[tokio::test]
    async fn post_action_registers_and_puts_bundle() {
        let server = MockServer::start().await;

        // Build an Xcode-archive layout with an Info.plist and a Podfile.lock
        // under SRCROOT.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let archive = root.join("App.xcarchive");
        let app = archive
            .join("Products")
            .join("Applications")
            .join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.example.myapp</string>
  <key>CFBundleShortVersionString</key><string>3.0.0</string>
  <key>CFBundleVersion</key><string>300</string>
</dict></plist>"#,
        )
        .unwrap();

        let srcroot = root.join("src");
        std::fs::create_dir_all(&srcroot).unwrap();
        // Minimal Podfile.lock with one dependency.
        std::fs::write(
            srcroot.join("Podfile.lock"),
            "PODS:\n  - Alamofire (5.9.1)\n\nDEPENDENCIES:\n  - Alamofire (= 5.9.1)\n",
        )
        .unwrap();

        // Empty dSYM folder so the dSYM step no-ops.
        let dsym_dir = root.join("dSYMs");
        std::fs::create_dir_all(&dsym_dir).unwrap();

        let put_url = format!("{}/build-info-put", server.uri());

        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .and(body_partial_json(json!({
                "request_build_info_upload": true,
                "format": "ipa",
                "package_id": "com.example.myapp",
                "version": "3.0.0",
                "build": "300",
                "build_configuration": "Release",
                "request_artifact_upload": false,
                "request_dependencies_upload": true,
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "build_id": "b1", "build_info_upload_endpoint": put_url }
            })))
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("PUT"))
            .and(path("/build-info-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("SDK_NAME", "iphoneos18.5"),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
        ]);

        let endpoint = server.uri();
        run_post_action(&env, &endpoint, Some("TKN"))
            .await
            .expect("post-action should succeed");

        // Assert the PUT body is a zstd ZIP with exactly dependencies.json,
        // and that its content is the deps blob with our pod.
        let received = server.received_requests().await.unwrap();
        let put = received
            .iter()
            .find(|r| r.url.path() == "/build-info-put")
            .expect("a PUT to the presigned URL");
        let mut archive_zip = ZipArchive::new(std::io::Cursor::new(put.body.clone())).unwrap();
        let names: Vec<String> = (0..archive_zip.len())
            .map(|i| archive_zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, vec!["dependencies.json"]);
        let mut content = String::new();
        archive_zip
            .by_name("dependencies.json")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        assert!(
            content.contains("Alamofire"),
            "deps blob should list the pod: {content}"
        );
        assert!(content.contains("\"schema_version\":1"));
    }

    /// When `$OBJROOT` resolves to a DerivedData tree with a build log, the
    /// post-action decodes timings: the bundle gains a RAW `timings.json` entry
    /// and the registration POST carries `build_metadata.timings`. Here there is
    /// NO Podfile.lock, so timings is the SOLE sidecar — proving timings alone
    /// makes the bundle non-empty and registers the build.
    #[tokio::test]
    async fn post_action_decodes_and_bundles_timings() {
        use crate::cli::xcactivitylog::fixtures;

        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Archive layout with an Info.plist.
        let archive = root.join("App.xcarchive");
        let app = archive
            .join("Products")
            .join("Applications")
            .join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.example.myapp</string>
  <key>CFBundleShortVersionString</key><string>3.0.0</string>
  <key>CFBundleVersion</key><string>300</string>
</dict></plist>"#,
        )
        .unwrap();

        // DerivedData tree with a synthetic build log. `$OBJROOT` is a deep
        // descendant so `find_derived_data_root` walks up to `<dd>`.
        let dd = root.join("DerivedData").join("MyApp-abc");
        let logs_build = dd.join("Logs").join("Build");
        std::fs::create_dir_all(&logs_build).unwrap();
        fixtures::write_synthetic_log(
            &logs_build,
            &[("Ld Alpha", fixtures::T0, fixtures::T0 + 2.0)], // packaging, 2000ms
            &[("Build target Alpha", fixtures::T0, fixtures::T0 + 2.0)],
        );
        let obj_root = dd
            .join("Build")
            .join("Intermediates.noindex")
            .join("ArchiveIntermediates")
            .join("MyApp")
            .join("IntermediateBuildFilesPath");
        std::fs::create_dir_all(&obj_root).unwrap();

        // SRCROOT with no Podfile.lock → deps empty → timings is the sole entry.
        let srcroot = root.join("src");
        std::fs::create_dir_all(&srcroot).unwrap();
        let dsym_dir = root.join("dSYMs");
        std::fs::create_dir_all(&dsym_dir).unwrap();

        let put_url = format!("{}/build-info-put", server.uri());
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .and(body_partial_json(json!({
                "request_build_info_upload": true,
                "build_metadata": { "timings": { "total_ms": 2000, "packaging_ms": 2000 } },
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "build_id": "b1", "build_info_upload_endpoint": put_url }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/build-info-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("OBJROOT", obj_root.to_str().unwrap()),
            ("SDK_NAME", "iphoneos18.5"),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
        ]);

        let endpoint = server.uri();
        run_post_action(&env, &endpoint, Some("TKN"))
            .await
            .expect("post-action should succeed");

        // The bundle's sole entry is a RAW timings.json (the Gantt blob).
        let received = server.received_requests().await.unwrap();
        let put = received
            .iter()
            .find(|r| r.url.path() == "/build-info-put")
            .expect("a PUT to the presigned URL");
        let mut archive_zip = ZipArchive::new(std::io::Cursor::new(put.body.clone())).unwrap();
        let names: Vec<String> = (0..archive_zip.len())
            .map(|i| archive_zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names, vec!["timings.json"]);
        let mut content = String::new();
        archive_zip
            .by_name("timings.json")
            .unwrap()
            .read_to_string(&mut content)
            .unwrap();
        // RAW (un-gzipped) compact JSON — the worker re-gzips internally.
        assert!(content.contains("\"schema_version\":1"));
        assert!(content.contains("\"category\":\"packaging\""));
        assert!(content.contains("Alpha"));
    }

    /// With `BUGSEE_SIZE_ANALYSIS_ENABLED=1`, the post-action packages the `.app`
    /// into a synthetic `.ipa`, registers with `request_artifact_upload: true`,
    /// PUTs the artefact ZIP (a STORED `.ipa`), AND ships the build-info bundle —
    /// all from one registration.
    #[tokio::test]
    async fn post_action_size_analysis_ships_ipa() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let archive = root.join("App.xcarchive");
        let app = archive
            .join("Products")
            .join("Applications")
            .join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.example.myapp</string>
  <key>CFBundleShortVersionString</key><string>3.0.0</string>
  <key>CFBundleVersion</key><string>300</string>
</dict></plist>"#,
        )
        .unwrap();

        // A Podfile.lock so a deps sidecar (and thus the build-info bundle) ships
        // alongside the artefact.
        let srcroot = root.join("src");
        std::fs::create_dir_all(&srcroot).unwrap();
        std::fs::write(
            srcroot.join("Podfile.lock"),
            "PODS:\n  - Alamofire (5.9.1)\n\nDEPENDENCIES:\n  - Alamofire (= 5.9.1)\n",
        )
        .unwrap();

        let dsym_dir = root.join("dSYMs");
        std::fs::create_dir_all(&dsym_dir).unwrap();

        let art_url = format!("{}/artefact-put", server.uri());
        let bi_url = format!("{}/build-info-put", server.uri());

        // One registration: request_artifact_upload AND request_build_info_upload
        // both true; the server signs BOTH endpoints.
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .and(body_partial_json(json!({
                "request_artifact_upload": true,
                "request_build_info_upload": true,
                "format": "ipa",
                "package_id": "com.example.myapp",
                "build_configuration": "Release",
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "build_id": "b1", "endpoint": art_url, "build_info_upload_endpoint": bi_url }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/artefact-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/build-info-put"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("SDK_NAME", "iphoneos18.5"),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ("BUGSEE_SIZE_ANALYSIS_ENABLED", "1"),
        ]);

        let endpoint = server.uri();
        run_post_action(&env, &endpoint, Some("TKN"))
            .await
            .expect("post-action should succeed");

        // The artefact PUT body is a ZIP whose sole entry is a STORED `.ipa`.
        let received = server.received_requests().await.unwrap();
        let art = received
            .iter()
            .find(|r| r.url.path() == "/artefact-put")
            .expect("an artefact PUT");
        let mut zip = ZipArchive::new(std::io::Cursor::new(art.body.clone())).unwrap();
        let names: Vec<String> = (0..zip.len())
            .map(|i| zip.by_index(i).unwrap().name().to_string())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(names[0].ends_with(".ipa"), "artefact entry: {}", names[0]);
        assert_eq!(
            zip.by_index(0).unwrap().compression(),
            zip::CompressionMethod::Stored
        );
        // The synthetic .ipa carries the app under Payload/.
        let mut ipa_bytes = Vec::new();
        zip.by_index(0)
            .unwrap()
            .read_to_end(&mut ipa_bytes)
            .unwrap();
        let inner = ZipArchive::new(std::io::Cursor::new(ipa_bytes)).unwrap();
        assert!(
            inner
                .file_names()
                .any(|n| n.starts_with("Payload/MyApp.app/")),
            "synthetic .ipa must place the app under Payload/"
        );
    }

    /// Build a minimal Release archive + empty SRCROOT/dSYM dirs for the
    /// size-check tests. Returns `(archive, srcroot, dsym_dir)`.
    fn size_check_archive(root: &Path) -> (PathBuf, PathBuf, PathBuf) {
        let archive = root.join("App.xcarchive");
        let app = archive
            .join("Products")
            .join("Applications")
            .join("MyApp.app");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(
            app.join("Info.plist"),
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
  <key>CFBundleIdentifier</key><string>com.example.myapp</string>
  <key>CFBundleShortVersionString</key><string>3.0.0</string>
  <key>CFBundleVersion</key><string>300</string>
</dict></plist>"#,
        )
        .unwrap();
        let srcroot = root.join("src"); // no Podfile.lock → deps empty
        std::fs::create_dir_all(&srcroot).unwrap();
        let dsym_dir = root.join("dSYMs"); // empty → dSYM step no-ops
        std::fs::create_dir_all(&dsym_dir).unwrap();
        (archive, srcroot, dsym_dir)
    }

    /// A size-check FAIL: a tiny baseline makes the freshly packaged `.ipa`
    /// "growth" past the fail gate. The post-action returns a TERMINAL error
    /// (`ExitCode::SizeCheckFailed`, which does NOT trigger bootstrapper
    /// fallback) carrying the `error:`-ready gate line.
    #[tokio::test]
    async fn post_action_size_check_fail_returns_terminal_error() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());

        // Baseline fetched BEFORE registration; 1-byte baseline forces growth.
        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/baseline"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "build": { "artifact_size": 1, "version": "2.9", "build": "299" } }
            })))
            .expect(1)
            .mount(&server)
            .await;
        // Register-only (no sidecars, no artefact ship): a single POST.
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ("BUGSEE_SIZE_CHECK_ENABLED", "1"),
            ("BUGSEE_SIZE_CHECK_FAIL_BYTES", "1"),
        ]);

        let endpoint = server.uri();
        let err = run_post_action(&env, &endpoint, Some("TKN"))
            .await
            .unwrap_err();
        // Terminal gate failure — NOT a structural/fallback exit code.
        assert_eq!(
            crate::error::classify(&err),
            crate::exit_code::ExitCode::SizeCheckFailed
        );
        let msg = format!("{err}");
        assert!(msg.contains("Bugsee size check:"), "msg: {msg}");
        assert!(msg.contains("exceeds fail threshold 1 B"), "msg: {msg}");
        // Baseline label comes from the fetched version/build.
        assert!(msg.contains("vs version 2.9 (299)"), "msg: {msg}");
    }

    /// A size-check that is under threshold: a huge baseline makes the small
    /// `.ipa` a shrink → PASS → the post-action returns Ok (build not failed).
    #[tokio::test]
    async fn post_action_size_check_pass_is_ok() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());

        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/baseline"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "build": { "artifact_size": 1_000_000_000, "version": "2.9" } }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ("BUGSEE_SIZE_CHECK_ENABLED", "1"),
            ("BUGSEE_SIZE_CHECK_FAIL_BYTES", "1"),
        ]);

        let endpoint = server.uri();
        run_post_action(&env, &endpoint, Some("TKN"))
            .await
            .expect("an under-threshold size check must not fail the build");
    }

    /// `run_post_action_inner` returns an accurate per-step OUTCOME report — the
    /// data a delegating bootstrapper reads to write its handshake manifest.
    /// Here: size-analysis ON + a Podfile (deps) + a server that signs both
    /// endpoints → `build_registered`, `artifact_uploaded`, `deps_collected`,
    /// `size_analysis` all true; `size_check` "skip" (not enabled).
    #[tokio::test]
    async fn post_action_inner_reports_outcomes() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        std::fs::write(
            srcroot.join("Podfile.lock"),
            "PODS:\n  - Alamofire (5.9.1)\n\nDEPENDENCIES:\n  - Alamofire (= 5.9.1)\n",
        )
        .unwrap();

        let art_url = format!("{}/art", server.uri());
        let bi_url = format!("{}/bi", server.uri());
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "build_id": "b1", "endpoint": art_url, "build_info_upload_endpoint": bi_url }
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ("BUGSEE_SIZE_ANALYSIS_ENABLED", "1"),
        ]);
        let endpoint = server.uri();
        let report = run_post_action_inner(&env, &endpoint, Some("TKN"))
            .await
            .expect("inner ok")
            .expect("a report (not gated out)");

        assert!(report.build_registered);
        assert!(
            report.artifact_uploaded,
            "size-analysis on + signed endpoint"
        );
        assert!(report.deps_collected);
        assert!(report.size_analysis);
        assert_eq!(report.size_check, "skip");
        assert!(report.size_check_fail_line.is_none());

        // The serialized stdout shape carries the outcome flags (not the internal
        // fail line).
        let v = serde_json::to_value(&report).unwrap();
        assert_eq!(v["artifact_uploaded"], json!(true));
        assert_eq!(v["size_check"], json!("skip"));
        assert!(v.get("size_check_fail_line").is_none());
    }

    /// Register-only (size-analysis OFF, no sidecars): the report shows a
    /// registered build but NO artefact shipped.
    #[tokio::test]
    async fn post_action_inner_report_register_only() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
        ]);
        let endpoint = server.uri();
        let report = run_post_action_inner(&env, &endpoint, Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        assert!(report.build_registered);
        assert!(
            !report.artifact_uploaded,
            "size-analysis off → no bytes shipped"
        );
        assert!(!report.size_analysis);
    }

    /// `BUGSEE_DEPENDENCIES_ENABLED=0` suppresses dep collection even with a
    /// Podfile present (privacy opt-out parity with the agent).
    #[tokio::test]
    async fn post_action_inner_respects_deps_optout() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        std::fs::write(
            srcroot.join("Podfile.lock"),
            "PODS:\n  - Alamofire (5.9.1)\n\nDEPENDENCIES:\n  - Alamofire (= 5.9.1)\n",
        )
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ("BUGSEE_DEPENDENCIES_ENABLED", "0"),
        ]);
        let endpoint = server.uri();
        let report = run_post_action_inner(&env, &endpoint, Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        // Deps suppressed despite the Podfile; build still registers.
        assert!(
            !report.deps_collected,
            "deps opt-out must suppress collection"
        );
        assert!(report.build_registered);
    }

    /// `BUGSEE_BUILD_INFO_TIMINGS_ENABLED=0` suppresses timings even with a
    /// decodable `.xcactivitylog` present (privacy opt-out parity), and the
    /// report's `timings` flag goes false.
    #[tokio::test]
    async fn post_action_inner_respects_timings_optout() {
        use crate::cli::xcactivitylog::fixtures;
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());

        // A DerivedData tree with a decodable build log (timings WOULD resolve).
        let dd = tmp.path().join("DerivedData").join("MyApp-abc");
        let logs_build = dd.join("Logs").join("Build");
        std::fs::create_dir_all(&logs_build).unwrap();
        fixtures::write_synthetic_log(
            &logs_build,
            &[("Ld Alpha", fixtures::T0, fixtures::T0 + 2.0)],
            &[("Build target Alpha", fixtures::T0, fixtures::T0 + 2.0)],
        );
        let obj_root = dd.join("Build").join("Intermediates.noindex");
        std::fs::create_dir_all(&obj_root).unwrap();

        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .mount(&server)
            .await;

        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("OBJROOT", obj_root.to_str().unwrap()),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ("BUGSEE_BUILD_INFO_TIMINGS_ENABLED", "0"),
        ]);
        let endpoint = server.uri();
        let report = run_post_action_inner(&env, &endpoint, Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        assert!(!report.timings, "timings opt-out must suppress decode");
        assert!(report.build_registered);
    }

    // ── CLI flags drive the full networked flow (parity with the env vars) ──
    //
    // The tests above prove each deep-flow behaviour via its `BUGSEE_*` env var.
    // These prove the equivalent `--enable-*` / `--disable-*` FLAG produces the
    // IDENTICAL networked outcome: the env is built WITHOUT the var, the flag
    // overlay is applied exactly as `dispatch` does, and the real flow runs
    // against the in-process mock. This closes the gap where a flag's effect was
    // only observable past the network boundary.

    /// Apply post-action override flags onto an env map, mirroring `dispatch`.
    fn with_flags(mut env: HashMap<String, String>, flags: &[&str]) -> HashMap<String, String> {
        apply_overrides(&mut env, &parse_overrides(flags));
        env
    }

    #[tokio::test]
    async fn flag_disable_dependencies_suppresses_collection_through_the_flow() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        // A Podfile that WOULD yield deps if collection ran.
        std::fs::write(
            srcroot.join("Podfile.lock"),
            "PODS:\n  - Alamofire (5.9.1)\n\nDEPENDENCIES:\n  - Alamofire (= 5.9.1)\n",
        )
        .unwrap();
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .mount(&server)
            .await;

        // No BUGSEE_DEPENDENCIES_ENABLED env var — the FLAG drives the opt-out.
        let env = with_flags(
            env_of(&[
                ("ACTION", "install"),
                ("ARCHIVE_PATH", archive.to_str().unwrap()),
                ("CONFIGURATION", "Release"),
                ("SRCROOT", srcroot.to_str().unwrap()),
                ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ]),
            &["--disable-dependencies"],
        );
        let report = run_post_action_inner(&env, &server.uri(), Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !report.deps_collected,
            "--disable-dependencies must suppress collection through the full flow"
        );
        assert!(report.build_registered);
    }

    #[tokio::test]
    async fn flag_disable_timings_suppresses_decode_through_the_flow() {
        use crate::cli::xcactivitylog::fixtures;
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        // A decodable build log — timings WOULD resolve without the opt-out.
        let dd = tmp.path().join("DerivedData").join("MyApp-abc");
        let logs_build = dd.join("Logs").join("Build");
        std::fs::create_dir_all(&logs_build).unwrap();
        fixtures::write_synthetic_log(
            &logs_build,
            &[("Ld Alpha", fixtures::T0, fixtures::T0 + 2.0)],
            &[("Build target Alpha", fixtures::T0, fixtures::T0 + 2.0)],
        );
        let obj_root = dd.join("Build").join("Intermediates.noindex");
        std::fs::create_dir_all(&obj_root).unwrap();
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .mount(&server)
            .await;

        // No BUGSEE_BUILD_INFO_TIMINGS_ENABLED env var — the FLAG drives it.
        let env = with_flags(
            env_of(&[
                ("ACTION", "install"),
                ("ARCHIVE_PATH", archive.to_str().unwrap()),
                ("CONFIGURATION", "Release"),
                ("SRCROOT", srcroot.to_str().unwrap()),
                ("OBJROOT", obj_root.to_str().unwrap()),
                ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ]),
            &["--disable-timings"],
        );
        let report = run_post_action_inner(&env, &server.uri(), Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            !report.timings,
            "--disable-timings must suppress decode through the full flow"
        );
        assert!(report.build_registered);
    }

    #[tokio::test]
    async fn flag_enable_size_analysis_ships_artifact_through_the_flow() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        let art_url = format!("{}/art", server.uri());
        // The registration signs an artefact endpoint, so a single PUT ships it.
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1", "endpoint": art_url }
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1) // exactly one artefact PUT
            .mount(&server)
            .await;

        // No BUGSEE_SIZE_ANALYSIS_ENABLED env var — the FLAG turns it on.
        let env = with_flags(
            env_of(&[
                ("ACTION", "install"),
                ("ARCHIVE_PATH", archive.to_str().unwrap()),
                ("CONFIGURATION", "Release"),
                ("SRCROOT", srcroot.to_str().unwrap()),
                ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ]),
            &["--enable-size-analysis"],
        );
        let report = run_post_action_inner(&env, &server.uri(), Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            report.size_analysis,
            "--enable-size-analysis must turn it on"
        );
        assert!(
            report.artifact_uploaded,
            "--enable-size-analysis must ship the artefact (the PUT must fire)"
        );
        assert!(report.build_registered);
        // server drops here → wiremock asserts the PUT .expect(1) count was met.
    }

    #[tokio::test]
    async fn flag_size_check_fail_returns_terminal_error_through_the_flow() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        // A 1-byte baseline forces the freshly packaged .ipa over the fail gate.
        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/baseline"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": { "build": { "artifact_size": 1, "version": "2.9", "build": "299" } }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1" }
            })))
            .mount(&server)
            .await;

        // The size-check enable AND the fail threshold both come from FLAGS, not
        // env vars — proving the numeric threshold flag flows through too.
        let env = with_flags(
            env_of(&[
                ("ACTION", "install"),
                ("ARCHIVE_PATH", archive.to_str().unwrap()),
                ("CONFIGURATION", "Release"),
                ("SRCROOT", srcroot.to_str().unwrap()),
                ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ]),
            &["--enable-size-check", "--size-check-fail-bytes", "1"],
        );
        let err = run_post_action(&env, &server.uri(), Some("TKN"))
            .await
            .unwrap_err();
        assert_eq!(
            crate::error::classify(&err),
            crate::exit_code::ExitCode::SizeCheckFailed,
            "size-check FAIL driven by flags must be a terminal gate error"
        );
        assert!(
            format!("{err}").contains("exceeds fail threshold 1 B"),
            "err: {err}"
        );
    }

    #[tokio::test]
    async fn flag_enable_chunked_upload_uses_chunked_transport_through_the_flow() {
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());

        // Chunked-transport endpoints. A huge chunk_size makes the small .ipa a
        // single chunk; `missing: []` means the server already has it, so no
        // chunk PUT happens and the (content-dependent) chunk hashes don't need
        // matching — method+path matchers suffice. The single-PUT path would hit
        // `POST /builds` instead and NEVER touch `chunk-options`, so the
        // `.expect(1)` on chunk-options is what proves the FLAG selected chunked.
        Mock::given(method("GET"))
            .and(path("/v2/apps/TKN/builds/chunk-options"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "chunk_size": 1_000_000_000u64, "max_chunks": 100 }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds/chunks/check"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "missing": [], "upload_urls": {} }
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds/chunked"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1", "build_info_upload_endpoint": "" }
            })))
            .expect(1)
            .mount(&server)
            .await;

        // Both size-analysis (so bytes ship at all) and the chunked transport
        // come from FLAGS — no env vars.
        let env = with_flags(
            env_of(&[
                ("ACTION", "install"),
                ("ARCHIVE_PATH", archive.to_str().unwrap()),
                ("CONFIGURATION", "Release"),
                ("SRCROOT", srcroot.to_str().unwrap()),
                ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
            ]),
            &["--enable-size-analysis", "--enable-chunked-upload"],
        );
        let report = run_post_action_inner(&env, &server.uri(), Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        assert!(report.size_analysis);
        assert!(
            report.artifact_uploaded,
            "chunked upload must still ship the artefact"
        );
        assert!(report.build_registered);
        // server drops → wiremock asserts chunk-options + /chunked .expect(1)
        // fired, i.e. the chunked transport (not single-PUT) was taken.
    }

    /// The happy path: with a decodable log and no opt-out, `report.timings` is
    /// true (guards against the opt-out test passing for the wrong reason).
    #[tokio::test]
    async fn post_action_inner_reports_timings_true_on_happy_path() {
        use crate::cli::xcactivitylog::fixtures;
        let server = MockServer::start().await;
        let tmp = tempfile::tempdir().unwrap();
        let (archive, srcroot, dsym_dir) = size_check_archive(tmp.path());
        let dd = tmp.path().join("DerivedData").join("MyApp-abc");
        let logs_build = dd.join("Logs").join("Build");
        std::fs::create_dir_all(&logs_build).unwrap();
        fixtures::write_synthetic_log(
            &logs_build,
            &[("Ld Alpha", fixtures::T0, fixtures::T0 + 2.0)],
            &[("Build target Alpha", fixtures::T0, fixtures::T0 + 2.0)],
        );
        let obj_root = dd.join("Build").join("Intermediates.noindex");
        std::fs::create_dir_all(&obj_root).unwrap();
        let put_url = format!("{}/bi", server.uri());
        Mock::given(method("POST"))
            .and(path("/v2/apps/TKN/builds"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true, "result": { "build_id": "b1", "build_info_upload_endpoint": put_url }
            })))
            .mount(&server)
            .await;
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
            ("SRCROOT", srcroot.to_str().unwrap()),
            ("OBJROOT", obj_root.to_str().unwrap()),
            ("DWARF_DSYM_FOLDER_PATH", dsym_dir.to_str().unwrap()),
        ]);
        let endpoint = server.uri();
        let report = run_post_action_inner(&env, &endpoint, Some("TKN"))
            .await
            .unwrap()
            .unwrap();
        assert!(
            report.timings,
            "a decodable log with no opt-out must report timings"
        );
    }

    /// Gated-out → `run_post_action_inner` returns `Ok(None)` (no report).
    #[tokio::test]
    async fn post_action_inner_gated_out_is_none() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Debug"), // not Release → gated out
        ]);
        let report = run_post_action_inner(&env, "http://127.0.0.1:1", None)
            .await
            .unwrap();
        assert!(report.is_none());
    }

    /// A gated-out env (Debug, no opt-in) makes `post-action` a no-op that
    /// returns Ok without any network I/O — even with no app token.
    #[tokio::test]
    async fn post_action_gated_out_is_noop_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Debug"),
        ]);
        // Unroutable endpoint + no token: must not be touched / required.
        run_post_action(&env, "http://127.0.0.1:1/", None)
            .await
            .expect("gated-out post-action must be a no-op Ok");
    }

    /// Admitted by the gate but no app token → hard config error.
    #[tokio::test]
    async fn post_action_missing_token_is_config_error() {
        use crate::exit_code::ExitCode;
        let tmp = tempfile::tempdir().unwrap();
        let archive = tmp.path().join("App.xcarchive");
        std::fs::create_dir_all(&archive).unwrap();
        let env = env_of(&[
            ("ACTION", "install"),
            ("ARCHIVE_PATH", archive.to_str().unwrap()),
            ("CONFIGURATION", "Release"),
        ]);
        let err = run_post_action(&env, "http://127.0.0.1:1/", None)
            .await
            .unwrap_err();
        assert_eq!(crate::error::classify(&err), ExitCode::ConfigInvalid);
    }
}
