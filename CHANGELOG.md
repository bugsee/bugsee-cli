# Changelog

All notable changes to `bugsee-cli` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Self-hosted install scripts: `curl … https://download.bugsee.com/cli/install.sh | sh`
  (macOS/Linux) and `irm …/cli/install.ps1 | iex` (Windows PowerShell). They
  resolve the latest version from the mirror, download + SHA-256-verify the
  host's binary from `download.bugsee.com` (no GitHub dependency), and install
  it — overridable via `BUGSEE_CLI_VERSION` / `BUGSEE_CLI_INSTALL_DIR` /
  `BUGSEE_CLI_BASE_URL`. Published at stable URLs by the release mirror.

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

[0.6.0]: https://github.com/bugsee/bugsee-cli/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/bugsee/bugsee-cli/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/bugsee/bugsee-cli/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/bugsee/bugsee-cli/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bugsee/bugsee-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/bugsee/bugsee-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bugsee/bugsee-cli/releases/tag/v0.1.0
