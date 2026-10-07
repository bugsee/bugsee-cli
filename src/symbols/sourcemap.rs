//! JS source-map identification for `debug-files upload --type sourcemaps`.
//!
//! A source map is keyed on the server by its *debug-id* — the deterministic
//! UUIDv5 that `bugsee-cli sourcemaps inject` embeds (`debug_id` / `debugId`),
//! falling back to a legacy top-level `uuid`. The id is read back through
//! [`crate::inject::read_debug_id`], whose precedence mirrors the worker's
//! `symbolfiles/sourcemap.py:parse` exactly — so the upload key and the ingest
//! key are guaranteed identical.
//!
//! The wire `hash` is SHA-1 of the raw `.map` bytes, matching the ELF / ProGuard
//! convention (the appserver dedups by uuid + format, not by this hash).

use sha1::{Digest as _, Sha1};
use std::path::Path;

use crate::error::Result;
use crate::inject;

/// Read a `.map` file and derive its upload identity.
///
/// The hash is streamed (64 KiB buffer) and the size comes from the file
/// itself, so identifying a map no longer copies it onto the heap.
pub fn identify(path: &Path) -> Result<SourcemapIdentity> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    let mut size_bytes = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size_bytes += n as u64;
    }
    let digest: [u8; 20] = hasher.finalize().into();
    Ok(SourcemapIdentity {
        debug_id: inject::read_debug_id(path)?,
        content_sha1_hex: hex::encode(digest),
        size_bytes,
    })
}

#[derive(Debug, Clone)]
pub struct SourcemapIdentity {
    /// The keying debug-id (`debug_id` / `debugId` / legacy `uuid`), or `None`
    /// when the map carries no id (caller must `sourcemaps inject` first or
    /// pass `--uuid`).
    pub debug_id: Option<String>,
    /// SHA-1 hex of the `.map` bytes, sent as the wire `hash`. The appserver does
    /// NOT dedup on it today (it matches the declared uuid + format).
    pub content_sha1_hex: String,
    /// File size in bytes; logged for diagnostics.
    pub size_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identify_streams_the_hash_and_size_across_buffer_boundaries() {
        // Bigger than the 64 KiB read buffer and not a multiple of it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.js.map");
        let body = format!(
            r#"{{"version":3,"debug_id":"did-big","mappings":"{}"}}"#,
            "A".repeat(300_001)
        );
        std::fs::write(&path, &body).unwrap();
        let id = identify(&path).unwrap();
        let expected: [u8; 20] = Sha1::digest(body.as_bytes()).into();
        assert_eq!(id.content_sha1_hex, hex::encode(expected));
        assert_eq!(id.size_bytes, body.len() as u64);
        assert_eq!(id.debug_id.as_deref(), Some("did-big"));
    }

    #[test]
    fn identify_still_reports_a_non_utf8_map_as_an_io_error() {
        // `read_debug_id` used `read_to_string`, so invalid UTF-8 was an I/O
        // `InvalidData` error (exit 10), not a JSON error (exit 11). The mapped
        // read must keep that classification.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.js.map");
        std::fs::write(&path, b"{\"debug_id\":\"\xff\xfe\"}").unwrap();
        assert!(matches!(identify(&path), Err(crate::error::Error::Io(_))));
    }

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
