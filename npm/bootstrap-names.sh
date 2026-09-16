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

# Names are UNSCOPED here — the scope is added below. Reject a scoped name
# rather than build "@bugsee/@bugsee/cli-...", since this script's own output
# prints fully-scoped names and copying one back in is the obvious mistake.
for n in "${names[@]}"; do
  case "$n" in
    @*|*/*)
      echo "ERROR: pass the unscoped name, e.g. 'cli-win32-arm64' not '$n'." >&2
      exit 2
      ;;
  esac
done

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

# Classify $1's trusted-publisher state for (REPO, WORKFLOW). Prints exactly
# one word and returns 0; the caller branches on the word.
#
#   granted  an entry exists for this workflow AND grants direct publish
#   other    an entry exists for this workflow but does NOT grant publish
#            (what npm < 11.15 creates, having no --allow-publish)
#   absent   no entry for this workflow
#   unknown  the check itself failed — npm errored, or its output was
#            unparseable
#
# `unknown` is a SEPARATE state on purpose. Collapsing it into `absent` means a
# transient registry 5xx or an expired session makes a correctly configured
# package look unconfigured, and the script then tells the operator to revoke a
# perfectly good trust entry. Never guess here.
#
# Prefers `--json`, because a parser invented for the human-readable output can
# only ever be tested against fixtures invented to match it — that proves the
# parser self-consistent and nothing about npm. The text path remains as a
# fallback for an npm whose --json is missing or empty.
trust_state() {
  local pkg="$1" out status

  out="$($TRUST_NPM trust list "$pkg" --json 2>/dev/null)"
  status=$?
  if [ "$status" -ne 0 ] || [ -z "$out" ]; then
    # Retry without --json: an older npm may not support it. Keep the exit
    # status, since that is what distinguishes "no entries" from "call failed".
    out="$($TRUST_NPM trust list "$pkg" 2>/dev/null)"
    status=$?
    [ "$status" -ne 0 ] && { echo unknown; return 0; }
    printf '%s' "$out" | awk -v WORKFLOW="$WORKFLOW" -v REPO="$REPO" '
      BEGIN { RS = ""; FS = "\n"; state = "absent" }
      {
        file = ""; repo = ""; perms = ""
        for (i = 1; i <= NF; i++) {
          line = $i
          sub(/^[ \t]+/, "", line)
          # Trim trailing whitespace AND a CR, so CRLF output still matches.
          gsub(/[ \t\r]+$/, "", line)
          if (line ~ /^file:[ \t]*/)        { sub(/^file:[ \t]*/, "", line);        file = line }
          if (line ~ /^repository:[ \t]*/)  { sub(/^repository:[ \t]*/, "", line);  repo = line }
          if (line ~ /^permissions:[ \t]*/) { sub(/^permissions:[ \t]*/, "", line); perms = line }
        }
        if (file != WORKFLOW || repo != REPO) next
        if (state == "absent") state = "other"
        n = split(perms, p, /[ \t]*,[ \t]*/)
        for (i = 1; i <= n; i++) {
          gsub(/^[ \t]+|[ \t\r]+$/, "", p[i])
          if (p[i] == "publish") state = "granted"
        }
      }
      END { print state }
    '
    return 0
  fi

  # JSON path. Deliberately tolerant about shape: find any object anywhere in
  # the document that names this workflow and repository, then look for an
  # exact `publish` permission. An unrecognised shape prints `unknown` rather
  # than a confident wrong answer.
  printf '%s' "$out" | node -e '
    let raw = "";
    process.stdin.on("data", d => raw += d).on("end", () => {
      let doc;
      try { doc = JSON.parse(raw); } catch { console.log("unknown"); return; }
      const wf = process.argv[1], repo = process.argv[2];
      let state = "absent", sawAny = false;
      const visit = (n) => {
        if (Array.isArray(n)) return n.forEach(visit);
        if (!n || typeof n !== "object") return;
        const file = n.file ?? n.workflow ?? n.workflowFilename;
        const r = n.repository ?? n.repo;
        if (typeof file === "string" && typeof r === "string") {
          sawAny = true;
          if (file.trim() === wf && r.trim() === repo) {
            if (state === "absent") state = "other";
            const perms = n.permissions ?? n.permission ?? [];
            const list = Array.isArray(perms) ? perms : String(perms).split(",");
            if (list.map(x => String(x).trim()).includes("publish")) state = "granted";
          }
        }
        Object.values(n).forEach(visit);
      };
      visit(doc);
      // An empty document is a legitimately unconfigured package; a non-empty
      // one in which nothing looked like an entry means we did not understand
      // it, which is not the same thing.
      const empty = Array.isArray(doc) ? doc.length === 0 : Object.keys(doc || {}).length === 0;
      console.log(state === "absent" && !sawAny && !empty ? "unknown" : state);
    });
  ' "$WORKFLOW" "$REPO"
}

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

