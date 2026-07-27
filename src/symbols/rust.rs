//! Rust (Cargo) debug-symbol discovery + build-configuration preflight.
//!
//! A Rust project's symbols are not one format — they are whichever format the
//! *target* uses: an Apple `.dSYM` bundle for `*-apple-*`, a Windows `.pdb` for
//! `*-pc-windows-msvc`, and the ELF binary itself (carrying its GNU build-id)
//! for `*-linux-*` / `*-android`. A developer should not have to know which,
//! nor hand-enumerate paths inside `target/`, so discovery here is
//! **content-based**: every candidate is classified by its container magic, not
//! by the host OS. That falls out correct for cross-compilation, where
//! `target/x86_64-unknown-linux-gnu/release/` on a macOS host holds ELF.
//!
//! The second job is diagnosis. Every one of the three formats has a build
//! setting that, if missing, produces an upload that is *accepted* and then
//! resolves nothing at crash time:
//!
//!   - `[profile.release] debug = 1` — without it rustc emits no DWARF at all.
//!   - `split-debuginfo = "packed"` (Apple) — without it there is no `.dSYM`;
//!     the debug info stays in scattered `.o` files that no upload can carry.
//!   - `-Wl,--build-id` (ELF) — without it the object has no `code_id`, and the
//!     symbol store is keyed on exactly that, so nothing can ever match it.
//!
//! Silence on any of these is the worst outcome: the user believes symbols are
//! uploaded, and only discovers otherwise when a crash arrives unsymbolicated
//! weeks later. So the walk records *near-misses* — a Mach-O with no sibling
//! `.dSYM`, an ELF with no build-id, an object with no DWARF — and the caller
//! turns them into an actionable Cargo snippet.

use std::path::{Path, PathBuf};
use symbolic_debuginfo::Archive;
use walkdir::WalkDir;

/// Cargo-internal directories skipped during a walk.
///
/// `target/<profile>/` is mostly *intermediates*: `deps/` alone holds a
/// hash-suffixed copy of the final binary plus an object for every dependency,
/// and `build/` holds a compiled executable for every build script in the
/// graph. Walking them would register a symbol document per dependency —
/// hundreds of uploads, none of which any crash can reference. Only the
/// profile root (and `examples/`) hold artifacts a user actually ships.
const CARGO_INTERNAL_DIRS: &[&str] = &["deps", "build", "incremental", ".fingerprint"];

/// What a discovered file turned out to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Magic {
    Elf,
    MachO,
    Pdb,
}

/// An ELF binary that can be uploaded: it carries the GNU build-id the symbol
/// store is keyed on.
#[derive(Debug, Clone)]
pub struct ElfCandidate {
    pub path: PathBuf,
    /// GNU build-id, lowercase hex — this file's identity, and the `code_id`
    /// the Rust SDK reports for the module at crash time.
    pub build_id: String,
    pub arch: String,
    /// Whether the object carries DWARF. An ELF with a build-id but no debug
    /// info still *matches* a crash, but resolves only exported symbol names.
    pub has_debug_info: bool,
}

/// Everything a Rust symbol walk turned up — uploadable artifacts first, then
/// the near-misses that drive the preflight advice.
#[derive(Debug, Default)]
pub struct Findings {
    /// Apple `.dSYM` bundles.
    pub dsyms: Vec<PathBuf>,
    /// Windows `.pdb` containers (confirmed by MSF magic).
    pub pdbs: Vec<PathBuf>,
    /// ELF binaries carrying a GNU build-id.
    pub elves: Vec<ElfCandidate>,

    /// Mach-O binaries with no sibling `.dSYM` — `split-debuginfo` is not
    /// `"packed"`, so the debug info was never collected into a bundle.
    pub macho_without_dsym: Vec<PathBuf>,
    /// ELF binaries with no GNU build-id — unmatchable at crash time.
    pub elf_without_build_id: Vec<PathBuf>,
    /// Uploadable artifacts that carry no DWARF — `debug` is 0.
    pub without_debug_info: Vec<PathBuf>,
}

impl Findings {
    /// Whether anything at all can be uploaded.
    pub fn is_empty(&self) -> bool {
        self.dsyms.is_empty() && self.pdbs.is_empty() && self.elves.is_empty()
    }

    pub fn uploadable_count(&self) -> usize {
        self.dsyms.len() + self.pdbs.len() + self.elves.len()
    }
}

