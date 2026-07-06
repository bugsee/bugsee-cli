//! Test-only synthetic Mach-O builders shared across the dSYM and IPA unit
//! tests.
//!
//! These are hand-assembled byte-for-byte — no committed binary blobs and no
//! host toolchain — so the exact same fixtures parse identically on every CI
//! target. That portability matters specifically here: Mach-O objects cannot be
//! produced by the Linux CI runners' toolchain, so a "compile a real fixture at
//! test time" approach would only work on macOS. Committing pre-built binaries
//! would work but hides the ground-truth identifiers; hand-assembly keeps the
//! expected `LC_UUID`/`cputype` visible in the source next to the assertion.
//!
//! Each builder embeds a known `LC_UUID` and `cputype`; the tests then assert
//! that `symbolic-debuginfo` reads back exactly those values. Because we control
//! the ground truth, this doubles as a cross-tool contract check: it pins that
//! `symbolic` reads the canonical `LC_UUID` load command and the standard
//! `cputype`, which is what guarantees the CLI and the worker's `symbolic`
//! agree on the identity of the same binary.

// `cputype`/`cpusubtype` from mach/machine.h. Stored little-endian inside the
// thin `mach_header_64`, big-endian inside a fat header's `fat_arch`.
pub const CPU_TYPE_X86_64: u32 = 0x0100_0007;
pub const CPU_TYPE_ARM64: u32 = 0x0100_000c;
pub const CPU_SUBTYPE_X86_64_ALL: u32 = 0x0000_0003;
pub const CPU_SUBTYPE_ARM64_ALL: u32 = 0x0000_0000;

/// A minimal valid thin Mach-O 64-bit object carrying a single `LC_UUID` load
/// command. `symbolic-debuginfo`'s `Archive::parse` + `debug_id()`/`arch()`
/// round-trip against exactly these bytes.
pub fn thin_macho(cputype: u32, cpusubtype: u32, uuid: [u8; 16]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(56);
    // mach_header_64 — little-endian fields.
    buf.extend_from_slice(&[0xcf, 0xfa, 0xed, 0xfe]); // MH_MAGIC_64
    buf.extend_from_slice(&cputype.to_le_bytes());
    buf.extend_from_slice(&cpusubtype.to_le_bytes());
    buf.extend_from_slice(&0x0000_000au32.to_le_bytes()); // filetype = MH_DSYM
    buf.extend_from_slice(&1u32.to_le_bytes()); // ncmds = 1
    buf.extend_from_slice(&24u32.to_le_bytes()); // sizeofcmds = 24
    buf.extend_from_slice(&0u32.to_le_bytes()); // flags
    buf.extend_from_slice(&0u32.to_le_bytes()); // reserved
                                                // LC_UUID load command.
    buf.extend_from_slice(&0x0000_001bu32.to_le_bytes()); // cmd = LC_UUID
    buf.extend_from_slice(&24u32.to_le_bytes()); // cmdsize = 24
    buf.extend_from_slice(&uuid);
    buf
}

/// A 32-bit fat (universal) Mach-O wrapping the given `(cputype, cpusubtype,
/// uuid)` thin slices, laid out contiguously right after the fat header.
/// Exercises `symbolic`'s multi-arch iteration — the path a single-arch fixture
/// never reaches.
pub fn fat_macho(slices: &[(u32, u32, [u8; 16])]) -> Vec<u8> {
    // fat_header + fat_arch[] are big-endian (FAT_MAGIC = 0xCAFEBABE).
    let header_len = 8 + slices.len() * 20;
    let thins: Vec<Vec<u8>> = slices
        .iter()
        .map(|(ct, cs, uuid)| thin_macho(*ct, *cs, *uuid))
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&0xcafe_babeu32.to_be_bytes()); // FAT_MAGIC
    out.extend_from_slice(&(slices.len() as u32).to_be_bytes()); // nfat_arch

    let mut offset = header_len as u32;
    for ((ct, cs, _), thin) in slices.iter().zip(thins.iter()) {
        out.extend_from_slice(&ct.to_be_bytes()); // cputype
        out.extend_from_slice(&cs.to_be_bytes()); // cpusubtype
        out.extend_from_slice(&offset.to_be_bytes()); // offset
        out.extend_from_slice(&(thin.len() as u32).to_be_bytes()); // size
        out.extend_from_slice(&0u32.to_be_bytes()); // align (log2) — 1-byte, tightly packed
        offset += thin.len() as u32;
    }
    for thin in &thins {
        out.extend_from_slice(thin);
    }
    out
}
