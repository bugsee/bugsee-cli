//! JS source-map identification for `debug-files upload --type sourcemaps`.
//!
//! A source map is keyed on the server by its *debug-id* — the deterministic
//! UUIDv5 that `bugsee-cli sourcemaps inject` embeds (`debug_id` / `debugId`),
//! falling back to a legacy top-level `uuid`. The id is read back through
//! [`crate::inject::read_debug_id`], whose precedence mirrors the worker's
//! `symbolfiles/sourcemap.py:parse` exactly — so the upload key and the ingest
//! key are guaranteed identical.
//!
//! The wire `hash` is SHA-1 of the raw `.map` bytes (server-side dedup),
//! matching the ELF / ProGuard convention.

use sha1::{Digest as _, Sha1};
use std::path::Path;

use crate::error::Result;
use crate::inject;

/// Read a `.map` file and derive its upload identity.
pub fn identify(path: &Path) -> Result<SourcemapIdentity> {
    let bytes = std::fs::read(path)?;
    Ok(SourcemapIdentity {
        debug_id: inject::read_debug_id(path)?,
        content_sha1_hex: sha1_hex(&bytes),
        size_bytes: bytes.len() as u64,
    })
}

#[derive(Debug, Clone)]
pub struct SourcemapIdentity {
    /// The keying debug-id (`debug_id` / `debugId` / legacy `uuid`), or `None`
    /// when the map carries no id (caller must `sourcemaps inject` first or
    /// pass `--uuid`).
    pub debug_id: Option<String>,
    /// SHA-1 hex of the `.map` bytes — server uses this for dedup.
    pub content_sha1_hex: String,
    /// File size in bytes; logged for diagnostics.
    pub size_bytes: u64,
}

fn sha1_hex(bytes: &[u8]) -> String {
    let digest: [u8; 20] = Sha1::digest(bytes).into();
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_reads_debug_id_and_hashes_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.js.map");
        let body = r#"{"version":3,"debug_id":"did-123","mappings":""}"#;
        std::fs::write(&path, body).unwrap();

        let id = identify(&path).unwrap();
        assert_eq!(id.debug_id.as_deref(), Some("did-123"));
        assert_eq!(id.size_bytes, body.len() as u64);
        // SHA-1 is over the exact file bytes.
        let expected = {
            let digest: [u8; 20] = Sha1::digest(body.as_bytes()).into();
            hex::encode(digest)
        };
        assert_eq!(id.content_sha1_hex, expected);
    }

    #[test]
    fn identify_returns_none_debug_id_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nokey.map");
        std::fs::write(&path, r#"{"version":3,"mappings":""}"#).unwrap();
        assert_eq!(identify(&path).unwrap().debug_id, None);
    }
}
