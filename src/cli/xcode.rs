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

use crate::cli::{build_env, debug_files, ios_deps, vcs_metadata};
use crate::compress::Strategy;
use crate::error::config_invalid;
use crate::upload::build_info::{self, Entry, Params};
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

/// `bugsee-cli xcode` argument shape.
#[derive(Subcommand, Debug)]
pub enum XcodeCommand {
    /// Run the build-publish flow from an Xcode post-action build phase.
    ///
    /// Reads the Xcode build settings from the process environment, gates on
    /// the `BUGSEE_BUILD_INFO_*` flags (Release-only by default), and — when
    /// admitted — registers the build, uploads the build-info bundle, and
    /// uploads dSYMs. A no-op (exit 0) when gated out: this runs as a
    /// post-action and must never fail an already-signed build.
    PostAction,
}

/// CLI dispatch. The app token comes from the global `--app-token` /
/// `BUGSEE_APP_TOKEN` like the other upload commands.
pub async fn dispatch(
    cmd: XcodeCommand,
    endpoint: Option<String>,
    app_token: Option<String>,
) -> anyhow::Result<()> {
    match cmd {
        XcodeCommand::PostAction => {
            let env: HashMap<String, String> = std::env::vars().collect();
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
fn build_registration_payload(
    env: &HashMap<String, String>,
    bundle: &BundleInfo,
    vcs: &vcs_metadata::VcsMetadata,
    machine: Option<&str>,
    xcode_version: Option<&str>,
    deps_summary_value: Option<Value>,
) -> Value {
    let mut payload = Map::new();

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

    // Phase 1 never ships artefact bytes.
    // TODO(phase 2): set request_artifact_upload from BUGSEE_SIZE_ANALYSIS_ENABLED.
    payload.insert("request_artifact_upload".into(), Value::Bool(false));

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
    // TODO(phase 2): build_metadata.timings (from .xcactivitylog decode).
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

/// Run the Phase-1 post-action flow. Returns `Ok(())` for both the
/// "ran the flow" and the "gated out / soft-failed" cases — this is a
/// post-action and must never fail an already-signed build. Only a hard
/// config error (no app token when one is required) returns `Err`.
async fn run_post_action(
    env: &HashMap<String, String>,
    endpoint: &str,
    app_token: Option<&str>,
) -> anyhow::Result<()> {
    // 1+2. Gate.
    match should_run(env) {
        Gate::Skip(reason) => {
            tracing::info!("Bugsee: {reason}. Skipping build-info upload.");
            return Ok(());
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
            return Ok(());
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

    // 6. Collect deps. Project root from PROJECT_DIR / SRCROOT / SOURCE_ROOT,
    // mirroring the agent's `_collect_dependencies_impl`.
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
    let scope = if collected.scope_label.is_empty() {
        DEPS_COLLECTION_SCOPE
    } else {
        collected.scope_label.as_str()
    };

    // A temp dir holds the payload + dependencies.json for the build-info run.
    let tmpdir = tempfile::tempdir()?;

    let mut entries: Vec<Entry> = Vec::new();
    let deps_summary_value = if collected.entries.is_empty() {
        None
    } else {
        let collected_at = iso8601_now();
        let summary = deps_summary(
            &collected.entries,
            collected.truncated,
            scope,
            &collected_at,
        );
        let blob = deps_blob(&collected.entries, collected.truncated, scope);
        let deps_path = tmpdir.path().join("dependencies.json");
        // Compact JSON (no whitespace) — matches the agent's `_deps_blob_gz`
        // `separators=(',', ':')`.
        std::fs::write(&deps_path, serde_json::to_vec(&blob)?)?;
        entries.push(Entry {
            name: "dependencies.json".into(),
            source: deps_path,
        });
        Some(summary)
    };

    // TODO(phase 2): collect build timings and push a `timings.json` Entry.

    // 7. Register + upload the build-info bundle (self-contained). When there
    // are no sidecars, there is nothing to bundle — register-only is a
    // Phase-2 concern (it requires the artefact path), so for now we log and
    // fall through to the dSYM step. `build_info::run` errors on empty
    // entries, so we must guard.
    if entries.is_empty() {
        tracing::info!(
            "Bugsee: no dependencies collected — nothing to bundle for build-info. \
             Skipping build-info upload (the build record is registered by the \
             artefact-upload path in a later phase)."
        );
        // TODO(phase 2): register the build even with no sidecars (via the
        // artefact-upload path) so a build record always lands.
    } else {
        let payload = build_registration_payload(
            env,
            &bundle,
            &vcs,
            machine.as_deref(),
            xcode_version.as_deref(),
            deps_summary_value,
        );
        let payload_path = tmpdir.path().join("payload.json");
        std::fs::write(&payload_path, serde_json::to_vec(&payload)?)?;

        let params = Params {
            endpoint,
            app_token: Some(app_token),
            payload_json: Some(&payload_path),
            upload_url: None,
            entries: &entries,
            strategy: Strategy::default(),
            out: None,
            dry_run: false,
        };
        // Soft-fail: a single upload hiccup must not fail the build. Log and
        // continue to the dSYM step, exactly as the agent does.
        match build_info::run(params, RetryPolicy::default()).await {
            Ok(outcome) => {
                tracing::info!(?outcome, "Bugsee: build-info bundle uploaded");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Bugsee: build-info upload failed (continuing)");
            }
        }
    }

    // 8. Discover + upload dSYMs. The folder is `$DWARF_DSYM_FOLDER_PATH`
    // (every Run-Script env carries it) or, for an archive, `<archive>/dSYMs`.
    upload_dsyms(env, endpoint, app_token, &bundle).await;

    Ok(())
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
/// missing folder or an upload error logs and returns — never fails the build.
async fn upload_dsyms(
    env: &HashMap<String, String>,
    endpoint: &str,
    app_token: &str,
    bundle: &BundleInfo,
) {
    let folder = match resolve_dsym_folder(env) {
        Some(f) => f,
        None => {
            tracing::info!("Bugsee: no dSYM folder found (DWARF_DSYM_FOLDER_PATH / archive dSYMs). Skipping dSYM upload.");
            return;
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
        Ok(()) => tracing::info!("Bugsee: dSYM upload complete"),
        Err(e) => {
            // Includes the "no .dSYM bundles found" case — a soft skip here,
            // not a build failure.
            tracing::warn!(error = %e, "Bugsee: dSYM upload skipped/failed (continuing)");
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

        let payload = build_registration_payload(
            &env,
            &bundle,
            &vcs,
            Some("github-actions:runner-1"),
            Some("16.2"),
            Some(summary),
        );
        let obj = payload.as_object().unwrap();

        assert_eq!(obj.get("format").and_then(Value::as_str), Some("ipa"));
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
        assert_eq!(
            obj.get("request_artifact_upload").and_then(Value::as_bool),
            Some(false)
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
        let payload = build_registration_payload(&env, &bundle, &vcs, None, None, None);
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
