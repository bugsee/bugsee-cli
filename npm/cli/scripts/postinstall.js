#!/usr/bin/env node
// Fallback binary fetch for @bugsee/cli.
//
// The optionalDependencies split covers the normal case. This script covers
// the three cases it cannot:
//
//   - `npm install --no-optional` / `--omit=optional` (and the CI images that
//     bake that in), where npm never even looks at the platform packages;
//   - an internal registry mirror or proxy that carries @bugsee/cli but not
//     the five @bugsee/cli-<platform> scopes, where npm silently skips the
//     optional dependency and leaves nothing behind;
//   - a platform we publish no package for at all.
//
// THE RULE FOR THIS FILE: it must never fail the install. `npm install` of a
// dependency tree that happens to contain @bugsee/cli must succeed on a
// Raspberry Pi, on Alpine, and inside an air-gapped mirror — @bugsee/cli is
// pulled in transitively by the RN/Cordova/Capacitor integrations, and a
// postinstall that exits non-zero there breaks an unrelated build for a tool
// the user may never invoke. So: every failure path warns on stderr and exits
// 0, and the missing binary is reported later, by the launcher, only if
// someone actually tries to run it.
//
// Environment:
//   BUGSEE_CLI_SKIP_DOWNLOAD=1   skip the fallback entirely (offline builds)
//   BUGSEE_CLI_BASE_URL=<url>    download root; the artifact is fetched from
//                                <url>/bugsee-cli-<triple>.<ext> and verified
//                                against <that>.sha256 — the same layout the
//                                GitHub release and download.bugsee.com/cli
//                                both use, so an internal mirror of either
//                                works. Default: the GitHub release for this
//                                package's version.

"use strict";

// ---------------------------------------------------------------------------
// The safety net goes FIRST — above the requires, not below them.
//
// Everything from here down can throw: a corrupted lib/platforms.js, a
// half-extracted package, a Node version that chokes on the syntax. A throw
// during a top-level `require` is an uncaught exception, and an uncaught
// exception in a postinstall exits non-zero and FAILS THE INSTALL — precisely
// the outcome this file exists to prevent. Registering the handler after the
// requires (as an earlier revision did) left exactly that hole: the file
// claimed "even a bug in this script must not take an install down" while
// corrupting lib/platforms.js produced exit 1.
//
// The logging helpers are defined here too, and swallow their own failures:
// console.warn/error write to a pipe that npm may have closed, and an EPIPE
// thrown from inside the handler would defeat the handler.
// ---------------------------------------------------------------------------

const warn = (msg) => {
  try {
    console.warn(`[@bugsee/cli] ${msg}`);
  } catch {
    /* EPIPE / closed stderr — nothing useful left to do */
  }
};
const info = (msg) => {
  try {
    console.error(`[@bugsee/cli] ${msg}`);
  } catch {
    /* as above */
  }
};

const describe = (err) => (err && err.message) || String(err);

process.on("uncaughtException", (err) => {
  warn(`postinstall failed: ${describe(err)}`);
  process.exit(0);
});
process.on("unhandledRejection", (err) => {
  warn(`postinstall failed: ${describe(err)}`);
  process.exit(0);
});
process.exitCode = 0;

const crypto = require("crypto");
const fs = require("fs");
const os = require("os");
const path = require("path");
const { spawnSync } = require("child_process");

const {
  PLATFORMS,
  artifactName,
  currentTriple,
} = require("../lib/platforms.js");
const { resolveBinaryPath, diagnostic } = require("../lib/index.js");
const { version } = require("../package.json");

const DEFAULT_BASE_URL = `https://github.com/bugsee/bugsee-cli/releases/download/v${version}`;

function baseUrl() {
  const override = process.env.BUGSEE_CLI_BASE_URL;
  return override ? override.replace(/\/+$/, "") : DEFAULT_BASE_URL;
}

function sha256(file) {
  const hash = crypto.createHash("sha256");
  hash.update(fs.readFileSync(file));
  return hash.digest("hex");
}

// The sidecar is `<hex>  *<filename>` (coreutils/BSD format) — install.sh
// reads it with `awk '{print $1}'`, and so do we.
function parseChecksum(text) {
  const token = text.trim().split(/\s+/)[0] || "";
  return /^[0-9a-f]{64}$/i.test(token) ? token.toLowerCase() : null;
}

