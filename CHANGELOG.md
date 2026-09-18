# Changelog

All notable changes to `bugsee-cli` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- **`debug-files upload --type sourcemaps` uploads several maps at a time.**
  Each map is an independent metadata POST + presigned PUT pair and the loop was
  strictly sequential, so a web build with one map per chunk spent its upload
  time waiting on round-trips. Measured against a mock with 50 ms of injected
  latency: 60 maps **7.09 s → 1.31 s**, 200 maps **23.58 s → 4.10 s**. A
  few-hundred-chunk app on a CI runner paid the serial floor every build.

  `--concurrency N` (1..=32) sets a **ceiling**, not a fixed width — no more
  uploads run than there are maps. Left unset it scales with the batch: one
  upload per 8 maps, at least 4, at most 8. The cap sits below the fastest value
  measured here (200 maps run in 2.75 s at `--concurrency 16`) on purpose: the
  machine that suffers most from serial uploads is a CI box on a thin uplink,
  where the transfer is bandwidth-bound and more streams only add latency.
  `--concurrency 1` restores the old sequential behaviour; the identification
  pass stays sorted and still fails before anything is uploaded.
- **`debug-files upload --allow-empty`** treats "nothing to upload" as success
  instead of exit 10 (`--type sourcemaps`). A monorepo package built without
  maps, or a framework whose server output has none, is a legitimate no-op — but
  it failed the caller's build, or (with the plugin's default) warned while the
  maps that DID exist elsewhere went unuploaded, because the pass had aborted.
  `xcode upload-dsyms` already treats nothing-to-upload as success by design.
  A path that does not exist is still an error (`path does not exist: …`), so a
  typo'd output directory is not swallowed by the flag.

### Fixed
- **A 429 no longer fails a non-idempotent upload request.** The symbol metadata
  POST and the build registration POST are sent with status-retries disabled,
  because a 5xx may mean the server processed the request and only the response
  was lost. A 429 carries no such ambiguity — the request was rejected without
  being processed — so it is now retried with the usual backoff even for those
  requests. They are also the first thing a server throttles when several
  uploads run at once, which concurrent source-map uploads make likelier.

## [0.7.9] - 2026-09-17

### Fixed
- **`sourcemaps inject` now registers a debug-id another tool already wrote.**
  A bundle carrying its own `//# debugId=` — Rollup 4 writes one with
  `output.sourcemapDebugIds: true` — counted as already injected, so it never
  got the `globalThis._bugseeDebugIds` runtime registration and the SDK could
  not attach its debug-id to a crash frame. Such a bundle now keeps its id (its
  map already carries it) and gains only the registration, without a second
  comment; it is still never re-keyed. `js_registered` in the log counts them.
  Verified against real Rollup 4.60 output: after inject, loading the bundle
  registers Rollup's id. See [#40].

