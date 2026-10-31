//! Turn a .NET assembly and its portable PDB into a Bugsee mapping file ([`super::bmf`]).
//!
//! This is the Rust port of `MappingParser` / `BaseParser` from `bugsee-dotnet` (the original
//! producer of BMF), minus what a build-time CLI cannot have: the original resolves referenced
//! assemblies through Cecil, this reads the assembly's own metadata only (see [`super::dotnet`]).
//! The output is meant to be byte-identical to the original generator's, so every quirk of its
//! signature spelling is kept on purpose; the worker looks frames up by exactly these strings.
//!
//! The Mono runtime prints a managed frame as `at Ns.Type.Method (type name, ...) [0x0000b] in
//! <MVID>:0`: the method, its parameters, an IL offset, and the module id. A BMF is therefore keyed
//! by the method SIGNATURE and IL offset, never by metadata token, which is what makes it survive
//! Unity's linker: the linker renumbers tokens and gives the shipped dll a new MVID, but keeps
//! names and, for every method it keeps, the IL (see [`Stats::dropped_il_mismatch`]).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use super::bmf::{Bmf, Header, ManagedPoint, Method, ARCH_DOTNET};
use super::dotnet::{Assembly, GenericParams, Metadata, MethodSig, NameContext, Ty};
use crate::error::{Error, Result};

/// `[/.]+<>c[/.]+`: the compiler's static-lambda holder, which the runtime sometimes omits.
static STATIC_LAMBDA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)[/.]+<>c[/.]+").expect("static regex"));
/// `[+/]+`: nesting separators, which the runtime prints as dots.
static NESTING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[+/]+").expect("static regex"));

/// What the conversion kept and what it left out, for the caller to report.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Stats {
    /// Methods written to the BMF.
    pub methods: usize,
    /// Methods the linker removed from the shipped assembly (they cannot appear in a crash).
    pub dropped_linked_away: usize,
    /// Methods whose IL size differs between the compiler's and the shipped assembly: the shipped
    /// method was rewritten, so the compiler's IL offsets would point at the wrong lines.
    pub dropped_il_mismatch: usize,
    /// Methods whose sequence points could not be decoded.
    pub damaged: usize,
}

pub struct Converted {
    pub bmf: Bmf,
    pub stats: Stats,
}

/// Why the pdb is (or is not) known to belong to the compiler's assembly.
#[derive(Debug, PartialEq)]
pub enum Pairing {
    /// The assembly's CodeView entry names this pdb.
    Verified,
    /// The assembly carries no CodeView entry, so nothing could be checked.
    Unverifiable,
}

/// Check a pdb against the assembly the compiler produced beside it. A pdb from another build
/// would map offsets to the wrong lines without any other symptom, so a mismatch is an error.
pub fn check_pairing(compiler: &Assembly, pdb: &Metadata) -> Result<Pairing> {
    let Some(pdb_id) = pdb.pdb_id() else {
        return Err(Error::InputInvalid(
            "the file is not a portable PDB (no #Pdb stream); Windows (MSF) PDBs are not supported"
                .into(),
        ));
    };
    match &compiler.codeview {
        None => Ok(Pairing::Unverifiable),
        Some(cv) if cv.pdb_id() == pdb_id => Ok(Pairing::Verified),
        Some(cv) => Err(Error::InputInvalid(format!(
            "the pdb does not belong to the assembly: the assembly wants pdb id {}, the pdb is {}",
            fmt_id(&cv.pdb_id()),
            fmt_id(&pdb_id)
        ))),
    }
}

fn fmt_id(id: &[u8; 20]) -> String {
    hex::encode(id)
}

