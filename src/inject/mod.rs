//! Debug-ID injection for source maps.
//!
//! Implements the deterministic-UUID scheme: each JS bundle gets a UUIDv5 derived from
//! the file's content AND its paired map's (so re-bundling identical code produces the
//! same id, and a map that changed under identical code gets a new one), appended
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
/// stable across runs and machines (deterministic from the bundle's content and
/// its paired map's).
const DEBUG_ID_NAMESPACE: Uuid = Uuid::from_bytes([
    0xb0, 0x95, 0xee, 0x5e, 0x53, 0x00, 0x4d, 0xa9, 0x8a, 0x05, 0x04, 0xde, 0xb0, 0x6a, 0x90, 0x01,
]);

/// The `//# debugId=` magic comment (the de-facto source-map debug-id
/// convention) appended to bundles and scanned for idempotency.
const DEBUG_ID_COMMENT_PREFIX: &str = "//# debugId=";

const SOURCE_MAPPING_URL_PREFIX: &str = "//# sourceMappingURL=";

/// Content-derived debug-id (UUIDv5 over the bundle bytes alone) — deterministic,
/// so identical bundles always get the same id and CI re-runs are stable. What
/// `inject` assigns a bundle that HAS a map is [`compute_debug_id_with_map`].
pub fn compute_debug_id(content: &[u8]) -> Uuid {
    Uuid::new_v5(&DEBUG_ID_NAMESPACE, content)
}

/// The debug-id `inject` assigns: over the bundle AND its paired map when it has
/// one, over the bundle alone when it has none ([`compute_debug_id`]).
///
/// The map is part of the identity because the server dedups source maps by id
/// alone. A minifier routinely emits byte-identical JS for a source edit that
/// moves original lines, and a bundle-only id then kept the STALE map on the
/// server while the upload reported the new one as already there. The bundle's
/// length is hashed ahead of it, so the boundary between the two inputs cannot
/// shift without changing the id.
pub fn compute_debug_id_with_map(bundle: &[u8], map: Option<&[u8]>) -> Uuid {
    let Some(map) = map else {
        return compute_debug_id(bundle);
    };
    let mut input = Vec::with_capacity(8 + bundle.len() + map.len());
    input.extend_from_slice(&(bundle.len() as u64).to_le_bytes());
    input.extend_from_slice(bundle);
    input.extend_from_slice(map);
    Uuid::new_v5(&DEBUG_ID_NAMESPACE, &input)
}