/// Read a file's leading bytes and classify its container.
///
/// Cheap gate before the (potentially very expensive) full parse: a release
/// binary with DWARF can be hundreds of megabytes, and `target/` holds plenty
/// of files that are not objects at all (`.d` depfiles, `.rlib` archives,
/// fingerprints). Returns `None` for anything unrecognized.
fn sniff(path: &Path) -> Option<Magic> {
    use std::io::Read as _;
    let mut head = [0u8; 4];
    let mut f = std::fs::File::open(path).ok()?;
    if f.read_exact(&mut head).is_err() {
        return None;
    }
    match head {
        [0x7f, b'E', b'L', b'F'] => Some(Magic::Elf),
        // Mach-O thin (32/64, both endiannesses) and fat/universal.
        [0xfe, 0xed, 0xfa, 0xce]
        | [0xfe, 0xed, 0xfa, 0xcf]
        | [0xce, 0xfa, 0xed, 0xfe]
        | [0xcf, 0xfa, 0xed, 0xfe]
        | [0xca, 0xfe, 0xba, 0xbe]
        | [0xbe, 0xba, 0xfe, 0xca] => Some(Magic::MachO),
        // "Micr" — narrows to a PDB candidate; crate::symbols::pdb confirms
        // against the full MSF magic.
        [b'M', b'i', b'c', b'r'] => Some(Magic::Pdb),
        _ => None,
    }
}

/// Structural test for an Apple `.dSYM` bundle (a directory, not a file).
fn is_dsym_bundle(p: &Path) -> bool {
    p.is_dir()
        && p.extension().and_then(|e| e.to_str()) == Some("dSYM")
        && p.join("Contents").join("Resources").join("DWARF").is_dir()
}

/// Parse an ELF's identity: `(build_id, arch, has_debug_info)`.
///
/// Uses the same `symbolic` major the worker parses uploads with, so the
/// build-id computed here is byte-identical to the one the symbol store keys
/// on — and to the `code_id` the Rust SDK reports for that module at crash
/// time. All three agreeing is what makes symbolication resolve.
fn parse_elf(path: &Path) -> Option<(Option<String>, String, bool)> {
    let bytes = std::fs::read(path).ok()?;
    let archive = Archive::parse(&bytes).ok()?;
    let obj = archive.objects().next()?.ok()?;
    Some((
        obj.code_id().map(|c| c.as_str().to_owned()),
        obj.arch().name().to_owned(),
        obj.has_debug_info(),
    ))
}

/// Walk `paths` and classify every Rust debug artifact found.
///
/// An explicitly-passed file or `.dSYM` is trusted as-is (so a caller pointing
/// at one specific artifact gets a clear parse error rather than a silent
/// skip); directories are walked with the Cargo intermediates filtered out.
pub fn discover(paths: &[PathBuf]) -> Findings {
    let mut f = Findings::default();
    let mut seen = std::collections::HashSet::new();

    for p in paths {
        // Explicit `.dSYM` bundle — take it even if malformed.
        if p.is_dir() && p.extension().and_then(|e| e.to_str()) == Some("dSYM") {
            if seen.insert(p.clone()) {
                f.dsyms.push(p.clone());
            }
            continue;
        }
        if p.is_file() {
            classify_file(p, &mut f, &mut seen, true);
            continue;
        }
        if !p.is_dir() {
            tracing::warn!(path = %p.display(), "path does not exist; skipping");
            continue;
        }

        let mut it = WalkDir::new(p).into_iter().filter_entry(|e| {
            // Never filter the root itself, else the walk yields nothing.
            if e.depth() == 0 || !e.file_type().is_dir() {
                return true;
            }
            let name = e.file_name().to_string_lossy();
            !CARGO_INTERNAL_DIRS.contains(&name.as_ref())
        });

        while let Some(entry) = it.next() {
            let Ok(entry) = entry else { continue };
            let ep = entry.path();
            let ft = entry.file_type();

            // Symlinks are load-bearing here, not an edge case: Cargo builds the
            // real `.dSYM` inside `deps/` and publishes the profile-root one as a
            // SYMLINK to it. `walkdir` does not follow symlinks, so treating this
            // entry as "not a directory" loses the bundle entirely — and since
            // `deps/` is skipped, the whole build looks unconfigured. That
            // misfires the preflight in the worst direction: telling a correctly
            // configured project to set `split-debuginfo`, which it already has.
            if ft.is_dir() || ft.is_symlink() {
                if ep.extension().and_then(|x| x.to_str()) == Some("dSYM") {
                    if is_dsym_bundle(ep) && seen.insert(ep.to_path_buf()) {
                        f.dsyms.push(ep.to_path_buf());
                    }
                    // The bundle is the upload unit. Descending into it would
                    // surface its inner DWARF Mach-O as a "binary with no
                    // .dSYM" — advising `split-debuginfo` at the exact moment
                    // it is already correct. (`filter_entry` cannot express
                    // this: returning false there drops the bundle itself.)
                    // Only meaningful for a real directory: `skip_current_dir`
                    // acts on the last yielded DIRECTORY, so calling it for a
                    // symlink would skip the rest of the parent instead.
                    if ft.is_dir() {
                        it.skip_current_dir();
                    }
                    continue;
                }
                if ft.is_dir() {
                    continue;
                }
                // A symlink to a regular file — classify what it points at.
                if ep.is_file() {
                    classify_file(ep, &mut f, &mut seen, false);
                }
                continue;
            }
            if ft.is_file() {
                classify_file(ep, &mut f, &mut seen, false);
            }
        }
    }

    // A Mach-O whose sibling .dSYM WAS found is not a near-miss. Resolve this
    // after the whole walk: file order is arbitrary, so the binary is often
    // visited before its bundle.
    if !f.macho_without_dsym.is_empty() && !f.dsyms.is_empty() {
        let have: std::collections::HashSet<PathBuf> = f.dsyms.iter().cloned().collect();
        f.macho_without_dsym
            .retain(|bin| !have.contains(&dsym_sibling(bin)));
    }
    f
}

