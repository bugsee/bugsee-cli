# Build-time upload unification

**Status:** Design — not yet implemented
**Owner:** Pending
**Affects:** `bugsee-cli`, `worker`, `appserver`, `android/gradle-plugin`, `ios/fastlane-plugin-bugsee`, `ios/sdk` BugseeAgent
**Out of scope:** Runtime SDK telemetry uploads (crashes, sessions, attributes — those use a different transport via the SDKs themselves)

## TL;DR

Today the platform has **five upload code paths** (Android Gradle plugin, iOS SDK BugseeAgent, fastlane plugin, `bugsee-cli debug-files`, ad-hoc Python in CI scripts) emitting **three wire formats** (ZIP-with-zstd entries, raw gzip streams, raw bytes) across **multiple endpoints** (`/apps/<token>/symbols`, `/v2/apps/<token>/builds`, `dependencies_upload_endpoint`, `timings_upload_endpoint`, ...).

This doc converges all of that to:

1. **`bugsee-cli` is the single canonical origin for every Bugsee build-time upload.** Every producer (Gradle plugin, both Python BugseeAgents, future plugins) shells to `bugsee-cli` for the actual POST + PUT orchestration. No producer maintains its own HTTP client, compression, retry, or signing logic.
2. **Three upload classes** with clear boundaries:
   - **Symbols** (mapping, native symbols, dSYM, ELF, PE/PDB) — standalone upload per symbol file.
   - **Build-info bundle** (deps + timings + future sidecars) — one ZIP, one PUT per build.
   - **Artefact** (`.apk`/`.aab`/`.ipa`) — pure binary, optionally chunked for large files.
3. **Single wire format for compressed payloads:** ZIP container with zstd-93 entries (matching the existing symbol-files contract). STORE-93 entries for already-compressed content (`.aab` inside the artefact ZIP).
4. **Soft rollout via S3 key-prefix routing** on the worker — no magic-byte sniffing, no producer awareness of which format the appserver expects.

## Guiding principle: `bugsee-cli` as the common origin

> **All Bugsee build-time data uploads MUST originate from `bugsee-cli`.**
>
> Producers (Gradle plugin, fastlane plugin, iOS SDK BugseeAgent, third-party CI scripts) are responsible for *what* to upload — finding the dSYM, computing the dependency graph, deciding when to ship — but never for *how* to upload it. Compression strategy, ZIP packing, presigned-URL handshake, retries, chunking, telemetry headers, and the on-wire codec are all internal to `bugsee-cli`.

This is the natural conclusion of the C-series migration that already moved VCS resolution, iOS dependency parsing, build-env helpers, and dSYM UUID extraction into `bugsee-cli`. The migration arc closes once upload orchestration follows the same path.

### Why this matters

| Without `bugsee-cli`-as-origin | With `bugsee-cli`-as-origin |
|---|---|
| 3 producers × 3 codecs × N retry strategies = drift, edge cases | 1 implementation, all producers shell to it |
| Wire-format change requires migrating 3+ producer codebases in lockstep | Wire-format change ships in `bugsee-cli`; producers just update their CLI version pin |
| Each producer needs its own HTTP client, TLS config, proxy handling, retry/backoff | One Rust HTTP client (reqwest), tested in one place |
| Python BugseeAgents need `zstandard` PyPI dep for Python <3.14 | Rust `zstd` crate already vendored in the CLI; Python knows nothing about codecs |
| Producer code is multi-hundred-LOC per repo | Producer code is ~50 LOC `subprocess.run` wrapper per repo |
| Adding a new upload class (size-report sidecar, vuln-scan blob, custom event log) requires N producer changes | Adding a new upload class is one new `bugsee-cli` subcommand + one PR per producer to call it |

### What this looks like for each producer

**Android Gradle plugin:**
```kotlin
// Today: BundleUploader.kt → custom HTTP, gzip, presigned-URL handshake (~400 LOC)
// Future: shell to bugsee-cli
exec.commandLine("bugsee-cli", "upload", "build-info",
    "--app-token", appToken,
    "--endpoint", endpoint,
    "--payload-json", payloadFile.absolutePath,
    "--deps", depsFile.absolutePath,
    "--timings", timingsFile.absolutePath,
)
```

