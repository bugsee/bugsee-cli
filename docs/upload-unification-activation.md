# Build-time upload unification — activation runbook

Companion to [`upload-unification.md`](./upload-unification.md). That doc is the
design; this is the **ordered go-live procedure** for turning on the
build-time upload unification (build-info bundle + zstd mapping packing) that is
currently built, tested, and **dormant behind activation gates**.

Status at time of writing (2026-06-15): all code is committed on
`build-info-bundle` branches across `bugsee-cli`, `android/gradle-plugin`,
`appserver`, `worker` (+ iOS agents). Nothing is live. `bugsee-cli` is at
`0.2.0` (unpublished); the Gradle plugin still pins `DEFAULT_VERSION = "0.1.0"`,
so every CLI-delegated path falls back to its native implementation.

---

## What actually activates, and how

There are **two independent activation tracks**. They share the CLI publish +
the Gradle `DEFAULT_VERSION` bump, but differ in their second gate:

| Track | Gates | Switch type |
|---|---|---|
| **Mapping zstd packing** (artefact upload ZIP's `mapping.txt` ships zstd-19 instead of DEFLATE-1) | (a) CLI 0.2.0 published, (b) plugin `DEFAULT_VERSION`→`0.2.0` **released and adopted by the customer** | Customer-adoption driven. No server flag. |
| **Build-info bundle** (deps+timings ship as one zstd ZIP via `upload build-info`) | (a)+(b) above, **plus** (c) appserver+worker deployed, (d) per-org flag `BUGSEE_FEATURE_BUILD_INFO_BUNDLE_ENABLED` on | Server-gated (the org flag). |

Consequences to keep in mind:

- **There is no central kill-switch for mapping zstd.** It turns on for a
  customer the moment they upgrade to a plugin whose `DEFAULT_VERSION` ships
  `pack` (≥0.2.0). The only thing protecting a not-yet-ready worker is the
  *ordering* below — get the worker able to read method-93 **before** any plugin
  with the bumped version is released.
- **The build-info bundle is fully reversible centrally** — flip the org flag
  off and the appserver stops signing `build_info_upload_endpoint`; producers
  fall back to legacy per-blob gzip PUTs on the next build.
- Every producer path **fails closed**: no CLI, old CLI, no signed endpoint,
  the `BUGSEE_LEGACY_BUILDINFO_GZIP` escape hatch, or any error → native/legacy
  path. So a mis-ordered step degrades gracefully rather than breaking builds —
  the ordering below is about correctness of the *new* path, not avoiding
  outages.

---

## Pre-flight checklist (verify before starting)

- [ ] **CLI builds cleanly at 0.2.0** with both subcommands:
      `cargo build --release && ./target/release/bugsee-cli upload --help &&
      ./target/release/bugsee-cli pack --help`. Full suite green:
      `cargo test` (165 tests).
- [ ] **Release artifacts for all 5 host triples** are produced by the CLI
      release pipeline:
      `aarch64-apple-darwin`, `x86_64-apple-darwin`,
      `aarch64-unknown-linux-gnu`, `x86_64-unknown-linux-gnu`
      (`.tar.xz`), `x86_64-pc-windows-msvc` (`.zip`). Each with a `.sha256`
      sidecar. These are the exact triples `CliBinaryResolver.hostTriple()`
      maps to; a missing triple makes that host fall back to native (safe but
      defeats the point).
- [ ] **PRODUCTION worker can read ZIP method 93 (zstd).** This is the single
      most important gate. The worker reads both the build-info bundle and the
      zstd mapping through `from utils.compression import zipfile`, which
      requires **Python 3.14+** (the stdlib `compression.zstd` backing).
      Confirm the deployed worker runtime is 3.14+ and the shim resolves
      `ZIP_ZSTANDARD`. If production is still on 3.12, **do not bump the plugin
      version** — zstd payloads would reach a worker that cannot decode them.
- [ ] **appserver** carries the build-info signing (`createBuild` +
      `createChunkedBuild` sign `build_info_upload_endpoint` behind the flag;
      `build_info_status` field + allowlist) and the terminal-transition /
      status-echo fixes.
- [ ] **worker** carries `jobs/build_info_bundle.py` and the
      `final/build-info/` → `build_info_bundle.process` dispatch.
- [ ] **A canary org** is chosen for first flag-flip (low-traffic, internal,
      or a friendly customer with Android size-analysis enabled).

---

## Activation steps (ordered)

> Owner legend: **[me]** = changes I can prepare/apply in-repo; **[you]** =
> publish / deploy / flag-flip / merge actions outside the working tree.

### Step 0 — Merge feature branches to integration branches [you]

Each repo's `build-info-bundle` branch must land on its integration branch
(per repo convention): **`nextgen`** for `android/gradle-plugin` (and the
Android SDK), **`master`** for `appserver` and `worker`, the CLI's own default
for `bugsee-cli`. Do NOT merge the `DEFAULT_VERSION` bump yet (it isn't applied
— see Step 3).

Verify: integration branch builds + tests green in CI for each repo.

### Step 1 — Publish `bugsee-cli` 0.2.0 [you]

The release is mechanized (cargo-dist + S3 mirror), so this is a tag push plus
one workflow run — no manual binary building:

1. **Push the `v0.2.0` tag.** `release.yml` (`on: push: tags`) runs cargo-dist,
   which builds all 5 targets (confirmed in `Cargo.toml` `[workspace.metadata.dist].targets`:
   `aarch64-apple-darwin`, `x86_64-apple-darwin`, `aarch64-unknown-linux-gnu`,
   `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc` — the exact set
   `CliBinaryResolver.hostTriple()` maps to), emits SHA-256 checksums, and
   creates the GitHub Release.
2. **Run `mirror-to-s3.yml`** (`workflow_dispatch`, input `tag = v0.2.0`). It
   copies the release assets to `s3://$S3_BUCKET/cli/v0.2.0/` (served as
   `https://download.bugsee.com/cli/v0.2.0/`) and refreshes `cli/latest/`.

The GitHub Release under
`github.com/bugsee/bugsee-cli/releases/download/v0.2.0/` is the documented
fallback the resolver also accepts.

Verify: `curl -fsSL https://download.bugsee.com/cli/v0.2.0/bugsee-cli-x86_64-unknown-linux-gnu.tar.xz.sha256`
returns a hash, and spot-check one darwin + the windows `.zip.sha256`. Until
these resolve, Step 3 will 404.

### Step 2 — Deploy appserver + worker [you]

Deploy the integration-branch builds. Worker first (or together), so the
method-93 read path and the `build_info_bundle` job are live before any zstd
payload can arrive.

Verify:
- Worker: a synthetic `final/build-info/<id>-<tid>.zip` (zstd ZIP) is processed
  by `build_info_bundle.process` without error; a `final/builds/...` wrapper ZIP
  carrying a **method-93** `mapping.txt` deobfuscates (size report shows real
  class names). The repo test `test_extracts_wrapper_zip_with_zstd_mapping`
  pins the read path; production parity = Python 3.14 + shim.
- appserver: with the flag OFF, `createBuild`/`createChunkedBuild` do **not**
  return `build_info_upload_endpoint` (default-off preserved).

### Step 3 — Bump the plugin `DEFAULT_VERSION` and release [me prepares / you release]

Apply this one-line change (kept un-applied until 0.2.0 is live to avoid 404s):

```
# android/gradle-plugin/src/main/kotlin/com/bugsee/android/gradle/upload/CliBinaryResolver.kt
-    const val DEFAULT_VERSION: String = "0.1.0"
+    const val DEFAULT_VERSION: String = "0.2.0"
```

This is the activation lever for **both** the build-info CLI path and zstd
mapping packing (`cliSupportsPack()` gates on `versionAtLeast(effective,
PACK_MIN_VERSION="0.2.0")`). With this released and adopted:
- Mapping zstd packing turns on (no further gate).
- Build-info bundle becomes *possible* — it still needs the org flag (Step 4).

Verify (local, before release): point a sample Android build at the
integration plugin, run an `assembleRelease` + size-analysis upload, and
confirm the plugin auto-downloads `bugsee-cli` v0.2.0 and the uploaded
artefact ZIP's `mapping.txt` entry is method 93 (zstd), not 8 (DEFLATE).
With the flag still off, build-info still PUTs legacy gzip blobs — expected.

> Adoption note: this is a plugin *release*; customers activate by upgrading.
> There is no server-side toggle for the mapping-zstd track.

### Step 4 — Flip the org flag for the canary [you]

Set `org.flags.BUGSEE_FEATURE_BUILD_INFO_BUNDLE_ENABLED = true` for the canary
org only.

Verify: a canary Android build (on the bumped plugin) →
`createBuild` returns `build_info_upload_endpoint`; the plugin uploads ONE
`final/build-info/<id>-<tid>.zip`; worker `build_info_bundle.process` fans it
out; the build's `build_info_status` reaches `ready`; deps + timings render in
the dashboard. Confirm the legacy per-blob gzip objects are **not** also
written (bundle path won, fallback didn't fire).

### Step 5 — Soak, then roll the flag out broadly [you]

After the canary soaks clean (build-info bundles land, size reports
deobfuscate, no error-rate change, telemetry shows `X-Bugsee-Uploader: cli` on
the build-info POSTs), expand the flag to more orgs in tranches.

---

## Verification matrix

| Surface | How to confirm it's live and correct |
|---|---|
| CLI published | `.sha256` reachable for all 5 triples at `download.bugsee.com/cli/v0.2.0/` |
| Plugin uses CLI | Build log shows `downloading bugsee-cli v0.2.0`; no `binary-missing`/`exit-2` fallback warnings |
| Mapping is zstd | Uploaded artefact ZIP `mapping.txt` entry `compress_type == 93` |
| Worker reads zstd mapping | Size report class names deobfuscated (not `a.b.C`) on a zstd-mapped build |
| Build-info signed | `createBuild` response contains `build_info_upload_endpoint` for a flagged org |
| Bundle processed | Build `build_info_status` transitions `uploading → processing → ready` |
| Fallback intact | An un-flagged org still gets `dependencies_upload_endpoint`/`timings_upload_endpoint` and legacy gzip blobs |

---

## Rollback

| If… | Do |
|---|---|
| Build-info bundles misbehave | Flip the org flag **off** → appserver stops signing the endpoint → producers fall back to legacy gzip on the next build. Fully central, no redeploy. |
| Worker can't keep up / zstd read breaks in prod | Re-deploy the prior worker build; **and** revert the plugin `DEFAULT_VERSION` to `0.1.0` in a patch release so new builds stop emitting zstd mappings (existing zstd objects already in S3 still need a method-93-capable worker to process — do not roll the worker back below 3.14 while such objects are queued). |
| CLI download flaky / artifact missing | Producers already fall back to native automatically (binary-missing/exec-failed → false). No action needed to stay safe; fix the mirror to re-enable. |
| Need a customer-side kill switch | `BUGSEE_LEGACY_BUILDINFO_GZIP=1` forces the native/legacy paths for both build-info and mapping packing on that build. |

---

## Gotchas / ordering rationale

- **Bump only after publish.** `DEFAULT_VERSION → 0.2.0` makes the plugin
  auto-download v0.2.0; if it isn't published the download 404s and the plugin
  falls back to native (safe, but you've shipped a no-op release). Step 1 → 3.
- **Worker-ready before plugin-bump.** Mapping zstd has no server gate, so the
  first bumped-plugin build emits a zstd mapping immediately. The worker must
  decode method 93 (Python 3.14 + shim) first. Step 2 → 3.
- **Deploy before flag.** The flag is read by appserver code that must already
  be deployed; flipping it on an old appserver does nothing. Step 2 → 4.
- **Mapping vs build-info are decoupled.** You can ship mapping zstd (Steps
  1–3) and leave the org flag off indefinitely; build-info stays on legacy. The
  reverse (flag on, plugin not bumped) is a no-op because a `0.1.0` CLI lacks
  `upload build-info` → structural fallback.
- **Old plugins are unaffected forever.** Customers on pre-bump plugins keep
  DEFLATE mappings + legacy gzip blobs; the worker reads both. No forced
  migration.
