//! Read-only memory-mapped file bytes, for identity parsing.
//!
//! Identifying a debug file (ELF build-id, Mach-O UUID, PDB GUID) touches only
//! its headers, yet the files are routinely 100+ MB and `std::fs::read` copies
//! every byte onto the heap. Mapping instead keeps the cost to the pages the
//! parser actually reads. This is the ONLY place the crate memory-maps a file, so
//! that `unsafe` lives here (the crate has two other, unrelated `unsafe` uses: the
//! `fork` in `daemon.rs` and `gethostname` in `build_env.rs`).

use std::ops::Deref;
use std::path::Path;

/// The bytes of a file, mapped read-only (empty for an empty file, which cannot
/// be mapped). Derefs to `[u8]`.
pub struct Mapped(Option<memmap2::Mmap>);

impl Deref for Mapped {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        self.0.as_deref().unwrap_or(&[])
    }
}

/// Map `path` read-only.
///
/// Keep the result scoped to the parse and drop it before the file is
/// rewritten or removed (Windows refuses to replace a mapped file).
pub fn map_file(path: &Path) -> std::io::Result<Mapped> {
    let file = std::fs::File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(Mapped(None));
    }
    // SAFETY: the map is read-only. The contract (memmap2's): nothing may truncate
    // or rewrite the file while it is mapped (SIGBUS / undefined behaviour
    // otherwise). It holds for finished build outputs and symbol files, which is all
    // this reads; a file still being written by a linker is not a supported input.
    Ok(Mapped(Some(unsafe { memmap2::Mmap::map(&file) }?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_file_contents_and_handles_an_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"abc").unwrap();
        assert_eq!(&*map_file(&p).unwrap(), b"abc");
        std::fs::write(&p, b"").unwrap();
        assert!(map_file(&p).unwrap().is_empty());
        assert!(map_file(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn a_directory_is_an_error_not_a_panic_or_empty_bytes() {
        // `File::open` succeeds on a directory on Unix; mapping it must not.
        let dir = tempfile::tempdir().unwrap();
        assert!(map_file(dir.path()).is_err());
    }

    #[test]
    fn large_and_page_straddling_files_map_whole() {
        let dir = tempfile::tempdir().unwrap();
        for len in [4095usize, 4096, 4097, 1_000_003] {
            let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
            let p = dir.path().join("f");
            std::fs::write(&p, &data).unwrap();
            let m = map_file(&p).unwrap();
            assert_eq!(m.len(), len);
            assert_eq!(&*m, &data[..]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_a_permission_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        std::fs::write(&p, b"secret").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o000)).unwrap();
        let readable = std::fs::File::open(&p).is_ok(); // root ignores modes
        let r = map_file(&p);
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        if !readable {
            assert_eq!(
                r.err().unwrap().kind(),
                std::io::ErrorKind::PermissionDenied
            );
        }
    }
}
