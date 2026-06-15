//! Debug-ID injection for source maps.
//!
//! Implements the deterministic-UUID scheme: each JS bundle gets a UUIDv5 derived from
//! the file's content (so re-bundling identical code produces the same id), appended
//! as `//# debugId=<uuid>` plus a runtime stub that registers the id with
//! `globalThis._bugseeDebugIds` keyed by `Error().stack`. The matching `.map` file is
//! rewritten to embed `"debug_id": "<uuid>"` and `"debugId": "<uuid>"` (both keys for
//! downstream tooling compatibility).
//!
//! Stage placement: `inject` runs after the bundler completes (Metro, webpack, vite,
//! rollup output) and BEFORE upload. For RN, this is at the Metro serializer step so
//! the debug id ends up in the bundle the device runs. For web, this is a post-build
//! CI step.
//!
//! The worker keys sourcemaps by this `debug_id` (`symbolfiles/sourcemap.py`, with a
//! legacy top-level `uuid` fallback); `debug-files upload --type sourcemaps` reads the
//! id back via [`read_debug_id`].

use std::path::{Path, PathBuf};

use uuid::Uuid;

use crate::error::{Error, Result};

/// Fixed namespace for Bugsee sourcemap debug-ids — keeps UUIDv5 generation
/// stable across runs and machines (deterministic from bundle content alone).
const DEBUG_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0xb0, 0x95, 0xee, 0x5e, 0x53, 0x00, 0x4d, 0xa9, 0x8a, 0x05, 0x04, 0xde, 0xb0, 0x6a, 0x90, 0x01,
]);

/// The `//# debugId=` magic comment (Sentry-compatible) appended to bundles and
/// scanned for idempotency.
const DEBUG_ID_COMMENT_PREFIX: &str = "//# debugId=";

const SOURCE_MAPPING_URL_PREFIX: &str = "//# sourceMappingURL=";

/// Content-derived debug-id (UUIDv5 over the bundle bytes) — deterministic, so
/// identical bundles always get the same id and CI re-runs are stable.
pub fn compute_debug_id(content: &[u8]) -> Uuid {
    Uuid::new_v5(&DEBUG_ID_NAMESPACE, content)
}

/// The runtime stub appended to each JS bundle. On load it registers
/// `globalThis._bugseeDebugIds[<this script's Error stack>] = <debug_id>` — the
/// Sentry-proven self-identification the SDK reads at crash time to recover the
/// debug-id of the bundle a frame belongs to. Defensive (try/catch, multi-env
/// global resolution) so it can never throw in a customer bundle.
fn runtime_stub(debug_id: &Uuid) -> String {
    format!(
        "\n;!function(){{try{{var e=\"undefined\"!=typeof window?window:\
\"undefined\"!=typeof global?global:\"undefined\"!=typeof globalThis?globalThis:\
\"undefined\"!=typeof self?self:{{}},n=(new e.Error).stack;\
n&&(e._bugseeDebugIds=e._bugseeDebugIds||{{}},e._bugseeDebugIds[n]=\"{id}\")\
}}catch(e){{}}}}();\n{prefix}{id}\n",
        id = debug_id,
        prefix = DEBUG_ID_COMMENT_PREFIX,
    )
}

/// Tally of an inject run.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct InjectStats {
    /// JS files freshly injected.
    pub js_injected: u32,
    /// JS files already carrying a debug-id (left unchanged).
    pub js_already: u32,
    /// `.map` files that gained a `debug_id`.
    pub maps_updated: u32,
}

/// Inject debug-ids across all `.js`/`.cjs`/`.mjs` under `paths`.
pub fn inject_paths(paths: &[PathBuf], dry_run: bool) -> Result<InjectStats> {
    let mut stats = InjectStats::default();
    for root in paths {
        for entry in walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let is_js = matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("js") | Some("cjs") | Some("mjs")
            );
            if is_js {
                inject_one(p, dry_run, &mut stats)?;
            }
        }
    }
    Ok(stats)
}

fn inject_one(js_path: &Path, dry_run: bool, stats: &mut InjectStats) -> Result<()> {
    let content = std::fs::read_to_string(js_path)?;

    let debug_id = match existing_debug_id(&content) {
        Some(id) => {
            stats.js_already += 1;
            id
        }
        None => {
            let id = compute_debug_id(content.as_bytes());
            if !dry_run {
                std::fs::write(js_path, format!("{content}{}", runtime_stub(&id)))?;
            }
            stats.js_injected += 1;
            tracing::info!(path = %js_path.display(), debug_id = %id, "injected debug-id");
            id.to_string()
        }
    };

    if let Some(map_path) = paired_map(js_path, &content) {
        if write_map_debug_id(&map_path, &debug_id, dry_run)? {
            stats.maps_updated += 1;
            tracing::debug!(path = %map_path.display(), debug_id = %debug_id, "wrote debug_id into map");
        }
    }
    Ok(())
}

/// Read an existing debug-id from a bundle's `//# debugId=` comment (idempotency).
fn existing_debug_id(content: &str) -> Option<String> {
    let idx = content.rfind(DEBUG_ID_COMMENT_PREFIX)?;
    let rest = &content[idx + DEBUG_ID_COMMENT_PREFIX.len()..];
    let token: String = rest.trim_start().chars().take(36).collect();
    Uuid::parse_str(token.trim()).ok().map(|u| u.to_string())
}

