#!/usr/bin/env bash
#
# Tests `has_publish_trust()` in bootstrap-names.sh against fixture
# `npm trust list` output. No network, no npm, no auth.
#
# This function decides whether a package is already correctly configured for
# OIDC publishing, and getting it wrong is invisible until release day — an
# entry that grants nothing still 404s the publish. It replaced a substring
# grep that accepted two things it should not have, both covered below.
#
# Run: bash npm/test-bootstrap-names.sh

set -uo pipefail

SCRIPT="$(dirname "$0")/bootstrap-names.sh"
FIXTURE=""

# REPO, WORKFLOW and TRUST_NPM are read by the function eval'd in below, and
# fake_npm is invoked through $TRUST_NPM — shellcheck cannot see either.
# shellcheck disable=SC2034
REPO="bugsee/bugsee-cli"
# shellcheck disable=SC2034
WORKFLOW="npm-publish.yml"

# Stands in for `$TRUST_NPM trust list <pkg>`.
# shellcheck disable=SC2329
fake_npm() { printf '%s' "$FIXTURE"; }
# shellcheck disable=SC2034
TRUST_NPM="fake_npm"

# Pull the function out of the shipped script so the test covers the real code.
# shellcheck disable=SC1090
eval "$(awk '/^has_publish_trust\(\) \{/,/^\}/' "$SCRIPT")"

fails=0
t() {
  local name="$1" expected="$2"
  FIXTURE="$3"
  if has_publish_trust "@bugsee/x"; then got=0; else got=1; fi
  if [ "$got" = "$expected" ]; then
    printf '  ok    %s\n' "$name"
  else
    printf '  FAIL  %s (expected %s, got %s)\n' "$name" "$expected" "$got"
    fails=1
  fi
}

t "an entry granting publish is accepted" 0 '
type: github
id: abc
file: npm-publish.yml
repository: bugsee/bugsee-cli
permissions: publish, stage publish
'

# The first thing a substring grep got wrong: "stage publish" CONTAINS
# "publish" but grants no direct publish, so the release would still 404.
t "stage-publish alone is rejected" 1 '
type: github
id: abc
file: npm-publish.yml
repository: bugsee/bugsee-cli
permissions: stage publish
'

# The second: a bare filename grep matched any entry mentioning the workflow.
t "an entry for another workflow is rejected" 1 '
type: github
id: abc
file: release.yml
repository: bugsee/bugsee-cli
permissions: publish
'

t "an entry for another repository is rejected" 1 '
type: github
id: abc
file: npm-publish.yml
repository: someone/else
permissions: publish
'

# npm supports multiple trusted publishers per package, so the right entry may
# not be the first one.
t "the right entry is found among several" 0 '
type: github
id: aaa
file: release.yml
repository: bugsee/bugsee-cli
permissions: publish

type: github
id: bbb
file: npm-publish.yml
repository: bugsee/bugsee-cli
permissions: publish
'

t "a package with no entries is rejected" 1 ''

t "an entry with no permissions line is rejected" 1 '
type: github
id: abc
file: npm-publish.yml
repository: bugsee/bugsee-cli
'

if [ "$fails" -eq 0 ]; then
  echo "all has_publish_trust cases pass"
else
  echo "has_publish_trust FAILURES" >&2
fi
exit "$fails"
