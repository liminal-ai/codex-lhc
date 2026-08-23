#!/usr/bin/env bash
# Pre-publication gate. Requires that neither tag v$VERSION nor release
# v$VERSION exists, so promotion cannot clobber public state.
#
# gh prints an API error body (e.g. 404 Not Found JSON) to stdout even when it
# exits nonzero, so existence must be decided by exit status, never by whether
# captured stdout is empty.
set -euo pipefail

MODE="${1:?mode (forward)}"
VERSION="${2:?version (SemVer without v prefix)}"
CANDIDATE_SHA="${3:?candidate commit sha}"
GH="${GH_BIN:-gh}"
REPO="${GITHUB_REPOSITORY:?GITHUB_REPOSITORY must be set}"

if ! tag_json="$("$GH" api "repos/${REPO}/git/refs/tags/v${VERSION}" 2>/dev/null)"; then
  tag_json=""
fi
if ! release_json="$("$GH" release view "v${VERSION}" --json tagName,targetCommitish 2>/dev/null)"; then
  release_json=""
fi

case "$MODE" in
  forward)
    if [ -n "$tag_json" ]; then
      echo "forward mode forbids existing tag v${VERSION}" >&2
      exit 1
    fi
    if [ -n "$release_json" ]; then
      echo "forward mode forbids existing release v${VERSION}" >&2
      exit 1
    fi
    echo "forward: no tag or release for v${VERSION}"
    ;;
  *)
    echo "mode must be forward, got: ${MODE}" >&2
    exit 1
    ;;
esac
