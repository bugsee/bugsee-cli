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
fn runtime_stub(debug_id: &impl std::fmt::Display) -> String {
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
    /// Already-injected JS files re-stamped with a new id because their map was
    /// regenerated with different content.
    pub js_restamped: u32,
}

/// Inject debug-ids across all `.js`/`.cjs`/`.mjs` under `paths`.
pub fn inject_paths(paths: &[PathBuf], dry_run: bool) -> Result<InjectStats> {
    let mut stats = InjectStats::default();
    for root in paths {
        // Sorted, so a run is the same on every file system (it decides, e.g.,
        // which bundle first stamps a map two bundles share).
        for entry in walkdir::WalkDir::new(root)
            .sort_by_file_name()
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

    let (debug_id, freshly_computed) = match existing_debug_id(&content) {
        Some(id) => match restamp_id(&content, &id, map_path.as_deref())? {
            // Our stub, over a map regenerated with different content: re-key the
            // bundle, or the upload dedups against the STALE map (webpack keeps a
            // `[contenthash]` JS file it considers unchanged but re-emits its map).
            Some((original, fresh)) => {
                if !dry_run {
                    std::fs::write(js_path, format!("{original}{}", runtime_stub(&fresh)))?;
                }
                stats.js_restamped += 1;
                tracing::info!(
                    path = %js_path.display(),
                    stale = %id,
                    debug_id = %fresh,
                    "re-stamped debug-id: the bundle's map was regenerated"
                );
                (fresh.to_string(), true)
            }
            None => {
                stats.js_already += 1;
                (id, false)
            }
        },
        None => {
            let map_bytes = map_path.as_deref().map(std::fs::read).transpose()?;
            let id = compute_debug_id_with_map(content.as_bytes(), map_bytes.as_deref());
            if !dry_run {
                std::fs::write(js_path, format!("{content}{}", runtime_stub(&id)))?;
            }
            stats.js_injected += 1;
            tracing::info!(path = %js_path.display(), debug_id = %id, "injected debug-id");
            (id.to_string(), true)
        }
    };

    if let Some(map_path) = map_path {
        if write_map_debug_id(&map_path, &debug_id, freshly_computed, dry_run)? {
            stats.maps_updated += 1;
            tracing::debug!(path = %map_path.display(), debug_id = %debug_id, "wrote debug_id into map");
        }
    }
    Ok(())
}

/// For a bundle that already carries `id`: the original bundle text and the id it
/// should carry NOW, when that differs — or `None` to keep `id`.
///
/// Re-keying needs two things. The id must be in OUR stub, the exact suffix
/// [`runtime_stub`] appends, so the original bytes are recoverable; a
/// `//# debugId=` another tool wrote cannot be stripped safely. And the map must
/// carry NO id, i.e. it was regenerated since the stamp. A stamped map is the
/// one the id was computed over, plus the id itself, so hashing it again could
/// never reproduce the id and would re-key on every run.
fn restamp_id<'a>(
    content: &'a str,
    id: &str,
    map_path: Option<&Path>,
) -> Result<Option<(&'a str, Uuid)>> {
    let Some(map_path) = map_path else {
        return Ok(None);
    };
    let Some(original) = content.strip_suffix(runtime_stub(&id).as_str()) else {
        return Ok(None);
    };
    let map_bytes = std::fs::read(map_path)?;
    // A map that is not JSON is not re-keyed here: writing the id into it fails
    // next, and it must fail BEFORE the bundle is rewritten, as it always did.
    let Ok(map) = serde_json::from_slice::<serde_json::Value>(&map_bytes) else {
        return Ok(None);
    };
    let carries_id = ["debug_id", "debugId"]
        .iter()
        .any(|k| map.get(*k).and_then(serde_json::Value::as_str).is_some());
    if carries_id {
        return Ok(None);
    }
    let fresh = compute_debug_id_with_map(original.as_bytes(), Some(&map_bytes));
    Ok((fresh.to_string() != id).then_some((original, fresh)))
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

/// What to do with a map's existing id keys, given its bundle's id.
#[derive(Debug, PartialEq, Eq)]
enum MapIdAction {
    /// Both keys already carry the bundle's id.
    Keep,
    /// A key is missing (or not a string) and none disagrees: write the bundle's id.
    Fill,
    /// A key carries a different id, and the bundle's id was JUST computed: replace it.
    Replace { stale: String },
    /// A key carries a different id, but the bundle already had its id: leave the map.
    Conflict { stale: String },
}

/// Decide [`MapIdAction`]. Ids compare case-insensitively (a UUID is the same id
/// in either case).
///
/// Replacing is right only when the bundle's id was just computed: that is the
/// re-emitted-bundle-beside-a-stamped-map case, where the fresh id is hashed over
/// a map that already carries the old one and could never match it again. When
/// the bundle ALREADY carried its id, a disagreeing map is someone else's — most
/// plausibly one map named by two bundles — and rewriting it would flip it back
/// and forth on every run.
fn map_id_action(
    snake: Option<&str>,
    camel: Option<&str>,
    debug_id: &str,
    freshly_computed: bool,
) -> MapIdAction {
    let stale = [snake, camel]
        .into_iter()
        .flatten()
        .find(|existing| !existing.eq_ignore_ascii_case(debug_id))
        .map(str::to_string);
    match stale {
        Some(stale) if freshly_computed => MapIdAction::Replace { stale },
        Some(stale) => MapIdAction::Conflict { stale },
        None if snake.is_some() && camel.is_some() => MapIdAction::Keep,
        None => MapIdAction::Fill,
    }
}

/// Make a `.map` JSON carry `debug_id` AND `debugId` equal to the bundle's id,
/// as [`map_id_action`] decides. Returns whether the map was changed.
fn write_map_debug_id(
    map_path: &Path,
    debug_id: &str,
    freshly_computed: bool,
    dry_run: bool,
) -> Result<bool> {
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
    let current = |k: &str| {
        obj.get(k)
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    };
    let (snake, camel) = (current("debug_id"), current("debugId"));
    match map_id_action(
        snake.as_deref(),
        camel.as_deref(),
        debug_id,
        freshly_computed,
    ) {
        MapIdAction::Keep => return Ok(false),
        MapIdAction::Fill => {}
        MapIdAction::Replace { stale } => tracing::warn!(
            path = %map_path.display(),
            stale = %stale,
            debug_id = %debug_id,
            "source map carried a different debug id than its freshly injected bundle; rewriting it to the bundle's"
        ),
        MapIdAction::Conflict { stale } => {
            tracing::warn!(
                path = %map_path.display(),
                map_debug_id = %stale,
                bundle_debug_id = %debug_id,
                "source map carries a different debug id than its bundle, which already had one; left as is \
                 (is this map shared by more than one bundle?)"
            );
            return Ok(false);
        }
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
    fn map_id_action_decides_per_key_and_ignores_case() {
        use MapIdAction::*;
        let id = "0b8c1a2e-0000-5000-8000-000000000001";
        let upper = "0B8C1A2E-0000-5000-8000-000000000001";
        let old = "11111111-1111-5111-8111-111111111111";
        let stale = || old.to_string();
        for fresh in [false, true] {
            assert_eq!(map_id_action(Some(id), Some(id), id, fresh), Keep);
            assert_eq!(map_id_action(Some(upper), Some(id), id, fresh), Keep);
            assert_eq!(map_id_action(None, Some(id), id, fresh), Fill);
            assert_eq!(map_id_action(Some(id), None, id, fresh), Fill);
            assert_eq!(map_id_action(None, None, id, fresh), Fill);
        }
        // A stale key is found whichever key holds it, even when the other matches.
        for (snake, camel) in [
            (Some(id), Some(old)),
            (Some(old), Some(id)),
            (Some(old), None),
            (None, Some(old)),
        ] {
            assert_eq!(
                map_id_action(snake, camel, id, true),
                Replace { stale: stale() }
            );
            assert_eq!(
                map_id_action(snake, camel, id, false),
                Conflict { stale: stale() }
            );
        }
    }

    /// A map carrying only `debugId` — as Rollup 4's `sourcemapDebugIds` writes it —
    /// beside a bundle that already has `//# debugId=`: it gains `debug_id` once,
    /// and a second run changes nothing.
    #[test]
    fn a_map_with_only_a_matching_debug_id_camel_key_gains_the_other_key_once() {
        let dir = tempfile::tempdir().unwrap();
        let id = "5d2f9a3c-1b7e-4c8a-9f10-2a3b4c5d6e7f";
        std::fs::write(
            dir.path().join("app.js"),
            format!("console.log(1)\n//# sourceMappingURL=app.js.map\n//# debugId={id}\n"),
        )
        .unwrap();
        let map = dir.path().join("app.js.map");
        std::fs::write(
            &map,
            format!(r#"{{"version":3,"mappings":"","debugId":"{id}"}}"#),
        )
        .unwrap();

        let first = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let map_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&map).unwrap()).unwrap();
        assert_eq!(first.maps_updated, 1);
        assert_eq!(map_json["debug_id"], id);
        assert_eq!(map_json["debugId"], id);
        let second = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        assert_eq!(second.maps_updated, 0);
    }

    /// Two bundles naming ONE map: it cannot carry both ids. Whatever the first run
    /// leaves, later runs must not keep rewriting it — `inject` is idempotent.
    #[test]
    fn a_map_shared_by_two_bundles_settles_instead_of_flipping_every_run() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.js", "b.js"] {
            std::fs::write(
                dir.path().join(name),
                format!("console.log('{name}')\n//# sourceMappingURL=shared.map\n"),
            )
            .unwrap();
        }
        let map = dir.path().join("shared.map");
        std::fs::write(&map, r#"{"version":3,"mappings":""}"#).unwrap();

        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let after_first = std::fs::read(&map).unwrap();
        for _ in 0..2 {
            let stats = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
            assert_eq!(stats.maps_updated, 0);
            assert_eq!(std::fs::read(&map).unwrap(), after_first);
        }
    }

    /// webpack 5 with `[contenthash]` filenames and no `output.clean` (its
    /// default): a source edit that only moves original lines leaves the JS
    /// byte-identical, so webpack SKIPS rewriting it — our stub and old id stay —
    /// and re-emits only the map, without an id. Filling the new map with the old
    /// id made the upload a server-side duplicate and kept the STALE map.
    #[test]
    fn a_map_regenerated_beside_an_unchanged_stamped_bundle_gets_a_new_id() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("main.abc123.js");
        let map = dir.path().join("main.abc123.js.map");
        let bundle = "console.log(1)\n//# sourceMappingURL=main.abc123.js.map\n";
        std::fs::write(&js, bundle).unwrap();
        std::fs::write(
            &map,
            r#"{"version":3,"sources":["a.ts"],"mappings":"AAEA"}"#,
        )
        .unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let first = read_debug_id(&map).unwrap().unwrap();

        // Rebuild: JS untouched on disk, map re-emitted with shifted mappings and no id.
        let moved = r#"{"version":3,"sources":["a.ts"],"mappings":"AAKA"}"#;
        std::fs::write(&map, moved).unwrap();
        let stats = inject_paths(&[dir.path().to_path_buf()], false).unwrap();

        let expected =
            compute_debug_id_with_map(bundle.as_bytes(), Some(moved.as_bytes())).to_string();
        assert_ne!(expected, first);
        let js_after = std::fs::read_to_string(&js).unwrap();
        assert_eq!(existing_debug_id(&js_after).unwrap(), expected);
        assert_eq!(js_after, format!("{bundle}{}", runtime_stub(&expected)));
        assert_eq!(read_debug_id(&map).unwrap().unwrap(), expected);
        assert_eq!(stats.js_restamped, 1);
        assert_eq!(stats.maps_updated, 1);

        // And it settles: nothing changes on the next run.
        let again = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        assert_eq!((again.js_restamped, again.maps_updated), (0, 0));
        assert_eq!(std::fs::read_to_string(&js).unwrap(), js_after);
    }

    /// The same rebuild with a map that did NOT change keeps the id and the bytes.
    #[test]
    fn an_identical_map_regenerated_beside_a_stamped_bundle_keeps_its_id() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        let original_map = r#"{"version":3,"sources":["a.ts"],"mappings":"AAEA"}"#;
        std::fs::write(&js, "console.log(1)\n").unwrap();
        std::fs::write(&map, original_map).unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let js_stamped = std::fs::read_to_string(&js).unwrap();
        let id = read_debug_id(&map).unwrap().unwrap();

        std::fs::write(&map, original_map).unwrap();
        let stats = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        assert_eq!(stats.js_restamped, 0);
        assert_eq!(std::fs::read_to_string(&js).unwrap(), js_stamped);
        assert_eq!(read_debug_id(&map).unwrap().unwrap(), id);
    }

    /// A `//# debugId=` another tool wrote (Rollup's `sourcemapDebugIds`) is not our
    /// stub, so the original bundle bytes cannot be recovered to re-key it: the
    /// map is filled with that id, as before.
    #[test]
    fn a_foreign_debug_id_comment_is_never_restamped() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let id = "5d2f9a3c-1b7e-4c8a-9f10-2a3b4c5d6e7f";
        let bundle = format!("console.log(1)\n//# debugId={id}\n");
        std::fs::write(&js, &bundle).unwrap();
        std::fs::write(
            dir.path().join("app.js.map"),
            r#"{"version":3,"mappings":"AAKA"}"#,
        )
        .unwrap();
        let stats = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        assert_eq!(stats.js_restamped, 0);
        assert_eq!(std::fs::read_to_string(&js).unwrap(), bundle);
        assert_eq!(
            read_debug_id(&dir.path().join("app.js.map"))
                .unwrap()
                .unwrap(),
            id
        );
    }

    #[test]
    fn a_regenerated_map_that_is_not_json_fails_without_touching_the_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        std::fs::write(&map, r#"{"version":3,"mappings":"AAEA"}"#).unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let js_stamped = std::fs::read_to_string(&js).unwrap();

        std::fs::write(&map, "not json").unwrap();
        assert!(inject_paths(&[dir.path().to_path_buf()], false).is_err());
        assert_eq!(std::fs::read_to_string(&js).unwrap(), js_stamped);
    }

    /// A stamped bundle whose map lost ONE of its keys still has its id in the map:
    /// it is the map the id was computed over, so it gains the missing key and the
    /// bundle is NOT re-keyed (which would repeat on every run).
    #[test]
    fn a_map_keeping_only_one_id_key_is_not_treated_as_regenerated() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        std::fs::write(&map, r#"{"version":3,"mappings":"AAEA"}"#).unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let js_stamped = std::fs::read_to_string(&js).unwrap();
        let id = read_debug_id(&map).unwrap().unwrap();

        std::fs::write(
            &map,
            format!(r#"{{"version":3,"mappings":"AAEA","debugId":"{id}"}}"#),
        )
        .unwrap();
        let stats = inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        assert_eq!(stats.js_restamped, 0);
        assert_eq!(std::fs::read_to_string(&js).unwrap(), js_stamped);
        assert_eq!(read_debug_id(&map).unwrap().unwrap(), id);
    }

    #[test]
    fn a_dry_run_reports_a_restamp_without_writing_it() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        std::fs::write(&map, r#"{"version":3,"mappings":"AAEA"}"#).unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let js_stamped = std::fs::read_to_string(&js).unwrap();
        std::fs::write(&map, r#"{"version":3,"mappings":"AAKA"}"#).unwrap();

        let stats = inject_paths(&[dir.path().to_path_buf()], true).unwrap();
        assert_eq!((stats.js_restamped, stats.maps_updated), (1, 1));
        assert_eq!(std::fs::read_to_string(&js).unwrap(), js_stamped);
        assert_eq!(read_debug_id(&map).unwrap(), None);
    }

    /// Dry-run takes the SAME decision the real run would: a freshly injected
    /// bundle beside a stale stamped map is a Replace (maps_updated 1), not a
    /// "shared map" Conflict.
    #[test]
    fn a_dry_run_replaces_a_stale_map_like_the_real_run() {
        let dir = tempfile::tempdir().unwrap();
        let js = dir.path().join("app.js");
        let map = dir.path().join("app.js.map");
        std::fs::write(&js, "console.log(1)\n").unwrap();
        std::fs::write(
            &map,
            r#"{"version":3,"mappings":"","debug_id":"11111111-1111-5111-8111-111111111111","debugId":"11111111-1111-5111-8111-111111111111"}"#,
        )
        .unwrap();
        let stats = inject_paths(&[dir.path().to_path_buf()], true).unwrap();
        assert_eq!((stats.js_injected, stats.maps_updated), (1, 1));
    }

    /// Bundles are processed in sorted order, so a map two bundles share ends up
    /// with the id of the one that sorts LAST, on every file system.
    #[test]
    fn bundles_are_walked_in_sorted_order() {
        let dir = tempfile::tempdir().unwrap();
        // Created in reverse order, so directory order is not name order by accident.
        for name in ["z.js", "m.js", "a.js"] {
            std::fs::write(
                dir.path().join(name),
                format!("console.log('{name}')\n//# sourceMappingURL=shared.map\n"),
            )
            .unwrap();
        }
        let map = dir.path().join("shared.map");
        std::fs::write(&map, r#"{"version":3,"mappings":""}"#).unwrap();
        inject_paths(&[dir.path().to_path_buf()], false).unwrap();
        let z =
            existing_debug_id(&std::fs::read_to_string(dir.path().join("z.js")).unwrap()).unwrap();
        assert_eq!(read_debug_id(&map).unwrap().unwrap(), z);
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
