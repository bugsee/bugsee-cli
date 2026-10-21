// Tests the exit-status contract of the @bugsee/cli launcher (npm/cli/bin/bugsee-cli.js).
// No network, npm or built binary needed.
//
// Integrators branch on the launcher's exit code (<= 2 falls back, >= 10 is a real
// failure), so a binary killed by a signal must never be reported as success:
// `spawnSync` returns `status: null` for it, and `process.exit(null)` exits 0. That
// is the bug that made the retired @bugsee/bugsee-cli launcher report a killed CLI
// as a success (bugsee/bugsee-cli#87).
//
// The launcher is copied into a temp dir and pointed at a fake binary through its
// `vendor/` fallback, so the test runs the real file without touching the package.
//
// Run: node --test npm/test-launcher.js

"use strict";

const { test } = require("node:test");
const assert = require("node:assert");
const { spawnSync } = require("node:child_process");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

const { PLATFORMS, currentTriple } = require("./cli/lib/platforms.js");

const triple = currentTriple();
// The fake binary is a shell script, and Windows has no signals to die from.
const skip =
  process.platform === "win32"
    ? "needs POSIX signals"
    : !triple && "no published binary for this platform";

/** Run the launcher against a fake binary whose body is `script`. */
function runLauncher(script, args = []) {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "bugsee-launcher-"));
  try {
    fs.cpSync(path.join(__dirname, "cli"), path.join(root, "cli"), {
      recursive: true,
      filter: (src) => !src.includes("node_modules"),
    });
    const vendor = path.join(root, "cli", "vendor");
    fs.mkdirSync(vendor, { recursive: true });
    const bin = path.join(vendor, PLATFORMS[triple].bin);
    fs.writeFileSync(bin, `#!/bin/sh\n${script}\n`, { mode: 0o755 });
    return spawnSync(
      process.execPath,
      [path.join(root, "cli", "bin", "bugsee-cli.js"), ...args],
      { encoding: "utf8" },
    );
  } finally {
    fs.rmSync(root, { recursive: true, force: true });
  }
}

test("a binary killed by SIGKILL is not reported as success", { skip }, () => {
  const r = runLauncher("kill -9 $$");
  assert.strictEqual(r.status, 128 + os.constants.signals.SIGKILL, r.stderr);
});

test("a binary killed by SIGTERM exits 128 + signum", { skip }, () => {
  const r = runLauncher("kill -15 $$");
  assert.strictEqual(r.status, 128 + os.constants.signals.SIGTERM, r.stderr);
});

test("every ordinary exit code is forwarded verbatim", { skip }, () => {
  for (const code of [0, 1, 2, 10, 23, 40]) {
    assert.strictEqual(runLauncher(`exit ${code}`).status, code, `exit ${code}`);
  }
});

test("argv and stdout pass through untouched", { skip }, () => {
  const r = runLauncher('printf "%s|" "$@"', ["a b", "--flag=1"]);
  assert.strictEqual(r.status, 0);
  assert.strictEqual(r.stdout, "a b|--flag=1|");
});
