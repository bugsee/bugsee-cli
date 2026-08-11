# Changelog

All notable changes to `bugsee-cli` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
