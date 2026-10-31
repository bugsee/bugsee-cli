# Unity Mono / .NET: portable PDB to BMF (`--type portable-pdb`)

bugsee-cli#86. This documents the problem, the BMF format (it had no written spec; the ground truth
was the code in `bugsee-dotnet` and the worker), and what the converter does and does not promise.

## The problem

A Unity **Mono** Release build (Android, Mono2x, ARMv7) ships no pdb. The runtime prints a managed
frame as

```
at Bugsee.Sample.Fixture126.FixtureFrames126.Sync (System.String marker, System.Int32 level) [0x00019] in <35bfffb23b81439da9f3a1f0fc9a9a07>:0
```

a method with its parameter types and NAMES, an IL offset, and the module id (MVID, 32 hex digits,
no dashes). The compiler's output has the lines (a portable pdb); the app has the id. The converter
pairs them.

Unity facts this rests on (verified on device, Unity 2021.3.45f1; the fixtures are described in the
issue):

- **Managed stripping Disabled:** no linker runs; the shipped dll is byte-identical to the compiler's
  (same MVID).
- **Low or higher:** the linker rewrites each dll: a NEW MVID, no CodeView debug directory, metadata
  tokens renumbered. Every method it KEEPS keeps its IL (identical size and opcode offsets, in all
  1309 methods of the sample's `Assembly-CSharp`); methods it can prove unused are removed.
- **Development builds** ship their pdbs and the runtime prints `in <path>:<line>` itself. Under Low
  stripping Unity also rewrites the pdbs to match. If the pdbs are deleted afterwards the frames
  print `<shipped-mvid>:0`, like Release, and the IL offsets are the no-pdb ones (not the same as with
  the pdb).
- Unity never produces `.msym` files, and Android Mono does no AOT, so there are no native sequence
  points here (the BMF model has them for Xamarin AOT; see the BMF section).
- **IL2CPP** is a different path (`--type il2cpp-linemap`).

## Why a BMF keyed by signature

Frames are looked up by method SIGNATURE and IL offset. The linker changes tokens and the module id
but not names or the IL of kept methods, so a mapping built from the compiler's pdb, registered under
the shipped module id, is exact for every method that survived. `--shipped-dir` supplies the shipped
dll: its MVID becomes the mapping's id, and each method is checked against it:

- absent from the shipped dll (linked away): left out, it cannot be in a crash;
- present with a different IL size: left out (the shipped body was rewritten, the compiler's offsets
  would be wrong); the run warns with a count;
- otherwise kept.

When the shipped and compiler MVIDs are the same (stripping Disabled) nothing is filtered.

The pdb is first checked against the compiler's dll (CodeView GUID + stamp = the pdb id). A pdb from
another build maps offsets to wrong lines with no other symptom, so it exits 11.

## What is read

`src/symbols/dotnet.rs` is a small ECMA-335 reader: the PE container, the metadata tables, signature
blobs, and the portable-PDB tables (`Document`, `MethodDebugInformation`, sequence points). It
resolves NO other assembly: naming a method needs only its own assembly's tables (a type reference is
named by namespace and name), and a Unity build directory does not have the `UnityEngine.*` assemblies
the original generator wanted. It works over a memory map and never panics on damaged input.

`src/symbols/dotnet_bmf.rs` is the port of `MappingParser` / `BaseParser` from `bugsee-dotnet`. Its
output is checked against that original, run unmodified, on the committed sample
(`tests/fixtures/dotnet/`, with the original's output beside it) and, when the Unity fixtures are
available, on the real builds: identical headers, file tables and method sets (the original keeps
methods in a culture-collated `SortedList`, so only their order differs).

## The BMF format

Everything is big-endian where it matters, and `str` is a `var_int` byte length then UTF-8.
`var_int` is a big-endian 7-bit VLQ in the minimal number of bytes (high bit = "more follows", no
bias): 127 -> `7f`, 128 -> `81 00`, `u32::MAX` -> `8f ff ff ff 7f`.

```
"BMFH"  version(1 byte, =1)
str arch              ".net"
str module id         the MVID, 32 lowercase hex digits ("N" format)
str module name       the assembly name
str module version    major.minor.build.revision
var_int n, n x str    signatures: further ids this module answers to (the MVID; for AOT also
                      MVID+AotId / MVID+EntryAotId)
var_int n, n x str    files, in first-seen order
var_int n, n x method
  method:
    var_int n, n x str    signatures (see below); the first is the key
    var_int n, n x (il_offset, line, file_index)        managed sequence points, ascending il_offset
    var_int n, n x (native_offset, il_offset)           native points (AOT only; empty for Unity)
```

The worker takes the module's images (the ids a crash is matched against) from the file itself: the
module id plus every entry of the signatures list. So both are set to the SHIPPED MVID.

### Signatures

Up to four per method, built from Cecil's names:

1. `Ns.Type.Method (ParamType name, ParamType name)`: type full names, nested types and generic arity
   as in `System.Collections.Generic.List`1`; nesting separators (`+`, `/`) become dots; a generic
   METHOD appends `[T,U]`; a top-level generic instance is reduced to its open type; an anonymous-type
   parameter is `<>f__AnonymousType0`2[<Id>j__TPar,<Label>j__TPar] x`.
2. The same with the compiler's static-lambda holder (`.<>c.`) removed.
3. and 4. For an `async` method's `MoveNext`: 1 and 2 under the ORIGINAL method's name, because the
   runtime prints the async method's own name for those frames.

A method with no sequence points is skipped, except an `async` kickoff stub, whose state machine is
processed in its place. The first signature must be unique (first wins). Sequence points: non-hidden,
sorted by IL offset, start line only.

## Not covered

- `.msym` / AOT (Xamarin iOS): native sequence points, `AotId` ids. The BMF model and writer have the
  fields; the reader does not, and no real AOT sample exists yet.
- Windows PDBs and embedded portable PDBs.
- Mono `.mdb`.
