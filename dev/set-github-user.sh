#!/usr/bin/env bash
# Usage: dev/set-github-user.sh <github-username> ["Your Name"]
# Replaces the YOUR_GITHUB_USER / YOUR_NAME placeholders everywhere they appear.
set -euo pipefail
[ $# -ge 1 ] || { echo "usage: $0 <github-username> [\"Your Name\"]"; exit 1; }
cd "$(dirname "$0")/.."
user="$1"; name="${2:-$1}"
files=$(grep -rl --exclude-dir=target --exclude-dir=.git --exclude="$(basename "$0")" -e YOUR_GITHUB_USER -e YOUR_NAME . || true)
[ -n "$files" ] || { echo "nothing to replace"; exit 0; }
for f in $files; do
  sed -i.bak -e "s#YOUR_GITHUB_USER#${user}#g" -e "s#YOUR_NAME#${name}#g" "$f" && rm -f "$f.bak"
  echo "  ✔ $f"
done
echo "done — now: git add -A && git commit -m 'set GitHub user' && git push"
