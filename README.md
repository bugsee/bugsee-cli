# bugsee-cli

Cross-platform Rust binary that collects debug information files (dSYM, ELF, Windows PDB, R8/ProGuard mappings, JS source maps, Unity IL2CPP line maps; PE, Portable PDB and Breakpad are planned), resolves build-environment metadata (VCS, CI provider, iOS dependency graph, Xcode version, Mach-O UUIDs), and uploads symbols to Bugsee. One binary, shelled by thin per-build-system orchestrators (Android Gradle plugin, Xcode Run Script via the iOS SDK's BugseeAgent, fastlane plugin, and the npm package used by the React Native and web tooling); .NET, Unity and Flutter integrations are planned.

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
`bugsee-cli update` (see [`update`](#update)). Linux builds are glibc-only; the installer
refuses musl (Alpine). Other channels: npm (`@bugsee/cli` —
`npx @bugsee/cli --version` to run it once, or `npm i -D @bugsee/cli` and then
`npx bugsee-cli` inside that project; a bare `npx bugsee-cli` anywhere else asks
npm for an unscoped `bugsee-cli` package, which does not exist), a Homebrew tap, or the
per-build-system bundles — see [Distribution](#distribution).

## Building

```sh
cargo build --release
```

Binary lands at `target/release/bugsee-cli`. Pinned to stable Rust via `rust-toolchain.toml`.

## Subcommands

The metadata-resolving subcommands print JSON (a bare string for `build-env xcode-version` / `machine-label`) to stdout and exit 0 on parseable failure (empty list / null / empty object) so Python integrators can shell with `check=False` and rely on the output shape rather than the exit code. (`xcode upload-dsyms` is deliberately not one of these: it prints nothing to stdout and is designed to exit non-zero so a build phase fails — see its section below.) Hard failures (network, auth, malformed argv) follow the [exit-code contract](#exit-code-contract) below.

### `debug-files upload <paths>...`

```
bugsee-cli debug-files upload <paths>... \
    --version <X> --build <Y> \
    [--type proguard|rust|elf|dsym|pdb|sourcemaps|il2cpp-linemap] \
    [--uuid <UUID>]   # required for elf; rejected for dsym/pdb/rust; IL2CPP module id(s), comma-separate for multi-ABI \
    [--il2cpp-uuid <UUID>] [--il2cpp-root <PATH>]  # il2cpp-linemap only \
    [--icon <PATH>]   # proguard only: attach launcher icon to the symbol zip \
    [--zstd-level N | --no-zstd]  # 9..=22, default 11; --no-zstd is diagnostic only
    [--extension <SUFFIX>]  # also match these file-name suffixes (repeatable / comma-separated), e.g. .so.sym
    [--force]         # re-upload even if the server already has it (dsym/pdb/elf/rust/il2cpp-linemap/sourcemaps; proguard ignores it)
    [--concurrency N] # sourcemaps only: ceiling on uploads in flight, 1..=32 (default: scaled)
    [--allow-empty]   # sourcemaps only: "nothing to upload" is success, not exit 10/11
    [--strip-sources-content]  # sourcemaps only: upload maps without the embedded source
    [--dry-run]
```

The upload flow itself. ProGuard, Rust, ELF, dSYM, PDB, sourcemap, and Unity IL2CPP line-map types are working; `pe`, `portable-pdb`, `breakpad`, `jvm`, `sourcebundle` and `wasm` are scaffold-only and rejected (exit 20).

`--extension <SUFFIX>` picks up files under a spelling the CLI does not know yet, IN ADDITION to each type's built-in names, so a toolchain change does not need a CLI release first. It matches the end of the whole name (`.so.sym` works), and each type's content check still applies to what it adds (ELF build-id, PDB container, dSYM `Contents/Resources/DWARF`). See `debug-files upload --help` for what it widens per type.

#### Android NDK — `--type elf`

Takes one or more paths: AGP's pre-built `native-debug-symbols.zip`, and/or directories, which are walked recursively for libraries (e.g. `build/intermediates/merged_native_libs/<variant>`) and read in place — nothing is re-zipped. Directory symlinks are not descended; an unreadable entry fails the run (exit 11) and a directory with no libraries exits 10. Every library found is uploaded as its own symbol, keyed by its GNU build-id — `--uuid` (the SDK's `BUILD_UUID`) is still required but only correlates logs. Built-in entry names are `.so`, `.so.dbg` (`ndk.debugSymbolLevel = 'FULL'`) and `.so.sym` (`'SYMBOL_TABLE'`, function names only — no `file:line`). A library without a build-id is warned about and skipped; two entries with the same build-id upload once (across all the given paths), preferring the one with DWARF. The server dedups on the build-id, and each library declares whether it carries debug info (`dwarf`) or only a symbol table (`symtab`), read from the file itself rather than its name. Switching a library from `SYMBOL_TABLE` to `FULL` therefore replaces the stored symbols on its own, transferring the bytes once (the run logs `upgraded SYMBOL_TABLE -> FULL`); re-running with the same file, or uploading a symbol table when the server holds debug info, transfers nothing. `--force` still means "always replace" and re-sends every library. A server that predates this behaviour skips the `FULL` upload as already present; the run then says so and `--force` is the only way to replace it.

#### Unity IL2CPP — `--type il2cpp-linemap`

Uploads `LineNumberMappings.json` (+ sibling `MethodMap.tsv` / `il2cppFileRoot.txt`) as format `il2cpp-linemap`, keyed by the IL2CPP module UUID(s) (`libil2cpp` / `UnityFramework`). The mappings JSON is validated first (`{cpp_path: {cs_path: {cpp_line: cs_line}}}`, integer line numbers, checked as a stream): a truncated or wrong file exits 11 naming the file and uploads nothing. See [`docs/unity-il2cpp-linenumber-mappings.md`](docs/unity-il2cpp-linenumber-mappings.md).

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
    # Pin the install dir and call the binary by path: the installer's default
    # (/usr/local/bin if writable, else ~/.local/bin) may not be on PATH here.
    curl -fsSL https://download.bugsee.com/cli/install.sh | BUGSEE_CLI_INSTALL_DIR="$HOME/.local/bin" sh
    "$HOME/.local/bin/bugsee-cli" debug-files upload --type rust target/release \
      --version "${{ github.ref_name }}" --build "${{ github.run_number }}"
  env:
    BUGSEE_APP_TOKEN: ${{ secrets.BUGSEE_APP_TOKEN }}
```

Pass `--dry-run` to verify discovery and see the preflight warnings without uploading.

### `vcs-metadata`

```
bugsee-cli vcs-metadata [--working-dir <PATH>]
```

Resolves VCS metadata (provider, commit SHA, branch, base branch, PR number, repo) from CI provider env vars (GitHub Actions, GitLab CI, Bitbucket Pipelines). On any other CI, or locally, it falls back to `git` in `--working-dir` (default: the current directory), which yields only `commit_sha` and `branch`. Output shape pinned by `tests/cross_language_contract.rs`:

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

Discovers and parses iOS dependency manifests at `<PATH>` and its ancestors (up to 6 levels): `Podfile.lock` (CocoaPods), `Package.resolved` (SPM — pure-package and Xcode-managed shapes, with sibling `*.xcodeproj` / `*.xcworkspace` probing), `Cartfile.resolved` (Carthage), and vendored frameworks linked into `--product-binary` (via `/usr/bin/otool`). Merges with field-wise url-preference dedup. Output:

```json
{
  "entries": [{"id":"library::Alamofire","group":"","name":"Alamofire","direct":true,"type":"library","parents":[],"version":"5.10.0","url":"https://github.com/Alamofire/Alamofire.git"}],
  "scope_label": "all",
  "truncated": false
}
```

`version`, `scope`, and `url` are omitted when absent; `parents` is always present (an empty list when none). Mirrored on the Python side. The `url` field is load-bearing for OSV SwiftURL ecosystem vuln lookups — drift on the optional-field semantics silently degrades vuln-scan coverage.

### `build-env` helpers

Three sub-subcommands; each prints its result to stdout or empty string on unresolved. Consumed by both Python BugseeAgents to eliminate duplicated in-process helpers.

- `build-env xcode-version` — reads `XCODE_VERSION_ACTUAL` env if set (`"1620"` → `"16.2.0"`), else shells `/usr/bin/xcodebuild -version` and normalises to 3-part dotted form. Empty string on failure.
- `build-env machine-label` — returns `<provider>[:<detail>]`, or the local hostname when no CI provider is detected, matching the Android Gradle plugin's `BuildMachineResolver` cascade so the dashboard can group iOS + Android builds from the same CI runner.
- `build-env read-plist <plist>` — emits a JSON dict of `key → string` for all scalar entries in the plist (string, int, real, bool, uint). Dict / array / Data / Date values are silently dropped (scalars-only contract). Returns `{}` on missing file.

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

`--strip-sources-content` uploads each map WITHOUT its `sourcesContent` — including the copies an
indexed map keeps inside `sections[].map` — for teams who would rather their source did not leave
the build machine. Symbolication still resolves file, line and column;
what is lost is the source snippet shown beside a crash frame. The map on disk is never modified —
only the copy that is uploaded — and the declared `hash` describes the stripped bytes. A map that
carries no `sourcesContent` is uploaded byte-for-byte unchanged.

`--allow-empty` turns "nothing to upload" into success (exit 0) instead of
exit 10 (no maps found) or 11 (only stylesheet / type-declaration maps found) — a monorepo package built without maps, or a framework whose server
output has none, is a legitimate no-op rather than a reason to fail the build.
A path that does not exist is an error regardless (`path does not exist: <p>`,
exit 10) — including when other paths do hold maps — so a typo or a build that
never ran cannot half-upload a build's symbols.

`--concurrency`, `--allow-empty` and `--strip-sources-content` apply to
`--type sourcemaps` only, and are rejected (exit 20) for any other type rather
than accepted and ignored.

`--exclude <glob>` (repeatable) keeps `inject` out of part of a build output — `--exclude
'**/node_modules/**'` leaves vendored third-party code inside a server bundle untouched, `--exclude
'polyfills*.js'` skips one file by name.

A pattern is tried against the absolute path, the path relative to the current directory, and the
path relative to each walked root, so `dist/vendor/**`, `vendor/**` and an absolute path all work
whether you pass `dist`, `./dist` or the absolute directory. `*` crosses `/` (globset's default), so
`*.js` matches `vendor/v.js` too — anchor with a leading `/` or a directory prefix if you do not want
that. An unparseable or empty pattern is a configuration error (exit 20) rather than a silent
"matches nothing", which would rewrite exactly the files you meant to protect. Patterns select
BUNDLES: an excluded bundle's `.map` is left alone with it, but a pattern matching only `.map` files
excludes nothing.

**A bundle with no source map is still stamped, on purpose.** It looks like waste — the id cannot
resolve to a symbol — but a crash frame carrying a debug-id whose map was never uploaded marks the
report `missing_sym`, which is what prompts you to upload it. An unstamped bundle is silently
unsymbolicated instead. Measured on a stock `next build` with browser source maps on: 39 JS files,
12 maps, so 27 bundles are stamped without one; that is the case that produces the prompt. Use
`--exclude` when you would rather those files were not touched at all.

**A build that pins its own script hashes is REFUSED** (exit 20). Injecting appends bytes to every
`.js`, so a Subresource Integrity hash the HTML already carries stops matching and the browser
refuses to run the script — measured on a real webpack + `webpack-subresource-integrity` build in
Chromium 151: before injecting the app ran, after it the entry script was blocked and the page
executed nothing. `inject` looks for `<script integrity src=…>` and
`<link rel=modulepreload|preload integrity href=…>` in the HTML under the paths it was given, and
stops before writing anything. Angular's `subresourceIntegrity: true` is the same mechanism.

Fix it by stamping BEFORE the hashes are computed, by `--exclude`-ing the pinned files, or — if your
build recomputes hashes after this runs — with `--allow-sri`.

The guard only ever refuses over a file this run would really REWRITE, so a re-run that changes
nothing is still a no-op — including on a build stamped once with `--allow-sri` — and an excluded
file, a stale page pinning a bundle that no longer exists, or a page pinning something outside the
output does not stop it either.

Pages are read from anywhere under the path you give it, plus any sitting directly in that path's
parent — the usual layout is `dist/index.html` beside `dist/assets/*.js`, so `inject dist/assets`
still sees the page that pins those bundles. `--dry-run` refuses too: the preview of a run that
would refuse is a refusal, and it tells you why. A URL is matched literally first and then by file name, so a `publicPath` — a CDN origin,
`/static/`, `/_next/` — still resolves to the local bytes it names; the cost of that fallback is that
a third-party script sharing a file name with one of your bundles would be treated as yours.

**It cannot see SRI that is not in the emitted HTML**: a manifest consumed by a server template
(`webpack-assets-manifest` with `integrity: true`), a page rendered at request time (Next.js
`experimental.sri`), or HTML your build writes outside the directory you point this at. Those builds
still break, so keep `--allow-sri` off and check a deploy before trusting it.

```
bugsee-cli sourcemaps inject <paths>... [--exclude <glob>]... [--allow-sri] [--dry-run]
bugsee-cli debug-files upload --type sourcemaps <paths>... --version <v> --build <b> \
    [--concurrency N] [--allow-empty] [--strip-sources-content]
```

### `upload build`

Registers a build record — the thing a symbol upload, a size analysis and a crash all hang off — and,
optionally, ships the build artefact's bytes.

```
bugsee-cli upload build --payload-json <path> \
    [--artifact <.aab|.apk|.ipa>]  # omit to REGISTER ONLY, shipping no bytes \
    [--mapping <mapping.txt>]      # needs --artifact (it rides inside the ZIP) \
    [--deps <deps.json>] [--timings <timings.json>] \
    [--chunked]                    # needs --artifact \
    [--zstd-level N | --no-zstd]   # 9..=22, default 11 \
    [--dry-run [--out <zip>]]      # --out needs --artifact
```

On success stdout carries the registered build id, so the producer can correlate.

`--payload-json` is the registration body, written by the producer (the Gradle plugin, the Xcode
post-action, a bundler plugin) and passed through verbatim apart from two fields the CLI injects:
`request_artifact_upload`, and `request_build_info_upload` when a sidecar is present.

**Without `--artifact` the build is registered and nothing is packed or sent.** That is the normal
case wherever size analysis is not enabled — and the only case a web build can express, having no
single artefact to ship. `--deps`/`--timings` still travel, because the build-info bundle is a
separate upload from the artefact. The flags that only describe how artefact bytes move (`--mapping`,
`--chunked`, `--out`) are rejected with exit 20 rather than ignored: dropping a `--mapping` silently
would cost symbolication.

Dedup is server-side on the payload's `uuid` (replace-then-create), which is why the registration POST
is retried on a transport error but never on a 5xx (a 429 is still retried: the server rejected the
request unprocessed).

### `upload build-info`

```
bugsee-cli upload build-info (--payload-json <path> | --upload-url <url>) \
    [--deps <dependencies.json>] [--timings <timings.json>] [--sidecar NAME=PATH]... \
    [--zstd-level N | --no-zstd] [--dry-run [--out <zip>]]
```

Bundles per-build metadata sidecars into one zstd ZIP and uploads it with a single PUT — on its own,
for a producer that registers builds elsewhere. With `--payload-json` it registers the build first
(injecting `request_build_info_upload: true`); with `--upload-url` it skips registration and PUTs to a
URL the producer already received. The bundle is additive: the worker tolerates unknown entry names.

### `pack`

```
bugsee-cli pack --artifact <.aab|.apk|.ipa> [--mapping <mapping.txt>] --out <zip> [--zstd-level N | --no-zstd]
```

Local only: writes the normalized upload ZIP the worker's size-analysis job consumes, and uploads
nothing. The artefact is STORED verbatim (it is already compressed); the mapping is zstd-compressed
(method 93). Lets the Gradle plugin delegate compression instead of bundling zstd-jni.

### `xcode post-action`

```
bugsee-cli xcode post-action [--force-foreground] [--enable-<x> | --disable-<x>]... [--size-check-* <value>]...
```

The whole iOS build-publish flow from an Xcode scheme **post-action**: build timings, `.app` → `.ipa`
packaging, build registration, artefact + build-info upload, dSYM upload and the in-build size check.
It is configured through `BUGSEE_*` environment variables and/or the equivalent `--enable-*` /
`--disable-*` toggle pairs and `--size-check-*` thresholds (a flag overrides its env var). It gates on
`BUGSEE_BUILD_INFO_*` (Release-only by default) and is a no-op, exit 0, when gated out.

It runs in the **background** by default — it detaches so the archive returns immediately, logging to
`$PROJECT_TEMP_DIR/bugsee-cli.log`. `--force-foreground` runs it synchronously, and is the only mode in
which a size-check FAIL can fail the build (exit 40). Every toggle and variable is listed in
`bugsee-cli xcode post-action --help`.

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

### `update`

```
bugsee-cli update [--check] [--version X.Y.Z] [--force] [--max-age <duration>]
```

Self-updates the binary in place: downloads the release for the host, verifies its SHA-256 and
atomically replaces the running executable. By default it takes the newest version **within the same
major**; `--version X.Y.Z` installs an exact version (a different major is allowed, with a warning).
`--check` only reports. `--max-age 12h` checks at most once per interval and makes every failure
best-effort (exit 0), so an integrator can run it on every build. Downloads come from
`download.bugsee.com/cli`; override with `BUGSEE_CLI_UPDATE_BASE_URL` for an internal mirror.

### `debug-files convert` (not yet implemented)

```
bugsee-cli debug-files convert <input> --to bmf|bsf --output <path>
```

Converts to Bugsee's legacy BMF/BSF formats, for existing deployments only. The arguments are fixed,
but the command currently always fails with exit 1.

### Global flags

`--endpoint` (env `BUGSEE_ENDPOINT`), `--app-token` (env `BUGSEE_APP_TOKEN`). Both global so every subcommand inherits the same `BUGSEE_ENDPOINT` override path the per-build-system integrators already standardise on. Only the upload-flavoured subcommands (`debug-files upload`, `upload build`, `upload build-info`, `xcode post-action`, `xcode upload-dsyms`) actually consume these values; metadata-resolving subcommands (`vcs-metadata`, `ios-deps`, `build-env`, `dsym`, `sourcemaps inject`) do no network I/O and ignore them, as do `pack` and `debug-files convert`. `update` does download, but from `download.bugsee.com/cli` (`BUGSEE_CLI_UPDATE_BASE_URL`), not `--endpoint`.

### Subcommand vocabulary

Multi-word subcommand names are hyphenated (`vcs-metadata`, `ios-deps`, `build-env`, `debug-files`). Single-word names are bare (`dsym`, `sourcemaps`). Sub-subcommands keep the hyphenation pattern (`debug-files upload`, `ios-deps collect`, `build-env xcode-version`). This is the same scheme `cargo`, `kubectl`, and `gh` follow, and the Python integrators consume the names verbatim — renaming any subcommand is a wire-shape break under the [compatibility policy](#wire-shape-compatibility-policy) below.

### Built-in help

- `bugsee-cli --help` and `bugsee-cli <subcommand> --help` print the full surface (clap-derived; covers every flag, env-var alias, and subcommand).
- `bugsee-cli --version` prints the SemVer. Integrators that bind to a specific output shape should pin against this — see the wire-shape compatibility policy.
- Sample output for the JSON-emitting metadata subcommands lives in [Subcommands](#subcommands). The pinned reference vectors used by the cross-language integration tests live under `tests/fixtures/`.

Full documentation: <https://docs.bugsee.com/cli/>. There is no `man bugsee-cli` page.

## Wire-shape compatibility policy

Each subcommand's stdout JSON shape is the source of truth for at least one Python script outside this repo (the fastlane plugin's `BugseeAgent` and/or the iOS SDK's `tools.bundle/BugseeAgent`). Drift = silent breakage on real builds.

Compatibility rules for releasing changes:

- **Backward-compatible (no version coordination required).** Adding a new subcommand. Adding a new optional output field with `#[serde(skip_serializing_if = "Option::is_none")]`.
- **Backward-compatible-with-care (announce in CHANGELOG).** Adding a new required output field (only if every consumer already tolerates unknown fields — verify in the consumer tests). Adding a new env var the resolver checks.
- **Breaking.** Renaming a field. Removing a field. Changing a field's type or casing. Changing `[]` ↔ `null` / `{}` ↔ `null` semantics. Changing exit-code semantics. Any of these requires a major version bump and a coordinated landing across the fastlane plugin (`fastlane-plugin-bugsee/BugseeAgent`) and the iOS SDK (`tools.bundle/BugseeAgent`).

Cross-language reference vectors live in `tests/cross_language_contract.rs` + `tests/fixtures/`. Each test runs the compiled binary against a checked-in fixture and pins the stdout JSON shape end-to-end. Mirror tests on the Python side (`scripts/test_bugsee_agent_*_cli.py` in the iOS SDK; `test/test_bugsee_cli_migration.py` in the fastlane plugin) feed canned JSON in the same shape — if the Rust output ever drifts, both sides break.

## Exit-code contract

Stable. Integrators (Gradle plugin, fastlane plugin, iOS SDK BugseeAgent) use these codes to decide whether to fall back to their in-language uploader during the dual-path rollout phase. The npm launcher passes them through unchanged.

| Code  | Meaning                                                              | Caller should fall back? |
|-------|----------------------------------------------------------------------|--------------------------|
| 0     | Success (uploaded, or server reports already-exists, or resolver returned empty/null output). | n/a                      |
| 1     | Unexpected / unhandled error.                                        | **yes**                  |
| 2     | Usage / argv error (likely a plugin↔CLI version mismatch).           | **yes**                  |
| 10–19 | Input / discovery problems (file not found, unparseable format).      | no                       |
| 20–29 | Configuration problems (missing/rejected token, incompatible flag combination). | no            |
| 30–39 | Upload problems (network, server 4xx/5xx).                            | no                       |
| 40    | Build gate failed deliberately (e.g. size-check FAIL).               | no                       |
| 41+   | Reserved.                                                            | no                       |

An unknown or malformed flag is clap's usage error, exit 2; exit 20 is a valid flag the CLI rejects in combination. If the binary is killed by a signal, the npm launcher reports `128 + signum`, which falls in the reserved range.

The fallback rule: codes ≤ 2 mean the CLI never got a fair chance to run; codes ≥ 10 are substantive failures the in-language uploader would hit the same way. See `src/exit_code.rs` for the source-of-truth enum.

Note: the metadata subcommands return exit **0** even when no useful result is found. The JSON emitters (`vcs-metadata`, `ios-deps collect`, `build-env read-plist`, `dsym *`) signal "no result" through the JSON shape (empty list / empty object / specific field absence), so Python integrators can use `check=False` + `json.loads(stdout)` without branching on returncode. `build-env xcode-version` and `build-env machine-label` print **plain text**, not JSON — an empty line when unresolved — so read their stdout as a string; `json.loads` fails on a successful `16.2.0`.

## Telemetry header

Every metadata `POST` sets `X-Bugsee-Uploader: cli`. The in-language fallback uploaders are meant to send a different value of the same header so the backend can count CLI-vs-fallback usage without touching customer code. The header is **not** added to the presigned S3 `PUT` — that signature is bound to a specific header set, and S3 would reject extras with `SignatureDoesNotMatch`.

## Distribution

| Channel | Used by |
|---|---|
| GitHub Releases + `download.bugsee.com/cli` (archives, `.sha256`, `latest/` and `v<major>.x/version.txt`), with the shell / PowerShell installers and `bugsee-cli update` | everyone; generic CI |
| npm `@bugsee/cli` — per-platform `optionalDependencies` | JS toolchains (React Native, web bundler plugins) |
| npm `@bugsee/bugsee-cli` — single package, `postinstall` downloader | legacy alias for the above |
| CDN download + SHA-256 checksum on first use | fastlane plugin (`resolveCli`), iOS SDK BugseeAgent |
| Homebrew tap `bugsee/tap` | macOS / Linux developers |

Planned, not published yet: Maven Central `com.bugsee:bugsee-cli`, NuGet `Bugsee.CLI` and UPM `com.bugsee.cli`.

Target platforms: macOS arm64 + x86_64, Linux x86_64 + aarch64 (glibc only; no musl build), Windows x86_64 + arm64. (Windows arm64 builds natively on a `windows-11-arm` runner — see [#20](https://github.com/bugsee/bugsee-cli/issues/20).)

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
(`--omit=optional`, a mirror carrying only the front package): it fetches the
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
  main.rs              arg parsing, optional daemonize (before the runtime), dispatch
  daemon.rs            Unix double-fork for background `xcode post-action` / `upload-dsyms`
  cli/                 clap command tree — one module per command
    mod.rs               top-level Cli / Command + dispatch
    debug_files.rs       debug-files upload / convert
    sourcemaps.rs        sourcemaps inject
    upload.rs            upload build / build-info
    pack.rs              pack
    vcs_metadata.rs      vcs-metadata
    ios_deps.rs          ios-deps collect
    build_env.rs         build-env xcode-version / machine-label / read-plist
    dsym.rs              dsym uuid / dsym slices
    xcode.rs             xcode post-action / upload-dsyms
    xcactivitylog.rs     build-timings decode (post-action)
    xcode_ipa.rs         .app → .ipa packaging + Mach-O UUID (post-action)
    size_check.rs        in-build size gate (post-action)
    update.rs            update
  symbols/             per-format discovery + identification (dsym, elf, pdb, proguard, sourcemap,
                       il2cpp_linemap; rust classifies across them by container magic; suffix = --extension)
  compress/            Zstd-in-ZIP packaging (the wire format)
  upload/
    http.rs              the one HTTP client: retry/backoff, telemetry header, log truncation
    build.rs             build registration + single-PUT artefact upload
    chunked.rs           chunked artefact protocol (`upload build --chunked`)
    build_info.rs        build-info metadata bundle
    presigned.rs         two-stage POST-metadata → PUT symbol upload (debug-files upload)
  inject/              JS source-map debug-ID injection (+ sri.rs: Subresource Integrity guard)
  error.rs             typed errors → exit codes
  exit_code.rs         source-of-truth enum for the exit-code contract above

tests/                 integration tests against the compiled binary (cross_language_contract.rs,
                       debug_files_flags.rs, elf_upload.rs, xcode_*.rs, …) + fixtures/
scripts/e2e_flows.py   end-to-end harness: every upload flow against a protocol-accurate mock server
npm/                   the @bugsee/cli package family (see npm/README.md)
installer/             install.sh / install.ps1 served from download.bugsee.com/cli
```

## License

[MIT](LICENSE) © Bugsee, Inc.