/// Build the mapping for `compiler` (the assembly the pdb was built with) so that it matches
/// frames of `shipped` (the assembly in the app) when that is a different file, e.g. after Unity's
/// linker rewrote it. With no `shipped`, the compiler's assembly is what runs.
pub fn convert(
    compiler: &Assembly,
    pdb: &Metadata,
    shipped: Option<&Assembly>,
) -> Result<Converted> {
    let compiler_mvid = compiler
        .metadata
        .mvid()
        .ok_or_else(|| Error::InputInvalid("the assembly has no module id (MVID)".into()))?;
    let module_id = match shipped {
        Some(s) => s
            .metadata
            .mvid()
            .ok_or_else(|| Error::InputInvalid("the shipped assembly has no module id".into()))?,
        None => compiler_mvid.clone(),
    };
    let (module_name, module_version) = compiler
        .metadata
        .assembly_identity()
        .ok_or_else(|| Error::InputInvalid("the assembly has no Assembly table".into()))?;

    // Identical modules (Unity with managed stripping off): nothing was rewritten, no filtering.
    let rewritten = shipped.filter(|_| module_id != compiler_mvid);
    let shipped_index = rewritten.map(ShippedIndex::build);

    let mut walk = Walk::new(&compiler.metadata, pdb);
    walk.run(compiler, shipped_index.as_ref());

    let Walk {
        files,
        methods,
        stats,
        ..
    } = walk;
    Ok(Converted {
        bmf: Bmf {
            header: Header {
                arch: ARCH_DOTNET.to_string(),
                module_id: module_id.clone(),
                module_name,
                module_version,
                signatures: vec![module_id],
            },
            files,
            methods,
        },
        stats,
    })
}

/// First signature -> IL sizes of the shipped methods that have it.
struct ShippedIndex(HashMap<String, Vec<Option<u32>>>);

impl ShippedIndex {
    fn build(shipped: &Assembly) -> ShippedIndex {
        // The shipped assembly is only read for its names and body sizes, never its pdb.
        let md = &shipped.metadata;
        let signer = Signer::new(md);
        let mut map: HashMap<String, Vec<Option<u32>>> = HashMap::new();
        for t in 1..=md.type_count() {
            for m in md.type_methods(t) {
                if let Some(sig) = signer.signatures(m, t, None).into_iter().next() {
                    map.entry(sig).or_default().push(shipped.il_size(m));
                }
            }
        }
        ShippedIndex(map)
    }
}

/// Spells methods the way the runtime and the worker expect.
struct Signer<'a, 'm> {
    md: &'m Metadata<'a>,
    enclosing: Vec<u32>,
    generics: GenericParams,
}

impl<'a, 'm> Signer<'a, 'm> {
    fn new(md: &'m Metadata<'a>) -> Self {
        Signer {
            enclosing: md.enclosing_types(),
            generics: md.generic_params(),
            md,
        }
    }

    fn clean(name: &str, simple: bool) -> String {
        let name = if simple {
            name.to_string()
        } else {
            STATIC_LAMBDA.replace_all(name, ".").into_owned()
        };
        NESTING.replace_all(&name, ".").into_owned()
    }

    /// `GetCleanMemberName`: the name, with `[T,U]` appended for a generic method when asked.
    fn member_name(&self, method: u32, with_generics: bool) -> String {
        let name = self.md.method_name(method).into_owned();
        let params = self.generics.of_method(method);
        if with_generics && !params.is_empty() {
            format!("{name}[{}]", params.join(","))
        } else {
            name
        }
    }

