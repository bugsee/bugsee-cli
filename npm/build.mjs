#!/usr/bin/env node
// Assemble the six npm packages of the @bugsee/cli family from a directory
// of cargo-dist release assets.
//
//   node npm/build.mjs --artifacts-dir release-assets [--version X.Y.Z]
//                      [--out npm/dist] [--allow-missing]
//
// Input is exactly what `gh release download vX.Y.Z` drops on disk: the
// `bugsee-cli-<triple>.tar.xz` / `.zip` archives and their `.sha256` sidecars.
// Every archive is SHA-256 verified against its sidecar before it is unpacked
// — the same check installer/install.sh and the postinstall fallback make, so
// a corrupted or substituted asset cannot reach a published package.
//
// Output is `<out>/<package-name-with-slash-replaced>/` directories, each
// ready for `npm pack` / `npm publish`.
//
// Zero dependencies on purpose: this runs in the publish workflow before
// anything has been installed.

import { createRequire } from "node:module";
import { spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.join(here, "..");

const { PLATFORMS, artifactName } = require("./cli/lib/platforms.js");

function die(msg) {
  console.error(`error: ${msg}`);
  process.exit(1);
}

function parseArgs(argv) {
  const out = { out: path.join(here, "dist"), allowMissing: false };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    const next = () => argv[++i] ?? die(`${arg} needs a value`);
    if (arg === "--artifacts-dir") out.artifactsDir = next();
    else if (arg === "--version") out.version = next();
    else if (arg === "--out") out.out = next();
    else if (arg === "--allow-missing") out.allowMissing = true;
    else die(`unknown argument: ${arg}`);
  }
  if (!out.artifactsDir) die("--artifacts-dir is required");
  return out;
}

/** The crate version is the single source of truth for all six packages. */
function crateVersion() {
  const toml = fs.readFileSync(path.join(repoRoot, "Cargo.toml"), "utf8");
  const pkg = toml.split(/^\[/m).find((s) => s.startsWith("package]"));
  const m = pkg && pkg.match(/^version\s*=\s*"([^"]+)"/m);
  if (!m) die("could not read version from Cargo.toml");
  return m[1];
}

const sha256 = (file) =>
  crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");

function verify(archive) {
  const sidecar = `${archive}.sha256`;
  if (!fs.existsSync(sidecar)) {
    die(
      `missing checksum sidecar ${path.basename(sidecar)} — refusing to package an unverified binary`,
    );
  }
  const expected = (
    fs.readFileSync(sidecar, "utf8").trim().split(/\s+/)[0] || ""
  ).toLowerCase();
  if (!/^[0-9a-f]{64}$/.test(expected))
    die(`unreadable checksum in ${sidecar}`);
  const actual = sha256(archive);
  if (actual !== expected) {
    die(
      `checksum mismatch for ${path.basename(archive)}: expected ${expected}, got ${actual}`,
    );
  }
  return actual;
}

function extract(archive, dest, ext) {
  const run = (cmd, args) => {
    const r = spawnSync(cmd, args, { encoding: "utf8" });
    if (r.error) die(`${cmd}: ${r.error.message}`);
    if (r.status !== 0)
      die(`${cmd} failed on ${path.basename(archive)}: ${r.stderr.trim()}`);
  };
  if (ext === ".tar.xz")
    run("tar", ["xf", archive, "--strip-components", "1", "-C", dest]);
  else if (ext === ".zip") run("unzip", ["-q", "-o", archive, "-d", dest]);
  else die(`unrecognised archive extension: ${ext}`);
}

function writeJson(file, value) {
  fs.writeFileSync(file, JSON.stringify(value, null, 2) + "\n");
}

function copyDir(from, to) {
  fs.cpSync(from, to, { recursive: true });
}

// ---------------------------------------------------------------------------

const args = parseArgs(process.argv.slice(2));
const version = args.version ?? crateVersion();
if (!/^\d+\.\d+\.\d+/.test(version)) die(`not a version: ${version}`);
if (args.version && args.version !== crateVersion()) {
  // A mismatch means the workflow checked out a ref that is not the tag it is
  // publishing — the packages would claim a version whose sources they are not.
  die(
    `--version ${args.version} does not match Cargo.toml's ${crateVersion()}`,
  );
}

fs.rmSync(args.out, { recursive: true, force: true });
fs.mkdirSync(args.out, { recursive: true });

const built = [];
const skipped = [];

