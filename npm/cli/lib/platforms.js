// The single source of truth for "which npm package carries which binary".
//
// Consumed by three things that MUST agree, or the front package resolves a
// binary the publisher never built:
//   - lib/index.js       (runtime resolution of the optional dependency)
//   - scripts/postinstall.js (the download fallback, which needs the release
//                             artifact name for the same triple)
//   - ../../build.mjs    (the assembler, which mints one npm package per row)
//
// `triple` is the Rust target triple, and it is the join key with everything
// outside this directory: `[workspace.metadata.dist].targets` in Cargo.toml
// names these, cargo-dist names its release assets after them
// (`bugsee-cli-<triple><archiveExt>`), and installer/install.sh builds the same
// filename. Adding a platform here without adding the triple to `targets`
// produces a package with nothing to put in it. (Do NOT then run
// `dist generate`: `allow-dirty = ["ci"]` makes it a silent no-op for
// release.yml. Check the plan with `dist plan --output-format=json` — see the
// `allow-dirty` comment in Cargo.toml.)
//
// Windows arm64 (`aarch64-pc-windows-msvc`) IS published as of 0.7.7 — see
// bugsee/bugsee-cli#20 for why it was absent before. It cross-compiles under
// cargo-xwin only if `ring` can assemble its ARM64 Windows `.S` files, which it
// cannot, so that leg builds NATIVELY on a `windows-11-arm` runner instead
// (`[workspace.metadata.dist.github-custom-runners]` in Cargo.toml). ci.yml
// build-checks the target on every PR so the leg cannot break a tag release:
// dist's `host` job needs EVERY build-local-artifacts leg, and a failing leg
// means no GitHub Release, hence no S3 mirror and no npm publish.

"use strict";

const PLATFORMS = Object.freeze({
  "aarch64-apple-darwin": {
    pkg: "@bugsee/cli-darwin-arm64",
    os: "darwin",
    cpu: "arm64",
    bin: "bugsee-cli",
    archiveExt: ".tar.xz",
    label: "macOS arm64 (Apple silicon)",
  },
  "x86_64-apple-darwin": {
    pkg: "@bugsee/cli-darwin-x64",
    os: "darwin",
    cpu: "x64",
    bin: "bugsee-cli",
    archiveExt: ".tar.xz",
    label: "macOS x86_64 (Intel)",
  },
  "aarch64-unknown-linux-gnu": {
    pkg: "@bugsee/cli-linux-arm64",
    os: "linux",
    cpu: "arm64",
    // Our Linux builds are `unknown-linux-gnu`. npm >= 10.5 (and pnpm/yarn)
    // honour this the way they honour os/cpu, so a musl host skips the
    // download entirely; older npm ignores the field and installs a binary it
    // cannot load, which is what currentTriple()'s musl check catches.
    libc: "glibc",
    bin: "bugsee-cli",
    archiveExt: ".tar.xz",
    label: "Linux arm64 (glibc)",
  },
  "x86_64-unknown-linux-gnu": {
    pkg: "@bugsee/cli-linux-x64",
    os: "linux",
    cpu: "x64",
    // See the note on aarch64-unknown-linux-gnu above.
    libc: "glibc",
    bin: "bugsee-cli",
    archiveExt: ".tar.xz",
    label: "Linux x86_64 (glibc)",
  },
  "x86_64-pc-windows-msvc": {
    pkg: "@bugsee/cli-win32-x64",
    os: "win32",
    cpu: "x64",
    bin: "bugsee-cli.exe",
    archiveExt: ".zip",
    label: "Windows x86_64",
  },
  "aarch64-pc-windows-msvc": {
    pkg: "@bugsee/cli-win32-arm64",
    os: "win32",
    cpu: "arm64",
    bin: "bugsee-cli.exe",
    archiveExt: ".zip",
    label: "Windows arm64",
  },
});

/** Release-asset filename cargo-dist publishes for a triple. */
function artifactName(triple) {
  const p = PLATFORMS[triple];
  return p ? `bugsee-cli-${triple}${p.archiveExt}` : null;
}

// The published Linux builds are `unknown-linux-gnu`; there is no musl target
// (install.sh refuses musl for the same reason). Node exposes the runtime glibc
// version in its process report ONLY when it is actually linked against glibc,
// so an absent value means musl (or a non-glibc libc) — this is the zero-
// dependency equivalent of the `detect-libc` call cargo-dist's installer makes.
function isMuslLinux() {
  if (process.platform !== "linux") return false;
  try {
    const report = process.report && process.report.getReport();
    const header = report && report.header;
    return !(header && header.glibcVersionRuntime);
  } catch {
    return false;
  }
}

/**
 * Rust target triple for the host, or null when we publish nothing for it.
 *
 * Note this is deliberately driven by the triple Node reports for ITSELF, not
 * by the hardware: an x64 Node under Rosetta on an arm64 Mac reports `x64` and
 * gets the x86_64 binary, which is what runs correctly in that process tree.
 */
function currentTriple() {
  const { platform, arch } = process;
  for (const [triple, p] of Object.entries(PLATFORMS)) {
    if (p.os === platform && p.cpu === arch) {
      if (platform === "linux" && isMuslLinux()) return null;
      return triple;
    }
  }
  return null;
}

module.exports = { PLATFORMS, artifactName, currentTriple, isMuslLinux };