[#40]: https://github.com/bugsee/bugsee-cli/pull/40

## [0.7.8] - 2026-09-17

### Fixed
- **Re-uploading a symbol the server already has no longer fails.** The
  appserver answers a duplicate with HTTP 200 and the code nested in its error
  envelope — `{ok: false, error: {type: "DuplicateSymbolsFoundError", code:
  16004}}` — but the client only recognised a top-level `code: 16004`, which no
  current route sends. Every already-uploaded symbol was therefore a hard failure
  (exit `30`) instead of an `already_existed` skip (exit `0`), and a
  `debug-files upload` over a directory stopped there: the second production
  build of a web app with one unchanged chunk could not upload its changed ones.
  The fix is in the shared presigned client, so it reaches every format that
  registers through it: dSYM, PDB, Rust, IL2CPP line maps, source maps, ProGuard
  and ELF. One exception on the server side: for an Android app with NO subtype,
  the appserver re-registers every ProGuard, ELF and source-map upload instead of
  deduping it, so those never received the duplicate reply (any subtype —
  Flutter, React Native, Unity, KMP, Cordova, Xamarin, .NET — and every
  non-Android app does).
  See [#35]. Visible effects beyond `debug-files upload`:
  - `xcode upload-dsyms` no longer **fails the Xcode build** on every rebuild
    whose dSYM is unchanged.
  - `xcode post-action` now reports `dsym_uploaded: true` when every dSYM was
    already on the server, instead of `false`.
- **`debug-files upload --type sourcemaps` no longer aborts on a CSS source
  map.** In a scanned directory, maps named as stylesheet or type-declaration
  maps (`.css.map`, `.d.ts.map`, `.d.mts.map`, `.d.cts.map`) are skipped without
  being read, instead of failing every other map with exit `11`. Any other map
  without a debug-id still fails the run — now BEFORE anything is uploaded, so
  that failure no longer leaves a partial upload (a network or server error
  mid-batch still can) — as does a map named explicitly
  on the command line and a scan that leaves nothing to upload. Directory scans
  are processed in sorted order.

### Changed
- **`sourcemaps inject` derives a bundle's debug-id from its paired map as well
  as its own bytes.** The server dedups source maps by id alone, and a minifier
  routinely emits byte-identical JS for a source edit that moves original lines,
  so a bundle-only id kept the STALE map on the server — silently, now that a
  duplicate is a success. That includes webpack 5's default rebuild with
  `[contenthash]` filenames: the JS is kept on disk unchanged — still carrying
  its stub and old id — and only the map is re-emitted, so `inject` now
  re-keys a bundle whose OWN stub is present when its map comes back without an
  id and with different content (`js_restamped` in the log). A `//# debugId=`
  another tool wrote is never re-keyed. A bundle with no map keeps its
  bundle-only id. Ids are
  never recomputed downstream (the runtime and the worker both read the embedded
  id), so nothing else changes — except that, once, after upgrading, every
  bundle that has a map gets a new id and its map uploads again. When a bundle
  is freshly injected beside a map that already carries a DIFFERENT id (a
  stamped map left beside a re-emitted bundle), the map is rewritten to the
  bundle's id with a warning instead of being left mismatched; a map that
  disagrees with a bundle which ALREADY had its id (e.g. one map named by two
  bundles) is left as is, with a warning, so `inject` stays idempotent. A map
  missing one of `debug_id` / `debugId` gains it. Bundles are walked in sorted
  order.
- **`debug-files upload --type sourcemaps --force`** now asks the server to
  replace a map it already has (`overwrite`), as it already did for dSYM, PDB,
  Rust and IL2CPP line maps.

[#35]: https://github.com/bugsee/bugsee-cli/pull/35

## [0.7.7] - 2026-09-16

### Added
- **Windows ARM64 (`aarch64-pc-windows-msvc`) is now a published target** — the
  sixth triple, bringing a seventh npm package (`@bugsee/cli-win32-arm64`).
  Closes [#20]. That leg builds NATIVELY on a `windows-11-arm` runner rather
  than cross-compiling: under cargo-xwin, `ring` assembles its ARM64 Windows
  `.S` files with the plain `clang` driver, which reads MSVC-style `/imsvc`
  include flags as filenames. `installer/install.ps1` now resolves ARM64 hosts
  (preferring `PROCESSOR_ARCHITEW6432`, so a 32-bit PowerShell on 64-bit Windows
  is not misread as x86), and `ci.yml` build-checks and e2e-tests the target on
  every PR so a broken leg cannot reach a tag push.
- **`bugsee-cli xcode upload-dsyms`** — dSYM upload from an Xcode Run Script
  build phase, with none of the `BUGSEE_BUILD_INFO_*` gating. It neither
  registers a build nor uploads build-info, so it is safe to run on every build,
  and it is the shape a React Native / Flutter config plugin can generate
  (`withXcodeProject`) rather than a scheme post-action. Closes [#19].

  A genuine failure **fails the build** by design — a missing, empty or rejected
  token (`20`/`21`), a server or network error (`30`/`31`), or a dSYM folder or
  bundle that could not be read (`10`/`11`) — because a build phase that
  swallows errors means symbolication silently stops working and nobody notices
  until a crash report is unreadable. "Nothing to upload" (no dSYM folder, or no
  `.dSYM` bundles in it) is a success, not a failure; "found bundles, uploaded
  none" is not.

  Failing the build and detaching the upload are **independent**, each with an
  on/off pair (`--fail`/`--no-fail`, `--background`/`--no-background`) and an env
  var (`BUGSEE_DSYM_UPLOAD_NO_FAIL`, `BUGSEE_DSYM_UPLOAD_BACKGROUND`); a flag
  overrides its env var. The default is strict and synchronous; `--no-fail`
  selects detaching unless `--no-background` says otherwise, which is usually
  what CI wants — never break the build, but still wait, so a runner tearing
  down its process tree cannot kill the upload mid-flight. Asking to fail the
  build *and* detach is refused (`2` as flags, `20` from the environment) rather
  than honoured, because a detached process's exit code reaches nobody. See
  [#28].

  `xcode post-action` is unchanged: `BUGSEE_BUILD_INFO_ENABLED=0` still disables
  dSYM upload there.

### Fixed
- **A non-UTF-8 environment variable no longer crashes the CLI.** `vcs-metadata`,
  `build-env machine-label` and `xcode post-action` collected the environment
  with `std::env::vars()`, which PANICS on a non-Unicode key or value anywhere
  in it — not just in a variable this CLI reads. They exited **101**, a code
  outside the documented contract entirely, so an integrator re-deriving
  `should_fallback` got neither a structural failure to fall back on nor a
  substantive one to propagate. Xcode and CI environments are large and not
  curated; a variable set by any other tool in the build was enough. Closes [#29].
- **`bugsee-cli update` on Windows ARM64** refused to run: `host_triple()` had
  no `("windows", "aarch64")` arm and returned `None`. The mapping is now a
  table rather than `match` arms — a `match` can only ever be exercised for the
  host it compiled on, which is how the platform went missing — and two tests
  pin it against `[workspace.metadata.dist].targets` so the two cannot drift
  again.

[0.7.7]: https://github.com/bugsee/bugsee-cli/compare/v0.7.6...v0.7.7

[#19]: https://github.com/bugsee/bugsee-cli/issues/19
[#20]: https://github.com/bugsee/bugsee-cli/issues/20
[#28]: https://github.com/bugsee/bugsee-cli/issues/28
[#29]: https://github.com/bugsee/bugsee-cli/issues/29


## [0.7.6] - 2026-09-16

Packaging and CI release. **No functional changes to the binary** — the CLI
surface, exit codes, stdout JSON shapes, and upload wire format are identical to
0.7.5, so no integrator needs to move its version floor.

### Added
- **A second npm channel, `@bugsee/cli`**, whose binary ships in five
  `os`/`cpu`-tagged packages (`@bugsee/cli-darwin-arm64`, `-darwin-x64`,
  `-linux-arm64`, `-linux-x64`, `-win32-x64`) declared as
  `optionalDependencies` and pinned to the exact crate version. npm resolves one
  of them, so an install downloads a single binary and **nothing runs at install
  time** — it works under `--ignore-scripts`, unlike the download-on-postinstall
  approach. A `postinstall` fallback still downloads from the GitHub release if
  no platform package resolved, and never fails the install. There is no Windows
  arm64 package: see [#20].
- `@bugsee/bugsee-cli` is **unchanged** and keeps publishing as a working alias.

### Changed
- `npm-publish.yml` gained a second, independent `publish-optional-deps` job for
  the family. It publishes platform packages **before** the front package (npm
  skips an unresolvable optional dependency silently, so the reverse order would
  ship a front package with no binary), is resumable across a partial failure
  (npm versions are immutable), and routes prereleases to the `next` dist-tag
  rather than moving `latest`.
- The S3 mirror now chains off the Release workflow automatically instead of
  requiring a manual dispatch — v0.7.4 was released and never mirrored, so the
  CDN served 0.7.3 for six weeks and `bugsee-cli update` never offered it.
- Release artifacts and the cargo-dist cache now expire after 1 day.

### Fixed
- Corrected the README, which described `@bugsee/cli` with per-OS optional
  dependencies as the existing npm channel. It was not — both the name and the
  mechanism were wrong.
- Corrected the `Cargo.toml` and `CLAUDE.md` guidance that said to run `dist
  generate` after a dist-config change. `allow-dirty = ["ci"]` disables dist's
  writer as well as its check, so `dist generate` and `dist generate --check`
  are silent no-ops for `release.yml`; verify with `dist plan
  --output-format=json` instead.

### Security
- **`rustls` 0.23.40 -> 0.23.45** (with `rustls-webpki` 0.103.13 -> 0.103.15) —
  [RUSTSEC-2026-0285], TLS 1.3 handshake messages incorrectly accepted across
  encryption level boundaries (medium, 5.3). Reachable: every upload goes through
  `reqwest`/`hyper-rustls`, which is the crate's only TLS stack.
- For the record, `h2` [RUSTSEC-2026-0258] does **not** apply to the shipped
  binary. `h2` is a dev-only dependency pulled in by `wiremock`; `reqwest` is
  configured without `http2`, and `cargo tree -i h2 -e normal` resolves to
  nothing.

### Dependencies
- **`plist` 1.10.0 -> 1.10.1** — no advisory, but it fixes a panic on
  out-of-range dates in binary plists, which is reachable through
  `build-env read-plist`. Pulls `quick-xml` 0.41.0 -> 0.42.0 and `base64`
  0.23.1.
- **`flate2` 1.1.9 -> 1.1.10** (`miniz_oxide` 0.8.9 -> 0.9.1) and **`uuid`
  1.24.1 -> 1.26.0** — routine maintenance, no advisories.
- MSRV 1.88 re-verified against the updated lockfile with
  `cargo +1.88 check --all-targets`.

## [0.7.5] - 2026-08-24

Dependency and security maintenance. **No functional changes** — the CLI
surface, exit codes, stdout JSON shapes, and upload wire format are byte-for-byte
identical to 0.7.4, so no integrator needs to move its version floor. Upgrade for
the dependency fixes below.

### Security
- **`quick-xml` 0.39.4 -> 0.41.0** (via `plist`) — fixes two denial-of-service
  advisories in XML parsing: [RUSTSEC-2026-0194] (quadratic run time when a start
  tag is checked for duplicate attribute names) and [RUSTSEC-2026-0195] (unbounded
  namespace-declaration allocation in `NsReader` enabling memory exhaustion). This
  code is reachable: `build-env read-plist` parses XML `Info.plist` files through
  `plist`.
- **`time` 0.3.48 -> 0.3.55** (via `plist`) — 0.3.48 was yanked upstream and had
  shipped since before 0.7.4.
- **`anyhow` 1.0.102 -> 1.0.104** — fixes unsoundness in `Error::downcast_mut()`.
  Not reachable here (this crate only calls `downcast_ref`), included for hygiene.
- Advisories that do **not** apply to the shipped binary, for the record:
  `quinn-proto` [RUSTSEC-2026-0185] and `h2` [RUSTSEC-2026-0258] are absent from
  the release build — `reqwest` is configured without `http2`, and `h2` is pulled
  in only by the `wiremock` dev-dependency. Verified by inspecting compiled
  artifacts, not the lockfile.

### Changed
- `zip` 2.4.2 -> 8.6.0 (two major bumps). The upload ZIP is unchanged: entry
  names, STORED artefacts, method 93 (Z_STANDARD) mappings, and the fixed
  1980-01-01 timestamps all produce byte-identical archives to 0.7.4.
- `sha1`, `sha2`, and `md-5` 0.10 -> 0.11 (RustCrypto `digest` 0.11). Content
  fingerprints, chunk identities, and the md5-derived Java-compatible
  `BUILD_UUID`s are unchanged.
- Routine bumps: `tokio` 1.52 -> 1.53, plus `clap`, `serde`, `serde_json`,
  `regex`, `uuid`, `globset`, `libc`, `plist`, `thiserror`, `futures-util`.
- **Declared MSRV corrected to 1.88** (`rust-version`). The previous `1.79` was
  inaccurate and had been for several releases — the locked tree already required
  1.88 via `gimli`, `globset`, `plist`, and `time`. This documents reality rather
  than dropping support: no toolchain that could build 0.7.4 loses the ability to
  build 0.7.5. Only affects building from source; released binaries are unaffected.

### Removed
- `indicatif` — declared but referenced nowhere in the source. Also prunes
  `console`, `encode_unicode`, `portable-atomic`, `unicode-width`, and the
  unmaintained `number_prefix` ([RUSTSEC-2025-0119]).

### Fixed
- **Tag releases were broken.** Dependabot's action bumps rewrote pins inside
  `.github/workflows/release.yml`, which cargo-dist generates and its `plan` job
  verifies; since `plan` is the first job of the release workflow, a `vX.Y.Z` tag
  push would have failed before building any artefact. `[workspace.metadata.dist]`
  now sets `allow-dirty = ["ci"]`. CI/release only — no effect on the binary.

[RUSTSEC-2026-0194]: https://rustsec.org/advisories/RUSTSEC-2026-0194
[RUSTSEC-2026-0195]: https://rustsec.org/advisories/RUSTSEC-2026-0195
[RUSTSEC-2026-0185]: https://rustsec.org/advisories/RUSTSEC-2026-0185
[RUSTSEC-2026-0258]: https://rustsec.org/advisories/RUSTSEC-2026-0258
[RUSTSEC-2025-0119]: https://rustsec.org/advisories/RUSTSEC-2025-0119

## [0.7.4] - 2026-08-11

### Added
- **`debug-files upload --type il2cpp-linemap`** — upload Unity IL2CPP
  `LineNumberMappings.json` (+ sibling `MethodMap.tsv` / `il2cppFileRoot.txt`)
  keyed by `libil2cpp` / `UnityFramework` module UUID(s). See
  [`docs/unity-il2cpp-linenumber-mappings.md`](docs/unity-il2cpp-linenumber-mappings.md).
- **`debug-files upload --type rust`** — one command for a Cargo project,
  whatever target it built for. A Rust project's symbols are a `.dSYM` (Apple),
  a `.pdb` (`*-pc-windows-msvc`), or the ELF binary itself keyed by its GNU
  build-id (Linux/Android); this discovers whichever is present and routes each
  to the same upload path its `--type`-specific command would use.
  - Discovery is **content-based** (container magic, not host OS), so a
    cross-compiled `target/<triple>/release` uploads correctly from any host.
  - Cargo intermediates — `deps/`, `build/`, `incremental/`, `.fingerprint/` —
    are skipped; walking them would register a symbol document per dependency
    and build script.
  - Loose ELF binaries are uploaded per-file, keyed by build-id with the
    Breakpad transform (the existing `--type elf` path takes AGP's pre-built
    `native-debug-symbols.zip` and is unchanged).
  - `--uuid` is rejected: every Rust debug format carries its own identity, and
    that identity is what the SDK reports for the module at crash time.
- **Build-configuration preflight for Rust.** Each format has a setting that,
  when missing, yields an upload that is accepted and then resolves nothing —
  no DWARF (`debug = 0`), no `.dSYM` (`split-debuginfo` not `"packed"`), no
  build-id (missing `-Wl,--build-id`). Near-misses found during the walk are
  warned about with the exact stanza that fixes them, and a walk that finds
  nothing uploadable fails with the full recipe instead of a bare "not found".
  Verified against real `cargo build --release` output in both directions.

### Fixed
- Cargo publishes the profile-root `.dSYM` as a **symlink** into `deps/`, which
  `walkdir` does not follow — combined with skipping `deps/`, a correctly
  configured macOS build would have been reported as missing its debug info and
  advised to set `split-debuginfo`, which it already had.

## [0.7.3] - 2026-07-16

### Fixed
- **Security — credential leak in logs.** The app token (embedded in
  registration paths) and S3 SigV2 signatures (in presigned-PUT query strings)
  no longer reach error messages or logs. A transport-error path echoed the full
  `reqwest` URL at the DEFAULT log level — and to the `xcode post-action` daemon
  log file — leaking both; it now scrubs the URL (`without_url()`), and the
  debug-level URL log fields are redacted via a shared `redact_url` helper.

### Changed
- Hardening from an adversarial review:
  - `update` refuses a non-HTTPS download base except loopback, and caps
    artefact (512 MiB) / metadata (1 MiB) download sizes so a hostile or
    misconfigured origin can't OOM the host before the SHA-256 check.
  - `update` archive extraction lists and rejects absolute / `..`-traversal
    entries before unpacking.
  - `sourcemaps inject` rejects `..` / absolute `//# sourceMappingURL=` targets,
    so a crafted bundle can't steer it at a file outside the bundle directory.
  - per-`.so` native-upload staging ZIPs are uniquely named, preventing a path
    collision under concurrent upload if two libraries shared a build-id.
  - chunked-upload buffers are bounded to the actual chunk length instead of the
    raw server-provided `chunk_size` (a large `chunk_size` for a small artefact
    no longer over-allocates).

## [0.7.2] - 2026-07-11

### Changed
- Bumped `symbolic-common` / `symbolic-debuginfo` `13.8.0` → `13.9.0`. The ELF
  `code_id` and Mach-O `debug_id` the CLI reads are stable across major-13
  minors; the arch/format identifier-pinning tests confirm no drift.

## [0.7.1] - 2026-07-06

### Changed
- Bumped `symbolic-common` / `symbolic-debuginfo` `13.6.0` → `13.8.0`. The ELF
  `code_id` and Mach-O `debug_id` the CLI reads are stable across major-13
  minors, so this stays identifier-compatible with the worker's `symbolic`.

### Tests
- Pinned `symbolic`'s identifier extraction with real-bytes fixtures across every
  arch/format the CLI parses, so a future crate bump that drifted an identifier
  is caught: ELF `code_id` **and** `arch` on a real aarch64 `.so`; Mach-O
  `debug_id` + `arch` for **x86_64, arm64, and fat/universal** binaries (a new
  hand-assembled, toolchain-free synthesizer exercises symbolic's multi-arch
  iteration); and the `xcode` IPA main-executable UUID extraction's positive path
  (previously only its `None` cases were covered).

## [0.7.0] - 2026-06-25

### Added
- Self-hosted install scripts: `curl … https://download.bugsee.com/cli/install.sh | sh`
  (macOS/Linux) and `irm …/cli/install.ps1 | iex` (Windows PowerShell). They
  resolve the latest version from the mirror, download + SHA-256-verify the
  host's binary from `download.bugsee.com` (no GitHub dependency), and install
  it — overridable via `BUGSEE_CLI_VERSION` / `BUGSEE_CLI_INSTALL_DIR` /
  `BUGSEE_CLI_BASE_URL`. Published at stable URLs by the release mirror.

### Changed
- `debug-files upload --type elf` now uploads native symbols **per `.so`**,
  keyed by each library's real GNU build-id (`code_id`, extracted via
  `symbolic-debuginfo` — identical to the worker's) instead of the build-level
  `--uuid`. Each `.so` is registered + uploaded as its own symbol document
  (pipelines run in parallel), which (a) stops native symbols from colliding
  with the ProGuard mapping server-side — both previously shared the build
  UUID — and (b) enables per-library dedup: an unchanged `.so` (same build-id)
  is skipped before its bytes transfer. A `.so` built without `-Wl,--build-id`
  has no `code_id`; it is warned about and skipped (it can't be matched at
  crash time anyway, so it is never faked with the build UUID).
- Bumped `symbolic-common` / `symbolic-debuginfo` `13` → `13.6.0`. The ELF
  `code_id` the native upload reads is stable across major-13 minors, so this
  stays identifier-compatible with the worker's `symbolic` 13.1.1.

### Fixed
- `upload build-info` now sends `Content-Type: application/octet-stream` on the
  presigned PUT. The appserver signs the build-info URL **with** that
  Content-Type, so omitting it made S3 reject the upload with a 403
  `SignatureDoesNotMatch`. The artefact and chunk PUTs already set it; this
  aligns the build-info path with them.

## [0.6.0] - 2026-06-17

### Added
- `update` — self-update the binary in place. Resolves the newest published
  version WITHIN THE SAME MAJOR as the running binary (minor/patch are
  non-breaking; a major bump is never auto-adopted), downloads and
  SHA-256-verifies the release for the host triple, and atomically replaces the
  current executable (`self-replace`, so the Windows running-`.exe` case works).
  `--check` reports only; `--version X.Y.Z` installs an exact version.
- Release mirror now publishes tiny version pointers for auto-update discovery:
  `cli/latest/version.txt` (absolute latest) and `cli/v<major>.x/version.txt`
  (latest within a major). Both advance-only. The per-major pointer is the
  shared contract the CLI's `update`, the Android Gradle plugin, and the iOS
  BugseeAgents all read to find the newest non-breaking version.

## [0.5.0] - 2026-06-17

### Added
- `xcode post-action` CLI flags as alternatives to its `BUGSEE_*` environment
  variables. Every toggle now has a matching `--enable-<x>` / `--disable-<x>`
  pair (build-info, all-actions, all-configurations, dependencies, timings,
  size-analysis, chunked-upload, size-check), and every size-check threshold a
  value flag (`--size-check-warning-pct` / `--size-check-fail-pct` /
  `--size-check-warning-bytes` / `--size-check-fail-bytes`). A flag passed on
  the command line overrides the corresponding env var; within a pair the last
  flag wins; an unset flag falls back to the env var / default.

## [0.4.0] - 2026-06-17

The release that moves the whole iOS build-publish flow into the CLI: one
`xcode post-action` command does what the iOS SDK's build script used to do in
process, and dSYM uploads gain recursive discovery + pre-upload dedup.

### Added
- `xcode post-action` — run the entire iOS build-publish flow from an Xcode
  "Run Script" post-action: decode build timings from the `.xcactivitylog`,
  package the `.app` into a synthetic `.ipa`, register the build, upload the
  artefact (when size-analysis is enabled) and the build-info bundle, upload
  dSYMs, and run an optional in-build size-check. Runs in the background by
  default (detaches so the archive returns immediately, logging to
  `$PROJECT_TEMP_DIR/bugsee-cli.log`); `--force-foreground` runs synchronously.
  Configured through `BUGSEE_*` environment variables — see
  `bugsee-cli xcode post-action --help`.
- `debug-files upload --type dsym` recursive discovery — point at an Xcode
  archive's `dSYMs/` folder (or a whole DerivedData tree) and every `*.dSYM`
  bundle is found and uploaded; no need to enumerate bundles yourself.
- dSYM pre-upload dedup — the Mach-O slice UUIDs are declared up front so the
  server can skip bundles it already has BEFORE the (possibly large) DWARF bytes
  are packed or transferred. `--force` re-uploads.
- In-build size-check — fail the build with the new exit code **40**
  (`SizeCheckFailed`) when the artefact grows past a configured threshold
  (in `--force-foreground`).

### Changed
- `--help` now documents every command, argument, option, and value-enum
  variant, including the `BUGSEE_*` environment variables that configure
  `xcode post-action`.

## [0.3.0] - 2026-06-15

The release that completes the build-time upload unification surface: artefact
uploads and JS source maps now both flow through the CLI, so producers (Gradle
plugin, fastlane, BugseeAgent) no longer maintain their own HTTP/compression/
retry/chunking stacks.

### Added
- `upload build` — register a build and upload its artefact in one shot. Packs
  the artefact (STORED) plus an optional R8/ProGuard mapping (zstd, method 93)
  into the normalized upload ZIP, then either single-PUTs it or runs the chunked
  protocol (`--chunked`). Emits the build-info bundle from the same registration
  and prints the resulting `build_id` to stdout.
- `upload build --chunked` — full builds chunked-upload protocol
  (chunk-options → streamed SHA-1 hashing → chunk check → PUT-missing dedup →
  chunked submit), for artefacts above the single-PUT threshold.
- `sourcemaps inject` — embed a deterministic, content-derived UUIDv5 debug-id
  into JS bundles (`//# debugId=` plus a defensive `globalThis._bugseeDebugIds`
  runtime stub) and into the paired `.map` (`debug_id` + `debugId`). Idempotent
  and `--dry-run` aware.
- `debug-files upload --type sourcemaps` — discover `.map` files, key each by
  its embedded debug-id (precedence `debug_id` → `debugId` → legacy `uuid`, or a
  caller-supplied `--uuid`), pack as a single zstd entry, and upload through the
  shared presigned protocol. The worker auto-detects the sourcemap format by
  content and re-derives the same key.

### Changed
- `presigned.rs` now runs entirely on the shared `upload::http` layer (one
  HTTP client, telemetry header, retry/backoff, and log-truncation
  implementation across every upload path), and takes an explicit `RetryPolicy`.

### Removed
- `sourcemaps upload` — folded into `debug-files upload --type sourcemaps` so
  every symbol/debug artefact uploads through one command surface.

## [0.2.0] - 2026-06

### Added
- `upload build-info` — per-build metadata bundle upload, plus the shared
  `upload::http` layer (Phase A of the upload unification).
- `pack` — build the normalized upload ZIP locally (artefact STORED, mapping
  zstd method 93) for producers that upload the result themselves.

### Changed
- Crate made fully `rustfmt` + `clippy` clean to keep CI green on `main`.

## [0.1.1] - 2026

### Added
- Homebrew and npm publish channels (the latter via OIDC / Trusted Publishing).

## [0.1.0] - 2026

### Added
- Initial release: debug-file collection, conversion, and upload — dSYM upload
  (`debug-files upload --type dsym`), dSYM UUID/slice inspection (`dsym`), and
  the canonical CI resolvers (`vcs-metadata`, `ios-deps`, `build-env`).

[0.7.6]: https://github.com/bugsee/bugsee-cli/compare/v0.7.5...v0.7.6
[0.7.5]: https://github.com/bugsee/bugsee-cli/compare/v0.7.4...v0.7.5
[0.7.4]: https://github.com/bugsee/bugsee-cli/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/bugsee/bugsee-cli/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/bugsee/bugsee-cli/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/bugsee/bugsee-cli/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/bugsee/bugsee-cli/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/bugsee/bugsee-cli/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/bugsee/bugsee-cli/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/bugsee/bugsee-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/bugsee/bugsee-cli/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bugsee/bugsee-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/bugsee/bugsee-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bugsee/bugsee-cli/releases/tag/v0.1.0
[RUSTSEC-2026-0285]: https://rustsec.org/advisories/RUSTSEC-2026-0285
[RUSTSEC-2026-0258]: https://rustsec.org/advisories/RUSTSEC-2026-0258
[#20]: https://github.com/bugsee/bugsee-cli/issues/20
