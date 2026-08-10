# Unity IL2CPP line maps & symbolication

**Status:** decisions locked (epic `il2cpp_linenumbermaps`). Implementation lands across
`bugsee-cli`, `appserver`, `worker`, and (last) `cross/bugsee-unity`.  
**Related:** Unity SDK crash/symbols plan (`unity_symbols_and_crashes`); UPM
`cross/bugsee-unity`; worker `crash/managed/unity.py`.

---

## 1. Problem

IL2CPP turns C# → C++ → native code. Crashes surface as native addresses in
`libil2cpp.so` / `UnityFramework`, mangled `Type_Method_mHASH` names, and/or
managed-looking strings without reliable file/line in Release.

Unity emits build-time mapping files that are **not shipped in the player**.
Bugsee already uploads dSYM / ELF / ProGuard / PDB / sourcemaps; this epic adds
first-class support for IL2CPP line maps.

---

## 2. Unity artifacts

### 2.1 `LineNumberMappings.json`

| | |
|---|---|
| **Role** | Maps generated IL2CPP C++ file/line → C# file/line. |
| **Typical path** | `Il2CppOutputProject/Source/il2cppOutput/Symbols/LineNumberMappings.json` |
| **Sibling** | `il2cppFileRoot.txt` — root prefix for paths in the mapping. |
| **Schema** | Nested JSON: `cpp_path → { cs_path → { cpp_line: cs_line } }` (Sentry `symbolic-il2cpp` semantics). Optional `__debug-id__` sentinel is ignored by parsers. |
| **Scope** | One file **per platform IL2CPP build** (whole managed codegen). Not shared Android↔iOS. Not one file per plugin `.so`. |
| **Must match binary** | Wrong map identity must never be applied. |

### 2.2 `MethodMap.tsv`

Maps mangled C++ symbol names → C# method signatures
(`Namespace.Type::Method(args)`). Required for the string/mangled fallback path.

### 2.3 Native symbols (unchanged types)

| Artifact | CLI | Notes |
|---|---|---|
| iOS `.dSYM` | `--type dsym` | Archive-time upload |
| Android ELF / symbols.zip | `--type elf` | Needs line-program / Breakpad line records for primary apply |
| ProGuard/R8 | `--type proguard` | Separate from linemap |

Engine libs (`libunity.so`, etc.) are **not** covered by LineNumberMappings.

---

## 3. Locked decisions

### Apply model

1. **Primary:** native address → dSYM/ELF/Breakpad → **C++ file + line** →
   `LineNumberMappings.json` → C# file/line.
2. **Fallback:** mangled / managed string → `MethodMap.tsv` (line map only when a
   C++ location exists). Existing `unity.py` string parse remains last resort.

### Storage & identity

- **Separate** `SymbolFile` with `format: il2cpp-linemap` — **not** packed inside
  ELF/dSYM zips.
- Bundle ZIP: `LineNumberMappings.json` + `MethodMap.tsv` + `il2cppFileRoot.txt`
  + `manifest.json`.
- Crash lookup starts from module UUID(s) on the frame.
- One linemap document may list **multiple** `images[].uuid` (all Android ABI
  build-ids for `libil2cpp`, or relevant iOS slice UUIDs).
- `version` / `build` / platform = metadata only (not the apply key).
- Mobile v1 modules: Android `libil2cpp.so`; iOS `UnityFramework`.
  `GameAssembly` deferred (desktop).

### Same-UUID coexistence (corrected)

**Not** “like mapping vs breakpad” (those UUIDs diverge after processing).
Requires:

- Dedup / overwrite scoped by **`(uuid, format)`**.
- Crash symbol batch must return **native symbols and** `il2cpp-linemap` for the
  same UUID (multiplicity), not last-wins collapse.

### Path & debug-info contracts

- Worker must preserve **full** cpp paths for IL2CPP modules and normalize with
  `il2cppFileRoot.txt` before LNM lookup (iOS basename-stripping is incompatible).
- Primary path needs DWARF / Breakpad **line** records. SYMBOL_TABLE-only /
  PUBLIC-only → native names only; LNM skipped (observable in diagnostics).

### SDK / event payload

| Event class | Required payload | Apply path | Dependency |
|---|---|---|---|
| Native fatal (iOS) | module UUID + PC | primary | dSYM with line info + linemap |
| Native fatal (Android) | module UUID + PC | primary | **NDK re-enabled** + ELF/Breakpad line info + linemap |
| Managed fatal (IL2CPP) | Prefer addresses; else mangled/managed strings | primary if addrs; else MethodMap | ExceptionPipeline / bridge |
| String-only / Phase A stacks | strings | fallback / partial | `unity.py` |

### Failure policy (CLI / collector)

