# CLAUDE.md

Guidance for working in the `bugsee-cli` repo.

## What this is

`bugsee-cli` is a single cross-platform Rust binary that collects debug
information files (dSYM, ELF, R8/ProGuard mappings, JS source maps), resolves
build-environment metadata (VCS, CI provider, iOS dependency graph, Xcode
version, Mach-O UUIDs), and uploads symbols + per-build metadata to Bugsee.

It is the canonical, language-neutral origin for build-time uploads: thin
per-build-system orchestrators shell to it instead of each re-implementing the
HTTP / compression / retry / presigned-handshake logic — the Android Gradle
plugin (Kotlin), and the iOS SDK + fastlane `BugseeAgent`s (Python).

Binary crate, no library target. Edition 2021, MSRV 1.88 (declared in
`Cargo.toml`'s `rust-version`). `rust-toolchain.toml` selects `stable`, NOT the
MSRV — and CI builds `stable` too, so nothing in CI catches MSRV drift. A
dependency bump can silently raise the real floor; check with `cargo +<ver>
check --all-targets` and update `rust-version` in the same change.

## Commands

```sh
cargo build [--release]                     # release -> target/release/bugsee-cli
cargo test                                  # unit + integration tests
cargo clippy --all-targets -- -D warnings   # lint (CI gate — warnings are errors)
cargo fmt -- --check                        # format check (CI gate)
```

- There is no lib target: `cargo test --bin bugsee-cli` runs the in-module unit
  tests; `cargo test --test <name>` runs an integration test from `tests/`.
- Format with `cargo fmt` (or `rustfmt --edition 2021 <file>` for one file). Keep
  reformatting scoped to the files you actually changed — don't bundle a
  whole-crate reformat into an unrelated diff.
- The crate ships on macOS, Linux, AND Windows. A platform-only dependency goes
  in a `[target.'cfg(unix)'.dependencies]` table that MUST come AFTER the plain
  `[dependencies]` table — TOML absorbs every key after a `[table]` header into
  that table, so placing it mid-`[dependencies]` silently moves all following
  deps under `cfg(unix)` and they vanish on Windows (native build stays green, so
  only the Windows CI leg catches it). Verify a Cargo.toml dep change with
  `cargo metadata --no-deps` (check each dep's `target`).

## Layout

`src/main.rs` parses args, optionally daemonizes (see the post-action note),
builds the tokio runtime, and dispatches.

- `src/cli/` — one module per command, each owning its clap arg struct + logic:
  `debug_files` (upload/convert), `sourcemaps` (inject), `upload`
  (build/build-info), `pack`, `vcs_metadata`, `ios_deps`, `build_env`, `dsym`,
  `xcode` (post-action). `mod.rs` defines the top-level `Cli`/`Command` and
  `dispatch`. The Xcode post-action's helpers live alongside: `xcactivitylog`
  (build-timings decode), `xcode_ipa` (`.app`→`.ipa` packaging + Mach-O UUID),
  `size_check` (in-build size gate).
- `src/upload/` — upload transports + shared HTTP. `http` owns the one reqwest
  client, retry/backoff, the telemetry header, and log truncation — ALL network
  I/O flows through it. `build` (artefact registration + single-PUT), `chunked`
  (chunked artefact protocol), `build_info` (metadata-bundle ZIP), `presigned`
  (two-stage POST-metadata-then-PUT symbol upload).
- `src/symbols/` — per-format identification/packaging (dSYM, ELF, PDB,
  ProGuard, source map). `rust` sits ACROSS the others: a Cargo target's symbols
  are a `.dSYM`, a `.pdb`, or the ELF itself depending on the triple, so it
  classifies by container magic (never host OS — cross-compilation is normal),
  skips Cargo intermediates, and diagnoses the build settings that silently
  produce unusable symbols.
- `src/compress/` — the ZIP + zstd packer (the wire format).
- `src/inject/` — source-map debug-id injection.
- `src/daemon.rs` — the Unix double-fork for `xcode post-action`'s background mode.
- `src/error.rs` / `src/exit_code.rs` — the typed error → stable exit-code mapping.

## Conventions & contracts

These are load-bearing — external integrators depend on them.

### `--help` is part of the public surface — keep it in sync
When you add or change any command, subcommand, positional argument, option/flag,
or value-enum variant, you MUST update its `--help` description in the SAME
change. The clap doc comment (`///`) on the field / variant / command IS the help
text:
- Every command, positional, option, and value-enum variant must carry a
  non-empty description.
- Behavior driven by environment variables (not clap flags) belongs in the
  command's `after_long_help` / `after_help` — see `xcode post-action`'s
  "Environment variables" section for the pattern.
- Verify with `cargo run -- <command path> --help` (e.g.
  `cargo run -- debug-files upload --help`).

### stdout is reserved for structured output
`stdout` carries ONLY a command's machine-readable result — JSON for the metadata
commands (`vcs-metadata`, `ios-deps`, `build-env`, `dsym`), the one-line result
report for `xcode post-action`. ALL logging / progress / diagnostics go to
`stderr` via `tracing`. The Python integrators parse stdout with `json.loads`, so
a stray `println!` to stdout from a metadata command breaks them. Never write to
stdout outside a command's defined output.

### Exit codes are a stable contract (`src/exit_code.rs`)
Integrators branch on the exit code (e.g. to decide whether to fall back to their
in-language path). The ranges are public: `0` success; `1`/`2` structural (caller
should fall back); `10–39` substantive failures (no fallback); `40` a deliberate
build gate (size-check). Changing a code's meaning is a breaking change that must
be coordinated across every consumer — add new codes, never repurpose old ones.

### Wire shapes are a public contract
Each command's stdout JSON, the upload ZIP layout/compression, the registration
POST body, and the bundle entry names are consumed by the worker, the appserver,
and the language integrators. Breaking a field name, casing, type, null-vs-omit
rule, or entry name requires a major version bump and a coordinated rollout.

### Other defaults
- Compression: zstd is the wire format, default level 11, production floor 9; the
  ZIP uses method 93 (Z_STANDARD). `--no-zstd` is diagnostic only.
- Every upload request carries the `X-Bugsee-Uploader: cli` telemetry header (set
  in `upload::http`).
- Return a typed `error::Error` (or `anyhow` wrapping one) so `main`'s `classify`
  maps it to the right exit code. A bare `anyhow::anyhow!` falls through to exit 1
  (structural) — use a typed variant whenever the code matters to a caller.

## Testing

- Unit tests live in `#[cfg(test)] mod tests` within each module. HTTP is mocked
  with `wiremock` (an in-process server); pin the exact request body + the
  response contract, not just "something happened".
- `tests/` holds integration tests that exec the COMPILED binary (`assert_cmd`)
  and pin exit codes + stdout JSON.
- `scripts/e2e_flows.py` is a stdlib-only end-to-end harness that drives a real
  binary through every upload flow against a protocol-accurate mock server. CI
  runs it from-source on every push (`.github/workflows/e2e.yml`) and against the
  published binary on demand (`windows-e2e.yml`).
- Write tests for every change and run `cargo test` before considering it done.

## The `xcode post-action` command

`bugsee-cli xcode post-action` runs the whole iOS build-publish flow from an Xcode
"Run Script" post-action — build-timings, `.app`→`.ipa` packaging, build
registration, artefact + build-info upload, dSYM upload, and the in-build
size-check. It is configured through `BUGSEE_*` environment variables and/or the
equivalent `--enable-*` / `--disable-*` toggle pairs and `--size-check-*` value
flags (a flag overrides its env var; within a pair the last one wins). Both
surfaces are documented in its `--help`. The flags are overlaid onto the
collected environment map in `dispatch` (`apply_overrides`) before any gate
runs, so the env-driven gate logic stays the single source of truth.

It runs in the BACKGROUND by default: `main` double-forks (`daemon.rs`) BEFORE
building the tokio runtime. Forking a live multi-threaded runtime is undefined
behavior, so the fork MUST stay ahead of any thread/runtime creation — keep
`should_daemonize` + `daemonize` strictly before the runtime is built.
`--force-foreground` runs synchronously and is the only mode in which a
size-check FAIL can fail the build (exit 40).

## Distribution & activation

Releases are tag-driven: pushing a `vX.Y.Z` tag runs `release.yml` (cargo-dist
builds the target triples + a GitHub Release with shell/powershell/npm/homebrew
installers). Two workflows then chain off it automatically via `workflow_run` —
`npm-publish.yml` (npm, via OIDC Trusted Publishing) and `mirror-to-s3.yml`
(`download.bugsee.com/cli/v<ver>/` + the `latest` alias and the
`cli/v<major>.x/version.txt` pointer that `bugsee-cli update` reads). Both also
keep a `workflow_dispatch` path for backfilling a missed version.

`npm-publish.yml` has TWO independent jobs, because npm ships two packages
for the same binary: `publish` takes the cargo-dist-generated
`bugsee-cli-npm-package.tar.gz` release asset straight to `@bugsee/bugsee-cli`
(a single package whose postinstall downloads the binary), while
`publish-optional-deps` runs `npm/build.mjs` over the release's binary archives
to assemble and publish `@bugsee/cli` plus its five `@bugsee/cli-<platform>`
packages (per-`os`/`cpu` `optionalDependencies`, so nothing downloads at install
time). Six packages, platform packages FIRST — npm skips an unresolvable
optional dependency silently, so the front package must never be live ahead of
them. See `npm/README.md`. Each package needs its own Trusted Publisher entry on
npmjs.com pointing at `npm-publish.yml` — and that entry lives on a PACKAGE's
settings page, so it cannot be added for a name that has never been published.
A new name therefore needs one manual token-auth publish first; the runbook is
`npm/README.md` ("First-publish bootstrap"). Skipping it fails only at publish
time, with a 404 on the `PUT` that reads as though the package does not exist
rather than as a missing trust configuration. The publish loop is written to be
resumable for exactly that reason — npm versions are immutable, so a partial
first run would otherwise wedge the version permanently.

`workflow_run` is the trigger because GitHub suppresses `release:`/`push:`
chains for activity initiated by the default `GITHUB_TOKEN`, which is what dist
publishes the Release with; `workflow_run` fires on another WORKFLOW completing
and is not suppressed. A tag push should therefore reach GitHub, npm, AND the
CDN with no manual step — if a release ever lands on only some of those, check
these two workflows first.

`release.yml` is GENERATED by cargo-dist from `[workspace.metadata.dist]` — do
not hand-edit it for anything except the action pins dependabot manages. That
config sets `allow-dirty = ["ci"]`, so `dist plan` no longer verifies the file
against the generator (dependabot's action bumps kept breaking that check, and
with it tag releases). The cost is that dist-config changes no longer propagate
on their own — and the flag disables the WRITER too, so `dist generate` (and
`--check`) are silent no-ops for release.yml, and clearing the flag to force a
regenerate would revert the hand-added `retention-days: 1` lines and the
dependabot-maintained `actions/checkout` pin. After editing `targets`,
`installers`, `publish-jobs`, or `cargo-dist-version`, verify with `dist plan
--output-format=json` (computed live; it is what the workflow's first job runs)
and hand-edit release.yml only if the plan needs something the file cannot
already express. A `targets` change does not: the build matrix — `container` and
`packages_install` included, which is how a cross-compiled leg would build under
cargo-xwin — comes from `plan` at run time. (Adding a triple is still a release
risk for a different reason: dist's `host` job needs EVERY
`build-local-artifacts` leg, nothing validates a new leg before a tag push
(`ci.yml` has no target matrix, `pr-run-mode = "plan"`), and a failed leg means
no Release — hence no S3 mirror and no npm publish, since both chain off
`workflow_run.conclusion == 'success'`. See bugsee/bugsee-cli#20.)

New capabilities are activated by integrators via a version FLOOR: each integrator
pins a minimum CLI version and only uses a new command/flag when the resolved
binary meets it. So shipping a feature is a two-step rollout — publish the CLI at
the new version, THEN bump the integrator's floor. Don't assume a new command is
live just because it's merged here.
