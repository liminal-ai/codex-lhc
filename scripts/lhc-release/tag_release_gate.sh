#!/usr/bin/env bash
# Tag/release existence gate for supplemental platform builds.
#
# forward:  requires that neither tag v$VERSION nor release v$VERSION exists.
# backfill: requires both to exist and to target exactly $CANDIDATE_SHA.
#
# gh prints an API error body (e.g. 404 Not Found JSON) to stdout even when it
# exits nonzero, so existence must be decided by exit status, never by whether
# captured stdout is empty.
set -euo pipefail

MODE="${1:?mode (forward|backfill)}"
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
  backfill)
    if [ -z "$tag_json" ]; then
      echo "backfill requires existing tag v${VERSION}" >&2
      exit 1
    fi
    if [ -z "$release_json" ]; then
      echo "backfill requires existing release v${VERSION}" >&2
      exit 1
    fi
    tag_sha="$(jq -r '.object.sha' <<<"$tag_json")"
    object_type="$(jq -r '.object.type' <<<"$tag_json")"
    if [ "$object_type" = tag ]; then
      tag_sha="$("$GH" api "repos/${REPO}/git/tags/${tag_sha}" --jq .object.sha)"
    fi
    release_sha="$(jq -r .targetCommitish <<<"$release_json")"
    test "$tag_sha" = "$CANDIDATE_SHA"
    test "$release_sha" = "$CANDIDATE_SHA"
    echo "backfill: tag and release target both ${CANDIDATE_SHA}"
    ;;
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
    echo "mode must be forward or backfill, got: ${MODE}" >&2
    exit 1
    ;;
esac