**iOS SDK BugseeAgent + fastlane BugseeAgent (Python):**
```python
# Today: _gzip_json_bytes + _put_dependencies_blob + _put_timings_blob (~200 LOC × 2 files)
# Future: shell to bugsee-cli (matches the existing _via_cli helper pattern)
subprocess.run([cli, "upload", "build-info",
    "--app-token", app_token,
    "--endpoint", endpoint,
    "--payload-json", payload_path,
    "--deps", deps_path,
    "--timings", timings_path,
], check=False, timeout=120)
```

The producer doesn't know about ZIP, zstd, presigned URLs, retries, or chunking. The producer's only job is to materialise the input files and pass their paths.

### CLI binary distribution

The CLI is already auto-downloaded by the fastlane plugin (`resolveCli` with SHA-pinned tarball from `download.bugsee.com`). The Gradle plugin needs the same plumbing — it already has `cliPath` plumbing for the Phase 1 dual-path mapping upload (`UploaderStrategy.CLI`); the build-info path extends that. The iOS SDK BugseeAgent uses `shutil.which("bugsee-cli")` and falls back gracefully when the CLI isn't on PATH (matches the existing C-series migration posture).

## The three upload classes

After cleanup, every Bugsee build-time upload falls into exactly one of these:

### 1. Symbols

**Payload:** Debug-information files for crash symbolication.
- Android ProGuard/R8 `mapping.txt`
- Android native symbols (`.so` with DWARF)
- iOS dSYM bundles
- ELF / PE/PDB / Portable PDB (other platforms)

**Endpoint:** `POST /apps/<token>/symbols` (unchanged from today)
**Wire format:** ZIP container, all entries zstd-93 (already in production for non-mapping symbol types; mapping ZIPs today use DEFLATE — migrating to zstd is part of this work)
**Topology:** One upload per symbol file. No batching.

**Why standalone:** Symbols can ship without an associated artefact upload (split build/publish CI, apps without the Bugsee Gradle plugin, late-arriving symbol files after a build is already registered). The symbol pipeline is keyed on debug-id, not build-id — it must work even when the build record doesn't exist yet.

### 2. Build-info bundle

**Payload:** All metadata sidecars for a build.
- `dependencies.json` — dep graph (CocoaPods, SPM, Carthage, Gradle deps)
- `timings.json` — build timings (xcactivitylog summary, Gradle task timings)
- Future entries: `vcs.json`, `build-env.json`, `xcactivitylog.json` (raw), or anything else that's per-build metadata

**Endpoint:** `POST /v2/apps/<token>/builds` returns `build_info_upload_endpoint` (NEW field)
**Wire format:** ZIP container, all entries zstd-93
**Topology:** ONE bundle per build, ONE PUT.

**Why bundled:** Single PUT cuts latency on slow links (3 sequential PUTs → 1); atomic success/failure semantics (no more `deps_status=ok, timings_status=failed` partial states); free sidecar slot for any future per-build metadata (no new endpoint required).

### 3. Artefact

**Payload:** The built binary itself, for size analysis.
- `.apk` / `.aab` (Android)
- `.ipa` (iOS — synthetic, packaged from `.app`)

**Endpoint:** `POST /v2/apps/<token>/builds` returns `endpoint` (artefact presigned URL, unchanged from today)
**Wire format:** Pure binary, no ZIP wrapping. Optionally chunked for files exceeding a threshold (~50 MB default).
**Topology:** One PUT per build (single-PUT) OR N PUTs (chunked).

**Why bare bytes:** The artefact is already a compressed container (`.aab` is ZIP, `.apk` is ZIP, `.ipa` is ZIP); wrapping it in another ZIP adds zero compression benefit and ~70 bytes of central-directory overhead. The chunked-upload path slices the raw byte stream — no inner format to preserve. Size-analysis worker has dedicated parsers (`androguard`, AAB protobuf manifest, Mach-O via `symbolic-debuginfo`) keyed on file extension.