function extract(archive, destDir, ext) {
  if (ext === ".tar.xz") {
    // The tarball has a single top-level directory; both shell installers
    // strip one component, so we match them exactly.
    return spawnSync(
      "tar",
      ["xf", archive, "--strip-components", "1", "-C", destDir],
      {
        stdio: ["ignore", "ignore", "pipe"],
        encoding: "utf8",
      },
    );
  }
  if (ext === ".zip") {
    if (process.platform === "win32") {
      // `unzip` is not present on a stock Windows image, so shell out to
      // Expand-Archive — via `-File` and a real script. NOT `-Command`: that
      // appends trailing arguments to the command TEXT instead of binding them
      // to $args, so the paths would be lost (and the append is an injection
      // shape). See the header of expand-archive.ps1.
      //
      // `-ExecutionPolicy Bypass` is required because the machine policy that
      // blocks unsigned .ps1 files is the default on Windows Server images;
      // it applies to this one invocation only.
      return spawnSync(
        "powershell.exe",
        [
          "-NoProfile",
          "-NonInteractive",
          "-ExecutionPolicy",
          "Bypass",
          "-File",
          path.join(__dirname, "expand-archive.ps1"),
          "-LiteralPath",
          archive,
          "-DestinationPath",
          destDir,
        ],
        { stdio: ["ignore", "ignore", "pipe"], encoding: "utf8" },
      );
    }
    return spawnSync("unzip", ["-q", "-o", archive, "-d", destDir], {
      stdio: ["ignore", "ignore", "pipe"],
      encoding: "utf8",
    });
  }
  return { error: new Error(`unrecognised archive extension: ${ext}`) };
}

async function main() {
  if (process.env.BUGSEE_CLI_SKIP_DOWNLOAD === "1") {
    info("BUGSEE_CLI_SKIP_DOWNLOAD=1 — skipping the binary download.");
    return;
  }

  // The happy path: npm already installed the right platform package. Do not
  // touch the network — the overwhelming majority of installs land here.
  const existing = resolveBinaryPath();
  if (existing) {
    info(`using ${existing}`);
    return;
  }

  const triple = currentTriple();
  if (!triple) {
    warn(diagnostic());
    return;
  }

  const platform = PLATFORMS[triple];
  const asset = artifactName(triple);
  const url = `${baseUrl()}/${asset}`;
  const vendorDir = path.join(__dirname, "..", "vendor");
  const target = path.join(vendorDir, platform.bin);

  info(
    `${platform.pkg} is not installed (optional dependencies skipped, or not ` +
      `available on this registry) — downloading ${asset} instead.`,
  );

  const { get, getText } = require("./download.js");
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "bugsee-cli-"));
  try {
    const archive = path.join(tmp, asset);
    const res = await get(url);
    await new Promise((resolve, reject) => {
      const sink = fs.createWriteStream(archive);
      res.pipe(sink);
      sink.on("error", reject);
      res.on("error", reject);
      sink.on("close", resolve);
    });

    // Verify before extracting, not after: the archive is what we fetched over
    // the wire, and `tar`/`Expand-Archive` are the things we do not want to
    // point at unverified bytes.
    const expected = parseChecksum(await getText(`${url}.sha256`));
    if (!expected)
      throw new Error(`could not read checksum from ${url}.sha256`);
    const actual = sha256(archive);
    if (actual !== expected) {
      throw new Error(
        `checksum mismatch for ${asset}: expected ${expected}, got ${actual}`,
      );
    }

    const result = extract(archive, tmp, platform.archiveExt);
    if (result.error) throw result.error;
    if (result.status !== 0) {
      throw new Error(
        `could not extract ${asset}: ${(result.stderr || "").toString().trim()}`,
      );
    }

    const extracted = path.join(tmp, platform.bin);
    if (!fs.existsSync(extracted)) {
      throw new Error(`${platform.bin} not found after extracting ${asset}`);
    }

    fs.mkdirSync(vendorDir, { recursive: true });
    fs.copyFileSync(extracted, target);
    fs.chmodSync(target, 0o755);
    info(`installed ${target} (sha256 ${actual})`);
  } catch (err) {
    warn(`could not download the bugsee-cli binary: ${err.message}`);
    warn(
      `\`bugsee-cli\` will report this if it is invoked. Re-run without ` +
        `--no-optional, or set BUGSEE_CLI_BASE_URL to an internal mirror of ` +
        `the release assets.`,
    );
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

// The expected-failure paths are already handled inside main(); this catch is
// for the unexpected ones. The process-level handlers registered at the top of
// this file cover anything that escapes even this.
main().catch((err) => {
  warn(`postinstall failed: ${describe(err)}`);
});
