# @bugsee/cli

The [Bugsee CLI](https://github.com/bugsee/bugsee-cli) — a cross-platform Rust
binary that collects debug information files (dSYM, ELF, PE/PDB, R8/ProGuard
mappings, JS source maps), resolves build-environment metadata, and uploads
symbols to Bugsee.

```sh
npm install --save-dev @bugsee/cli
npx bugsee-cli --version
```

## How the binary gets here

The binary is **not** in this package. It ships in five per-platform packages,
declared as `optionalDependencies`:

| Package                    | `os` / `cpu`       |
| -------------------------- | ------------------ |
| `@bugsee/cli-darwin-arm64` | `darwin` / `arm64` |
| `@bugsee/cli-darwin-x64`   | `darwin` / `x64`   |
| `@bugsee/cli-linux-arm64`  | `linux` / `arm64`  |
| `@bugsee/cli-linux-x64`    | `linux` / `x64`    |
| `@bugsee/cli-win32-x64`    | `win32` / `x64`    |

npm installs only the one matching your machine and skips the rest. Nothing is
downloaded at install time, so this works with `--ignore-scripts`, with a
lockfile-pinned CI install, and offline from a warm cache.

The Linux builds link glibc and are marked `"libc": ["glibc"]`. On musl (Alpine)
the install still succeeds, and `bugsee-cli` reports that plainly if invoked —
use a glibc base image, or build from source.

If the platform package is unavailable — `--no-optional`, a registry mirror
that carries only this package, or an unsupported platform — a `postinstall`
fallback downloads the release archive for your host and verifies its SHA-256
before unpacking it into `vendor/`. That fallback **never fails the install**:
if it cannot fetch the binary it warns and exits 0, and the error surfaces only
if you actually invoke `bugsee-cli`.

| Variable                     | Effect                                                                                                                                                                    |
| ---------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `BUGSEE_CLI_SKIP_DOWNLOAD=1` | Skip the fallback download entirely.                                                                                                                                      |
| `BUGSEE_CLI_BASE_URL=<url>`  | Fetch `<url>/bugsee-cli-<triple>.<ext>` (+ `.sha256`) instead of the GitHub release — point it at an internal mirror, or at `https://download.bugsee.com/cli/v<version>`. |

## Programmatic use

```js
const { spawnSync } = require("node:child_process");
const { binaryPath } = require("@bugsee/cli");

const out = spawnSync(binaryPath(), ["vcs-metadata"], { encoding: "utf8" });
const metadata = JSON.parse(out.stdout);
```

- `binaryPath()` — absolute path to the binary; throws with a diagnostic if
  there is none for this platform.
- `resolveBinaryPath()` — the same lookup, returning `null` instead of throwing.
- `version` — the CLI version this package carries.

## Exit codes

`bugsee-cli` exits with a
[stable, documented code](https://github.com/bugsee/bugsee-cli#exit-code-contract)
and the launcher forwards it unchanged (`0` success, `1`/`2` structural — the
caller should fall back, `10`–`39` substantive failures, `40` a deliberate build
gate). A launcher that cannot find a binary at all exits `1`, which is the
"structural, fall back" case by that same contract.

## Other install channels

`@bugsee/bugsee-cli` is the same CLI published by cargo-dist as a single
package that downloads its binary in a `postinstall`. It remains supported as
an alias; **prefer `@bugsee/cli`** — it is the one that works under
`--ignore-scripts`. See the
[Distribution section](https://github.com/bugsee/bugsee-cli#distribution) for
the shell installer, Homebrew tap, and the per-build-system bundles.
