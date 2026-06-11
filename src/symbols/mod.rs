//! Symbol / debug-file discovery, identification, and normalization.
//!
//! Submodules handle format-specific concerns:
//!   - `proguard`    — R8 / ProGuard mapping files (v0.1)
//!   - `elf`         — NDK native-debug-symbols zip pass-through
//!     (v0.1 pass-through; per-`.so` build-id extraction TBD)
//!   - `dsym`        — Apple Mach-O dSYM bundles (v0.1: single bundle;
//!     multi-bundle directory walk + BCSymbolMap support TBD)
//!   - `pe_pdb`      — Windows PE binaries and PDB files                   [TODO]
//!   - `portable_pdb` — Portable PDB (.NET / MAUI managed code)            [TODO]
//!   - `breakpad`    — Breakpad ASCII symbols                              [TODO]
//!   - `jvm`         — JVM source-context bundles                          [TODO]
//!
//! All formats normalize to a `(debug_id, content_hash)` pair. The
//! wire format for upload is ZIP-with-Zstd entries (see `crate::compress`)
//! keyed by debug-id on the server.

pub mod dsym;
pub mod elf;
pub mod proguard;
