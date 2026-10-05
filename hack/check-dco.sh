#!/usr/bin/env bash
# Check the Developer Certificate of Origin sign-off of every non-merge
# commit in a range: each needs a "Signed-off-by:" trailer with its
# author's name and email (git commit -s).
#
# Usage: hack/check-dco.sh <base> <head>
# An all-zero or empty base checks <head> alone.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

base="${1:-}"
head="${2:?usage: hack/check-dco.sh <base> <head>}"
if [ -z "$base" ] || [ "$base" = 0000000000000000000000000000000000000000 ]; then
  range=("$head" -1)
else
  range=("$base..$head")
fi

bad=0
checked=0
while read -r commit; do
  [ -n "$commit" ] || continue
  checked=$((checked + 1))
  author="$(git log -1 --format='%an <%ae>' "$commit")"
  if git log -1 --format='%(trailers:key=Signed-off-by,valueonly)' "$commit" | grep -qxF "$author"; then
    echo "ok   $commit $author"
  else
    echo "FAIL $commit has no Signed-off-by: $author"
    bad=$((bad + 1))
  fi
done < <(git rev-list --no-merges "${range[@]}")

echo "$checked commits checked, $bad without sign-off"
[ "$bad" -eq 0 ]
