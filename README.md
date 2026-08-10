# bugsee-cli

Cross-platform Rust binary that collects debug information files (dSYM, ELF, PE/PDB, Portable PDB, Breakpad, R8/ProGuard mappings, JS source maps), resolves build-environment metadata (VCS, CI provider, iOS dependency graph, Xcode version, Mach-O UUIDs), and uploads symbols to Bugsee. One binary, shelled by thin per-build-system orchestrators (Android Gradle plugin, Xcode Run Script via the iOS SDK's BugseeAgent, fastlane plugin, MSBuild target, Unity post-build hook, Flutter Dart plugin, npm package).

## Installation

Install the latest release — it downloads and **SHA-256-verifies** the binary for
your host from `download.bugsee.com` (no GitHub dependency):

**macOS / Linux**
```sh
curl --proto '=https' --tlsv1.2 -sSfL https://download.bugsee.com/cli/install.sh | sh
```

**Windows (PowerShell)**
```powershell
powershell -ExecutionPolicy ByPass -c "irm https://download.bugsee.com/cli/install.ps1 | iex"
```

The installer auto-detects OS/arch and installs to `/usr/local/bin` (or
`~/.local/bin`) on Unix / `%LOCALAPPDATA%\Bugsee\bin` on Windows, printing a PATH
hint if needed. Override via env vars: `BUGSEE_CLI_VERSION` (pin an exact
`X.Y.Z`), `BUGSEE_CLI_INSTALL_DIR` (install location), `BUGSEE_CLI_BASE_URL`
(download root, e.g. an internal mirror). Keep it current afterwards with
`bugsee-cli update` (same-major only). Other channels: npm `@bugsee/cli`, a
Homebrew tap, or the per-build-system bundles — see [Distribution](#distribution).

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
    [--type proguard|rust|elf|dsym|pdb|sourcemaps|il2cpp-linemap] \
    [--uuid <UUID>]   # override / IL2CPP module id(s); comma-separate for multi-ABI \
    [--icon <PATH>]   # attach launcher icon to the symbol zip \
    [--zstd-level N]  # 9..=22, default 11; or pass --no-zstd
    [--force]         # re-upload even if the server already has it (dsym/pdb/il2cpp-linemap)
    [--dry-run]
```

The upload flow itself. ProGuard, Rust, ELF, dSYM, PDB, sourcemap, and Unity IL2CPP line-map types are working; other types are planned via [`debug-files convert`](#debug-files-convert-planned) once their wire format stabilises.

#### Unity IL2CPP — `--type il2cpp-linemap`

Uploads `LineNumberMappings.json` (+ sibling `MethodMap.tsv` / `il2cppFileRoot.txt`) as format `il2cpp-linemap`, keyed by the IL2CPP module UUID(s) (`libil2cpp` / `UnityFramework`). See [`docs/unity-il2cpp-linenumber-mappings.md`](docs/unity-il2cpp-linenumber-mappings.md).

```sh
bugsee-cli debug-files upload path/to/Symbols/LineNumberMappings.json \
  --type il2cpp-linemap \
  --version 1.2.3 --build 45 \
  --uuid <arm64-build-id>,<armeabi-build-id>
```

#### Rust (Cargo) projects — `--type rust`

A Rust project has no single symbol format: it is a `.dSYM` bundle for Apple targets, a `.pdb` for `*-pc-windows-msvc`, and the ELF binary itself (keyed by its GNU build-id) for Linux/Android. `--type rust` discovers whichever the build produced, so one command covers every target:

```sh
bugsee-cli debug-files upload --type rust target/release --version 1.4.0 --build 250
```

- **Content-based discovery.** Artifacts are classified by container magic, not by host OS, so a cross-compiled `target/x86_64-unknown-linux-gnu/release` uploads correctly from a Mac.
- **Cargo intermediates are skipped** — `deps/`, `build/`, `incremental/`, `.fingerprint/`. Walking them would register a symbol document for every dependency and build script.
- **Symlinked `.dSYM`s are followed.** Cargo writes the real bundle into `deps/` and leaves a symlink at the profile root; that symlink is the only path to it once `deps/` is skipped.
- **`--uuid` is rejected.** Every Rust debug format carries its own identity (Mach-O UUID / PDB debug id / GNU build-id), and that identity is what the SDK reports for the module at crash time — an override could never match it.

**Build configuration is required.** A stock `cargo build --release` emits nothing uploadable, and each format has a setting that, if missing, produces an upload that is accepted and then resolves nothing. The command reports whichever is missing and exits `10` when it finds no symbols at all:

```toml
# Cargo.toml
[profile.release]
debug = 1                       # emit DWARF at all
split-debuginfo = "packed"      # macOS/iOS: collect it into a .dSYM
```

```toml
# .cargo/config.toml — Linux/Android only
[target.'cfg(target_os = "linux")']
rustflags = ["-C", "link-arg=-Wl,--build-id"]
```

CI recipe (GitHub Actions):

```yaml
- run: cargo build --release
- run: |
    curl -fsSL https://download.bugsee.com/cli/install.sh | sh
    bugsee-cli debug-files upload --type rust target/release \
      --version "${{ github.ref_name }}" --build "${{ github.run_number }}"
  env:
    BUGSEE_APP_TOKEN: ${{ secrets.BUGSEE_APP_TOKEN }}
```

Pass `--dry-run` to verify discovery and see the preflight warnings without uploading.

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

### `sourcemaps inject`

Embeds a deterministic, content-derived UUIDv5 debug-id into JS bundles
(`//# debugId=` + a `globalThis._bugseeDebugIds` runtime stub) and into the
paired `.map` (`debug_id` + `debugId`). Idempotent. Upload the injected maps
through `debug-files upload --type sourcemaps`.

```
bugsee-cli sourcemaps inject <paths>... [--dry-run]
bugsee-cli debug-files upload --type sourcemaps <paths>... --version <v> --build <b>
```

### `debug-files convert` (planned)

```
bugsee-cli debug-files convert <input> --to bmf|bsf --output <path>
```

### Global flags

`--endpoint` (env `BUGSEE_ENDPOINT`), `--app-token` (env `BUGSEE_APP_TOKEN`). Both global so every subcommand inherits the same `BUGSEE_ENDPOINT` override path the per-build-system integrators already standardise on. Only the upload-flavoured subcommands (`debug-files upload`, `upload build`, `upload build-info`) actually consume these values; metadata-resolving subcommands (`vcs-metadata`, `ios-deps`, `build-env`, `dsym`, `sourcemaps inject`) do no network I/O and ignore them.

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

## Design notes

| Doc | Topic |
|---|---|
| [`docs/upload-unification.md`](docs/upload-unification.md) | Build-info bundle, chunked upload, cross-platform producers |
| [`docs/upload-unification-activation.md`](docs/upload-unification-activation.md) | Rollout / activation |
| [`docs/unity-il2cpp-linenumber-mappings.md`](docs/unity-il2cpp-linenumber-mappings.md) | Unity IL2CPP `LineNumberMappings.json` / MethodMap design (format `il2cpp-linemap`) |

## Layout

```
src/
  main.rs              entry point
  cli/                 clap command tree
    debug_files.rs       debug-files upload / convert
    sourcemaps.rs        sourcemaps inject
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
