#!/bin/bash
# Dev-sandbox bootstrap: reinstall toolchain + deps after an environment reset.
# Idempotent. Not needed on a real machine (install.sh covers that).
set -u
cd "$(dirname "$0")/.."

need_rust=0; command -v "$HOME/.cargo/bin/cargo" >/dev/null 2>&1 || need_rust=1
need_apt=0; for b in zsh sqlite3 jq; do command -v "$b" >/dev/null 2>&1 || need_apt=1; done
need_ttyd=0; command -v ttyd >/dev/null 2>&1 || need_ttyd=1

if [ $need_rust = 1 ]; then
  ( curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal --default-toolchain stable >/tmp/rustup.log 2>&1; echo "rustup: $?" ) &
fi
if [ $need_apt = 1 ]; then
  ( sudo apt-get update -qq >/dev/null 2>&1; sudo apt-get install -y -qq zsh sqlite3 jq >/dev/null 2>&1; echo "apt: $?" ) &
fi
if [ $need_ttyd = 1 ]; then
  ( curl -sSL -o /tmp/ttyd https://github.com/tsl0922/ttyd/releases/latest/download/ttyd.x86_64 && chmod +x /tmp/ttyd && sudo mv /tmp/ttyd /usr/local/bin/ttyd; echo "ttyd: $?" ) &
fi
wait
export PATH="$HOME/.cargo/bin:$PATH"

# A half-persisted cargo registry produces bizarre "file not found for module" errors. Nuke it.
if ! cargo build --release 2>/dev/null; then
  rm -rf ~/.cargo/registry/src ~/.cargo/registry/cache ~/.cargo/registry/index
  cargo build --release 2>&1 | grep -E "^error|Finished" | head -5
fi
[ -d .git ] || { git init -q && git add -A && git -c user.name=fore -c user.email=fore@local commit -qm "checkpoint" && echo "git re-initialised"; }
ls -lh target/release/fore
