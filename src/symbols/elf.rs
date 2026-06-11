//! NDK / native debug-symbol archive identification.
//!
//! Phase 1 scope is intentionally narrow: the caller hands the CLI an
//! already-packaged `native-debug-symbols.zip` (the artifact AGP writes
//! under `build/outputs/native-debug-symbols/<variant>/`), and the CLI
//! forwards it as-is. We compute SHA-1 of the input bytes for the wire
//! `hash` field (server-side dedup), but the wire `uuid` is the resolved
//! BUILD_UUID the SDK reports at runtime via the asset channel — supplied
//! by the caller through `--uuid`.
//!
//! What this *doesn't* do yet:
//!   - Walk a directory of unstripped `.so` files (AGP's
//!     `build/intermediates/native_debug_metadata/<variant>/out` case).
//!     The Gradle plugin pre-zips that case before invoking the CLI.
//!   - Re-pack with Zstd. The AGP archive is DEFLATE; the backend
//!     accepts it. Re-packing 100+ MB of `.so` files just to swap the
//!     compression method isn't worth the CI time in Phase 1.
//!   - Read individual `.so` build-IDs. Per-library matching is the
//!     backend's job today; if we later switch to debug-id-keyed lookup
//!     (see the `bugsee-cli` README), we'd extract them here via
//!     `symbolic-debuginfo`.

use sha1::{Digest as _, Sha1};
use std::path::Path;

/// Read the archive at `path` and compute its content fingerprint.
pub fn identify(path: &Path) -> std::io::Result<ElfArchiveIdentity> {
    let bytes = std::fs::read(path)?;
    Ok(ElfArchiveIdentity {
        content_sha1_hex: sha1_hex(&bytes),
        size_bytes: bytes.len() as u64,
    })
}

#[derive(Debug, Clone)]
pub struct ElfArchiveIdentity {
    /// SHA-1 hex of the archive bytes — server uses this for dedup.
    pub content_sha1_hex: String,
    /// Archive size in bytes; logged for diagnostics.
    pub size_bytes: u64,
}

fn sha1_hex(bytes: &[u8]) -> String {
    let digest: [u8; 20] = Sha1::digest(bytes).into();
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn identify_returns_sha1_of_full_file_bytes() {
        // Use the FIPS-180 test vector for SHA-1("abc"): there's no
        // ambiguity about what the right answer is.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.zip");
        {
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(b"abc").unwrap();
        }
        let id = identify(&path).unwrap();
        assert_eq!(id.size_bytes, 3);
        assert_eq!(
            id.content_sha1_hex,
            "a9993e364706816aba3e25717850c26c9cd0d89d"
        );
    }
}
