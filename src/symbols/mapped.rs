//! Read-only memory-mapped file bytes, for identity parsing.
//!
//! Identifying a debug file (ELF build-id, Mach-O UUID, PDB GUID) touches only
//! its headers, yet the files are routinely 100+ MB and `std::fs::read` copies
//! every byte onto the heap. Mapping instead keeps the cost to the pages the
//! parser actually reads. This is the ONLY place the crate maps a file, so the
//! `unsafe` lives here.

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
    // SAFETY: the map is read-only; the only hazard is another process
    // truncating the file while it is being parsed, which the build output
    // and symbol files scanned here do not do.
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
}