/// The `.dSYM` path Cargo/dsymutil would emit for a binary: a sibling
/// directory with `.dSYM` appended to the full filename (`app` → `app.dSYM`).
fn dsym_sibling(binary: &Path) -> PathBuf {
    let name = binary
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    binary.with_file_name(format!("{name}.dSYM"))
}

/// Classify one regular file into `f`.
///
/// `explicit` marks a path the user named directly: it bypasses the container
/// sniff so a mistyped or truncated artifact produces a real error downstream
/// instead of vanishing from the results.
fn classify_file(
    path: &Path,
    f: &mut Findings,
    seen: &mut std::collections::HashSet<PathBuf>,
    explicit: bool,
) {
    let magic = sniff(path);

    // An explicitly-named `.pdb` is trusted without the MSF confirmation.
    if explicit && magic.is_none() {
        if path.extension().and_then(|e| e.to_str()) == Some("pdb")
            && seen.insert(path.to_path_buf())
        {
            f.pdbs.push(path.to_path_buf());
        }
        return;
    }

    match magic {
        Some(Magic::Pdb) => {
            if crate::symbols::pdb::looks_like_pdb(path) && seen.insert(path.to_path_buf()) {
                f.pdbs.push(path.to_path_buf());
            }
        }
        Some(Magic::MachO) => {
            // The upload unit on Apple is the .dSYM, never the binary; a
            // Mach-O is only interesting as evidence that a bundle is missing.
            if !seen.contains(path) && !is_dsym_inner_binary(path) {
                f.macho_without_dsym.push(path.to_path_buf());
            }
        }
        Some(Magic::Elf) => {
            if !seen.insert(path.to_path_buf()) {
                return;
            }
            match parse_elf(path) {
                Some((Some(build_id), arch, has_debug_info)) => {
                    if !has_debug_info {
                        f.without_debug_info.push(path.to_path_buf());
                    }
                    f.elves.push(ElfCandidate {
                        path: path.to_path_buf(),
                        build_id,
                        arch,
                        has_debug_info,
                    });
                }
                Some((None, _, _)) => f.elf_without_build_id.push(path.to_path_buf()),
                // Not a parseable object (a `.rlib` archive member, a stray
                // ELF fragment) — not an error, just not a symbol source.
                None => {
                    seen.remove(path);
                }
            }
        }
        None => {}
    }
}

/// Whether this Mach-O lives inside a `.dSYM` bundle (its DWARF payload).
fn is_dsym_inner_binary(path: &Path) -> bool {
    path.ancestors()
        .any(|a| a.extension().and_then(|e| e.to_str()) == Some("dSYM"))
}

