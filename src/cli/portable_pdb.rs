//! `debug-files upload --type portable-pdb` and `debug-files convert --to bmf`: .NET assemblies and
//! their portable PDBs, turned into Bugsee mapping files ([`crate::symbols::bmf`]).
//!
//! The use case is Unity's Mono scripting backend, where a Release build ships NO pdb and the
//! runtime prints managed frames with only a method, an IL offset and the module id
//! (`at Ns.Type.Method (args) [0x0000b] in <MVID>:0`). The compiler's own output (a dll and its
//! pdb) has the lines; the app has the module id. This pairs the two:
//!
//! - the INPUT is the compiler's output: `paths` (directories scanned recursively, or `.dll` /
//!   `.pdb` files), each `X.dll` needing its `X.pdb` beside it;
//! - `--shipped-dir` is where the app's own copies of those assemblies are (Unity's
//!   `ManagedStripped`, or the `Managed` folder of an unpacked APK). Its MVID is the one the
//!   runtime prints, so it becomes the mapping's id. Left out, the compiler's assemblies are
//!   taken to be what ships (Unity with managed stripping disabled).
//!
//! Everything is converted BEFORE anything is sent, so a pdb that does not belong to its assembly
//! fails the run with nothing uploaded.

use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

use crate::compress::{self, Strategy, ZipEntry};
use crate::error::{input_invalid, input_not_found, Error};
use crate::symbols::dotnet::{self, Assembly, Metadata};
use crate::symbols::dotnet_bmf::{self, Pairing};
use crate::symbols::{elf, mapped};
use crate::upload::http::{self, RetryPolicy};
use crate::upload::presigned;

/// One assembly to convert.
#[derive(Debug, PartialEq)]
pub struct Pair {
    pub dll: PathBuf,
    pub pdb: PathBuf,
    /// The app's copy of the assembly, when the build rewrites assemblies.
    pub shipped: Option<PathBuf>,
}

/// A converted assembly, written to disk.
#[derive(Debug)]
pub struct Converted {
    /// The `<module id>.bmf` file.
    pub bmf: PathBuf,
    pub module_id: String,
    pub module_name: String,
}

fn is_dll(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dll"))
}

fn sibling_pdb(dll: &Path) -> Option<PathBuf> {
    let pdb = dll.with_extension("pdb");
    pdb.is_file().then_some(pdb)
}