| Situation | Behavior |
|---|---|
| Map not found during auto-discovery | Warn + continue dSYM/ELF/ProGuard; skip linemap |
| `--no-il2cpp-mapping` | Skip discovery; no warn |
| `--il2cpp-mapping PATH` but file missing | Fail that upload |
| UUID(s) unresolved for the bundle | Fail `il2cpp-linemap` upload |
| Wrong map identity at apply time | Never apply; leave native symbolication intact |

---

## 4. Competitors (context)

- **Bugsnag:** CLI `unity-android` / `unity-ios`; uploads `LineNumberMappings.json`
  with native symbols; fail unless `--no-upload-il2cpp-mapping`.
- **Sentry:** Capture-time addresses + server apply via `symbolic-il2cpp`; upload
  with debug files.
- **Firebase:** NDK / symbols.zip focus; less public LineNumberMappings emphasis.

Bugsee matches Bugsnag for thin CLI orchestration and Sentry for the primary
address→map apply model.

---

## 5. Bugsee gap (pre-epic)

| Layer | Gap |
|---|---|
| `bugsee-cli` | No `--type il2cpp-linemap` |
| Appserver | No format; UUID-only dedup; `formatBatch` last-wins |
| Worker | No LNM/MethodMap apply; iOS path basename strip |
| Unity SDK | No Editor collect; Phase A string frames only |

Do **not** reuse `mapping` / `sourcemap` / BMF/BSF processors.

---

## 6. Implementation sequencing

1. **B0** Fixtures (Android + iOS; multi-ABI build-ids preferred).
2. **B1a** Worker offline spike (fixtures on disk).
3. **B2** Appserver format + format-aware dedup + multi-format lookup.
4. **B1b** Worker production apply (after B2).
5. **B3** `bugsee-cli --type il2cpp-linemap`.
6. **B4** Unity Editor/CI collect (after B1b–B3).

---

## 7. CLI surface

```text
bugsee-cli debug-files upload PATH/TO/LineNumberMappings.json \
  --type il2cpp-linemap \
  --version … --build … \
  [--uuid UUID…]          # IL2CPP module UUID(s); repeatable / comma-separated
  [--il2cpp-root PATH]    # optional override for il2cppFileRoot.txt
```

Siblings (`MethodMap.tsv`, `il2cppFileRoot.txt`) are auto-discovered next to the
JSON when present. Optional later: `unity-android` / `unity-ios` multi-artifact
orchestrators (not MVP).

---

## 8. Worker apply sketch

```text
Crash report (Unity / IL2CPP)
  → native symbolicate (dSYM/ELF/Breakpad) where addresses exist
     (preserve full cpp path for IL2CPP modules)
  → if format il2cpp-linemap present for module UUID:
       → stash pre-remap frames on thread/exception `original_frames`
         (same pattern as ProGuard deobfuscate / managed mono+dart)
       → overwrite existing display fields with C# file / line / method
         (`data.source`, `data.line`, `data.member*`, `trace-sym`)
       → MethodMap demangle when mangled names present
  → existing unity.py string parse as last resort
```

Do **not** invent parallel fields (`csharp_*`, `il2cpp_cpp_*`). Natives live under
`original_frames`; remapped values occupy the normal display fields.

### Acceptance

1. Native-address frames gain C# file/line when map + line info match.
2. Mangled `Type_Method_mHASH` demangles when MethodMap present.
3. Wrong map identity is **not** applied.
4. Missing map leaves native symbolication intact.
5. Multi-ABI UUID on the same linemap document applies.

---

## 9. Host / CI checklist

- [ ] Scripting Backend = IL2CPP
- [ ] Android: Create symbols.zip = Debugging (line info, not SYMBOL_TABLE-only)
- [ ] iOS: dSYM enabled for Release; Script Debugging off if dSYM must succeed
- [ ] Emit IL2CPP line mapping files (confirm Player Setting / IL2CPP args)
- [ ] Archive `LineNumberMappings.json`, `il2cppFileRoot.txt`, `MethodMap.tsv`
- [ ] Upload with IL2CPP module UUID(s); version/build as metadata
- [ ] Do not ship mapping folders in the player
- [ ] Android native primary apply requires Bugsee NDK enabled

---

## 10. References

- Epic plan: Cursor `il2cpp_linenumbermaps_epic`
- Unity symbols plan: Cursor `unity_symbols_and_crashes`
- `bugsee-cli` `debug-files upload` types; `src/cli/debug_files.rs`
- Appserver `symbolFormats` / `formatBatch` / `(uuid, transform)` Android create
- Worker `crash/symbolicator.py`, `crash/helpers/android_ndk.py`, `crash/managed/unity.py`
- Sentry `symbolic-il2cpp` `LineMapping`
- Bugsnag Unity stacktraces / `unity-android` / `unity-ios` CLI
