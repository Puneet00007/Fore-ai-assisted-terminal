# Testing fore on a Linux VM

Fifteen minutes, start to finish. Written for Ubuntu 22.04/24.04; Fedora/Arch commands noted where they differ.

## 0. VM

Anything works: VirtualBox, VMware, UTM, Multipass, a cloud VM. Give it **2 CPUs, 4 GB RAM, 15 GB disk**
(the Rust build is the only heavy step — 1 GB RAM is not enough to compile). A plain user account with
sudo. You do not need a desktop; SSH into it from your normal terminal if you prefer.

Optional but recommended: **snapshot the VM before you start**, so you can also test `--uninstall`
against a clean state and try again.

## 1. Prerequisites (2 min)

```sh
# Ubuntu / Debian
sudo apt-get update && sudo apt-get install -y zsh git curl build-essential pkg-config sqlite3

# Fedora:  sudo dnf install -y zsh git curl gcc make pkgconf-pkg-config sqlite
# Arch:    sudo pacman -S --needed zsh git curl base-devel sqlite

# Rust (if you don't have it) — takes ~1 min
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# make zsh your login shell (log out and back in afterwards — or just run `zsh` for now)
chsh -s "$(command -v zsh)"
```

`install.sh` checks all of this and prints the exact command if something is missing.

## 2. Get the code onto the VM

Either copy `fore-0.3.0.tar.gz` in (scp / shared folder / drag-and-drop), or if you've pushed it to a
git remote, clone it. Then:

```sh
tar xzf fore-0.3.0.tar.gz && cd fore     # or: git clone <your-remote> fore && cd fore
./install.sh
```

What you should see (about 2–3 minutes, mostly the build):

```
fore install
  ✔ Linux x86_64
  ✔ cargo 1.8x
  building (release, first time takes 1–3 min)…
  ✔ built target/release/fore (8.9M)
  ✔ installed /home/you/.local/bin/fore
  ✔ wrote config template /home/you/.config/fore/config.toml
  ✔ added plugin to ~/.zshrc
  ✔ systemd user service installed and started: …/fore.service
  ✔ daemon running
  · importing your existing shell history …
fore doctor  v0.3.0
  … all ✔ except "model server not reachable" (expected until step 5)
```

If the systemd line says **"failed (no user session bus?)"** — this happens over some SSH setups and in
containers. Not a blocker: the plugin starts the daemon itself. To fix it properly:
`sudo loginctl enable-linger $USER`, log out/in, `fore install --no-zshrc`.

## 3. Open a new zsh and try things (5 min)

Open a **new terminal** (or `exec zsh`). Then, in order:

| try | expect |
|---|---|
| `fore status` | `running  pong  v0.3.0 …` |
| `fore doctor` | ✔ for binary, shell, config, daemon, latency; ! only for model |
| run `ls -la`, `git status`, `echo hello` a couple of times, then type `ec` | grey ghost text `ho hello`; `→` accepts |
| `mkdir -p /tmp/t && cd /tmp/t && touch a b c && mkdir d && touch d/e` then `rm -rf d` | `→ trash (1 files …) — fore undo restores` |
| `fore undo` | `restored /tmp/t/d …`; `ls d` shows `e` again |
| `fore trash list` | the operation, marked `[restored]` |
| `echo hi > a` then `echo hi > a` again | second time: `! \`> a\` overwrites an existing 3 B file — did you mean \`>>\`?` |
| `git init -q && git checkout -q -b main && git add . && git commit -qm x` then type `git push --force origin main` + Enter | red ✖, "press Enter again to run" — press Ctrl-U to abandon |
| `false` then **Ctrl-/** | instantly: "no model server at http://localhost:11434/v1 — start Ollama …" (not a hang) |
| ` echo secret-thing` (note the leading space) then `fore stats` / `sqlite3 ~/.local/share/fore/history.db "select cmd from history where cmd like '%secret%'"` | nothing recorded |
| `export MY_TOKEN=abcd1234efgh5678` then the same sqlite query with `%MY_TOKEN%` | stored as `export MY_TOKEN=<REDACTED>` |
| `fore stop` → type/run a few commands → `fore start` | nothing breaks while stopped; ghost text back after start |
| close every terminal, log out, log back in, `fore status` | still running (systemd) |
| reboot the VM, open a terminal, `fore status` | running (this is the autostart test) |
| `fore config edit` → set `ghost_style = "fg=242,italic"` → `fore restart` → new shell | ghost text style changed |

Time each thing subjectively: **nothing should ever feel slow** except Ctrl-/ / Ctrl-Space with a real
model. If Enter ever lags, tell me what command you typed.

## 4. Things that are supposed to happen (not bugs)

- `rm` prints one extra dim line (`→ trash …`). `command rm` / `\rm` is the real one.
- The first `git …` Enter in a big repo can take ~20 ms (a `git status --porcelain`).
- Ghost text needs ≥2 typed characters and only shows commands you have actually run.
- `fore aliases` says "No alias proposals yet" until you've repeated things ≥4×.

## 5. Optional: a real model (10 min, needs ~2 GB disk)

```sh
curl -fsSL https://ollama.com/install.sh | sh
ollama pull qwen2.5-coder:1.5b        # the default in config.toml
fore doctor                            # model line should turn ✔
```

Then: run `cargo tset` (or any typo) → **Ctrl-/** → the fix appears in your prompt line with a risk badge.
Type `files bigger than 100mb modified this week` → **Ctrl-Space** → a `find` command appears.
On a 2-CPU VM without a GPU the 1.5B model answers in 2–6 s; that's the model, not fore.

## 6. Uninstall

```sh
./install.sh --uninstall           # keeps history; add --purge to delete everything
```

Check: `grep fore ~/.zshrc` → nothing; `systemctl --user status fore` → not found; `which fore` → nothing.

## 7. Report back

What I'd love to know, in this order of usefulness:

1. Anything that printed an error or looked wrong (copy the text).
2. `fore doctor` output after install.
3. Any moment the shell felt slower than before fore.
4. Whether the reboot-autostart test passed.
5. The `fore stats` output after a day of use, if you keep it around.

`~/.local/state/fore/daemon.log` has everything the daemon did; attach it if something misbehaves.