    fn params_string(&self, method: u32, declaring: u32) -> String {
        let Some(MethodSig { params, .. }) = self.md.method_signature(method) else {
            return String::new();
        };
        let type_params = self.generics.of_type(declaring);
        let method_params = self.generics.of_method(method);
        let ctx = NameContext {
            md: self.md,
            enclosing: &self.enclosing,
            type_params: &type_params,
            method_params: &method_params,
        };
        let names = self.md.param_names(method);
        params
            .iter()
            .enumerate()
            .map(|(i, ty)| {
                let name = names.get(&(i as u32 + 1)).map(String::as_str).unwrap_or("");
                if ctx.full_name(ty).contains("f__AnonymousType") {
                    if let Some(spelled) = self.anonymous(ty, name) {
                        return spelled;
                    }
                }
                format!("{} {name}", ctx.reduced_name(ty))
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// An anonymous-type parameter is spelled by its property names, because the runtime prints it
    /// with generic parameters named `<Prop>j__TPar` (see Roslyn's `GeneratedNameKind`).
    fn anonymous(&self, ty: &Ty, name: &str) -> Option<String> {
        let mut t = ty;
        loop {
            t = match t {
                Ty::Generic(open, _) => open,
                Ty::ByRef(i) | Ty::Ptr(i) | Ty::Array(i, _) | Ty::Pinned(i) => i,
                Ty::Mod(i, _, _) => i,
                _ => break,
            };
        }
        let Ty::Def(row) = t else {
            return None;
        };
        let props: Vec<String> = self
            .md
            .property_names(*row)
            .into_iter()
            .map(|p| format!("<{p}>j__TPar"))
            .collect();
        Some(format!(
            "{}[{}] {name}",
            self.md.type_name(*row),
            props.join(",")
        ))
    }

    /// `GetMethodSignatures`: up to four spellings. The first is the key; the others let the
    /// worker's retry attempts (static-lambda holder dropped, async state machine named by the
    /// method that contains it) land on the same entry.
    fn signatures(&self, method: u32, declaring: u32, containing: Option<u32>) -> Vec<String> {
        let params = self.params_string(method, declaring);
        let full = format!(
            "{}.{}",
            self.md.type_full_name(declaring, &self.enclosing),
            self.member_name(method, containing.is_none())
        );
        let mut out = vec![format!("{} ({params})", Self::clean(&full, true))];
        let second = format!("{} ({params})", Self::clean(&full, false));
        if !out.contains(&second) {
            out.push(second);
        }
        if let Some(outer) = containing {
            let outer_declaring = self.md.type_of_method(outer).unwrap_or(declaring);
            let outer_params = self.params_string(outer, outer_declaring);
            let outer_full = format!(
                "{}.{}",
                self.md.type_full_name(outer_declaring, &self.enclosing),
                self.member_name(outer, true)
            );
            for simple in [true, false] {
                let s = format!("{} ({outer_params})", Self::clean(&outer_full, simple));
                if !out.contains(&s) {
                    out.push(s);
                }
            }
        }
        out
    }
}

/// The walk over the assembly's types, producing methods in the original generator's order.
struct Walk<'a, 'm> {
    signer: Signer<'a, 'm>,
    pdb: &'m Metadata<'m>,
    async_machines: HashMap<u32, String>,
    type_index: HashMap<String, u32>,
    nested: HashMap<u32, Vec<u32>>,
    seen: HashSet<String>,
    file_index: HashMap<String, u32>,
    files: Vec<String>,
    methods: Vec<Method>,
    stats: Stats,
}

impl<'a, 'm> Walk<'a, 'm> {
    fn new(md: &'m Metadata<'a>, pdb: &'m Metadata<'m>) -> Self {
        let signer = Signer::new(md);
        let async_machines = md.async_state_machines(&signer.enclosing);
        let type_index = if async_machines.is_empty() {
            HashMap::new()
        } else {
            md.type_index(&signer.enclosing)
        };
        Walk {
            async_machines,
            type_index,
            nested: md.nested_types(),
            signer,
            pdb,
            seen: HashSet::new(),
            file_index: HashMap::new(),
            files: Vec::new(),
            methods: Vec::new(),
            stats: Stats::default(),
        }
    }

    fn run(&mut self, compiler: &Assembly, shipped: Option<&ShippedIndex>) {
        let tops: Vec<u32> = self
            .signer
            .md
            .top_level_types(&self.signer.enclosing)
            .collect();
        self.types(&tops, None, compiler, shipped);
    }

    fn types(
        &mut self,
        types: &[u32],
        containing: Option<u32>,
        compiler: &Assembly,
        shipped: Option<&ShippedIndex>,
    ) {
        for &t in types {
            if self.signer.md.type_name(t) == "<Module>" {
                continue;
            }
            for m in self.signer.md.type_methods(t) {
                let Some(points) = self.pdb.sequence_points(m) else {
                    self.stats.damaged += 1;
                    continue;
                };
                if points.is_empty() {
                    // No sequence points: an `async` method's kickoff stub. Its code lives in the
                    // state machine's MoveNext, so process that type NOW, naming it after this
                    // method (the runtime prints the async method's own name for its frames).
                    let machine = self.async_machines.get(&m).and_then(|n| {
                        self.type_index
                            .get(n.split(',').next().unwrap_or("").trim())
                            .copied()
                    });
                    if let Some(machine) = machine {
                        self.types(&[machine], Some(m), compiler, shipped);
                    }
                    continue;
                }
                self.register(m, t, containing, &points, compiler, shipped);
            }
            if let Some(nested) = self.nested.get(&t).cloned() {
                self.types(&nested, None, compiler, shipped);
            }
        }
    }

    fn register(
        &mut self,
        method: u32,
        declaring: u32,
        containing: Option<u32>,
        points: &[super::dotnet::SeqPoint],
        compiler: &Assembly,
        shipped: Option<&ShippedIndex>,
    ) {
        let signatures = self.signer.signatures(method, declaring, containing);
        let Some(first) = signatures.first() else {
            return;
        };
        // The same method is reached twice for an async one (through its kickoff stub, then as a
        // member of its nested type); the first visit wins, as in the original.
        if self.seen.contains(first) {
            return;
        }
        if let Some(index) = shipped {
            match index.0.get(first) {
                None => {
                    self.stats.dropped_linked_away += 1;
                    return;
                }
                Some(sizes) => {
                    let ours = compiler.il_size(method);
                    if !sizes.contains(&ours) {
                        self.stats.dropped_il_mismatch += 1;
                        return;
                    }
                }
            }
        }
        self.seen.insert(first.clone());

        let mut visible: Vec<&super::dotnet::SeqPoint> =
            points.iter().filter(|p| !p.hidden).collect();
        visible.sort_by_key(|p| p.il_offset);
        let managed = visible
            .into_iter()
            .map(|p| {
                let name = self.pdb.document_name(p.document);
                let next = self.files.len() as u32;
                let index = *self.file_index.entry(name.clone()).or_insert_with(|| next);
                if index == next {
                    self.files.push(name);
                }
                ManagedPoint {
                    il_offset: p.il_offset,
                    line: p.line,
                    file_index: index,
                }
            })
            .collect();
        self.methods.push(Method {
            signatures,
            managed,
            native: Vec::new(),
        });
        self.stats.methods += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::symbols::mapped::map_file;
    use std::path::Path;

    /// Convert `dll` + `pdb` (optionally against a rewritten `shipped` dll) to BMF bytes.
    fn convert_files(dll: &Path, pdb: &Path, shipped: Option<&Path>) -> (Vec<u8>, Stats) {
        let dll = map_file(dll).unwrap();
        let pdb = map_file(pdb).unwrap();
        let asm = Assembly::parse(&dll).unwrap();
        let pdb_md = Metadata::parse(&pdb).unwrap();
        check_pairing(&asm, &pdb_md).unwrap();
        let shipped_map = shipped.map(|p| map_file(p).unwrap());
        let shipped_asm = shipped_map.as_ref().map(|m| Assembly::parse(m).unwrap());
        let out = convert(&asm, &pdb_md, shipped_asm.as_ref()).unwrap();
        let mut bytes = Vec::new();
        out.bmf.write(&mut bytes).unwrap();
        (bytes, out.stats)
    }

    /// Byte-for-byte against the original generator's output, on real Unity builds. Gated on the
    /// fixture checkout, like the reference round trip in `bmf.rs`.
    #[test]
    fn matches_the_original_generator_on_unity_fixtures() {
        let (Ok(root), Ok(golden)) = (
            std::env::var("BUGSEE_UNITY_FIXTURE"),
            std::env::var("BUGSEE_UNITY_GOLDEN"),
        ) else {
            return;
        };
        for (dir, golden_name) in [
            (
                "release-stripping-disabled",
                "release-stripping-disabled.Assembly-CSharp.bmf",
            ),
            (
                "development-stripping-disabled",
                "development-stripping-disabled.Assembly-CSharp.bmf",
            ),
        ] {
            let base = Path::new(&root).join(dir).join("compiler-output");
            let (got, stats) = convert_files(
                &base.join("Assembly-CSharp.dll"),
                &base.join("Assembly-CSharp.pdb"),
                None,
            );
            let want = std::fs::read(Path::new(&golden).join(golden_name)).unwrap();
            assert_eq!(stats.methods, 824, "{dir}");
            // The original keeps methods in a culture-collated `SortedList`; the worker looks them up
            // by signature, so only the SET of methods (and the file table, which follows the walk
            // order) is compared, not their order in the file.
            let mut a = Bmf::read(&mut &got[..]).unwrap();
            let mut b = Bmf::read(&mut &want[..]).unwrap();
            assert_eq!(a.header, b.header, "{dir}: header");
            assert_eq!(a.files, b.files, "{dir}: files");
            a.methods
                .sort_by(|x, y| x.signatures[0].cmp(&y.signatures[0]));
            b.methods
                .sort_by(|x, y| x.signatures[0].cmp(&y.signatures[0]));
            assert_eq!(a.methods.len(), b.methods.len(), "{dir}");
            for (x, y) in a.methods.iter().zip(&b.methods) {
                assert_eq!(x, y, "{dir}");
            }
        }
    }

    /// A much larger assembly (the SDK itself: generics, async, anonymous and nested types), and
    /// the linker-rewritten pairing, against the original generator.
    #[test]
    fn matches_the_original_generator_on_the_matrix_fixtures() {
        let (Ok(root), Ok(golden)) = (
            std::env::var("BUGSEE_UNITY_MATRIX"),
            std::env::var("BUGSEE_UNITY_GOLDEN"),
        ) else {
            return;
        };
        let low = Path::new(&root).join("run1-dev-low");
        let compiler = low.join("compiler-output");
        // Bugsee.Runtime, compiler dll with its own pdb, no linker involved in the comparison.
        let (got, stats) = convert_files(
            &compiler.join("Bugsee.Runtime.dll"),
            &compiler.join("Bugsee.Runtime.pdb"),
            None,
        );
        let want = std::fs::read(Path::new(&golden).join("devlow.Bugsee.Runtime.bmf")).unwrap();
        let mut a = Bmf::read(&mut &got[..]).unwrap();
        let mut b = Bmf::read(&mut &want[..]).unwrap();
        assert_eq!(stats.methods, 1571);
        assert_eq!(a.header, b.header);
        assert_eq!(a.files, b.files);
        a.methods
            .sort_by(|x, y| x.signatures[0].cmp(&y.signatures[0]));
        b.methods
            .sort_by(|x, y| x.signatures[0].cmp(&y.signatures[0]));
        for (x, y) in a.methods.iter().zip(&b.methods) {
            assert_eq!(x, y);
        }
        assert_eq!(a.methods.len(), b.methods.len());

        // The Low-stripped, rewritten dll the device ran: the same compiler pdb, the shipped MVID.
        for (asm, kept) in [("Assembly-CSharp", 824usize), ("Bugsee.Runtime", 0)] {
            let shipped = low.join("shipped-from-apk").join(format!("{asm}.dll"));
            let (bytes, stats) = convert_files(
                &compiler.join(format!("{asm}.dll")),
                &compiler.join(format!("{asm}.pdb")),
                Some(&shipped),
            );
            let bmf = Bmf::read(&mut &bytes[..]).unwrap();
            let mvid = Assembly::parse(&map_file(&shipped).unwrap())
                .unwrap()
                .metadata
                .mvid()
                .unwrap();
            assert_eq!(bmf.header.module_id, mvid);
            assert_eq!(bmf.header.signatures, vec![mvid]);
            assert_eq!(stats.dropped_il_mismatch, 0, "{asm}");
            eprintln!("{asm}: {stats:?}");
            if kept > 0 {
                assert_eq!(stats.methods, kept, "{asm}");
                assert_eq!(stats.dropped_linked_away, 0, "{asm}");
            }
        }
    }

    fn fixture(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/dotnet")
            .join(name)
    }

    fn sorted(mut bmf: Bmf) -> Bmf {
        bmf.methods
            .sort_by(|x, y| x.signatures[0].cmp(&y.signatures[0]));
        bmf
    }

    /// The committed sample (`tests/fixtures/dotnet/BugseeSample.cs`, built with Roslyn) against
    /// the output of the original generator, which is checked in beside it.
    #[test]
    fn the_sample_assembly_matches_the_original_generator() {
        let (bytes, stats) = convert_files(
            &fixture("BugseeSample.dll"),
            &fixture("BugseeSample.pdb"),
            None,
        );
        let got = sorted(Bmf::read(&mut &bytes[..]).unwrap());
        let want = sorted(
            Bmf::read(&mut &std::fs::read(fixture("BugseeSample.oracle.bmf")).unwrap()[..])
                .unwrap(),
        );
        assert_eq!(got.header, want.header);
        assert_eq!(got.files, want.files);
        for (x, y) in got.methods.iter().zip(&want.methods) {
            assert_eq!(x, y);
        }
        assert_eq!(got.methods.len(), want.methods.len());
        assert_eq!(stats.methods, want.methods.len());
    }

    /// A rewritten shipped assembly (here: a rebuild with one method removed and one method's body
    /// changed, standing in for Unity's linker): the mapping carries the SHIPPED module id, and a
    /// method the shipped assembly no longer has, or whose IL changed size, is left out.
    #[test]
    fn a_rewritten_assembly_is_paired_by_signature_and_il_size() {
        let (bytes, stats) = convert_files(
            &fixture("BugseeSample.dll"),
            &fixture("BugseeSample.pdb"),
            Some(&fixture("BugseeSample.rewritten.dll")),
        );
        let bmf = Bmf::read(&mut &bytes[..]).unwrap();
        let shipped_mvid =
            Assembly::parse(&map_file(&fixture("BugseeSample.rewritten.dll")).unwrap())
                .unwrap()
                .metadata
                .mvid()
                .unwrap();
        let compiler_mvid = Assembly::parse(&map_file(&fixture("BugseeSample.dll")).unwrap())
            .unwrap()
            .metadata
            .mvid()
            .unwrap();
        assert_ne!(shipped_mvid, compiler_mvid);
        assert_eq!(bmf.header.module_id, shipped_mvid);
        assert_eq!(bmf.header.signatures, vec![shipped_mvid]);
        assert_eq!(stats.dropped_il_mismatch, 1);
        // `Local` and the local function it contained.
        assert_eq!(stats.dropped_linked_away, 2);
        assert_eq!(stats.methods, 21 - 3);
        let firsts: Vec<&str> = bmf
            .methods
            .iter()
            .map(|m| m.signatures[0].as_str())
            .collect();
        assert!(!firsts.iter().any(|s| s.starts_with("Demo.App.Plain.Add ")));
        assert!(!firsts.iter().any(|s| s.contains("<Local>g__Twice")));
        assert!(firsts
            .iter()
            .any(|s| s.starts_with("Demo.App.Plain.op_Addition ")));
    }

    #[test]
    fn a_pdb_from_another_build_is_refused() {
        // The rewritten dll's own pdb does not exist here, so pair the dll with a pdb it never had:
        // the pdb id check is what stands between a wrong pairing and silently wrong line numbers.
        let dll = map_file(&fixture("BugseeSample.rewritten.dll")).unwrap();
        let pdb = map_file(&fixture("BugseeSample.pdb")).unwrap();
        let err = check_pairing(
            &Assembly::parse(&dll).unwrap(),
            &Metadata::parse(&pdb).unwrap(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("does not belong"), "{err}");
        // And the right pairing is verified.
        let dll = map_file(&fixture("BugseeSample.dll")).unwrap();
        assert_eq!(
            check_pairing(
                &Assembly::parse(&dll).unwrap(),
                &Metadata::parse(&pdb).unwrap()
            )
            .unwrap(),
            Pairing::Verified
        );
    }

    /// Run the whole pipeline on possibly damaged bytes: errors are fine, panics are not.
    fn exercise(dll: &[u8], pdb: &[u8]) -> Option<String> {
        let asm = Assembly::parse(dll).ok();
        let pdb_md = Metadata::parse(pdb).ok();
        if let (Some(asm), Some(pdb_md)) = (&asm, &pdb_md) {
            let _ = check_pairing(asm, pdb_md);
            if let Ok(out) = convert(asm, pdb_md, Some(asm)) {
                let mut sink = Vec::new();
                let _ = out.bmf.write(&mut sink);
            }
        }
        asm.and_then(|a| a.metadata.mvid())
    }

    #[test]
    fn a_truncated_assembly_or_pdb_never_panics_or_changes_identity() {
        let dll = std::fs::read(fixture("BugseeSample.dll")).unwrap();
        let pdb = std::fs::read(fixture("BugseeSample.pdb")).unwrap();
        let identity = exercise(&dll, &pdb).expect("the intact sample has an MVID");
        for len in 0..dll.len() {
            // A cut-off assembly may fail to parse, but must never claim another module id.
            if let Some(id) = exercise(&dll[..len], &pdb) {
                assert_eq!(id, identity, "truncated at {len}");
            }
        }
        for len in 0..pdb.len() {
            exercise(&dll, &pdb[..len]);
        }
    }

    #[test]
    fn mutated_assemblies_and_pdbs_never_panic() {
        let dll = std::fs::read(fixture("BugseeSample.dll")).unwrap();
        let pdb = std::fs::read(fixture("BugseeSample.pdb")).unwrap();
        // A fixed xorshift so a failure reproduces.
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for round in 0..3000 {
            let (mut d, mut p) = (dll.clone(), pdb.clone());
            for _ in 0..1 + next() % 6 {
                let target = if next() % 2 == 0 { &mut d } else { &mut p };
                let at = (next() as usize) % target.len();
                target[at] = match next() % 3 {
                    0 => 0xFF,
                    1 => 0x00,
                    _ => next() as u8,
                };
            }
            exercise(&d, &p);
            let _ = round;
        }
    }
}
