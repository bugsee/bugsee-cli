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
`bugsee-cli update` (same-major only). Other channels: npm (`@bugsee/cli` —
`npm i -D @bugsee/cli && npx bugsee-cli --version`), a Homebrew tap, or the
per-build-system bundles — see [Distribution](#distribution).

## Building

```sh
cargo build --release
```

Binary lands at `target/release/bugsee-cli`. Pinned to stable Rust via `rust-toolchain.toml`.

## Subcommands

The metadata-resolving subcommands print JSON to stdout and exit 0 on parseable failure (empty list / null / empty object) so Python integrators can shell with `check=False` and rely on the output shape rather than the exit code. (`xcode upload-dsyms` is deliberately not one of these: it prints nothing to stdout and is designed to exit non-zero so a build phase fails — see its section below.) Hard failures (network, auth, malformed argv) follow the [exit-code contract](#exit-code-contract) below.

### `debug-files upload <paths>...`

```
bugsee-cli debug-files upload <paths>... \
    --version <X> --build <Y> \
    [--type proguard|rust|elf|dsym|pdb|sourcemaps|il2cpp-linemap] \
    [--uuid <UUID>]   # override / IL2CPP module id(s); comma-separate for multi-ABI \
    [--icon <PATH>]   # attach launcher icon to the symbol zip \
    [--zstd-level N]  # 9..=22, default 11; or pass --no-zstd
    [--force]         # re-upload even if the server already has it (dsym/pdb/rust/il2cpp-linemap/sourcemaps)
    [--concurrency N] # sourcemaps only: ceiling on uploads in flight, 1..=32 (default: scaled)
    [--allow-empty]   # sourcemaps only: "nothing to upload" is success, not exit 10
    [--strip-sources-content]  # sourcemaps only: upload maps without the embedded source
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

The debug-id is derived from the bundle's bytes and its map's, so a map that
changes under byte-identical minified JS still gets a new id (and uploads) —
including when the bundler keeps an already-stamped bundle on disk and re-emits
only its map (webpack `[contenthash]`): `inject` re-keys that bundle.

A bundle that already carries a `//# debugId=` another tool wrote (Rollup 4's
`output.sourcemapDebugIds`) keeps that id — its map already carries it — and
gains only the `_bugseeDebugIds` runtime registration, without which the SDK
cannot attach the id to a crash frame.

When the upload scans a directory, maps named as stylesheet or type-declaration
maps (`.css.map`, `.d.ts.map`, `.d.mts.map`, `.d.cts.map`) are skipped: they
never carry a debug-id. Any other map without one fails the run before anything
is uploaded — inject did not stamp that bundle — and so does a scan that leaves
nothing to upload, unless `--allow-empty` says otherwise. A map the server
already has is skipped and the batch continues, so rebuilding an app with
unchanged chunks uploads only the changed ones; `--force` re-uploads it anyway.

Maps upload **several at a time**. Each map is an independent register + PUT
pair, so a build with one map per chunk used to spend its upload time waiting on
round-trips. Against a mock with 50 ms of latency:

| maps | serial (`--concurrency 1`) | default |
|------|----------------------------|---------|
| 60   | 7.09 s                     | 1.31 s  |
| 200  | 23.58 s                    | 4.10 s  |

`--concurrency N` sets a **ceiling**, not a fixed width: no more uploads run than
there are maps, so a 3-map build runs 3 at a time whatever the ceiling says. Left
unset, the ceiling scales with the batch — one upload per 8 maps, at least 4, at
most 8.

That cap is deliberately modest. On a fast link more streams keep helping (200
maps: 2.75 s at `--concurrency 16`), but the machine that suffers most from
serial uploads is a CI box on a thin uplink, where the transfer is
bandwidth-bound and extra streams only add latency to each one. Raise it if you
have measured your own link; `--concurrency 1` restores strictly sequential
uploads.

An explicit `--uuid` keys every map in the scan under one id, so it forces
sequential uploads whatever the ceiling says — those registrations must not race
each other. A failed upload stops the batch rather than letting the rest run
into a server that has already refused one.

A `--dry-run` discovers and packs but sends nothing, so a map that carries no debug-id is reported
rather than fatal there (`unkeyed` in the completion log) — the whole flow can be previewed on a
freshly built directory, where `sourcemaps inject --dry-run` has deliberately written nothing yet.
A REAL run still refuses such a map (exit 11): uploading it would register a symbol nothing can find.

`--strip-sources-content` uploads each map WITHOUT its `sourcesContent`, for teams who would rather
their source did not leave the build machine. Symbolication still resolves file, line and column;
what is lost is the source snippet shown beside a crash frame. The map on disk is never modified —
only the copy that is uploaded — and the declared `hash` describes the stripped bytes. A map that
carries no `sourcesContent` is uploaded byte-for-byte unchanged.

`--allow-empty` turns "nothing to upload" into success (exit 0) instead of
exit 10 — a monorepo package built without maps, or a framework whose server
output has none, is a legitimate no-op rather than a reason to fail the build.
A path that does not exist is an error regardless (`path does not exist: <p>`,
exit 10) — including when other paths do hold maps — so a typo or a build that
never ran cannot half-upload a build's symbols.

Both flags apply to `--type sourcemaps` only, and are rejected (exit 20) for any
other type rather than accepted and ignored.

`--exclude <glob>` (repeatable) keeps `inject` out of part of a build output — `--exclude
'**/node_modules/**'` leaves vendored third-party code inside a server bundle untouched, `--exclude
'polyfills*.js'` skips one file by name. Globs are matched against the path relative to each walked
root and against the full path; an unparseable pattern is a configuration error (exit 20) rather
than a silent "matches nothing", which would rewrite exactly the files you meant to protect.

**A bundle with no source map is still stamped, on purpose.** It looks like waste — the id cannot
resolve to a symbol — but a crash frame carrying a debug-id whose map was never uploaded marks the
report `missing_sym`, which is what prompts you to upload it. An unstamped bundle is silently
unsymbolicated instead. Measured on a stock `next build` with browser source maps on: 39 JS files,
12 maps, so 27 bundles are stamped without one; that is the case that produces the prompt. Use
`--exclude` when you would rather those files were not touched at all.

```
bugsee-cli sourcemaps inject <paths>... [--exclude <glob>]... [--dry-run]
bugsee-cli debug-files upload --type sourcemaps <paths>... --version <v> --build <b> \
    [--concurrency N] [--allow-empty]
```

### `xcode upload-dsyms`

Uploads dSYMs from an Xcode **Run Script build phase**, with none of the
`BUGSEE_BUILD_INFO_*` gating — it neither registers a build nor uploads
build-info, so it is safe to run on every build. This is the shape a
React Native / Flutter config plugin can generate (`withXcodeProject` edits
`project.pbxproj`), as opposed to the scheme post-action, which means editing
`.xcscheme` XML.

```sh
# Run Script build phase, after "Embed Frameworks"
"$SRCROOT/path/to/bugsee-cli" xcode upload-dsyms --app-token "$BUGSEE_APP_TOKEN"
```

It reads `DWARF_DSYM_FOLDER_PATH`, which Xcode sets in every Run Script phase,
and falls back to `<ARCHIVE_PATH>/dSYMs`.

**A failure fails the build, on purpose.** A build phase that swallows errors
means symbolication silently stops working and nobody notices until a crash
report is unreadable:

| Situation | Exit | Build |
| --- | --- | --- |
| Uploaded, or nothing to upload | `0` | continues |
| A bundle could not be read or packed | `10` / `11` | **fails** |
| Missing / rejected app token, or a refused flag combination from the environment | `20` / `21` | **fails** |
| Server error / network failure | `30` / `31` | **fails** |

"Nothing to upload" — no dSYM folder, or a folder with no `.dSYM` bundles — is a
success, not a failure: a target that produces no debug symbols is a normal
state. Only real problems fail the build.

Whether a failure breaks the build and whether the upload detaches are
**independent**, each with an on/off pair (`--fail` / `--no-fail`,
`--background` / `--no-background`) and a matching env var
(`BUGSEE_DSYM_UPLOAD_NO_FAIL`, `BUGSEE_DSYM_UPLOAD_BACKGROUND`). A flag
overrides its env var, so a job exporting one globally can still opt a single
invocation back.

| | fails the build | waits for the upload |
| --- | --- | --- |
| *(default)* | yes | yes |
| `--no-fail` | no | no — detaches |
| `--no-fail --no-background` | no | **yes** |
| `--fail --background` | *refused* — exit `2` (flags) or `20` (env) | — |

`--no-fail --no-background` is usually what CI wants: never break the build, but
still wait, so a runner tearing down its process tree the moment `xcodebuild`
returns cannot kill the upload mid-flight.

`--fail --background` is refused rather than honoured, because a detached
process's exit code reaches nobody — "fail the build" would silently do nothing.
A detached run's warnings also go to `$PROJECT_TEMP_DIR/bugsee-cli.log` rather
than the Xcode build log, so `--no-background` is how you keep them visible. On
Windows there is no fork and every run is synchronous.

> **Xcode 15+:** `ENABLE_USER_SCRIPT_SANDBOXING` defaults to `YES`, which stops a
> build phase reading the dSYM folder. Set it to `NO` on the target, or declare
> the folder in the phase's input file lists. The scheme post-action is
> unaffected.

For the full build-publish flow — build registration, build-info, size checks —
use `xcode post-action` instead; see `bugsee-cli xcode post-action --help`.

### `debug-files convert` (planned)

```
bugsee-cli debug-files convert <input> --to bmf|bsf --output <path>
```

### Global flags

`--endpoint` (env `BUGSEE_ENDPOINT`), `--app-token` (env `BUGSEE_APP_TOKEN`). Both global so every subcommand inherits the same `BUGSEE_ENDPOINT` override path the per-build-system integrators already standardise on. Only the upload-flavoured subcommands (`debug-files upload`, `upload build`, `upload build-info`, `xcode post-action`, `xcode upload-dsyms`) actually consume these values; metadata-resolving subcommands (`vcs-metadata`, `ios-deps`, `build-env`, `dsym`, `sourcemaps inject`) do no network I/O and ignore them.

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
| 40    | Build gate failed deliberately (e.g. size-check FAIL).               | no                       |
| 41+   | Reserved.                                                            | no                       |

The fallback rule: codes ≤ 2 mean the CLI never got a fair chance to run; codes ≥ 10 are substantive failures the in-language uploader would hit the same way. See `src/exit_code.rs` for the source-of-truth enum.

Note: subcommands that emit JSON (vcs-metadata, ios-deps collect, build-env *, dsym *) return exit **0** even when no useful result is found — callers distinguish "no result" from "tool error" by checking the JSON shape (empty list / empty object / specific field absence), not the exit code. This lets Python integrators use `check=False` + `json.loads(stdout)` without branching on returncode.

## Telemetry header

Every metadata `POST` sets `X-Bugsee-Uploader: cli`. The in-language fallback uploaders send a different value of the same header (e.g. `kotlin-fallback-cli-exec-failed`) so the backend can count CLI-vs-fallback usage without touching customer code. The header is **not** added to the presigned S3 `PUT` — that signature is bound to a specific header set, and S3 would reject extras with `SignatureDoesNotMatch`.

## Distribution

| Channel | Used by |
|---|---|
| npm `@bugsee/cli` — per-platform `optionalDependencies` | RN, Cordova, Capacitor, web |
| npm `@bugsee/bugsee-cli` — single package, `postinstall` downloader | legacy alias for the above |
| Maven Central `com.bugsee:bugsee-cli` jar bundling binaries | Android Gradle plugin |
| NuGet `Bugsee.CLI` bundle | .NET MAUI MSBuild target |
| UPM `com.bugsee.cli` package | Unity Editor post-build |
| CDN download + SHA-256 checksum on first use | Flutter (Dart plugin), fastlane plugin (`resolveCli`) |
| Homebrew tap + curl installer | iOS / generic CI |

Target platforms: macOS arm64 + x86_64, Linux x86_64 + aarch64 (glibc; musl if Alpine CI demand exists), Windows x86_64 + arm64. (Windows arm64 builds natively on a `windows-11-arm` runner — see [#20](https://github.com/bugsee/bugsee-cli/issues/20).)

### The two npm packages

Both ship the same binary at the same version, and both are published from
`.github/workflows/npm-publish.yml` off the same release. They differ in how
the binary reaches `node_modules`:

**`@bugsee/cli` — prefer this one.** The binary lives in six per-platform
packages (`@bugsee/cli-darwin-arm64`, `-darwin-x64`, `-linux-arm64`,
`-linux-x64`, `-win32-x64`, `-win32-arm64`), each declaring `os`/`cpu` and
pinned to the exact version by the front package's `optionalDependencies`. npm
resolves exactly one. Nothing is downloaded at install time, so it works under
`--ignore-scripts`, under a lockfile-pinned CI install, and offline from a warm
cache — which is why it is the right default for a JS toolchain. A
`postinstall` fallback covers the cases optional dependencies cannot
(`--no-optional`, a mirror carrying only the front package): it fetches the
same release archive and SHA-256-verifies it, and never fails the install —
it warns and exits 0, leaving the error to surface only if `bugsee-cli` is
actually invoked. It also exports `binaryPath()` for spawning the binary
directly. Sources: [`npm/`](npm/).

**`@bugsee/bugsee-cli` — the cargo-dist package, kept as a working alias.**
One package, no binary inside; its `postinstall` runs `install.js`, which
downloads the archive for the host on every fresh install. That means it needs
network at install time and does nothing at all under `--ignore-scripts`.
Generated by cargo-dist from `installers = [..., "npm"]`; unchanged, still
published, still supported. New integrations should use `@bugsee/cli`.

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
