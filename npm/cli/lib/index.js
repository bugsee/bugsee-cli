// Programmatic entry point for @bugsee/cli.
//
//   const { binaryPath } = require("@bugsee/cli");
//   spawnSync(binaryPath(), ["vcs-metadata"], { encoding: "utf8" });
//
// Exists so a Node integrator (the React Native / Cordova / Capacitor hooks)
// can spawn the binary directly and read its stdout JSON, instead of going
// through `bin` and paying for an extra node process just to re-exec.

"use strict";

const fs = require("fs");
const path = require("path");
const { PLATFORMS, currentTriple, isMuslLinux } = require("./platforms.js");

const { version } = require("../package.json");

/** Where scripts/postinstall.js parks a binary it downloaded itself. */
function vendorPath(triple) {
  const p = PLATFORMS[triple];
  if (!p) return null;
  return path.join(__dirname, "..", "vendor", p.bin);
}

/**
 * Locate the binary, or return null.
 *
 * Two sources, in order:
 *
 *  1. The platform package installed as an optionalDependency. This is the
 *     normal path and the whole point of the package split: npm picks exactly
 *     one of the six by `os`/`cpu`, and it works under `--ignore-scripts`
 *     because nothing had to run to put the binary there.
 *  2. `vendor/`, where the postinstall fallback downloads to when (1) is
 *     unavailable — `--no-optional`, a registry mirror that carries only the
 *     front package, or a corporate proxy that 404s the platform scopes.
 *
 * Never throws: callers that want an exception want binaryPath().
 */
function resolveBinaryPath() {
  const triple = currentTriple();
  if (!triple) return null;
  const { pkg, bin } = PLATFORMS[triple];

  // Exact-file resolution: `bin/bugsee-cli` has no extension, so Node's
  // LOAD_AS_FILE matches it verbatim. Kept as the primary lookup because it
  // fails loudly-in-try if the package is present but was packed wrong.
  try {
    return require.resolve(`${pkg}/bin/${bin}`);
  } catch {
    /* fall through */
  }

  // Same package, resolved via its manifest. This survives a consumer that
  // restricts subpath resolution (yarn PnP strictness, a future `exports`
  // field on the platform packages) where the direct lookup would not.
  try {
    const manifest = require.resolve(`${pkg}/package.json`);
    const candidate = path.join(path.dirname(manifest), "bin", bin);
    if (fs.existsSync(candidate)) return candidate;
  } catch {
    /* fall through */
  }

  const vendored = vendorPath(triple);
  if (vendored && fs.existsSync(vendored)) return vendored;

  return null;
}

/** Human-readable reason there is no binary — used in both error paths. */
function diagnostic() {
  if (isMuslLinux()) {
    return (
      "bugsee-cli publishes glibc Linux builds only; this looks like a musl " +
      "system (e.g. Alpine). Use a glibc base image, or build from source: " +
      "https://github.com/bugsee/bugsee-cli"
    );
  }
  const triple = currentTriple();
  if (!triple) {
    return (
      `bugsee-cli has no published binary for ${process.platform}-${process.arch}. ` +
      `Supported: ${Object.values(PLATFORMS)
        .map((p) => `${p.os}-${p.cpu}`)
        .join(", ")}.`
    );
  }
  const { pkg } = PLATFORMS[triple];
  return (
    `bugsee-cli binary not found. Expected the optional dependency ${pkg} ` +
    `(installed automatically) or a postinstall download in vendor/. ` +
    `If you installed with --no-optional, re-run without it, or run ` +
    `\`node node_modules/@bugsee/cli/scripts/postinstall.js\` to fetch it.`
  );
}

/** Absolute path to the binary. Throws if there is none. */
function binaryPath() {
  const resolved = resolveBinaryPath();
  if (!resolved) throw new Error(diagnostic());
  return resolved;
}

module.exports = { binaryPath, resolveBinaryPath, diagnostic, version };
