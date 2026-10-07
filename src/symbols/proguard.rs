//! R8 / ProGuard mapping file identification, hashing, and UUID derivation.
//!
//! Backward compatibility note: the UUID scheme MUST match the existing
//! Android Gradle plugin's `BugseeBuildIdDeriver.deriveFromMappingFile`,
//! which uses Java's `UUID.nameUUIDFromBytes(mappingFileBytes)`. That is
//! NOT a true RFC 4122 v3 — it's MD5 of the raw bytes with the v3 +
//! IETF-variant bits set, without a namespace.
//!
//! The SDK reports this UUID at runtime via the asset channel
//! (`bugsee_build_id.properties`); if the upload-side UUID and the
//! runtime-side UUID diverge, crash symbolication never resolves.

use md5::{Digest as _, Md5};
use sha1::Sha1;
use std::path::Path;
use uuid::{Builder, Uuid};

/// Heuristic: filename matches what AGP emits for R8/ProGuard.
///
/// Accepts: `mapping.txt`, `mapping-<variant>.txt`. Case-sensitive (Android paths are
/// case-sensitive on real devices; build outputs are always lowercase).
pub fn looks_like_mapping_filename(name: &str) -> bool {
    name == "mapping.txt" || (name.starts_with("mapping") && name.ends_with(".txt"))
}

/// Read a file from disk and compute (debug-id, sha1 hex).
///
/// Both digests are computed in ONE streamed pass (64 KiB buffer), so memory
/// stays flat however large the mapping is: Java's `nameUUIDFromBytes` is just
/// an MD5 of the bytes with the version/variant bits set, and MD5 is
/// incremental like SHA-1.
pub fn identify(path: &Path) -> std::io::Result<MappingIdentity> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path)?;
    let mut md5 = Md5::new();
    let mut sha1 = Sha1::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        md5.update(&buf[..n]);
        sha1.update(&buf[..n]);
    }
    let md5_bytes: [u8; 16] = md5.finalize().into();
    let sha1_bytes: [u8; 20] = sha1.finalize().into();
    Ok(MappingIdentity {
        debug_id: Builder::from_md5_bytes(md5_bytes).into_uuid(),
        content_sha1_hex: hex::encode(sha1_bytes),
    })
}

#[cfg(test)]
pub fn identify_bytes(bytes: &[u8]) -> MappingIdentity {
    MappingIdentity {
        debug_id: java_name_uuid_from_bytes(bytes),
        content_sha1_hex: sha1_hex(bytes),
    }
}

#[derive(Debug, Clone)]
pub struct MappingIdentity {
    /// UUID matching Java `UUID.nameUUIDFromBytes(bytes)` — the BUILD_UUID
    /// the SDK reports at runtime, and the key the server stores under.
    pub debug_id: Uuid,
    /// SHA-1 hex of the content, sent as the `hash` metadata field for
    /// server-side dedup.
    pub content_sha1_hex: String,
}

#[cfg(test)]
/// Replicates Java `UUID.nameUUIDFromBytes(name)`:
///   1. MD5 of input bytes.
///   2. Set the version nibble of byte 6 to 3.
///   3. Set the variant of byte 8 to IETF (10xx).
///
/// `Builder::from_md5_bytes` does steps 2–3 for us.
pub fn java_name_uuid_from_bytes(name: &[u8]) -> Uuid {
    let md5_bytes: [u8; 16] = Md5::digest(name).into();
    Builder::from_md5_bytes(md5_bytes).into_uuid()
}

#[cfg(test)]
fn sha1_hex(bytes: &[u8]) -> String {
    let digest: [u8; 20] = Sha1::digest(bytes).into();
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn java_name_uuid_matches_reference_vectors() {
        // Reference: Java `UUID.nameUUIDFromBytes("hello".getBytes(UTF_8))`
        // = MD5("hello")=5d41402abc4b2a76b9719d911017c592, then byte6 &= 0x0f | 0x30
        // (2a → 3a) and byte8 &= 0x3f | 0x80 (b9 → b9, unchanged because b9 already has IETF bits).
        assert_eq!(
            java_name_uuid_from_bytes(b"hello").to_string(),
            "5d41402a-bc4b-3a76-b971-9d911017c592"
        );
        // Reference: Java `UUID.nameUUIDFromBytes(new byte[0])`
        // = MD5("")=d41d8cd98f00b204e9800998ecf8427e, then byte6 b2 → 32 and byte8 e9 → a9.
        assert_eq!(
            java_name_uuid_from_bytes(b"").to_string(),
            "d41d8cd9-8f00-3204-a980-0998ecf8427e"
        );
    }

    #[test]
    fn streamed_identify_equals_in_memory_identify_across_buffer_boundaries() {
        // Bigger than the 64 KiB buffer and not a multiple of it: a chunking bug
        // would change either digest relative to the one-shot computation.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mapping.txt");
        let data: Vec<u8> = (0..300_001u32).map(|i| (i % 251) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let streamed = identify(&p).unwrap();
        let whole = identify_bytes(&data);
        assert_eq!(streamed.debug_id, whole.debug_id);
        assert_eq!(streamed.content_sha1_hex, whole.content_sha1_hex);
        std::fs::write(&p, b"").unwrap();
        assert_eq!(
            identify(&p).unwrap().debug_id.to_string(),
            "d41d8cd9-8f00-3204-a980-0998ecf8427e"
        );
    }

    #[test]
    fn sha1_hex_matches_reference() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }

    #[test]
    fn identifies_typical_mapping_filenames() {
        assert!(looks_like_mapping_filename("mapping.txt"));
        assert!(looks_like_mapping_filename("mapping-release.txt"));
        assert!(!looks_like_mapping_filename("MAPPING.TXT"));
        assert!(!looks_like_mapping_filename("notmapping.txt"));
        assert!(!looks_like_mapping_filename("mapping.json"));
    }
}