/// Resolve the `.map` a bundle points at: its `//# sourceMappingURL=` (relative,
/// non-`data:`), else the conventional `<bundle>.map` sibling.
fn paired_map(js_path: &Path, content: &str) -> Option<PathBuf> {
    let dir = js_path.parent().unwrap_or_else(|| Path::new("."));
    if let Some(idx) = content.rfind(SOURCE_MAPPING_URL_PREFIX) {
        let url = content[idx + SOURCE_MAPPING_URL_PREFIX.len()..]
            .lines()
            .next()
            .unwrap_or("")
            .trim();
        if !url.is_empty() && !url.starts_with("data:") {
            let cand = dir.join(url);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let sibling = PathBuf::from(format!("{}.map", js_path.display()));
    sibling.is_file().then_some(sibling)
}

/// Insert both `debug_id` and `debugId` into a `.map` JSON (no-op if either is
/// already present). Returns whether the map was changed.
fn write_map_debug_id(map_path: &Path, debug_id: &str, dry_run: bool) -> Result<bool> {
    let raw = std::fs::read_to_string(map_path)?;
    let mut value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        Error::InputInvalid(format!(
            "sourcemap is not valid JSON: {e} ({})",
            map_path.display()
        ))
    })?;
    let obj = value.as_object_mut().ok_or_else(|| {
        Error::InputInvalid(format!(
            "sourcemap is not a JSON object: {}",
            map_path.display()
        ))
    })?;
    let has = |k: &str, o: &serde_json::Map<String, serde_json::Value>| {
        o.get(k).and_then(serde_json::Value::as_str).is_some()
    };
    if has("debug_id", obj) || has("debugId", obj) {
        return Ok(false);
    }
    obj.insert(
        "debug_id".into(),
        serde_json::Value::String(debug_id.to_string()),
    );
    obj.insert(
        "debugId".into(),
        serde_json::Value::String(debug_id.to_string()),
    );
    if !dry_run {
        let serialized = serde_json::to_string(&value)
            .map_err(|e| Error::InputInvalid(format!("failed to serialize sourcemap JSON: {e}")))?;
        std::fs::write(map_path, serialized)?;
    }
    Ok(true)
}

/// Read the keying id from a `.map` for upload: `debug_id` (modern) / `debugId`
/// / legacy `uuid`, in that precedence. Mirrors the worker's
/// `symbolfiles/sourcemap.py:parse`.
pub fn read_debug_id(map_path: &Path) -> Result<Option<String>> {
    let raw = std::fs::read_to_string(map_path)?;
    let value: serde_json::Value = serde_json::from_str(&raw).map_err(|e| {
        Error::InputInvalid(format!(
            "sourcemap is not valid JSON: {e} ({})",
            map_path.display()
        ))
    })?;
    Ok(["debug_id", "debugId", "uuid"]
        .iter()
        .find_map(|k| value.get(*k).and_then(serde_json::Value::as_str))
        .map(str::to_string))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_id_is_deterministic_and_content_derived() {
        let a = compute_debug_id(b"console.log(1)");
        let b = compute_debug_id(b"console.log(1)");
        let c = compute_debug_id(b"console.log(2)");
        assert_eq!(a, b, "same content -> same id");
        assert_ne!(a, c, "different content -> different id");
    }

    #[test]
    fn inject_is_idempotent_and_writes_map() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        std::fs::write(&js, "console.log('hi')\n//# sourceMappingURL=app.js.map\n").unwrap();
        std::fs::write(&map, r#"{"version":3,"sources":[],"mappings":""}"#).unwrap();

        let s1 = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        assert_eq!(s1.js_injected, 1);
        assert_eq!(s1.maps_updated, 1);

        let js_after = std::fs::read_to_string(&js).unwrap();
        assert!(js_after.contains("//# debugId="));
        assert!(js_after.contains("_bugseeDebugIds"));
        let bundle_did = existing_debug_id(&js_after).unwrap();
        // Map carries the same id under BOTH keys.
        let map_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&map).unwrap()).unwrap();
        assert_eq!(
            map_json.get("debug_id").unwrap().as_str().unwrap(),
            bundle_did
        );
        assert_eq!(
            map_json.get("debugId").unwrap().as_str().unwrap(),
            bundle_did
        );

        // Re-running is a no-op (idempotent).
        let s2 = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        assert_eq!(s2.js_injected, 0, "already injected");
        assert_eq!(s2.js_already, 1);
        assert_eq!(
            read_debug_id(&map).unwrap().unwrap(),
            bundle_did,
            "id unchanged"
        );
    }

    #[test]
    fn dry_run_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.mjs");
        std::fs::write(&js, "export const x = 1\n").unwrap();
        let before = std::fs::read_to_string(&js).unwrap();
        let s = inject_paths(&[dir.path().to_path_buf()], true).unwrap();
        assert_eq!(s.js_injected, 1);
        assert_eq!(
            std::fs::read_to_string(&js).unwrap(),
            before,
            "dry-run left file unchanged"
        );
    }

    #[test]
    fn read_debug_id_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("m.map");
        std::fs::write(&p, r#"{"debug_id":"new","uuid":"old"}"#).unwrap();
        assert_eq!(read_debug_id(&p).unwrap().as_deref(), Some("new"));
        std::fs::write(&p, r#"{"uuid":"legacy"}"#).unwrap();
        assert_eq!(read_debug_id(&p).unwrap().as_deref(), Some("legacy"));
        std::fs::write(&p, r#"{"version":3}"#).unwrap();
        assert_eq!(read_debug_id(&p).unwrap(), None);
    }
}
