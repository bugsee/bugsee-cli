# bugsee-cli

Cross-platform Rust binary that collects debug information files (dSYM, ELF, PE/PDB, Portable PDB, Breakpad, R8/ProGuard mappings, JS source maps) and uploads them to Bugsee. One binary, shelled by thin per-build-system orchestrators (Android Gradle plugin, Xcode Run Script, MSBuild target, Unity post-build hook, Flutter Dart plugin, npm package).

Built on the `symbolic` Rust crate family — the same crates the Bugsee worker already uses via Python wheels for `symbolic.debuginfo`, `symbolic.sourcemap`, `symbolic.symcache` — so producer and consumer parse artifacts with the same code.

## Scope

- **In scope.** Symbol discovery, debug-ID normalization, source-map debug-ID injection, Zstd compression, chunked upload, legacy presigned upload.
- **Out of scope.** Build timings, dependency-tree collection, app-size analysis — those stay in the per-build-system plugins (Gradle plugin, MSBuild target, etc.) where they have access to build-system internals.

## Locked design decisions

1. **BMF/BSF — keep for existing setups, drop for new ones.** Existing deployments continue to convert PDB/MDB → BMF/BSF via `bugsee-cli debug-files convert`. New deployments upload Portable PDB / dSYM directly and rely on `symbolic-debuginfo` server-side.
2. **Zstd is the default wire format.** Default level **11**, minimum production level **9**. The worker already accepts ZIP files with Z_STANDARD (compression method 93) entries.
3. **Debug ID is the universal identifier.** Native artifacts (Mach-O UUID, ELF GNU build-id, PE Timestamp+Size) carry it naturally. JS bundles get a deterministic UUIDv5 via `bugsee-cli sourcemaps inject`. Server lookup is by debug-id; the old `(uuid, version, build)` tuple stays as a backward-compat shim during fleet migration.
4. **Chunked upload from v0.1.** `GET /v2/files/chunk-upload` → sha1-keyed chunks → assemble. Required for Flutter Dart `.symbols` and Unity IL2CPP artifacts that routinely exceed 50–100 MB.

## Status

v0.1 scaffold — modules and CLI shape only. All `dispatch` paths currently return `not yet implemented`.

## Building

```sh
cargo build --release
```

Binary lands at `target/release/bugsee-cli`. Pinned to stable Rust via `rust-toolchain.toml`.

## CLI surface

Working today (v0.1, ProGuard only — other types are scaffold):

```
bugsee-cli debug-files upload <paths>... \
    --version <X> --build <Y> \
    [--type proguard] \
    [--uuid <UUID>]   # override the auto-computed debug-id (caller owns it)
    [--icon <PATH>]   # attach launcher icon to the symbol zip
    [--zstd-level N]  # 9..=22, default 11; or pass --no-zstd
    [--dry-run]
```

Planned:

```
bugsee-cli debug-files convert <input> --to bmf|bsf --output <path>
bugsee-cli sourcemaps   inject <paths>...   [--dry-run]
bugsee-cli sourcemaps   upload <paths>...
```

Global flags: `--endpoint` (env `BUGSEE_ENDPOINT`), `--app-token` (env `BUGSEE_APP_TOKEN`).

## Exit-code contract

Stable. Integrators (Gradle plugin, MSBuild target, fastlane plugin, npm wrapper) use these codes to decide whether to fall back to their in-language uploader during the dual-path rollout phase.

| Code  | Meaning                                                              | Caller should fall back? |
|-------|----------------------------------------------------------------------|--------------------------|
| 0     | Success (uploaded, or server reports already-exists).                | n/a                      |
| 1     | Unexpected / unhandled error.                                        | **yes**                  |
| 2     | Usage / argv error (likely a plugin↔CLI version mismatch).           | **yes**                  |
| 10–19 | Input / discovery problems (file not found, unparseable format).      | no                       |
| 20–29 | Configuration problems (bad token, invalid flags).                   | no                       |
| 30–39 | Upload problems (network, server 4xx/5xx).                            | no                       |
| 40+   | Reserved.                                                            | no                       |

The fallback rule: codes ≤ 2 mean the CLI never got a fair chance to run; codes ≥ 10 are substantive failures the in-language uploader would hit the same way. See `src/exit_code.rs` for the source-of-truth enum.

## Telemetry header

Every metadata `POST` sets `X-Bugsee-Uploader: cli`. The in-language fallback uploaders send a different value of the same header (e.g. `kotlin-fallback-cli-exec-failed`) so the backend can count CLI-vs-fallback usage without touching customer code. The header is **not** added to the presigned S3 `PUT` — that signature is bound to a specific header set, and S3 would reject extras with `SignatureDoesNotMatch`.

## Distribution (planned)

| Channel | Used by |
|---|---|
| `@bugsee/cli` npm with per-OS `optionalDependencies` | RN, Cordova, Capacitor, web |
| Maven Central `com.bugsee:bugsee-cli` jar bundling binaries | Android Gradle plugin |
| NuGet `Bugsee.CLI` bundle | .NET MAUI MSBuild target |
| UPM `com.bugsee.cli` package | Unity Editor post-build |
| CDN download + SHA-256 checksum on first use | Flutter (Dart plugin) |
| Homebrew tap + curl installer | iOS / generic CI |

Target platforms: macOS arm64 + x86_64, Linux x86_64 + aarch64 (glibc; musl if Alpine CI demand exists), Windows x86_64.

## Layout

```
src/
  main.rs              entry point
  cli/                 clap command tree
    debug_files.rs       debug-files upload / convert
    sourcemaps.rs        sourcemaps inject / upload
  symbols/             format-specific discovery + identification (dsym, elf, pdb, portable_pdb, breakpad, proguard, jvm)
  compress/            Zstd-in-ZIP packaging
  upload/
    chunked.rs           modern chunked protocol (default)
    presigned.rs         legacy two-stage POST → PUT
  inject/              JS source-map debug-ID injection
  error.rs
```
