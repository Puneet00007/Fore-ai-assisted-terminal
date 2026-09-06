# fore — step-by-step setup (Linux VM)

Follow top to bottom. Each step says what to type and what you should see.
Total time: ~20 minutes, most of it waiting for downloads and the build.

---

## Step 1 — Create the VM

Any of these work: VirtualBox, VMware, UTM (Mac), Hyper-V, Multipass, or a cloud VM.

| setting | value |
|---|---|
| OS image | **Ubuntu 24.04 LTS** (Desktop or Server — both fine) |
| CPUs | 2 |
| RAM | **4 GB** (the Rust build needs it; 1–2 GB will fail) |
| Disk | 20 GB |
| Network | default (NAT) — it needs internet for downloads |

Install Ubuntu normally, create a user (say `dev`), log in.

> **Tip:** once Ubuntu is installed and updated, take a **VM snapshot** named "clean".
> You can then test install → uninstall → install again from a known state.

---

## Step 2 — Get the fore source into the VM

You have `fore-0.3.0.tar.gz` (download it from this workspace). Get it into the VM by
any of these:

- **Shared folder / drag-and-drop** (VirtualBox/VMware guest additions), or
- **scp** from your host: `scp fore-0.3.0.tar.gz dev@<vm-ip>:~/`
  (find the VM's IP with `ip a` inside the VM), or
- **Copy-paste**: it's 101 KB, so even `base64` through the clipboard works.

Then in the VM:

```sh
cd ~
tar xzf fore-0.3.0.tar.gz
ls fore
```

You should see: `Cargo.lock  Cargo.toml  README.md  demo  dev  docs  install.sh  shell  src  tools`

---

## Step 3 — Install the prerequisites

Open a terminal in the VM and run:

```sh
sudo apt-get update
sudo apt-get install -y zsh git curl build-essential pkg-config sqlite3
```

Then Rust (about 1 minute):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
cargo --version
```

You should see something like `cargo 1.8x.x`.

---

## Step 4 — Make zsh your shell

fore's plugin is for zsh. Ubuntu defaults to bash, so switch:

```sh
chsh -s "$(command -v zsh)"
```

It asks for your password. **Log out and log back in** (or reboot the VM) so it takes effect.

Open a new terminal. If zsh shows a first-run menu ("This is the Z Shell configuration
function for new users"), press **`0`** — that creates an empty `~/.zshrc`, which is what we want.

Confirm:

```sh
echo $SHELL
```

Should print `/usr/bin/zsh` (or `/bin/zsh`).

---

## Step 5 — Run the installer

```sh
cd ~/fore
./install.sh
```

Takes 2–3 minutes (compiling). Expected output, roughly:

```
fore install

  ✔ Linux x86_64
  ✔ cargo 1.8x.x
  building (release, first time takes 1–3 min)…
  ✔ built target/release/fore (9.0M)
  ✔ installed /home/dev/.local/bin/fore

fore install

  ✔ wrote config template /home/dev/.config/fore/config.toml
  ✔ added plugin to ~/.zshrc
  ✔ systemd user service installed and started: /home/dev/.config/systemd/user/fore.service
  ✔ daemon running
  · importing your existing shell history (so suggestions work today):
  /home/dev/.zsh_history  12 entries → 12 new

Open a new terminal (or run: source ~/.zshrc). Then try typing a command you've used before.

fore doctor  v0.3.0
  ...
  ! model server not reachable  connection refused — start Ollama: `ollama serve` ...
  ...
2 warning(s), nothing blocking.
```

**The "model server not reachable" warning is expected** — that's Step 8 (optional).

If instead you see:

- `! missing: …` followed by a `sudo apt-get install …` line → run that line, then `./install.sh` again.
- `! autostart not installed: systemctl --user … failed (no user session bus?)` → this happens
  when installing over SSH or in a container. Run:
  ```sh
  sudo loginctl enable-linger $USER
  ```
  log out/in, then `fore install --no-zshrc`. (Even without this, fore works — the plugin
  starts the daemon itself. You'd only lose "already running before the first terminal opens".)

---

## Step 6 — Open a NEW terminal and check

Close the terminal, open a fresh one. Then:

```sh
fore status
```

→ `running  pong  v0.3.0  pid 1234  up 45s  rows 12  model qwen2.5-coder:1.5b  (0.2 ms)`

```sh
fore doctor
```

→ all ✔ except the model line (`!`).

If `fore` says "command not found": run `source ~/.zshrc` once — or check that the
terminal you opened is actually zsh (`echo $SHELL`).

---

## Step 7 — Try it (the 10-minute tour)

Do these in order. Each takes a few seconds.

**1. Ghost text** — build a little history first:
```sh
ls -la
git --version
echo hello world
echo hello world
```
Now type `ec` (don't press Enter). Grey text `ho hello world` appears after the cursor.
Press **→** to accept, Enter to run. Press **Ctrl-U** any time to clear the line.

**2. Undo a delete:**
```sh
mkdir -p /tmp/t && cd /tmp/t
touch a b c && mkdir d && touch d/e
rm -rf d
```
→ `→ trash (1 files, 0 B) — fore undo restores`
```sh
ls            # d is gone
fore undo     # → restored /tmp/t/d …
ls d          # e is back
fore trash list
```

**3. Overwrite warning:**
```sh
echo hi > a
echo hi > a
```
Second time, before it runs: `! \`> a\` overwrites an existing 3 B file — did you mean \`>>\`?`

**4. Block on force-push (nothing is pushed — there is no remote):**
```sh
git init -q && git checkout -q -b main 2>/dev/null; git add . && git commit -qm x
```
Type `git push --force origin main` and press **Enter**.
→ red `✖ force-push to main …` and `press Enter again to run, or edit the line`.
Press **Ctrl-U** to abandon it.

**5. Instant answer when there's no AI model:**
```sh
false
```
Press **Ctrl-/** (Control + forward slash; if your terminal eats it, use **Alt-f**).
→ within a few ms: `fore: no model server at http://localhost:11434/v1 — start Ollama …`
(It must not hang.)

**6. Privacy:**
```sh
 echo top-secret-thing         # NOTE the leading space
export MY_TOKEN=abcd1234efgh5678
sqlite3 ~/.local/share/fore/history.db "select cmd from history where cmd like '%secret%' or cmd like '%MY_TOKEN%'"
```
→ only `export MY_TOKEN=<REDACTED>`. The leading-space one is nowhere.

**7. Offline / restart:**
```sh
fore stop
ls; echo still works; cd ~
fore start
fore status
```
Nothing breaks while it's stopped; ghost text returns after start.

**8. Stats:**
```sh
fore stats
```

**9. Autostart (the important one):** reboot the VM, open a terminal, run `fore status`.
→ `running …` without you starting anything.

**10. Change a setting:**
```sh
fore config edit        # opens nano/vim; set  ghost_style = "fg=242,italic"  under [ui]; save
fore restart
```
Open a new terminal; ghost text is now italic.

---

## Step 8 — (Optional) a real AI model for Ctrl-/ and Ctrl-Space

**Option A — local (free, private).** Needs ~2 GB of disk and a few minutes of download.

```sh
curl -fsSL https://ollama.com/install.sh | sh
ollama pull qwen2.5-coder:1.5b
fore model             # ✔ server reachable, model available
```

**Option B — a cloud provider (no download, needs a key).** On a slow VM this is much faster
per answer. Free tiers exist at Groq, Google AI Studio (Gemini) and GitHub Models:

```sh
fore model use groq          # prints where to get a key, asks for it (hidden), saves it 0600,
                             # writes [llm] in config.toml and restarts the daemon
fore model test              # one real round trip: "list files modified today" → a command
fore model list              # openai · anthropic · gemini · github · openrouter · mistral · deepseek · …
```

Switching back is `fore model use ollama`. Only redacted text is ever sent — `fore redact 'export
TOKEN=abc123'` shows exactly what a provider would see.

Then:

- run `cargo tset` (a typo) → press **Ctrl-/** → the corrected command appears in your prompt
  line with a badge like `[read-only]`. Press Enter to run it or Ctrl-U to discard.
- type `files bigger than 100mb modified this week` → press **Ctrl-Space** (or **Alt-a**) →
  a `find …` command appears. Proposals are never run automatically.
- type `delete all node_modules folders` → **Ctrl-Space** → it arrives as `# find … -exec rm …`
  with a red `[DESTRUCTIVE]` badge; the `# ` means Enter does nothing until you remove it.

On a 2-CPU VM without GPU a local model takes 2–6 seconds per answer (a cloud provider: well under 1 s). That's the model, not fore.

---

## Step 9 — Uninstall (to test it, or when done)

```sh
cd ~/fore
./install.sh --uninstall          # add --purge to also delete history/config/trash
```

Check it's really gone:

```sh
grep fore ~/.zshrc                # → nothing
systemctl --user status fore      # → could not be found
which fore                        # → nothing
```

Restore the "clean" snapshot to try the whole thing again.

---

## Where things live

| what | path |
|---|---|
| binary | `~/.local/bin/fore` |
| config | `~/.config/fore/config.toml` |
| history database | `~/.local/share/fore/history.db` |
| trash (undo) | `~/.local/share/fore/trash/` |
| daemon log | `~/.local/state/fore/daemon.log` |
| socket / pid | `$XDG_RUNTIME_DIR/fore/` (falls back to `~/.local/state/fore/run/`) |
| autostart unit | `~/.config/systemd/user/fore.service` |

`fore config paths` prints this list for your machine.

---

## If something goes wrong

1. `fore doctor` — read the ✖ lines; each has a fix.
2. `tail -50 ~/.local/state/fore/daemon.log`
3. `fore restart`
4. Nuclear: `./install.sh --uninstall --purge && ./install.sh`

Send me: the exact text that looked wrong, the `fore doctor` output, and the last lines of the log.