/// Build-configuration advice derived from a walk.
///
/// Returned as lines so the caller can log each at its own level; empty when
/// nothing is wrong. The snippets are exact and paste-able — a user hitting
/// this is usually one `Cargo.toml` stanza away from working symbolication,
/// and making them search documentation for it is the whole failure mode this
/// preflight exists to prevent.
pub fn preflight_advice(f: &Findings) -> Vec<String> {
    let mut out = Vec::new();

    if !f.macho_without_dsym.is_empty() {
        out.push(format!(
            "{} Mach-O binary/binaries found with no sibling .dSYM (e.g. {}). \
             Apple debug info is only collected into a bundle when Cargo is told to \
             pack it; otherwise it stays in per-object files that no upload can carry. \
             Add to Cargo.toml:\n\
             \n    [profile.release]\n    debug = 1\n    split-debuginfo = \"packed\"\n",
            f.macho_without_dsym.len(),
            f.macho_without_dsym[0].display(),
        ));
    }

    if !f.elf_without_build_id.is_empty() {
        out.push(format!(
            "{} ELF binary/binaries found with no GNU build-id (e.g. {}). \
             The symbol store is keyed on the build-id, and the SDK reports each module \
             by that same id at crash time — without one, an upload can never be matched. \
             Link with --build-id, e.g. in .cargo/config.toml:\n\
             \n    [target.'cfg(target_os = \"linux\")']\n    rustflags = [\"-C\", \"link-arg=-Wl,--build-id\"]\n",
            f.elf_without_build_id.len(),
            f.elf_without_build_id[0].display(),
        ));
    }

    if !f.without_debug_info.is_empty() {
        out.push(format!(
            "{} artifact(s) carry no DWARF debug info (e.g. {}). They will upload and match, \
             but resolve only exported symbol names — no file/line. Add to Cargo.toml:\n\
             \n    [profile.release]\n    debug = 1\n",
            f.without_debug_info.len(),
            f.without_debug_info[0].display(),
        ));
    }

    out
}