## Mapping deduplication (cleanup)

**Today's redundancy:** Android builds with both mapping upload AND size-analysis enabled upload mapping TWICE — once standalone via `MappingUploadTask` to `/symbols`, and again embedded in the artefact ZIP via `BundleUploadTask.writeNormalizedUploadZip`. The worker stores two copies in two different schemas.

**The fix:**
- `MappingUploadTask` continues to upload mapping standalone (unchanged).
- `BundleUploadTask.writeNormalizedUploadZip` drops the `mapping: File?` parameter; the artefact ZIP becomes pure binary.
- The size-analysis worker cross-references the symbol-store (keyed on `(app_token, build_uuid)`) to deobfuscate class names in the size report.

**Net effect:** Mapping ships once instead of twice; pipelines have cleaner boundaries; ~6 lines removed from the Gradle plugin; one cross-pipeline lookup added on the worker.

## Wire format details

### ZIP-with-zstd contract

Every compressed entry in every ZIP container uses zstd (`ZipEntry.method = 93`, the Z_STANDARD method registered by InfoZIP). The Bugsee worker already supports this for the symbol-files endpoint (`utils.compression.zipfile`, redirected through Python 3.14's stdlib `compression.zstd`).

**Default level: 11.** Minimum production floor: 9. The CLI's existing symbol upload uses these values; the build-info path inherits them.

**STORE-93 for already-compressed entries.** When a producer knows an entry is already a compressed container (`.aab`/`.apk`/`.ipa` inside an outer artefact ZIP if we ever wrap one; `.png`/`.mp4`/`.car` inside a synthetic `.ipa`), use STORE (method 0) to avoid recompression that would grow the entry. The existing `_IPA_STORE_EXTENSIONS` list in both BugseeAgents is the reference set.

### Entry naming convention

The ZIP entry names are stable contract — the worker dispatches per-asset processing off them.

**Build-info bundle entries:**
- `dependencies.json` — the dep blob (when emitted)
- `timings.json` — the timings blob (when emitted)
- Future: `vcs.json`, `build-env.json`, `<feature>.json` — additive, worker tolerates unknown names

**Symbol bundle entries:**
- `mapping.txt` + optional `icon.{png,jpg}` for the Android mapping case
- `<binary-name>` for Mach-O / ELF / PDB (the file inside `Contents/Resources/DWARF/` or equivalent)
- One symbol class per ZIP (the symbol-files pipeline is keyed on debug-id; mixing classes complicates the worker dispatch)

**Artefact:** N/A — pure binary, no entries.

## Worker ingestion: S3 key-prefix routing

**The migration model:** the appserver stops signing presigned URLs at the old keys and starts signing at NEW keys. The worker grows new S3 event rules pointing the new prefixes at the new (or unchanged) handlers. The OLD handlers stay running unchanged to serve in-flight uploads through their natural expiry.

This is **strictly better than magic-byte sniffing** because:

- The dispatch happens at S3-event time, before any blob download. No false-positive risk on corrupt/truncated bytes.
- The appserver is the single source of truth for which format a given build uses (chosen at presigned-URL signing time).
- Rollback is one appserver config flip — no worker change needed.
- Telemetry is trivial: count S3 events per prefix; legacy count decays to zero as old presigned URLs expire.

### Concrete key shape

| Today | Migration | Routes to |
|---|---|---|
| `final/dependencies/<build_id>-<tid>.json` | `final/legacy-gz/dependencies/<build_id>-<tid>.json` (during soak window) | `jobs.dependencies.Process` (kept until soak ends) |
| `final/timings/<build_id>-<tid>.json` | `final/legacy-gz/timings/<build_id>-<tid>.json` (during soak window) | `jobs.timings.Process` (kept) |
| — (new) | `final/build-info/<build_id>-<tid>.zip` | `jobs.build_info_bundle.Process` (new) |
| `final/builds/<build_id>-<tid>.zip` | unchanged | `jobs.builds.Process` (unchanged, artefact pipeline) |
| `symbols/<symbol_id>` | unchanged | `jobs.symbols.Process` (unchanged) |

The `legacy-gz/` prefix is **only added during the soak window** so the worker can distinguish "this was signed by an old appserver" from "this is the new bundled format" via path alone, without inspecting the blob.

## Rollout phases

Each phase is additive and shippable independently. Rollback at any point is one config flip.

### Phase A — `bugsee-cli` upload subcommands (~2 weeks)

- New `bugsee-cli upload build-info` subcommand: takes paths to `deps.json` / `timings.json` / future sidecars + a metadata JSON; ZIPs with zstd-93; POSTs registration; PUTs to returned URL.
- New `bugsee-cli upload symbols --type {mapping,dsym,elf,...}` (consolidates the existing per-type flags into one subcommand).
- New `bugsee-cli upload artefact` (continues the work for #97 — chunked upload + single-PUT fallback).
- All three subcommands share the same HTTP client, retry policy, telemetry header (`X-Bugsee-Uploader: cli`), and exit-code contract.
- Tests: integration tests against a mock appserver (the `wiremock-rs` crate or similar).

### Phase B — Worker accepts the new format (~1 week)

- Add `jobs.build_info_bundle.Process` job class that downloads the ZIP, walks entries, and fans out to existing `jobs.dependencies` / `jobs.timings` / future handlers per-entry.
- Add S3 event rule for `final/build-info/*` → new job class.
- Keep `final/dependencies/*` and `final/timings/*` rules pointing at existing handlers (legacy producers).
- Tests: ZIP-walker, per-entry try/except, idempotent re-entry on SQS redelivery.

### Phase C — Appserver issues new URLs (~3 days)

- `POST /v2/apps/<token>/builds` response gains `build_info_upload_endpoint` field. Producers that know how to consume it use it; producers that don't continue to use `dependencies_upload_endpoint` + `timings_upload_endpoint` (kept signed during soak).
- Feature flag: `BUGSEE_FEATURE_BUILD_INFO_BUNDLE_ENABLED=1` per-org or per-app, default off, ramp to default on once Phase D producers are deployed.

### Phase D — Producers cut over (~2 weeks, parallel across 3 repos)

In parallel:
- **`android/gradle-plugin`**: `BundleUploadTask` shells to `bugsee-cli upload build-info` when `build_info_upload_endpoint` is present in the registration response. Falls back to legacy two-PUT path when absent.
- **`ios/fastlane-plugin-bugsee/BugseeAgent`**: `_run_dependencies_pipeline` shells to `bugsee-cli upload build-info`. Falls back identically.
- **`ios/sdk/tools.bundle/BugseeAgent`**: same as fastlane.

Each producer ships its cutover behind `BUGSEE_LEGACY_BUILDINFO_GZIP=1` escape hatch for emergency rollback during the soak.

### Phase E — Mapping cleanup (~1 week)

- `android/gradle-plugin`: `BundleUploadTask.writeNormalizedUploadZip` drops the `mapping: File?` parameter; artefact ZIP becomes pure binary.
- Worker size-analysis pipeline gains a "look up mapping from symbol-store by `(app_token, build_uuid)`" path.
- The duplicate-upload elimination ships AFTER the bundled-build-info migration so we don't compound migration risk.

### Phase F — Telemetry watch (1 release cycle)

- Watch S3 event arrival counts on `final/legacy-gz/*` decay toward zero.
- Watch the `X-Bugsee-Uploader: cli` vs `X-Bugsee-Uploader: kotlin-fallback-*` header rates on metadata POSTs.

### Phase G — Drop legacy paths (~3 days)

- Worker removes `jobs.dependencies.Process` and `jobs.timings.Process` (replaced by `jobs.build_info_bundle.Process`'s fan-out).
- Appserver stops signing `dependencies_upload_endpoint` / `timings_upload_endpoint` URLs.
- Producers remove their legacy-fallback code paths and the `BUGSEE_LEGACY_BUILDINFO_GZIP` escape hatch.

## Per-repo change list

| Repo | Phase A | Phase B | Phase C | Phase D | Phase E | Phase G |
|---|---|---|---|---|---|---|
| `bugsee-cli` | New `upload` subcommand tree (~800 LOC + tests) | — | — | — | — | — |
| `worker` | — | New job class + S3 event rule + tests (~300 LOC) | — | — | Size-analysis cross-pipeline lookup (~50 LOC) | Drop legacy job classes |
| `appserver` | — | — | New presigned URL field + feature flag (~50 LOC) | — | — | Drop legacy fields |
| `android/gradle-plugin` | — | — | — | Shell to CLI in `BundleUploadTask` (~150 LOC retained for fallback) | Drop `mapping` from `writeNormalizedUploadZip` | Drop fallback path |
| `ios/fastlane-plugin-bugsee` | — | — | — | Shell to CLI in `_run_dependencies_pipeline` (~50 LOC) | — | Drop fallback path |
| `ios/sdk/tools.bundle/BugseeAgent` | — | — | — | Shell to CLI in `_run_dependencies_pipeline` (~50 LOC) | — | Drop fallback path |
| `viewer` | — | — | — | — | — | — |

**Viewer: zero changes** at any phase. The dashboard fetches parsed JSON via presigned S3 URLs with `Content-Encoding: gzip`; the browser transparently decompresses; no client-side codec code anywhere. Wire-format migration is fully invisible.

## Cross-platform readiness

Build-info collection ships first for **bare Android (Gradle) and iOS (Xcode)** builds. The cross-platform producers — **React Native, Flutter, Unity, Cordova, Kotlin Multiplatform, .NET (MAUI/Xamarin)** — are built *on top of* this foundation rather than alongside it. This section records why the foundation generalizes and the few items to settle before those producers are written.

### Why the foundation already generalizes

The transport, storage, and diff layers are platform-agnostic by construction, and the dependency model is already ecosystem-aware:

- **Dependency identity is `(type, group, name)`** with `group` optional and `version` nullable (`worker/builds/dependencies_diff.py`). `type` is the ecosystem discriminator — Maven (`group:name:version`), CocoaPods / SPM / Carthage (url-keyed, often no group), npm (no group), NuGet, pub, Cargo all fit. Entries of different `type` never collide, so a single list can mix ecosystems.
- **It is already multi-ecosystem in production.** iOS deps (CocoaPods / SPM / Carthage, parsed by `bugsee-cli ios-deps`) flow through the *same* `dependencies.json` → `dependencies.process` path as Android Gradle/Maven deps. Multi-ecosystem-through-one-schema is proven, not aspirational.
- **The worker fans out by entry *name*, not platform** (`jobs.build_info_bundle.Process` dispatches on `dependencies.json` / `timings.json`). A new platform reuses the same handlers unchanged.
- **The bundle is additive.** A platform can introduce its own sidecar (`metro-stats.json`, `il2cpp.json`, …) and the worker tolerates the unknown entry with zero changes until a handler is added.
- **`bugsee-cli upload build-info` is pure pack-and-PUT** — it neither knows nor cares which platform produced the inputs.

Net: cross-platform is mostly **more `type` values + more producers**, not new architecture.

### What must be handled (additive, not rework)

| Item | Detail |
|---|---|
| **Caps at npm / Unity scale** | npm transitive trees (React Native) are the largest dependency graphs in existence (tens of thousands of packages); Unity asset / dependency graphs are also large. A big RN monorepo can approach the current `8 MiB`-compressed / `100k`-entry deps caps that are generous for bare mobile. **Resolved by the native/wrapper split below** — each layer gets its own cap, so the npm-scale graph (wrapper) no longer shares a ceiling with the bounded native graph. |
| **Producer parsers** | Each platform must *emit* `dependencies.json` / `timings.json` in the schema. Extend the `bugsee-cli ios-deps` pattern (`npm-deps`, `pub-deps`, `nuget-deps`, Unity manifest) or produce it per-wrapper. Net-new, purely additive — does not touch the bundle/worker foundation. |
| **Schema evolution** | The worker validators are permissive on *entry fields* (only `schema_version` + array shape are checked), so new ecosystem-specific fields are free. A `schema_version` **bump**, however, requires a coordinated worker update (`_SUPPORTED_SCHEMA_VERSIONS`) and per-`type` viewer rendering. Grow the schema additively; bump the version only when unavoidable. |

### Multi-ecosystem layout (settled): `native-deps.json` + `wrapper-deps.json`

Cross-platform builds have two distinct dependency layers, and the bundle keeps them in **two files** rather than merging into one:

- **`native-deps.json`** — the underlying native build's dependencies: Maven/Gradle (Android), CocoaPods / SPM / Carthage (iOS). Present for **every** build, bare or cross-platform. This *is* today's deps blob — the existing pipeline (schema, validator `dependencies_helpers`, `dependencies_diff`, store key, `dependencies_*` build-doc fields) is reused unchanged; `native-deps.json` is the bundle entry name that feeds it.
- **`wrapper-deps.json`** — the cross-platform framework's own dependencies: npm/yarn (React Native, Cordova), pub (Flutter), NuGet (.NET MAUI / Xamarin), Unity Package Manager (Unity), Kotlin/Gradle (KMP shared). Absent for bare Android/iOS. A new parallel pipeline that **reuses the same schema, validator, and diff code** as native — only the store key (`wrapper-deps.json`) and build-doc fields (`wrapper_deps_ref` / `wrapper_deps_status` / `wrapper_deps_diff_*`) are new.

Both files carry the identical `{schema_version, dependencies: [{type, group, name, version}, …]}` shape, so `type` still discriminates ecosystems **within** each layer. Native/wrapper is a *layer* grouping on top of the existing type-tagged identity, not a replacement for it. `timings.json` stays a single per-build timeline (not split by layer).

**Why two files, not one merged list** (this revises the earlier "(A) merged" recommendation):

- **Per-layer caps — the decisive reason.** npm trees (wrapper) are the largest dependency graphs in existence; native graphs are bounded. Two files let `wrapper-deps.json` carry a large cap (and be the disk-streaming candidate) while `native-deps.json` keeps the tight bare-mobile cap. A single merged list forces one cap across both — exactly the cross-platform cap problem. **This split resolves the cap question.**
- **Meaningful UX boundary.** "The packages I manage" (wrapper / `package.json`) vs. "the native libraries that actually ship in the binary" (native) are different mental models for size analysis and vuln scanning; two files map cleanly to two viewer sections.
- **Zero migration for native.** Bare Android/iOS and the native half of cross-platform stay byte-identical to today.

**Layer classification is producer-decided** at collection time. The mapping above is unambiguous for RN / Flutter / Cordova / .NET / Unity. **KMP is the fuzzy case** — its shared Kotlin/Gradle deps are arguably "wrapper", the per-platform resolved artifacts "native"; the KMP producer owns that call and documents it when built.

(Naming note: the wrapper layer uses fully symmetric `wrapper_deps_*` storage + fields; the native layer keeps its existing `dependencies_*` storage/fields internally — `native-deps.json` is the bundle *entry* name mapping onto them, so no viewer/appserver migration is forced. Renaming native storage to `native_deps_*` for full symmetry is an optional cosmetic follow-up.)

### Action items

- [x] **Multi-ecosystem layout** — settled: two files, `native-deps.json` (existing native pipeline) + `wrapper-deps.json` (new parallel pipeline, same schema/validator/diff).
- [ ] **Build the `wrapper-deps.json` pipeline.** worker: `report_store.put_wrapper_deps` + keys + a layer-parameterised variant of `builds/diff_helpers.py` + a `wrapper-deps.json` arm in `build_info_bundle._process_entries`; appserver: `wrapper_deps_ref` / `wrapper_deps_status` build-doc fields; viewer: a wrapper-deps section. *Gate: first cross-platform producer.*
- [ ] **Re-tune caps per layer.** `native-deps.json` keeps the tight cap; `wrapper-deps.json` gets the npm/Unity-scale cap and is the candidate for `download_file()`-to-disk on large bundles. Folds into Open Question #2. *Gate: before React Native / Unity producers ship.*
- [ ] **Add per-platform producers** emitting `native-deps.json` + `wrapper-deps.json` (and `timings.json`), following the `bugsee-cli ios-deps` pattern (`npm-deps`, `pub-deps`, `nuget-deps`, Unity manifest). *Gate: per-platform rollout.*
- [ ] **Keep schema growth additive**; bump `schema_version` only with a paired worker `_SUPPORTED_SCHEMA_VERSIONS` + viewer update.

## Open questions

1. **Mapping wire format upgrade**: today the mapping ZIP from the Gradle plugin uses DEFLATE. Migrating to zstd-93 is in scope, but is it on the same rollout schedule, or a separate sub-track? Recommendation: bundle with Phase A (the CLI's new symbol-upload subcommand emits zstd by default; the Gradle plugin's existing CLI fallback path picks it up for free).

2. **Chunked upload for build-info**: today only the artefact path is chunked. The build-info bundle is typically < 5 MB compressed, which is well under S3's single-PUT cap. Recommendation: single-PUT only for the build-info bundle in v1; revisit if telemetry shows blobs approaching 100 MB.

3. **Sidecar entries the bundle should add immediately**: candidates include `vcs.json` (currently inline in the metadata POST body — could move to bundle if the body shape gets unwieldy), `build-env.json` (machine, xcode version, host OS), `xcactivitylog.json` (raw Xcode log when the user opts in for deep timing analysis). Recommendation: ship v1 with just `dependencies.json` + `timings.json`; let new sidecars land additively without re-rolling the bundle format.

4. **Per-org rollout pacing**: do we ramp Phase C/D per-app or globally? Per-org gives us a safety valve for large customers; global is simpler. Recommendation: per-org for the first 10% of traffic, then global.

5. **`bugsee-cli` minimum version pinning by producers**: when the Gradle plugin / Python BugseeAgents shell to `bugsee-cli upload build-info`, they need v0.2.0+ on the host. The fastlane plugin's `resolveCli` already auto-downloads; the Gradle plugin's CLI plumbing needs an enforced minimum version. Recommendation: `BUGSEE_CLI_MIN_VERSION` constant per-producer, checked at task registration time; downgrade to legacy path on version skew with a one-time warning.

6. **Diagnostic / debug builds**: how does an integrator inspect what the CLI actually emitted? Recommendation: `bugsee-cli upload --dry-run` writes the would-be-uploaded ZIP to a path and exits 0 without POSTing; producers expose this via their existing debug flags.

## What this doc supersedes

- The "raw gzip stream for build-info" architecture documented inline in `BundleUploader.kt`, `_gzip_json_bytes` (both BugseeAgents), and the worker's `jobs.dependencies.Process` / `jobs.timings.Process` handlers.
- The "mapping is uploaded twice when size-analysis is on" architecture in the Gradle plugin's `BundleUploadTask.writeNormalizedUploadZip`.
- The "each producer has its own HTTP client + retry + signing logic" pattern documented in `BundleUploader.kt`, `ChunkedBundleUploader.kt`, `SymbolUploader.kt`, and the equivalent Python code in both BugseeAgents.

## Related work

- **`bugsee-cli` C-series migration** (completed): VCS metadata, iOS deps, build-env helpers, dSYM UUID extraction moved from Python BugseeAgents to canonical Rust subcommands. The build-time upload arc is the natural conclusion of this pattern applied to the upload orchestration itself.
- **Phase 4 cross-platform artefact upload** (completed for single-PUT): the iOS artefact upload action shipped with single-PUT; the chunked upload portion is tracked as task #97 and folds into Phase A of this doc.
- **App-size-analysis arc** (active, see `app-size-analysis-research/`): the size-report blob is one of the asset classes that currently uses raw gzip stream + dedicated endpoint; it should move to the build-info bundle (or a parallel `size-report-bundle` class) as part of Phase D.
