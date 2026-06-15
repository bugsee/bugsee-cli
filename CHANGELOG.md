# Changelog

All notable changes to `bugsee-cli` are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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

[0.3.0]: https://github.com/bugsee/bugsee-cli/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bugsee/bugsee-cli/compare/v0.1.1...v0.2.0
[0.1.1]: https://github.com/bugsee/bugsee-cli/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/bugsee/bugsee-cli/releases/tag/v0.1.0