for n in "${names[@]}"; do
  pkg="@bugsee/$n"
  echo
  echo "=== $pkg ==="

  # 1. The name must exist before it can carry a trusted publisher.
  #
  # E404 means "absent"; ANY other failure means the check did not work, and
  # publishing then would put a 0.0.0 placeholder on top of a real package —
  # with no --tag, that also moves `latest` onto an empty package until the next
  # release.
  view_out="$(npm view "$pkg" version 2>&1)"
  view_status=$?
  if [ "$view_status" -eq 0 ]; then
    echo "  registry: already published ($view_out)"
  elif ! printf '%s' "$view_out" | grep -q 'E404'; then
    echo "  STOPPED: could not determine whether $pkg exists:" >&2
    printf '%s\n' "$view_out" | sed 's/^/    /' >&2
    echo "  Not publishing a placeholder on a guess — re-run when the registry answers." >&2
    exit 1
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
    # --tag bootstrap, NOT latest: a placeholder must never become the version
    # `npm install @bugsee/cli` resolves. The real release publishes to latest.
    if ! npm publish "$tmp/$n" --access public --tag bootstrap; then
      echo "  STOPPED: could not publish $pkg@0.0.0 — nothing after it was attempted." >&2
      echo "  If this was 'rate limited otp', wait ~15 minutes and re-run." >&2
      exit 1
    fi
    echo "  registry: published $pkg@0.0.0"
  fi

  # 2. Attach the trusted publisher, so npm-publish.yml's OIDC can publish it.
  case "$(trust_state "$pkg")" in
    granted)
      echo "  trust:    already grants publish for $WORKFLOW"
      ;;
    unknown)
      echo "  STOPPED: could not read $pkg's trust configuration." >&2
      echo "  NOT treating that as 'unconfigured' — a transient registry error or an" >&2
      echo "  expired session would otherwise look identical to a missing entry." >&2
      $TRUST_NPM trust list "$pkg" 2>&1 | sed 's/^/    /' >&2
      exit 1
      ;;
    other)
      # npm allows only ONE trusted-publisher configuration per package, so
      # `npm trust github` errors here rather than replacing the entry. The
      # operator has to revoke first, and needs the id to do it.
      echo "  STOPPED: $pkg has an entry for $WORKFLOW that does NOT grant publish." >&2
      echo "  That is what npm < 11.15 creates (no --allow-publish): it looks" >&2
      echo "  configured and still 404s at release time. npm permits one" >&2
      echo "  configuration per package, so it must be revoked before re-adding:" >&2
      echo >&2
      $TRUST_NPM trust list "$pkg" 2>&1 | sed 's/^/    /' >&2
      echo >&2
      echo "    npm trust revoke $pkg --id=<the id above>" >&2
      echo "    bash npm/bootstrap-names.sh $n" >&2
      exit 1
      ;;
    absent)
      echo "  trust:    adding $REPO / $WORKFLOW"
      if ! $TRUST_NPM trust github "$pkg" \
             --file "$WORKFLOW" --repo "$REPO" --allow-publish --yes; then
        echo "  STOPPED: could not configure trust for $pkg." >&2
        exit 1
      fi
      # Read it back: a successful exit does not prove the entry grants publish.
      if [ "$(trust_state "$pkg")" != granted ]; then
        echo "  STOPPED: $pkg still has no entry granting publish for $WORKFLOW." >&2
        $TRUST_NPM trust list "$pkg" 2>&1 | sed 's/^/    /' >&2
        exit 1
      fi
      echo "  trust:    configured"
      ;;
  esac

  # npm's own docs recommend pausing between bulk trust calls, and this repo has
  # already tripped the per-account rate limiter once during a bootstrap.
  [ "$n" = "${names[${#names[@]}-1]}" ] || sleep 2
done

echo
echo "=== final state ==="
for n in "${names[@]}"; do
  echo "--- @bugsee/$n"
  $TRUST_NPM trust list "@bugsee/$n" 2>&1 | grep -E 'type|file|repository|permissions' | sed 's/^/    /'
done
