# IL2CPP line-map fixtures

Synthetic fixtures matching the Sentry `symbolic-il2cpp` nested JSON schema
(`cpp_path → { cs_path → { cpp_line: cs_line } }`) plus a minimal MethodMap.tsv
and `il2cppFileRoot.txt`.

**Not captured from a real Unity Editor build.** Replace with redacted Unity 6
Android/iOS samples when available; keep the multi-ABI `images[]` in
`android/manifest.json` for identity tests.

Provenance: hand-authored for bugsee-cli / worker unit tests (2026-08).