/// Find the assemblies (with their pdbs) under `paths`.
///
/// A directory is walked recursively without following symlinks; a dll with no pdb beside it is
/// skipped (Unity's own `UnityEngine.*` and the BCL ship no pdb), and so is a file that is not a
/// .NET assembly. A path NAMING a file is stricter: it must have its pdb.
pub fn discover(paths: &[PathBuf], shipped_dir: Option<&Path>) -> anyhow::Result<Vec<Pair>> {
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new();
    for path in paths {
        if path.is_dir() {
            for entry in WalkDir::new(path).follow_links(false).sort_by_file_name() {
                let entry = entry
                    .map_err(|e| input_not_found(format!("cannot read {}: {e}", path.display())))?;
                if entry.file_type().is_file() && is_dll(entry.path()) {
                    match sibling_pdb(entry.path()) {
                        Some(pdb) => found.push((entry.path().to_path_buf(), pdb)),
                        None => tracing::debug!(
                            dll = %entry.path().display(),
                            "no .pdb beside it, skipped"
                        ),
                    }
                }
            }
        } else if path.is_file() {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let dll = match ext.as_str() {
                "dll" => path.clone(),
                "pdb" => path.with_extension("dll"),
                _ => {
                    return Err(input_invalid(format!(
                        "{} is neither a .dll nor a .pdb",
                        path.display()
                    )))
                }
            };
            let pdb = if ext == "pdb" {
                path.clone()
            } else {
                dll.with_extension("pdb")
            };
            if !dll.is_file() || !pdb.is_file() {
                return Err(input_not_found(format!(
                    "need both {} and {}",
                    dll.display(),
                    pdb.display()
                )));
            }
            found.push((dll, pdb));
        } else {
            return Err(input_not_found(format!(
                "path does not exist: {}",
                path.display()
            )));
        }
    }
    if found.is_empty() {
        return Err(input_not_found(format!(
            "no .NET assemblies with a .pdb found under: {}",
            paths
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    if let Some(dir) = shipped_dir {
        if !dir.is_dir() {
            return Err(input_not_found(format!(
                "--shipped-dir is not a directory: {}",
                dir.display()
            )));
        }
    }
    Ok(found
        .into_iter()
        .map(|(dll, pdb)| {
            let shipped = shipped_dir.and_then(|d| dll.file_name().map(|n| d.join(n)));
            Pair { dll, pdb, shipped }
        })
        .collect())
}

fn wrap(path: &Path, what: &str, e: Error) -> anyhow::Error {
    match e {
        Error::InputInvalid(m) => input_invalid(format!("{} ({what}): {m}", path.display())),
        other => other.into(),
    }
}

/// Convert one pair into `out_dir/<module id>.bmf`. `None` when the pair is skipped (not an
/// assembly, no methods with lines, or its shipped copy is absent); a damaged or mismatched input
/// is an error.
pub fn convert_pair(pair: &Pair, out_dir: &Path) -> anyhow::Result<Option<Converted>> {
    let dll_map = mapped::map_file(&pair.dll)
        .map_err(|e| input_not_found(format!("cannot read {}: {e}", pair.dll.display())))?;
    let asm = match Assembly::parse(&dll_map) {
        Ok(a) => a,
        Err(e) if dotnet::is_not_an_assembly(&e) => {
            tracing::debug!(dll = %pair.dll.display(), "not a .NET assembly, skipped");
            return Ok(None);
        }
        Err(e) => return Err(wrap(&pair.dll, "assembly", e)),
    };

    let pdb_map = mapped::map_file(&pair.pdb)
        .map_err(|e| input_not_found(format!("cannot read {}: {e}", pair.pdb.display())))?;
    let pdb = Metadata::parse(&pdb_map).map_err(|e| wrap(&pair.pdb, "pdb", e))?;

    let shipped_map = match &pair.shipped {
        Some(p) if !p.is_file() => {
            tracing::warn!(
                assembly = %pair.dll.display(),
                expected = %p.display(),
                "no copy of this assembly in --shipped-dir (removed by the build?), skipped"
            );
            return Ok(None);
        }
        Some(p) => Some(
            mapped::map_file(p)
                .map_err(|e| input_not_found(format!("cannot read {}: {e}", p.display())))?,
        ),
        None => None,
    };
    let shipped = match (&shipped_map, &pair.shipped) {
        (Some(m), Some(p)) => Some(Assembly::parse(m).map_err(|e| wrap(p, "shipped assembly", e))?),
        _ => None,
    };

    match dotnet_bmf::check_pairing(&asm, &pdb).map_err(|e| wrap(&pair.pdb, "pdb", e))? {
        Pairing::Verified => {}
        Pairing::Unverifiable => tracing::warn!(
            assembly = %pair.dll.display(),
            "the assembly names no pdb (no CodeView entry), so the pdb could not be checked against it"
        ),
    }

    let converted = dotnet_bmf::convert(&asm, &pdb, shipped.as_ref())
        .map_err(|e| wrap(&pair.dll, "assembly", e))?;
    if converted.bmf.methods.is_empty() {
        tracing::info!(dll = %pair.dll.display(), "no methods with line information, skipped");
        return Ok(None);
    }
    let stats = converted.stats.clone();
    let header = converted.bmf.header.clone();

    fs::create_dir_all(out_dir)?;
    let bmf_path = out_dir.join(format!("{}.bmf", header.module_id));
    {
        let mut out = BufWriter::new(fs::File::create(&bmf_path)?);
        converted.bmf.write(&mut out)?;
        std::io::Write::flush(&mut out)?;
    }

    tracing::info!(
        assembly = %header.module_name,
        module_id = %header.module_id,
        methods = stats.methods,
        dropped_linked_away = stats.dropped_linked_away,
        dropped_il_mismatch = stats.dropped_il_mismatch,
        "converted"
    );
    if stats.dropped_il_mismatch > 0 {
        tracing::warn!(
            assembly = %header.module_name,
            methods = stats.dropped_il_mismatch,
            "these methods were rewritten by the build (their IL differs from the compiler's), \
             so frames in them will not get line numbers"
        );
    }
    if stats.damaged > 0 {
        tracing::warn!(
            assembly = %header.module_name,
            methods = stats.damaged,
            "these methods have damaged line information in the pdb and were left out"
        );
    }
    Ok(Some(Converted {
        bmf: bmf_path,
        module_id: header.module_id,
        module_name: header.module_name,
    }))
}

/// Convert every discovered pair; fail (before any upload) on the first damaged one.
fn convert_all(pairs: &[Pair], out_dir: &Path) -> anyhow::Result<Vec<Converted>> {
    let mut out: Vec<Converted> = Vec::new();
    for pair in pairs {
        if let Some(c) = convert_pair(pair, out_dir)? {
            // Two inputs with one module id would overwrite each other's file.
            if out.iter().any(|o| o.module_id == c.module_id) {
                return Err(input_invalid(format!(
                    "two assemblies under the given paths have the module id {} ({} is the second)",
                    c.module_id,
                    pair.dll.display()
                )));
            }
            out.push(c);
        }
    }
    if out.is_empty() {
        return Err(input_not_found(
            "none of the assemblies found had line information to convert (are the pdbs \
             portable PDBs, and does --shipped-dir hold the shipped copies?)",
        ));
    }
    Ok(out)
}

/// `debug-files convert --to bmf`: write `<module id>.bmf` for each assembly into `output`.
pub fn run_convert(
    input: &[PathBuf],
    shipped_dir: Option<&Path>,
    output: &Path,
) -> anyhow::Result<()> {
    if output.exists() && !output.is_dir() {
        return Err(input_invalid(format!(
            "--output must be a directory (one <module id>.bmf is written per assembly): {}",
            output.display()
        )));
    }
    let pairs = discover(input, shipped_dir)?;
    let converted = convert_all(&pairs, output)?;
    tracing::info!(
        assemblies = converted.len(),
        output = %output.display(),
        "convert complete"
    );
    Ok(())
}

/// The registration manifest the worker reads from the bundle (`items[].entry` names the file).
fn manifest_json(c: &Converted, entry: &str, version: &str, build: &str) -> String {
    serde_json::json!({
        "items": [{
            "name": format!("{}.bmf", c.module_name),
            "entry": entry,
            "uuids": [&c.module_id],
            "version": version,
            "build": build,
            "format": "bmf",
            "arch": ".net",
        }],
        "uuid": &c.module_id,
    })
    .to_string()
}

/// `debug-files upload --type portable-pdb`.
#[allow(clippy::too_many_arguments)]
pub async fn run_upload(
    paths: &[PathBuf],
    shipped_dir: Option<&Path>,
    endpoint: &str,
    app_token: &str,
    version: &str,
    build: &str,
    strategy: Strategy,
    force: bool,
    dry_run: bool,
) -> anyhow::Result<()> {
    let pairs = discover(paths, shipped_dir)?;
    let workdir = tempfile::tempdir()?;
    let converted = convert_all(&pairs, &workdir.path().join("bmf"))?;

    let client = if dry_run {
        None
    } else {
        Some(http::build_client()?)
    };

    let (mut uploaded, mut already_existed) = (0u32, 0u32);
    // One upload per assembly, so the server decides "already have it" and `--force` per module.
    for c in &converted {
        let entry = format!("{}.bmf", c.module_id);
        let manifest_path = workdir
            .path()
            .join(format!("{}.manifest.json", c.module_id));
        fs::write(&manifest_path, manifest_json(c, &entry, version, build))?;
        let zip_path = workdir.path().join(format!("{}.zip", c.module_id));
        let entries = [
            ZipEntry::compressed(&entry, &c.bmf),
            ZipEntry::compressed("manifest.json", &manifest_path),
        ];
        let zip_size = compress::pack_entries(&entries, &zip_path, strategy)?;
        let hash = elf::sha1_hex_of_file(&zip_path)?;
        tracing::info!(assembly = %c.module_name, zip_size, ?strategy, sha1 = %hash, "packed");

        if dry_run {
            tracing::info!(assembly = %c.module_name, "dry-run: not uploading");
            uploaded += 1;
            continue;
        }

        let uuids = [c.module_id.clone()];
        let metadata = presigned::Metadata {
            uuid: Some(&c.module_id),
            version,
            build,
            hash: Some(&hash),
            transform: None,
            format: Some("bmf"),
            uuids: Some(&uuids),
            overwrite: if force { Some(true) } else { None },
            format_variant: None,
            replace_if_richer: None,
        };
        let outcome = presigned::upload(
            client.as_ref().expect("client built when not dry-run"),
            RetryPolicy::default(),
            endpoint,
            app_token,
            &metadata,
            &zip_path,
        )
        .await?;
        match outcome {
            presigned::Outcome::Uploaded => {
                tracing::info!(assembly = %c.module_name, module_id = %c.module_id, "uploaded");
                uploaded += 1;
            }
            presigned::Outcome::AlreadyExists => {
                tracing::info!(
                    assembly = %c.module_name,
                    module_id = %c.module_id,
                    "already on server, skipped"
                );
                already_existed += 1;
            }
        }
    }
    tracing::info!(uploaded, already_existed, "portable-pdb upload complete");
    Ok(())
}