/// The guidance shown when a walk found nothing uploadable at all.
///
/// This is the most common first-run failure — a user points at a `target/`
/// directory built with stock release settings, which emits no debug info on
/// any platform — so it spells out the whole recipe rather than naming one
/// missing knob.
pub fn nothing_found_advice(paths: &[PathBuf]) -> String {
    let where_ = paths
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "no Rust debug symbols (.dSYM bundle, .pdb, or build-id-bearing ELF) found under: {where_}\n\
         \n\
         A stock `cargo build --release` emits none of these. Configure the build:\n\
         \n    # Cargo.toml\n    [profile.release]\n    debug = 1                      # emit DWARF\n    \
         split-debuginfo = \"packed\"     # macOS/iOS: collect it into a .dSYM\n\
         \n    # .cargo/config.toml — Linux/Android only\n    [target.'cfg(target_os = \"linux\")']\n    \
         rustflags = [\"-C\", \"link-arg=-Wl,--build-id\"]\n\
         \n\
         then rebuild and point this command at target/<triple>/release."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::pdb::fixture::{synth_pdb, MACHINE_AMD64};
    use uuid::Uuid;

    fn touch(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, bytes).unwrap();
        p
    }

    /// The committed aarch64 ELF fixture — a real object with a real build-id.
    fn real_elf_bytes() -> Vec<u8> {
        std::fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/elf/libsymbol1.so"),
        )
        .unwrap()
    }

    fn make_dsym(dir: &Path, name: &str) -> PathBuf {
        let bundle = dir.join(format!("{name}.dSYM"));
        let dwarf = bundle.join("Contents").join("Resources").join("DWARF");
        std::fs::create_dir_all(&dwarf).unwrap();
        std::fs::write(dwarf.join(name), b"\xcf\xfa\xed\xfe stub macho").unwrap();
        bundle
    }

    #[test]
    fn sniff_recognizes_the_three_containers_and_rejects_noise() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            sniff(&touch(tmp.path(), "a.elf", b"\x7fELF\x02\x01\x01\x00")),
            Some(Magic::Elf)
        );
        assert_eq!(
            sniff(&touch(tmp.path(), "b.bin", b"\xcf\xfa\xed\xfe....")),
            Some(Magic::MachO)
        );
        assert_eq!(
            sniff(&touch(tmp.path(), "c.fat", b"\xca\xfe\xba\xbe....")),
            Some(Magic::MachO)
        );
        assert_eq!(
            sniff(&touch(tmp.path(), "d.pdb", b"Microsoft C/C++")),
            Some(Magic::Pdb)
        );
        // Cargo drops plenty of these next to the binary.
        assert_eq!(
            sniff(&touch(tmp.path(), "app.d", b"app: src/main.rs")),
            None
        );
        // Too short to classify.
        assert_eq!(sniff(&touch(tmp.path(), "tiny", b"ab")), None);
    }

    /// The load-bearing filter: `target/release/deps` holds a copy of the final
    /// binary plus an object per dependency, and `build/` an executable per
    /// build script. Walking them would register a symbol document for each.
    #[test]
    fn cargo_intermediate_dirs_are_not_walked() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let elf = real_elf_bytes();
        touch(root, "myapp", &elf);
        touch(root, "deps/myapp-9f8a7b6c", &elf);
        touch(root, "deps/libserde-1234.so", &elf);
        touch(root, "build/somecrate-abc/build-script-build", &elf);
        touch(root, "incremental/foo/bar.o", &elf);
        touch(root, ".fingerprint/x/y", &elf);

        let f = discover(&[root.to_path_buf()]);
        let names: Vec<_> = f
            .elves
            .iter()
            .map(|c| c.path.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(names, vec!["myapp"], "only the profile-root binary");
    }

    #[test]
    fn discovers_all_three_formats_in_one_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        touch(root, "linux-app", &real_elf_bytes());
        make_dsym(root, "mac-app");
        touch(
            root,
            "win-app.pdb",
            &synth_pdb(
                "dfb8e43a-f242-3d73-a453-aeb6a777ef75"
                    .parse::<Uuid>()
                    .unwrap(),
                1,
                1,
                MACHINE_AMD64,
            ),
        );

        let f = discover(&[root.to_path_buf()]);
        assert_eq!(f.elves.len(), 1);
        assert_eq!(f.dsyms.len(), 1);
        assert_eq!(f.pdbs.len(), 1);
        assert_eq!(f.uploadable_count(), 3);
        assert!(!f.is_empty());
        // The real fixture's build-id — the identity all three sides agree on.
        assert_eq!(f.elves[0].build_id, "bca64abfec40dbb631bb8f1c37414472");
        assert_eq!(f.elves[0].arch, "arm64");
    }

    /// A `.dSYM`'s inner Mach-O must not be reported as a binary missing its
    /// bundle — that would advise `split-debuginfo` at the exact moment it is
    /// already correct.
    #[test]
    fn a_dsym_bundle_suppresses_its_own_macho() {
        let tmp = tempfile::tempdir().unwrap();
        make_dsym(tmp.path(), "app");
        let f = discover(&[tmp.path().to_path_buf()]);
        assert_eq!(f.dsyms.len(), 1);
        assert!(
            f.macho_without_dsym.is_empty(),
            "inner DWARF binary is part of the bundle, not a near-miss"
        );
    }

    /// The `split-debuginfo` diagnosis: a Mach-O binary with no sibling bundle.
    #[test]
    fn macho_without_a_sibling_dsym_is_flagged_with_advice() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "app", b"\xcf\xfa\xed\xfe stub macho binary");
        let f = discover(&[tmp.path().to_path_buf()]);
        assert!(f.is_empty(), "a bare Mach-O is not uploadable");
        assert_eq!(f.macho_without_dsym.len(), 1);

        let advice = preflight_advice(&f).join("\n");
        assert!(advice.contains("split-debuginfo = \"packed\""));
        assert!(advice.contains("debug = 1"));
    }

    /// Order-independence: the binary is visited before its bundle here, so the
    /// pairing must be resolved after the walk rather than inline.
    #[test]
    fn a_binary_paired_with_its_bundle_is_not_flagged_whatever_the_walk_order() {
        let tmp = tempfile::tempdir().unwrap();
        touch(tmp.path(), "app", b"\xcf\xfa\xed\xfe stub macho binary");
        make_dsym(tmp.path(), "app");
        let f = discover(&[tmp.path().to_path_buf()]);
        assert_eq!(f.dsyms.len(), 1);
        assert!(
            f.macho_without_dsym.is_empty(),
            "app.dSYM sits next to app; nothing is missing"
        );
        assert!(preflight_advice(&f).is_empty());
    }

    /// The `--build-id` diagnosis. An ELF without one can never be matched, so
    /// it must NOT be uploaded — it would consume quota and resolve nothing.
    #[test]
    fn elf_without_build_id_is_excluded_and_diagnosed() {
        let tmp = tempfile::tempdir().unwrap();
        // Strip the build-id by truncating the fixture's note section: simplest
        // reliable way to get a parseable ELF whose code_id is None is a
        // minimal hand-built header.
        let mut hdr = vec![0u8; 64];
        hdr[..4].copy_from_slice(b"\x7fELF");
        hdr[4] = 2; // ELFCLASS64
        hdr[5] = 1; // little endian
        hdr[6] = 1; // EV_CURRENT
        hdr[16] = 2; // ET_EXEC
        hdr[18] = 0x3e; // EM_X86_64
        touch(tmp.path(), "noid", &hdr);

        let f = discover(&[tmp.path().to_path_buf()]);
        assert!(f.elves.is_empty(), "unmatchable ELF is never uploaded");
        assert_eq!(f.elf_without_build_id.len(), 1);
        assert!(preflight_advice(&f).join("\n").contains("--build-id"));
    }

    /// The real macOS Cargo layout: `cargo build --release` with
    /// `split-debuginfo = "packed"` writes the bundle into `deps/` and leaves a
    /// SYMLINK at the profile root. `walkdir` does not follow symlinks, and
    /// `deps/` is skipped — so mishandling this loses the bundle and then
    /// advises `split-debuginfo` on a project that already sets it. Verified
    /// against a real `cargo build --release`.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_dsym_at_the_profile_root_is_found() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // deps/ holds the real bundle and the hash-suffixed binary.
        let deps = root.join("deps");
        std::fs::create_dir_all(&deps).unwrap();
        make_dsym(&deps, "app-e5e456b0c12af5f0");
        touch(
            &deps,
            "app-e5e456b0c12af5f0",
            b"\xcf\xfa\xed\xfe stub macho",
        );
        // The profile root: the shipped binary + a symlink to the bundle.
        touch(root, "app", b"\xcf\xfa\xed\xfe stub macho");
        symlink(
            deps.join("app-e5e456b0c12af5f0.dSYM"),
            root.join("app.dSYM"),
        )
        .unwrap();

        let f = discover(&[root.to_path_buf()]);
        assert_eq!(f.dsyms.len(), 1, "the symlinked bundle is discovered");
        assert_eq!(f.dsyms[0], root.join("app.dSYM"));
        assert!(
            f.macho_without_dsym.is_empty(),
            "a correctly-configured build must not be told to set split-debuginfo"
        );
        assert!(preflight_advice(&f).is_empty());
    }

    /// A dangling symlink (a stale `target/` after `deps/` was cleaned) is not
    /// a bundle and must not be uploaded — but must not panic either.
    #[test]
    #[cfg(unix)]
    fn a_dangling_dsym_symlink_is_ignored() {
        use std::os::unix::fs::symlink;
        let tmp = tempfile::tempdir().unwrap();
        symlink(
            tmp.path().join("deps/gone.dSYM"),
            tmp.path().join("app.dSYM"),
        )
        .unwrap();
        let f = discover(&[tmp.path().to_path_buf()]);
        assert!(f.is_empty());
    }

    #[test]
    fn explicit_file_paths_are_trusted_without_a_walk() {
        let tmp = tempfile::tempdir().unwrap();
        let elf = touch(tmp.path(), "app", &real_elf_bytes());
        let f = discover(std::slice::from_ref(&elf));
        assert_eq!(f.elves.len(), 1);
        assert_eq!(f.elves[0].path, elf);
    }

    #[test]
    fn an_artifact_reachable_twice_is_reported_once() {
        let tmp = tempfile::tempdir().unwrap();
        let elf = touch(tmp.path(), "app", &real_elf_bytes());
        let f = discover(&[tmp.path().to_path_buf(), elf]);
        assert_eq!(f.elves.len(), 1);
    }

    #[test]
    fn nothing_found_advice_spells_out_the_whole_recipe() {
        let msg = nothing_found_advice(&[PathBuf::from("target/release")]);
        assert!(msg.contains("debug = 1"));
        assert!(msg.contains("split-debuginfo"));
        assert!(msg.contains("--build-id"));
        assert!(msg.contains("target/release"));
    }

    #[test]
    fn a_nonexistent_path_yields_nothing_without_panicking() {
        let f = discover(&[PathBuf::from("/definitely/not/here")]);
        assert!(f.is_empty());
        assert!(preflight_advice(&f).is_empty());
    }
}
