#!/usr/bin/env bash
#
# One-time bootstrap for a NEW @bugsee/cli-family npm package name.
#
# npm Trusted Publishing is keyed on a package, and the configuration can only
# be attached to a name that already exists on the registry — so a name CI has
# never published cannot be granted the right to be published by CI. Each new
# name therefore needs one manual placeholder publish, then a trust entry.
# Skipping it fails the release with a 404 on the PUT, which reads as though the
# package does not exist rather than as a missing trust configuration.
#
# Usage:
#   bash npm/bootstrap-names.sh                 # the whole family (idempotent)
#   bash npm/bootstrap-names.sh cli-win32-arm64 # just the new name(s)
#
# RUN THIS IN A REAL TERMINAL. npm requires an interactive 2FA challenge to
# publish and to write trust configuration; it prints a URL and completes in the
# browser, and that session then covers the rest of the run.
#
# Idempotent and fail-fast: an already-published name is left alone, an existing
# trust entry is left alone, and the first error stops the run so a rate-limited
# npm is not hammered with the remaining names.

set -uo pipefail

REPO="bugsee/bugsee-cli"
WORKFLOW="npm-publish.yml"

# Front package LAST: npm skips an unresolvable optional dependency silently, so
# @bugsee/cli must never exist on the registry ahead of the platform packages it
# pins. Same invariant npm-publish.yml's publish loop enforces.
DEFAULT_NAMES=(
  cli-darwin-arm64 cli-darwin-x64
  cli-linux-arm64 cli-linux-x64
  cli-win32-arm64 cli-win32-x64
  cli
)

names=("$@")
[ ${#names[@]} -eq 0 ] && names=("${DEFAULT_NAMES[@]}")

if [ ! -t 0 ]; then
  echo "ERROR: no TTY. Run this directly in a terminal so npm can prompt for auth." >&2
  exit 2
fi

# `npm trust ... --allow-publish` needs npm >= 11.15. An older npm accepts the
# command WITHOUT that flag and creates an entry carrying no publish permission
# — which looks configured and still 404s at release time. Use a new enough npm
# for the trust calls rather than trusting whatever is on PATH.
npm_major_minor="$(npm --version | cut -d. -f1,2)"
if [ "$(printf '%s\n11.15\n' "$npm_major_minor" | sort -V | head -1)" = "11.15" ]; then
  TRUST_NPM="npm"
else
  echo "note: local npm $(npm --version) predates --allow-publish; using npx npm@latest for trust calls"
  TRUST_NPM="npx -y npm@latest"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

for n in "${names[@]}"; do
  pkg="@bugsee/$n"
  echo
  echo "=== $pkg ==="

  # 1. The name must exist before it can carry a trusted publisher.
  if npm view "$pkg" version >/dev/null 2>&1; then
    echo "  registry: already published ($(npm view "$pkg" version 2>/dev/null))"
  else
    echo "  registry: absent — publishing a 0.0.0 placeholder"
    mkdir -p "$tmp/$n"
    cat > "$tmp/$n/package.json" <<JSON
{
  "name": "$pkg",
  "version": "0.0.0",
  "description": "Placeholder — see https://github.com/$REPO",
  "repository": { "type": "git", "url": "git+https://github.com/$REPO.git" },
  "license": "MIT"
}
JSON
    # --access public: a scoped package defaults to restricted, and a restricted
    # package is not installable. No --provenance: it needs CI OIDC.
    if ! npm publish "$tmp/$n" --access public; then
      echo "  STOPPED: could not publish $pkg@0.0.0 — nothing after it was attempted." >&2
      echo "  If this was 'rate limited otp', wait ~15 minutes and re-run." >&2
      exit 1
    fi
    echo "  registry: published $pkg@0.0.0"
  fi

  # 2. Attach the trusted publisher, so npm-publish.yml's OIDC can publish it.
  if $TRUST_NPM trust list "$pkg" 2>/dev/null | grep -q "$WORKFLOW"; then
    echo "  trust:    already configured for $WORKFLOW"
  else
    echo "  trust:    adding $REPO / $WORKFLOW"
    if ! $TRUST_NPM trust github "$pkg" \
           --file "$WORKFLOW" --repo "$REPO" --allow-publish --yes; then
      echo "  STOPPED: could not configure trust for $pkg." >&2
      exit 1
    fi
    # Verify rather than trust the exit code — an entry without publish
    # permission is indistinguishable from a correct one until release day.
    if ! $TRUST_NPM trust list "$pkg" 2>/dev/null | grep -q "$WORKFLOW"; then
      echo "  STOPPED: trust command succeeded but no entry is listed for $pkg." >&2
      exit 1
    fi
    echo "  trust:    configured"
  fi
done

echo
echo "=== final state ==="
for n in "${names[@]}"; do
  echo "--- @bugsee/$n"
  $TRUST_NPM trust list "@bugsee/$n" 2>&1 | grep -E 'type|file|repository|permissions' | sed 's/^/    /'
done
