#!/bin/bash
# One-time setup for the browser demo: install fore for THIS user exactly like a
# real machine would (binary in ~/.local/bin, config file, autostart), then point
# the config at the mock model server and seed a little history.
set -e
cd "$(dirname "$0")/.."
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"
[ -x target/release/fore ] || cargo build --release
mkdir -p ~/.local/bin
fore stop >/dev/null 2>&1 || true
install -m 755 target/release/fore ~/.local/bin/fore
mkdir -p ~/.config/fore
[ -f ~/.config/fore/config.toml ] || fore config init >/dev/null
# demo-specific: mock model on :11434 (same port as Ollama, so the default base_url works)
sed -i 's/^model = .*/model = "mock-1"/' ~/.config/fore/config.toml
export FORE_NO_AUTOSTART=1
fore start
demo/reset-playground.sh >/dev/null
# seed history so ghost text has something to say on day one
S="demo-seed-$$"
seed() { fore exec --session "$S" --cwd "$1" -- "$2"; fore done --session "$S" --exit-code "${3:-0}" --duration-ms "${4:-120}"; }
for i in 1 2 3 4 5 6; do seed /home/user/fore "cargo test"; seed /home/user/fore "cargo build --release" 0 41000; seed /home/user/fore "git status"; done
for i in 1 2 3 4 5; do seed /home/user/fore "git add -A && git commit -m wip && git push"; seed /home/user/fore "docker compose up -d"; seed /home/user/fore "kubectl get pods -n staging"; done
seed /home/user/fore "cargo tset" 101; seed /home/user/fore "python -m pytest tests/" 1 3400
seed /home/user/playground "make -j8" 0 65000; seed /home/user/playground "ls -la"; seed /home/user/playground "git log --oneline -20"
for i in 1 2 3; do seed /home/user/fore "cargo test"; done
fore status