/// The runtime stub appended to each JS bundle. On load it registers
/// `globalThis._bugseeDebugIds[<this script's Error stack>] = <debug_id>` — the
/// self-identification the SDK reads at crash time to recover the
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
    let map_path = paired_map(js_path, &content);

    let debug_id = match existing_debug_id(&content) {
        Some(id) => {
            stats.js_already += 1;
            id
        }
        None => {
            let map_bytes = map_path.as_deref().map(std::fs::read).transpose()?;
            let id = compute_debug_id_with_map(content.as_bytes(), map_bytes.as_deref());
            if !dry_run {
                std::fs::write(js_path, format!("{content}{}", runtime_stub(&id)))?;
            }
            stats.js_injected += 1;
            tracing::info!(path = %js_path.display(), debug_id = %id, "injected debug-id");
            id.to_string()
        }
    };

    if let Some(map_path) = map_path {
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
        // Only follow a relative URL that stays within the bundle's directory.
        // The URL comes from an (attacker-controllable) `//# sourceMappingURL=`
        // comment; without this guard `../../x.json` would let `inject` re-serialize
        // and annotate a JSON file outside the bundle tree.
        if !url.is_empty() && !url.starts_with("data:") && is_contained_relative(url) {
            let cand = dir.join(url);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    let sibling = PathBuf::from(format!("{}.map", js_path.display()));
    sibling.is_file().then_some(sibling)
}

/// True if `url` is a plain relative path that cannot escape its base directory:
/// no `..` component, not absolute, no Windows drive/UNC prefix or root. Only
/// `Normal` / `CurDir` (`.`) components are allowed.
fn is_contained_relative(url: &str) -> bool {
    use std::path::Component;
    Path::new(url)
        .components()
        .all(|c| matches!(c, Component::Normal(_) | Component::CurDir))
}

/// Make a `.map` JSON carry `debug_id` AND `debugId` equal to the bundle's id.
/// Returns whether the map was changed (a no-op when both already match).
///
/// The BUNDLE's id is authoritative — it is what the runtime reports — so a map
/// holding a different id is rewritten rather than left alone. That happens when
/// a bundler re-emits the JS without its stub but leaves a previously stamped map
/// on disk: the fresh id is computed over a map that already carries the old
/// one, and keeping the old one silently unsymbolicated the bundle.
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
    let current = |k: &str, o: &serde_json::Map<String, serde_json::Value>| {
        o.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let (snake, camel) = (current("debug_id", obj), current("debugId", obj));
    if snake.as_deref() == Some(debug_id) && camel.as_deref() == Some(debug_id) {
        return Ok(false);
    }
    if let Some(stale) = snake.or(camel).filter(|existing| existing != debug_id) {
        tracing::warn!(
            path = %map_path.display(),
            stale = %stale,
            debug_id = %debug_id,
            "source map carried a different debug id than its bundle; rewriting it to the bundle's"
        );
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
    fn is_contained_relative_rejects_traversal_and_absolute() {
        assert!(is_contained_relative("bundle.js.map"));
        assert!(is_contained_relative("./maps/bundle.js.map"));
        assert!(is_contained_relative("maps/bundle.js.map"));
        // Escapes are rejected so a `//# sourceMappingURL=` comment can't steer
        // `inject` at a file outside the bundle directory.
        assert!(!is_contained_relative("../secret.json"));
        assert!(!is_contained_relative("../../etc/x.json"));
        assert!(!is_contained_relative("maps/../../escape.json"));
        assert!(!is_contained_relative("/etc/passwd"));
    }

    #[test]
    fn debug_id_is_deterministic_and_content_derived() {
        let a = compute_debug_id(b"console.log(1)");
        let b = compute_debug_id(b"console.log(1)");
        let c = compute_debug_id(b"console.log(2)");
        assert_eq!(a, b, "same content -> same id");
        assert_ne!(a, c, "different content -> different id");
    }

    /// The id must change when the MAP changes, not only the bundle. A minifier
    /// routinely emits byte-identical JS for a source edit that moves original
    /// lines (a comment added above a function); the map then differs. The
    /// server dedups source maps by id alone, so a bundle-only id kept the
    /// STALE map there — and the upload reported the new one as already present.
    #[test]
    fn a_changed_map_under_an_unchanged_bundle_gets_a_new_debug_id() {
        let inject_with_map = |map: &str| {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("app.js"), "console.log(1)\n").unwrap();
            std::fs::write(dir.path().join("app.js.map"), map).unwrap();
            inject_paths(&[dir.path().to_path_buf()], false).unwrap();
            read_debug_id(&dir.path().join("app.js.map"))
                .unwrap()
                .unwrap()
        };
        let before = inject_with_map(r#"{"version":3,"sources":["a.ts"],"mappings":"AAAA"}"#);
        let same = inject_with_map(r#"{"version":3,"sources":["a.ts"],"mappings":"AAAA"}"#);
        let moved = inject_with_map(r#"{"version":3,"sources":["a.ts"],"mappings":"AACA"}"#);
        assert_eq!(before, same, "same bundle + same map -> same id");
        assert_ne!(before, moved, "same bundle + changed map -> new id");
    }

    /// Pinned to values derived INDEPENDENTLY (Python: SHA-1 over the namespace
    /// bytes + name, version/variant bits set per RFC 4122 §4.3). A refactor that
    /// changes the encoding — u32 length, big-endian, map before bundle — would
    /// silently give every bundle a new id and re-upload every map once.
    #[test]
    fn the_id_encoding_is_pinned_to_independently_derived_values() {
        let bundle = b"console.log(1)\n";
        let map = br#"{"version":3,"mappings":"AAAA"}"#;
        assert_eq!(
            compute_debug_id(bundle).to_string(),
            "9ca80e34-5d30-5aa4-912b-e209b8a484eb"
        );
        assert_eq!(
            compute_debug_id_with_map(bundle, Some(map)).to_string(),
            "256ca4fa-1d71-5674-9d8a-eb359b60f199"
        );
    }

    /// A bundle rewritten without its stub while the previously STAMPED map stays
    /// on disk: the new id is computed over a map that already carries the old one,
    /// so the two can never match again. The bundle's id is the one the runtime
    /// reports, so the map must follow it — leaving the old id there silently
    /// unsymbolicated that bundle.
    #[test]
    fn a_reinjected_bundle_moves_a_stale_map_id_to_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        std::fs::write(
            &map,
            r#"{"version":3,"sources":["a.ts"],"mappings":"AAAA"}"#,
        )
        .unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();

        // The bundler re-emits the JS (no stub) but leaves the stamped map.
        std::fs::write(&js, "console.log(1)\n").unwrap();
        let stats = inject_paths(&[dir.path().to_path_buf()], false).unwrap();

        let bundle_id = existing_debug_id(&std::fs::read_to_string(&js).unwrap()).unwrap();
        let map_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&map).unwrap()).unwrap();
        assert_eq!(map_json["debug_id"].as_str().unwrap(), bundle_id);
        assert_eq!(map_json["debugId"].as_str().unwrap(), bundle_id);
        assert_eq!(stats.maps_updated, 1);
    }

    /// A map that exists but cannot be read fails the run BEFORE the bundle is
    /// touched — the id depends on the map, so there is nothing sound to write.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_map_fails_before_the_bundle_is_modified() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        std::fs::write(&map, "{}").unwrap();
        std::fs::set_permissions(&map, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads regardless of mode bits; the premise does not hold there.
        if std::fs::read(&map).is_ok() {
            eprintln!("skipping: running with permission to read a 000 file");
            return;
        }
        let result = inject_paths(&[dir.path().to_path_buf()], false);
        std::fs::set_permissions(&map, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(result.is_err());
        assert_eq!(std::fs::read_to_string(&js).unwrap(), "console.log(1)\n");
    }

    #[test]
    fn a_bundle_without_a_map_is_keyed_by_its_own_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let injected = std::fs::read_to_string(&js).unwrap();
        assert_eq!(
            existing_debug_id(&injected).unwrap(),
            compute_debug_id(b"console.log(1)\n").to_string()
        );
    }

    #[test]
    fn the_map_bytes_change_the_id_but_the_bundle_bytes_still_do_too() {
        let bundle_a = compute_debug_id_with_map(b"console.log(1)", Some(b"{}"));
        let bundle_b = compute_debug_id_with_map(b"console.log(2)", Some(b"{}"));
        let map_b = compute_debug_id_with_map(b"console.log(1)", Some(b"{ }"));
        assert_ne!(bundle_a, bundle_b);
        assert_ne!(bundle_a, map_b);
        // The boundary between the two inputs is part of the hash: moving bytes
        // from the end of the bundle to the start of the map is a different pair.
        assert_ne!(
            compute_debug_id_with_map(b"ab", Some(b"c")),
            compute_debug_id_with_map(b"a", Some(b"bc"))
        );
        assert_ne!(
            compute_debug_id_with_map(b"console.log(1)", Some(b"")),
            compute_debug_id_with_map(b"console.log(1)", None)
        );
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
        // Pair it with a map so the dry-run also exercises write_map_debug_id's
        // guard — a dropped `if !dry_run` on EITHER the bundle or the map write
        // must be caught.
        let map = dir.path().join("app.mjs.map");
        std::fs::write(
            &js,
            "export const x = 1\n//# sourceMappingURL=app.mjs.map\n",
        )
        .unwrap();
        std::fs::write(&map, r#"{"version":3,"sources":[],"mappings":""}"#).unwrap();
        let js_before = std::fs::read_to_string(&js).unwrap();
        let map_before = std::fs::read_to_string(&map).unwrap();

        let s = inject_paths(&[dir.path().to_path_buf()], true).unwrap();
        assert_eq!(s.js_injected, 1);
        // The map WOULD have changed (no debug_id yet), so the intent is tallied
        // even though nothing is written to disk in dry-run.
        assert_eq!(s.maps_updated, 1);
        assert_eq!(
            std::fs::read_to_string(&js).unwrap(),
            js_before,
            "dry-run left the bundle unchanged"
        );
        assert_eq!(
            std::fs::read_to_string(&map).unwrap(),
            map_before,
            "dry-run left the source map unchanged"
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
