#!/usr/bin/env bash
# Enforce the Baryon writing rules on text files: ASCII only, which excludes
# emojis, em dashes, en dashes, the section sign, and typographic quotes.
#
# Usage:
#   hack/check-text-style.sh            check all tracked and untracked text files
#   hack/check-text-style.sh FILE...    check the given files
set -euo pipefail

cd "$(git rev-parse --show-toplevel)"

patterns=(
  '*.md' '*.rs' '*.toml' '*.yaml' '*.yml' '*.sh' '*.proto' '*.json'
  '*.txt' '*.service' '*.conf' '*.mk' 'Dockerfile' '*/Dockerfile'
  'Containerfile' '*/Containerfile' 'Makefile' '*/Makefile'
)

if [ "$#" -gt 0 ]; then
  files=("$@")
else
  mapfile -t files < <(git ls-files -co --exclude-standard -- "${patterns[@]}")
fi

status=0
for f in "${files[@]}"; do
  [ -f "$f" ] || continue
  if hits="$(LC_ALL=C grep -nP '[^\x00-\x7F]' "$f")"; then
    echo "$f: non-ASCII characters (use ASCII punctuation; no emojis, em dashes, or section signs):"
    printf '%s\n' "$hits" | head -20 | sed 's/^/  /'
    status=1
  fi
done
exit "$status"
