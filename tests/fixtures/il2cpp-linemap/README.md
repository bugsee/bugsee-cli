# IL2CPP line-map fixtures

Synthetic fixtures matching the Sentry `symbolic-il2cpp` nested JSON schema
(`cpp_path → { cs_path → { cpp_line: cs_line } }`) plus a minimal MethodMap.tsv,
`il2cppFileRoot.txt` and an example `manifest.json`, under `android/` and `ios/`.

**Not captured from a real Unity Editor build.** Replace with redacted Unity 6
Android/iOS samples when available.

`manifest.json` shows the bundle manifest's shape (multi-ABI `images[]` on
Android). Nothing in this repo reads it: the CLI writes its own manifest when
packing, and multi-ABI identity is tested through `--uuid` lists in
`tests/debug_files_flags.rs`.

Provenance: hand-authored for bugsee-cli / worker unit tests (2026-08).
