//! Symbol / debug-file discovery, identification, and normalization.
//!
//! Submodules handle format-specific concerns:
//!   - `proguard`    — R8 / ProGuard mapping files (v0.1)
//!   - `elf`         — NDK native-debug-symbols zip pass-through
//!     (v0.1 pass-through; per-`.so` build-id extraction TBD)
//!   - `dsym`        — Apple Mach-O dSYM bundles (v0.1: single bundle;
//!     multi-bundle directory walk + BCSymbolMap support TBD)
//!   - `pdb`         — Windows PDB (MSF container); identity is the debug id
//!     (GUID + age), matching the worker's `symbolfiles/pdb.py`. PE binaries
//!     themselves are still [TODO]
//!   - `rust`        — Cargo-project discovery ACROSS the three above: a Rust
//!     target's symbols are a `.dSYM`, a `.pdb`, or the ELF itself depending on
//!     the triple, so this classifies by container magic (not host OS) and
//!     diagnoses the build settings that silently produce unusable symbols
//!   - `portable_pdb` — Portable PDB (.NET / MAUI managed code)            [TODO]
//!   - `breakpad`    — Breakpad ASCII symbols                              [TODO]
//!   - `jvm`         — JVM source-context bundles                          [TODO]
//!
//! All formats normalize to a `(debug_id, content_hash)` pair. The
//! wire format for upload is ZIP-with-Zstd entries (see `crate::compress`)
//! keyed by debug-id on the server.

pub mod dsym;
pub mod elf;
pub mod il2cpp_linemap;
pub mod pdb;
pub mod proguard;
pub mod rust;
pub mod sourcemap;

// Hand-assembled Mach-O fixtures shared by the dSYM + IPA unit tests.
#[cfg(test)]
pub(crate) mod test_macho;
