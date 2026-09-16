#!/usr/bin/env node
// Launcher for the `bugsee-cli` bin of @bugsee/cli.
//
// This is a pass-through, and "pass-through" is a contract here, not a
// nicety: every integrator that shells to the CLI branches on its EXIT CODE
// (see the exit-code contract in README.md — <= 2 means "fall back to the
// in-language uploader", >= 10 means "a real failure, do not fall back"), and
// the metadata commands' stdout is parsed with `json.loads`. So this wrapper
// must forward argv verbatim, leave all three stdio streams untouched, and
// reproduce the child's exit status exactly.

"use strict";

const { spawnSync } = require("child_process");
const os = require("os");
const { resolveBinaryPath, diagnostic } = require("../lib/index.js");

const binary = resolveBinaryPath();
if (!binary) {
  console.error(diagnostic());
  // Exit 1 = "structural" in the CLI's own exit-code contract: the CLI never
  // got a fair chance to run, so a caller is expected to fall back. Any other
  // code here would lie about what happened.
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), {
  stdio: "inherit",
  // No `shell: true` — it would re-parse arguments (quoting, globs, `&`) that
  // are meant to reach clap untouched, and paths with spaces are routine here.
  windowsHide: true,
});

if (result.error) {
  console.error(`failed to execute ${binary}: ${result.error.message}`);
  process.exit(1);
}

if (result.signal) {
  // The child died from a signal, which has no exit code. Report it the way a
  // shell does (128 + signum) so `$?` still tells a CI script what happened.
  const signum = os.constants.signals[result.signal];
  process.exit(signum ? 128 + signum : 1);
}

process.exit(result.status === null ? 1 : result.status);
