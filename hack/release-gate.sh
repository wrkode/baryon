#!/usr/bin/env bash
# Release gate: checks, before anything is built for publication, that the
# commit being released is a valid release target (docs/release.md, Release
# gate). Fails closed: any check that cannot be made is a failure.
#
# Run by the gate job of .github/workflows/release.yaml with a full clone of
# the repository (fetch-depth 0). Environment:
#   GH_TOKEN      token with actions: read and statuses: read on REPO
#   REPO          owner/name
#   EVENT         push (a tag push: publication) or workflow_dispatch (a dry
#                 run: the tag checks are skipped and a missing release
#                 qualification is reported, not fatal)
#   GITHUB_SHA    the commit the workflow runs on
#   ALLOWED_SIGNERS  Git allowed signers file content (SSH keys allowed to
#                 sign release tags), from the repository variable
#                 RELEASE_ALLOWED_SIGNERS; required for a tag push
#   QUALIFIERS    space-separated GitHub logins allowed to record release
#                 qualification, from the repository variable
#                 RELEASE_QUALIFIERS; required for a tag push
#   GITHUB_REF_NAME, GITHUB_OUTPUT  as set by GitHub Actions
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

fail() { echo "release gate: $*" >&2; exit 1; }
note() { echo "release gate: $*"; }

: "${REPO:?}" "${EVENT:?}" "${GITHUB_SHA:?}"
sha="$(git rev-parse HEAD)"
[ "$sha" = "$GITHUB_SHA" ] || fail "checked out $sha, the workflow runs on $GITHUB_SHA"
[ -z "$(git status --porcelain)" ] || fail "the checkout is not clean"

version="v$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
case "$version" in
  v[0-9]*.[0-9]*.[0-9]*) ;;
  *) fail "no workspace version in Cargo.toml ($version)" ;;
esac
[ -s "docs/release-notes/$version.md" ] || fail "docs/release-notes/$version.md is missing"
note "version $version, commit $sha"

if [ "$EVENT" = push ]; then
  tag="${GITHUB_REF_NAME:?}"
  [ "$tag" = "$version" ] || fail "tag $tag does not match the workspace version $version"
  [ "$(git cat-file -t "refs/tags/$tag")" = tag ] || fail "tag $tag is not an annotated tag"
  [ "$(git rev-parse "refs/tags/$tag^{commit}")" = "$sha" ] || fail "tag $tag does not point to $sha"
  # The tag must be signed with a key pinned in the repository settings, and
  # GitHub must verify the signature for the tagger.
  [ -n "${ALLOWED_SIGNERS:-}" ] || fail "RELEASE_ALLOWED_SIGNERS is not set"
  signers="$(mktemp)"
  printf '%s\n' "$ALLOWED_SIGNERS" > "$signers"
  git -c gpg.ssh.allowedSignersFile="$signers" verify-tag "$tag" \
    || fail "tag $tag is not signed by an allowed signer"
  verification="$(gh api "repos/$REPO/git/tags/$(git rev-parse "refs/tags/$tag")" --jq '"\(.verification.verified) \(.verification.reason)"')"
  [ "$verification" = "true valid" ] || fail "GitHub does not verify the signature of $tag ($verification)"
  # The tag name must not have been used for a published release already.
  if out="$(gh api "repos/$REPO/releases/tags/$tag" 2>&1)"; then
    fail "a release for $tag exists already"
  fi
  grep -q 'HTTP 404' <<< "$out" || fail "cannot check for an existing release of $tag: $out"
  note "tag $tag: annotated, signed by an allowed signer and verified, points to the commit"
fi

# The commit must be on main as GitHub has it, not only in this checkout.
git fetch --quiet --no-tags origin "+refs/heads/main:refs/remotes/origin/main"
git merge-base --is-ancestor "$sha" refs/remotes/origin/main \
  || fail "$sha is not reachable from origin/main"
note "reachable from origin/main ($(git rev-parse refs/remotes/origin/main))"

# CI (.github/workflows/ci.yaml) must have passed on this exact commit, on
# its push to main.
ci="$(gh api "repos/$REPO/actions/workflows/ci.yaml/runs?head_sha=$sha&event=push&per_page=100" \
  --jq '[.workflow_runs[] | select(.head_sha == "'"$sha"'")] | sort_by(.run_started_at) | last | "\(.id) \(.status) \(.conclusion)"')"
if [ -z "$ci" ] || [ "$ci" = "null null null" ]; then fail "no CI run on main for $sha"; fi
case "$ci" in
  *" completed success") note "CI run ${ci%% *} passed on $sha" ;;
  *) fail "the latest CI run on main for $sha is not a success: $ci" ;;
esac

# Release qualification runs outside GitHub (hardware E2E). Its result is a
# commit status, context baryon/release-qualification, that a maintainer
# sets on the exact qualified commit (docs/release.md).
qual="$(gh api "repos/$REPO/commits/$sha/statuses?per_page=100" \
  --jq '[.[] | select(.context == "baryon/release-qualification")] | first | "\(.state) \(.creator.type) \(.creator.login)"')"
qualifier="${qual##* }"
case " ${QUALIFIERS:-} " in
  *" $qualifier "*) allowed=yes ;;
  *) allowed=no ;;
esac
case "$qual" in
  "success User "*) [ "$allowed" = yes ] || [ "$EVENT" != push ] \
      || fail "release qualification on $sha was recorded by $qualifier, who is not in RELEASE_QUALIFIERS"
    note "release qualification recorded on $sha by $qualifier (allowed: $allowed)" ;;
  *)
    if [ "$EVENT" = push ]; then
      fail "no successful baryon/release-qualification status on $sha (${qual:-none})"
    fi
    note "dry run: no successful release qualification status on $sha (${qual:-none}); publication would fail"
    ;;
esac

publish=false
[ "$EVENT" = push ] && publish=true
{
  echo "version=$version"
  echo "sha=$sha"
  echo "publish=$publish"
} >> "${GITHUB_OUTPUT:-/dev/null}"
