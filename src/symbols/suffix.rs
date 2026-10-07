//! Caller-supplied file-name suffixes (`debug-files upload --extension`).
//!
//! Every symbol type recognizes its files by a fixed set of names (`.so`,
//! `.map`, `.dSYM`, `mapping*.txt`, …). When a toolchain starts emitting a new
//! spelling — AGP's `SYMBOL_TABLE` level switching `.so` to `.so.sym` is the
//! case that motivated this — those files were silently skipped until the CLI
//! shipped a new release. A caller can now name the extra suffixes itself; they
//! ADD to the built-in names, never replace them.
//!
//! A suffix is matched against the END of the file (or bundle) name rather than
//! against `Path::extension`, which only sees the last component: `.so.sym`
//! must match `libfoo.so.sym`, and `extension()` would report `sym`. Whatever
//! content check a type applies after the name (PDB container magic, ELF
//! build-id, dSYM `DWARF` directory) still applies to a suffix match.

use std::path::Path;

use crate::error::config_invalid;

/// Extra file-name suffixes, each normalized to start with `.`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtraSuffixes(Vec<String>);

impl ExtraSuffixes {
    /// Validate and normalize raw `--extension` values: surrounding whitespace
    /// is trimmed, a missing leading `.` is added (`so.sym` → `.so.sym`), and
    /// duplicates are dropped. A value that is empty, only dots, or contains a
    /// path separator is rejected — it would match far more than intended (an
    /// empty suffix matches every file) or never match a file name at all.
    pub fn parse(raw: &[String]) -> anyhow::Result<Self> {
        let mut out: Vec<String> = Vec::new();
        for value in raw {
            let trimmed = value.trim();
            if trimmed.trim_start_matches('.').is_empty() {
                return Err(config_invalid(format!(
                    "--extension value {value:?} is empty; pass a suffix such as `.so.sym`"
                )));
            }
            if trimmed.contains(['/', '\\']) {
                return Err(config_invalid(format!(
                    "--extension value {value:?} contains a path separator; pass a \
                     file-name suffix such as `.so.sym`, not a path"
                )));
            }
            let suffix = if trimmed.starts_with('.') {
                trimmed.to_string()
            } else {
                format!(".{trimmed}")
            };
            if !out.contains(&suffix) {
                out.push(suffix);
            }
        }
        Ok(Self(out))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The normalized suffixes, for diagnostics.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Whether `name` ends with any suffix (case-sensitive).
    pub fn matches(&self, name: &str) -> bool {
        self.0.iter().any(|s| name.ends_with(s.as_str()))
    }

    /// Whether `name` ends with any suffix, ignoring ASCII case — for types
    /// whose built-in check is itself case-insensitive (`.pdb`).
    pub fn matches_ignore_ascii_case(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        self.0
            .iter()
            .any(|s| name.ends_with(&s.to_ascii_lowercase()))
    }

    /// [`Self::matches`] against a path's final component.
    pub fn matches_path(&self, path: &Path) -> bool {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| self.matches(n))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(values: &[&str]) -> anyhow::Result<ExtraSuffixes> {
        ExtraSuffixes::parse(&values.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn parse_normalizes_leading_dot_whitespace_and_duplicates() {
        let s = parse(&["so.sym", " .so.sym ", ".dbg"]).unwrap();
        assert_eq!(s.as_slice(), [".so.sym", ".dbg"]);
    }

    #[test]
    fn parse_rejects_empty_dot_only_and_path_values_as_config_invalid() {
        for bad in ["", "  ", ".", "..", "a/b", "dir\\x.so"] {
            let err = parse(&[bad]).unwrap_err();
            assert!(
                matches!(
                    err.downcast_ref::<crate::error::Error>(),
                    Some(crate::error::Error::ConfigInvalid(_))
                ),
                "{bad:?} must be rejected as ConfigInvalid, got {err:?}"
            );
        }
    }

    #[test]
    fn matches_whole_multi_part_suffix_not_just_last_extension() {
        let s = parse(&[".so.sym"]).unwrap();
        assert!(s.matches("libfoo.so.sym"));
        assert!(!s.matches("libfoo.sym"));
        assert!(!s.matches("libfoo.so"));
        assert!(!s.matches("libfoo.SO.SYM"), "case-sensitive by default");
        assert!(s.matches_ignore_ascii_case("libfoo.SO.SYM"));
        assert!(s.matches_path(Path::new("arm64-v8a/libfoo.so.sym")));
    }

    #[test]
    fn empty_set_matches_nothing() {
        let s = ExtraSuffixes::default();
        assert!(s.is_empty());
        assert!(!s.matches("anything.so"));
        assert!(!s.matches_ignore_ascii_case("anything.so"));
    }
}
