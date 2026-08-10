//! Unity IL2CPP LineNumberMappings bundle discovery and packing.
//!
//! Bundle layout (one ZIP uploaded as `format: il2cpp-linemap`):
//!   LineNumberMappings.json  (required)
//!   MethodMap.tsv            (optional sibling)
//!   il2cppFileRoot.txt       (optional sibling)
//!   manifest.json            (written by the CLI)

use std::fs;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::error::{config_invalid, input_not_found};

pub const LINE_NUMBER_MAPPINGS: &str = "LineNumberMappings.json";
pub const METHOD_MAP: &str = "MethodMap.tsv";
pub const FILE_ROOT: &str = "il2cppFileRoot.txt";
pub const MANIFEST: &str = "manifest.json";

/// A discovered IL2CPP line-map directory (or a direct path to the JSON).
#[derive(Debug, Clone)]
pub struct LinemapBundle {
    pub json_path: PathBuf,
    pub method_map: Option<PathBuf>,
    pub file_root: Option<PathBuf>,
}

/// Discover `LineNumberMappings.json` under `paths` (file or directory walk).
pub fn discover(paths: &[PathBuf]) -> Vec<LinemapBundle> {
    let mut found = Vec::new();
    for path in paths {
        if path.is_file() {
            if is_linemap_json(path) {
                if let Some(bundle) = bundle_from_json(path) {
                    found.push(bundle);
                }
            }
            continue;
        }
        if !path.is_dir() {
            continue;
        }
        for entry in WalkDir::new(path).into_iter().filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.is_file() && is_linemap_json(p) {
                if let Some(bundle) = bundle_from_json(p) {
                    found.push(bundle);
                }
            }
        }
    }
    found.sort_by(|a, b| a.json_path.cmp(&b.json_path));
    found.dedup_by(|a, b| a.json_path == b.json_path);
    found
}

fn is_linemap_json(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()) == Some(LINE_NUMBER_MAPPINGS)
}

fn bundle_from_json(json_path: &Path) -> Option<LinemapBundle> {
    let dir = json_path.parent()?.to_path_buf();
    let method_map = {
        let p = dir.join(METHOD_MAP);
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };
    let file_root = {
        let p = dir.join(FILE_ROOT);
        if p.is_file() {
            Some(p)
        } else {
            None
        }
    };
    Some(LinemapBundle {
        json_path: json_path.to_path_buf(),
        method_map,
        file_root,
    })
}

/// Parse `--uuid` values: comma-separated and/or repeated spellings.
pub fn parse_uuids(raw: &[String]) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for item in raw {
        for part in item.split(',') {
            let trimmed = part.trim();
            if !trimmed.is_empty() {
                out.push(trimmed.to_string());
            }
        }
    }
    if out.is_empty() {
        // Same class as `--type elf` missing `--uuid` (exit 20 / config_invalid):
        // caller-supplied identity is configuration, not a malformed input file.
        return Err(config_invalid(
            "--uuid is required for --type il2cpp-linemap (one or more IL2CPP \
             module build-ids / Mach-O UUIDs; comma-separate for multi-ABI)",
        ));
    }
    Ok(out)
}

/// Build a manifest.json body for the upload ZIP.
///
/// `items` lists only entries that are actually packed (plus the required
/// LineNumberMappings.json and this manifest itself is not listed).
pub fn packed_item_names(has_method_map: bool, has_file_root: bool) -> Vec<&'static str> {
    let mut items = vec![LINE_NUMBER_MAPPINGS];
    if has_method_map {
        items.push(METHOD_MAP);
    }
    if has_file_root {
        items.push(FILE_ROOT);
    }
    items
}

pub fn manifest_json(uuids: &[String], target: Option<&str>, items: &[&str]) -> String {
    serde_json::json!({
        "format": "il2cpp-linemap",
        "target": target.unwrap_or("unknown"),
        "images": uuids.iter().map(|u| serde_json::json!({
            "arch": "unknown",
            "uuid": u,
        })).collect::<Vec<_>>(),
        "items": items.iter().map(|e| serde_json::json!({ "entry": e })).collect::<Vec<_>>(),
    })
    .to_string()
}

/// Require that at least one bundle was found.
pub fn require_bundles(paths: &[PathBuf], bundles: &[LinemapBundle]) -> anyhow::Result<()> {
    if bundles.is_empty() {
        return Err(input_not_found(format!(
            "no {} found under: {}",
            LINE_NUMBER_MAPPINGS,
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    Ok(())
}

/// Read optional file-root override or sibling file contents (for logging).
pub fn read_file_root(
    bundle: &LinemapBundle,
    override_path: Option<&Path>,
) -> anyhow::Result<Option<String>> {
    if let Some(p) = override_path {
        let s = fs::read_to_string(p).map_err(|e| {
            input_not_found(format!(
                "--il2cpp-root path unreadable ({}): {}",
                p.display(),
                e
            ))
        })?;
        return Ok(Some(s.trim().to_string()));
    }
    if let Some(ref p) = bundle.file_root {
        let s = fs::read_to_string(p).unwrap_or_default();
        return Ok(Some(s.trim().to_string()));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_json_and_siblings() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/il2cpp-linemap/android");
        let found = discover(&[root]);
        assert_eq!(found.len(), 1);
        assert!(found[0].method_map.is_some());
        assert!(found[0].file_root.is_some());
    }

    #[test]
    fn parse_uuids_comma_and_multi() {
        let got = parse_uuids(&["aaa,bbb".to_string(), "ccc".to_string()]).unwrap();
        assert_eq!(got, vec!["aaa", "bbb", "ccc"]);
    }

    #[test]
    fn parse_uuids_empty_errors() {
        assert!(parse_uuids(&[]).is_err());
        assert!(parse_uuids(&["".to_string(), "  ".to_string()]).is_err());
    }

    #[test]
    fn manifest_items_only_list_packed_siblings() {
        let json = manifest_json(
            &["u1".into()],
            Some("android"),
            &packed_item_names(false, true),
        );
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entries: Vec<&str> = v["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|i| i["entry"].as_str().unwrap())
            .collect();
        assert_eq!(
            entries,
            vec!["LineNumberMappings.json", "il2cppFileRoot.txt"]
        );
        assert!(!entries.contains(&"MethodMap.tsv"));
    }
}
