# bugsee-cli

Cross-platform Rust binary that collects debug information files (dSYM, ELF, PE/PDB, Portable PDB, Breakpad, R8/ProGuard mappings, JS source maps), resolves build-environment metadata (VCS, CI provider, iOS dependency graph, Xcode version, Mach-O UUIDs), and uploads symbols to Bugsee. One binary, shelled by thin per-build-system orchestrators (Android Gradle plugin, Xcode Run Script via the iOS SDK's BugseeAgent, fastlane plugin, MSBuild target, Unity post-build hook, Flutter Dart plugin, npm package).

Built on the `symbolic` Rust crate family — the same crates the Bugsee worker already uses via Python wheels for `symbolic.debuginfo`, `symbolic.sourcemap`, `symbolic.symcache` — so producer and consumer parse artifacts with the same code.

## Scope

- **In scope.** Symbol discovery, debug-ID normalization, source-map debug-ID injection, Zstd compression, chunked upload, legacy presigned upload, **and** the canonical build-environment resolvers consumed by the iOS-side Python BugseeAgents (VCS metadata, iOS dependency collection, Xcode/build-env helpers, dSYM UUID extraction). Centralising these in one Rust binary eliminates the historical duplication between the fastlane plugin's and the iOS SDK's Python implementations.
- **Out of scope.** Build timings, app-size analysis, dependency-tree collection on non-iOS platforms — those stay in the per-build-system plugins (Gradle plugin, MSBuild target, etc.) where they have access to build-system internals.

## Locked design decisions

1. **BMF/BSF — keep for existing setups, drop for new ones.** Existing deployments continue to convert PDB/MDB → BMF/BSF via `bugsee-cli debug-files convert`. New deployments upload Portable PDB / dSYM directly and rely on `symbolic-debuginfo` server-side.
2. **Zstd is the default wire format.** Default level **11**, minimum production level **9**. The worker already accepts ZIP files with Z_STANDARD (compression method 93) entries.
3. **Debug ID is the universal identifier.** Native artifacts (Mach-O UUID, ELF GNU build-id, PE Timestamp+Size) carry it naturally. JS bundles get a deterministic UUIDv5 via `bugsee-cli sourcemaps inject`. Server lookup is by debug-id; the old `(uuid, version, build)` tuple stays as a backward-compat shim during fleet migration.
4. **Chunked upload from v0.1.** `GET /v2/files/chunk-upload` → sha1-keyed chunks → assemble. Required for Flutter Dart `.symbols` and Unity IL2CPP artifacts that routinely exceed 50–100 MB.
5. **Cross-language wire shape is a public contract.** Each subcommand's stdout JSON shape is consumed by at least one external Python script (see Subcommands § below). Breaking changes to field names, casing, types, or null-vs-omit semantics require a major version bump and a coordinated rollout across the fastlane plugin and the iOS SDK's BugseeAgent.

## Building

```sh
cargo build --release
```

Binary lands at `target/release/bugsee-cli`. Pinned to stable Rust via `rust-toolchain.toml`.

## Subcommands

All subcommands print JSON to stdout and exit 0 on parseable failure (empty list / null / empty object) so Python integrators can shell with `check=False` and rely on the output shape rather than the exit code. Hard failures (network, auth, malformed argv) follow the [exit-code contract](#exit-code-contract) below.

### `debug-files upload <paths>...`

```
bugsee-cli debug-files upload <paths>... \
    --version <X> --build <Y> \
    [--type proguard|elf|dsym] \
    [--uuid <UUID>]   # override the auto-computed debug-id (caller owns it)
    [--icon <PATH>]   # attach launcher icon to the symbol zip
    [--zstd-level N]  # 9..=22, default 11; or pass --no-zstd
    [--dry-run]
```

The upload flow itself. ProGuard, ELF, and dSYM types are working; other types are planned via [`debug-files convert`](#debug-files-convert-planned) once their wire format stabilises.

### `vcs-metadata`

Resolves VCS metadata (provider, commit SHA, branch, base branch, PR number, repo) from CI provider env vars (GitHub Actions, GitLab CI, Bitbucket Pipelines, CircleCI, Bitrise, Jenkins, Xcode Cloud, generic `CI`) or a `git` fallback. Output shape pinned by `tests/cross_language_contract.rs`:

```json
{
  "provider": "github",
  "commit_sha": "abc123…",
  "repo": "org/repo",
  "branch": "main"
}
```

Absent fields are omitted, not serialised as `null`. Consumed by the fastlane plugin's BugseeAgent and the iOS SDK's `tools.bundle/BugseeAgent` (`_resolve_vcs_metadata_via_cli`).

### `ios-deps collect`

```
bugsee-cli ios-deps collect --project-root <PATH> [--product-binary <PATH>] [--max-entries N]
```

Discovers and parses iOS dependency manifests under `<PATH>`: `Podfile.lock` (CocoaPods), `Package.resolved` (SPM — pure-package and Xcode-managed shapes, with sibling `*.xcodeproj` / `*.xcworkspace` probing), `Cartfile.resolved` (Carthage), and vendored frameworks linked into `--product-binary` (via `/usr/bin/otool`). Merges with field-wise url-preference dedup. Output:

```json
{
  "entries": [{"id":"library::Alamofire","group":"","name":"Alamofire","direct":true,"type":"library","parents":[],"version":"5.10.0","url":"https://github.com/Alamofire/Alamofire.git"}],
  "scope_label": "all",
  "truncated": false
}
```

`version`, `scope`, `url`, and `parents` are emitted only when non-empty (mirrored on the Python side). The `url` field is load-bearing for OSV SwiftURL ecosystem vuln lookups — drift on the optional-field semantics silently degrades vuln-scan coverage.

### `build-env` helpers

Three sub-subcommands; each prints its result to stdout or empty string on unresolved. Consumed by both Python BugseeAgents to eliminate duplicated in-process helpers.

- `build-env xcode-version` — reads `XCODE_VERSION_ACTUAL` env if set (`"1620"` → `"16.2.0"`), else shells `/usr/bin/xcodebuild -version` and normalises to 3-part dotted form. Empty string on failure.
- `build-env machine-label` — returns `<provider>[:<detail>]` matching the Android Gradle plugin's `BuildMachineResolver` cascade so the dashboard can group iOS + Android builds from the same CI runner.
- `build-env read-plist <plist>` — emits a JSON dict of `key → string` for all scalar entries in the plist (string, int, real, bool, uint). Dict / array / Data values are silently dropped (scalars-only contract). Returns `{}` on missing file.

### `dsym uuid <path>` / `dsym slices <path>`

Extract Mach-O UUIDs from a `.dSYM` bundle directory OR a single Mach-O binary inside one. Replaces the per-Python-BugseeAgent `dwarfdump -u` shell-outs with one canonical `symbolic-debuginfo` Mach-O parser.

- `dsym uuid <path>` — JSON array of uppercase hyphenated UUID strings, in archive order:
  ```json
  ["54D75FB3-747F-387F-8A93-4EA034B1F8CF","8D00647D-E563-30F9-9F17-E1FFCEFF70B4"]
  ```
- `dsym slices <path>` — same input, arch-aware output. Used by the iOS SDK's `get_main_executable_uuid` to pick the `arm64` slice from a fat .app binary:
  ```json
  [{"uuid":"54D75FB3-747F-387F-8A93-4EA034B1F8CF","arch":"x86_64"},
   {"uuid":"8D00647D-E563-30F9-9F17-E1FFCEFF70B4","arch":"arm64"}]
  ```

Both return `[]` (exit 0) on any parseable failure. Uppercase casing matches `dwarfdump -u`'s historic output so cross-tool string comparisons don't break.

### `sourcemaps inject` / `sourcemaps upload` (planned)

```
bugsee-cli sourcemaps inject <paths>... [--dry-run]
bugsee-cli sourcemaps upload <paths>...
```

### `debug-files convert` (planned)

```
bugsee-cli debug-files convert <input> --to bmf|bsf --output <path>
```

### Global flags

`--endpoint` (env `BUGSEE_ENDPOINT`), `--app-token` (env `BUGSEE_APP_TOKEN`). Both global so every subcommand inherits the same `BUGSEE_ENDPOINT` override path the per-build-system integrators already standardise on. Only the upload-flavoured subcommands (`debug-files upload`, `sourcemaps upload`) actually consume these values; metadata-resolving subcommands (`vcs-metadata`, `ios-deps`, `build-env`, `dsym`) do no network I/O and ignore them.

### Subcommand vocabulary

Multi-word subcommand names are hyphenated (`vcs-metadata`, `ios-deps`, `build-env`, `debug-files`). Single-word names are bare (`dsym`, `sourcemaps`). Sub-subcommands keep the hyphenation pattern (`debug-files upload`, `ios-deps collect`, `build-env xcode-version`). This is the same scheme `cargo`, `kubectl`, and `gh` follow, and the Python integrators consume the names verbatim — renaming any subcommand is a wire-shape break under the [compatibility policy](#wire-shape-compatibility-policy) below.

### Built-in help and machine-readable schemas

- `bugsee-cli --help` and `bugsee-cli <subcommand> --help` print the full surface (clap-derived; covers every flag, env-var alias, and subcommand).
- `bugsee-cli --version` prints the SemVer. Integrators that bind to a specific output shape should pin against this — see the wire-shape compatibility policy.
- Sample output for every subcommand lives in this README's [Subcommands](#subcommands) section. The pinned reference vectors used by the cross-language integration tests live under `tests/fixtures/`.

There is no `man bugsee-cli` page today; the README and built-in `--help` cover the same ground. A future docs site at `docs.bugsee.com/cli/` is planned but not shipped.

## Wire-shape compatibility policy

Each subcommand's stdout JSON shape is the source of truth for at least one Python script outside this repo (the fastlane plugin's `BugseeAgent` and/or the iOS SDK's `tools.bundle/BugseeAgent`). Drift = silent breakage on real builds.

Compatibility rules for releasing changes:

- **Backward-compatible (no version coordination required).** Adding a new subcommand. Adding a new optional output field with `#[serde(skip_serializing_if = "Option::is_none")]`.
- **Backward-compatible-with-care (announce in CHANGELOG).** Adding a new required output field (only if every consumer already tolerates unknown fields — verify in the consumer tests). Adding a new env var the resolver checks.
- **Breaking.** Renaming a field. Removing a field. Changing a field's type or casing. Changing `[]` ↔ `null` / `{}` ↔ `null` semantics. Changing exit-code semantics. Any of these requires a major version bump and a coordinated landing across the fastlane plugin (`fastlane-plugin-bugsee/BugseeAgent`) and the iOS SDK (`tools.bundle/BugseeAgent`).

Cross-language reference vectors live in `tests/cross_language_contract.rs` + `tests/fixtures/`. Each test runs the compiled binary against a checked-in fixture and pins the stdout JSON shape end-to-end. Mirror tests on the Python side (`scripts/test_bugsee_agent_*_cli.py` in the iOS SDK; `test/test_bugsee_cli_migration.py` in the fastlane plugin) feed canned JSON in the same shape — if the Rust output ever drifts, both sides break.

## Exit-code contract

Stable. Integrators (Gradle plugin, MSBuild target, fastlane plugin, npm wrapper) use these codes to decide whether to fall back to their in-language uploader during the dual-path rollout phase.

| Code  | Meaning                                                              | Caller should fall back? |
|-------|----------------------------------------------------------------------|--------------------------|
| 0     | Success (uploaded, or server reports already-exists, or resolver returned empty/null output). | n/a                      |
| 1     | Unexpected / unhandled error.                                        | **yes**                  |
| 2     | Usage / argv error (likely a plugin↔CLI version mismatch).           | **yes**                  |
| 10–19 | Input / discovery problems (file not found, unparseable format).      | no                       |
| 20–29 | Configuration problems (bad token, invalid flags).                   | no                       |
| 30–39 | Upload problems (network, server 4xx/5xx).                            | no                       |
| 40+   | Reserved.                                                            | no                       |

The fallback rule: codes ≤ 2 mean the CLI never got a fair chance to run; codes ≥ 10 are substantive failures the in-language uploader would hit the same way. See `src/exit_code.rs` for the source-of-truth enum.

Note: subcommands that emit JSON (vcs-metadata, ios-deps collect, build-env *, dsym *) return exit **0** even when no useful result is found — callers distinguish "no result" from "tool error" by checking the JSON shape (empty list / empty object / specific field absence), not the exit code. This lets Python integrators use `check=False` + `json.loads(stdout)` without branching on returncode.

## Telemetry header

Every metadata `POST` sets `X-Bugsee-Uploader: cli`. The in-language fallback uploaders send a different value of the same header (e.g. `kotlin-fallback-cli-exec-failed`) so the backend can count CLI-vs-fallback usage without touching customer code. The header is **not** added to the presigned S3 `PUT` — that signature is bound to a specific header set, and S3 would reject extras with `SignatureDoesNotMatch`.

## Distribution

| Channel | Used by |
|---|---|
| `@bugsee/cli` npm with per-OS `optionalDependencies` | RN, Cordova, Capacitor, web |
| Maven Central `com.bugsee:bugsee-cli` jar bundling binaries | Android Gradle plugin |
| NuGet `Bugsee.CLI` bundle | .NET MAUI MSBuild target |
| UPM `com.bugsee.cli` package | Unity Editor post-build |
| CDN download + SHA-256 checksum on first use | Flutter (Dart plugin), fastlane plugin (`resolveCli`) |
| Homebrew tap + curl installer | iOS / generic CI |

Target platforms: macOS arm64 + x86_64, Linux x86_64 + aarch64 (glibc; musl if Alpine CI demand exists), Windows x86_64.

## Layout

```
src/
  main.rs              entry point
  cli/                 clap command tree
    debug_files.rs       debug-files upload / convert
    sourcemaps.rs        sourcemaps inject / upload
    vcs_metadata.rs      vcs-metadata
    ios_deps.rs          ios-deps collect
    build_env.rs         build-env xcode-version / machine-label / read-plist
    dsym.rs              dsym uuid / dsym slices
  symbols/             format-specific discovery + identification (dsym, elf, pdb, portable_pdb, breakpad, proguard, jvm)
  compress/            Zstd-in-ZIP packaging
  upload/
    chunked.rs           modern chunked protocol (default)
    presigned.rs         legacy two-stage POST → PUT
  inject/              JS source-map debug-ID injection
  error.rs
  exit_code.rs         source-of-truth enum for the exit-code contract above

tests/
  cross_language_contract.rs   integration tests against the compiled binary
  fixtures/                    checked-in JSON / lockfile / plist fixtures
```
