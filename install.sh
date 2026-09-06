#!/usr/bin/env bash
# fore installer — build from source, put the binary on PATH, wire up zsh + autostart.
#
#   git clone https://github.com/YOUR_GITHUB_USER/fore && cd fore && ./install.sh
#
# Options:  --prefix DIR   install binary to DIR (default ~/.local/bin)
#           --no-zshrc     don't touch ~/.zshrc
#           --no-service   don't install launchd/systemd autostart
#           --uninstall    remove everything fore installed (keeps history unless --purge)
set -euo pipefail

PREFIX="${FORE_PREFIX:-$HOME/.local/bin}"
EXTRA=()
UNINSTALL=0
for a in "$@"; do
  case $a in
    --prefix=*) PREFIX="${a#*=}" ;;
    --prefix) shift; PREFIX="$1" ;;
    --no-zshrc|--no-service) EXTRA+=("$a") ;;
    --uninstall) UNINSTALL=1 ;;
    --purge) EXTRA+=("--purge") ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
  esac
done

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
ok()   { printf '  \033[32m✔\033[0m %s\n' "$*"; }
warn() { printf '  \033[33m!\033[0m %s\n' "$*"; }
die()  { printf '  \033[1;31m✖\033[0m %s\n' "$*" >&2; exit 1; }

cd "$(dirname "$0")"

if [ "$UNINSTALL" = 1 ]; then
  if command -v fore >/dev/null 2>&1; then fore uninstall "${EXTRA[@]}"; fi
  rm -f "$PREFIX/fore" && ok "removed $PREFIX/fore"
  exit 0
fi

bold "fore install"
echo

# --- 0. platform sanity -----------------------------------------------------------
case "$(uname -s)" in
  Darwin|Linux) ok "$(uname -s) $(uname -m)" ;;
  *) die "unsupported OS: $(uname -s) (macOS and Linux only)" ;;
esac
pkg_hint() {   # $1 = what's missing → the exact command for this distro/OS
  if [ "$(uname -s)" = Darwin ]; then echo "xcode-select --install   (and: brew install $1)"; return; fi
  if command -v apt-get >/dev/null; then echo "sudo apt-get install -y $2"
  elif command -v dnf >/dev/null; then echo "sudo dnf install -y $3"
  elif command -v pacman >/dev/null; then echo "sudo pacman -S --needed $4"
  elif command -v zypper >/dev/null; then echo "sudo zypper install -y $3"
  elif command -v apk >/dev/null; then echo "sudo apk add $5"
  else echo "install: $1"; fi
}
MISSING=""
command -v zsh  >/dev/null 2>&1 || MISSING="$MISSING zsh"
command -v git  >/dev/null 2>&1 || MISSING="$MISSING git"
command -v curl >/dev/null 2>&1 || MISSING="$MISSING curl"
command -v cc   >/dev/null 2>&1 || MISSING="$MISSING cc"
if [ -n "$MISSING" ]; then
  warn "missing:$MISSING"
  echo "    run this first, then re-run ./install.sh:"
  echo "      $(pkg_hint "zsh git curl" "zsh git curl build-essential pkg-config" "zsh git curl gcc make pkgconf-pkg-config" "zsh git curl base-devel" "zsh git curl build-base")"
  if [ ! -x ./fore ] || [ -d src ]; then case "$MISSING" in *cc*|*curl*) exit 1 ;; esac; fi   # only fatal when building from source
fi
[ "$(basename "${SHELL:-}")" = zsh ] || warn "your login shell is $(basename "${SHELL:-unknown}") — switch with:  chsh -s \$(command -v zsh)   (log out/in afterwards)"

# --- 1+2. get a binary: prebuilt (release tarball) or build from source ------------------
if [ -x ./fore ] && [ ! -d src ] && ./fore --version >/dev/null 2>&1; then
  # Release tarball: the binary is already next to this script. No Rust needed.
  mkdir -p target/release && cp ./fore target/release/fore
  ok "prebuilt binary: $(./fore --version)"
else
  export PATH="$HOME/.cargo/bin:$PATH"
  if ! command -v cargo >/dev/null 2>&1; then
    warn "Rust toolchain not found."
    printf '    Install it now via rustup (https://rustup.rs)? [Y/n] '
    read -r ans
    case "${ans:-Y}" in
      [Yy]*) curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
             export PATH="$HOME/.cargo/bin:$PATH" ;;
      *) die "cargo is required to build fore (or download a release tarball with a prebuilt binary)" ;;
    esac
  fi
  ok "cargo $(cargo --version | cut -d' ' -f2)"
  echo "  building (release, first time takes 1–3 min)…"
  if ! cargo build --release --quiet 2>/tmp/fore-build.log; then
    grep -E "^error" -A 8 /tmp/fore-build.log | head -40 >&2
    die "build failed (full log: /tmp/fore-build.log)"
  fi
  # Respect a global CARGO_TARGET_DIR (some people set one): normalise to target/release/fore.
  if [ -n "${CARGO_TARGET_DIR:-}" ] && [ -x "$CARGO_TARGET_DIR/release/fore" ] && [ ! "$CARGO_TARGET_DIR/release/fore" -ef target/release/fore ]; then
    mkdir -p target/release && cp "$CARGO_TARGET_DIR/release/fore" target/release/fore
  fi
  [ -x target/release/fore ] || die "build finished but target/release/fore is missing"
  ok "built target/release/fore ($(du -h target/release/fore | cut -f1))"
fi

# --- 3. install binary -----------------------------------------------------------------
mkdir -p "$PREFIX"
# Stop a running daemon first: replacing a busy binary is fine on Unix, but the old
# daemon would keep running old code. `fore install` below restarts it.
if command -v fore >/dev/null 2>&1; then fore stop >/dev/null 2>&1 || true; fi
install -m 755 target/release/fore "$PREFIX/fore"
ok "installed $PREFIX/fore"
case ":$PATH:" in
  *":$PREFIX:"*) ;;
  *) warn "$PREFIX is not on your PATH — an export line will be added to ~/.zshrc" ;;
esac

# --- 4. shell + service + doctor (the binary does the rest) ----------------------------
echo
"$PREFIX/fore" install "${EXTRA[@]}"