// --- the five platform packages --------------------------------------------
for (const [triple, platform] of Object.entries(PLATFORMS)) {
  const asset = artifactName(triple);
  const archive = path.join(args.artifactsDir, asset);
  if (!fs.existsSync(archive)) {
    if (!args.allowMissing) {
      die(`missing release asset ${asset} in ${args.artifactsDir}`);
    }
    console.warn(`warning: skipping ${platform.pkg} — ${asset} not found`);
    skipped.push(platform.pkg);
    continue;
  }

  const digest = verify(archive);
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "bugsee-cli-pkg-"));
  extract(archive, tmp, platform.archiveExt);
  const binary = path.join(tmp, platform.bin);
  if (!fs.existsSync(binary))
    die(`${platform.bin} not found after extracting ${asset}`);

  const dir = path.join(
    args.out,
    platform.pkg.replace("/", "-").replace(/^@/, ""),
  );
  fs.mkdirSync(path.join(dir, "bin"), { recursive: true });
  fs.copyFileSync(binary, path.join(dir, "bin", platform.bin));
  // npm preserves the executable bit for files under `bin/` of a packed
  // tarball, but only if it is set on disk here. Without it the launcher's
  // spawn fails with EACCES on every Unix install.
  fs.chmodSync(path.join(dir, "bin", platform.bin), 0o755);
  fs.rmSync(tmp, { recursive: true, force: true });

  writeJson(path.join(dir, "package.json"), {
    name: platform.pkg,
    version,
    description: `${platform.label} binary for @bugsee/cli.`,
    homepage: "https://github.com/bugsee/bugsee-cli",
    repository: "https://github.com/bugsee/bugsee-cli",
    author: "Bugsee",
    // The whole mechanism: npm consults os/cpu before downloading an optional
    // dependency, so exactly one of the five is ever fetched.
    os: [platform.os],
    cpu: [platform.cpu],
    ...(platform.libc ? { libc: [platform.libc] } : {}),
    engines: { node: ">=14.18" },
    // Yarn PnP would otherwise keep this in a zip, where the binary cannot be
    // exec'd.
    preferUnplugged: true,
    files: ["bin"],
  });

  fs.writeFileSync(
    path.join(dir, "README.md"),
    `# ${platform.pkg}\n\nThe ${platform.label} binary for ` +
      `[\`@bugsee/cli\`](https://www.npmjs.com/package/@bugsee/cli).\n\n` +
      `This package is installed automatically as an optional dependency of ` +
      `\`@bugsee/cli\` on matching hosts. Do not depend on it directly.\n\n` +
      `Built from \`${triple}\` at bugsee-cli ${version} ` +
      `(\`${asset}\`, sha256 \`${digest}\`).\n`,
  );

  built.push({ pkg: platform.pkg, dir, triple, digest });
}

// --- the front package ------------------------------------------------------
const frontSrc = path.join(here, "cli");
const frontDir = path.join(args.out, "cli");
fs.mkdirSync(frontDir, { recursive: true });
for (const entry of ["bin", "lib", "scripts", "README.md"]) {
  copyDir(path.join(frontSrc, entry), path.join(frontDir, entry));
}

const template = JSON.parse(
  fs.readFileSync(path.join(frontSrc, "package.json"), "utf8"),
);

// Guard against the one drift that silently produces a broken front package:
// a platform added to lib/platforms.js but not to the committed
// optionalDependencies (or vice versa). The list is regenerated below either
// way, but a mismatch means the committed template no longer describes what we
// publish, and a reviewer should see that as a diff rather than a surprise.
const declared = Object.keys(template.optionalDependencies ?? {}).sort();
const expected = Object.values(PLATFORMS)
  .map((p) => p.pkg)
  .sort();
if (declared.join(",") !== expected.join(",")) {
  die(
    `npm/cli/package.json optionalDependencies (${declared.join(", ")}) do not ` +
      `match lib/platforms.js (${expected.join(", ")})`,
  );
}

template.version = version;
// Pinned EXACTLY (no range): a platform package is the binary for THIS build,
// and a caret would let npm resolve a mismatched pair.
template.optionalDependencies = Object.fromEntries(
  expected.map((p) => [p, version]),
);
writeJson(path.join(frontDir, "package.json"), template);

// --- report -----------------------------------------------------------------
console.log(`built @bugsee/cli ${version} -> ${args.out}`);
for (const b of built) console.log(`  ${b.pkg.padEnd(28)} ${b.triple}`);
if (skipped.length) console.log(`  skipped: ${skipped.join(", ")}`);
console.log(`  @bugsee/cli`);
