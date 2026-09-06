# Publishing fore to GitHub

Ten minutes, start to finish. Do this on your own machine (Mac or Linux), not in a sandbox.

## 0. What kind of project is this? (for the repo description and topics)

fore is a **zsh plugin + background daemon**, not a terminal emulator and not an "AI agent".
It belongs in the same GitHub neighbourhood as `zsh-autosuggestions`, `atuin`, `mcfly`, `thefuck`
and `zoxide` — tools that plug into the shell you already have. Say so in the description; people
searching for those tools are your audience.

## 1. Get the code onto your machine

```sh
tar xzf fore-0.4.0.tar.gz && cd fore
git init -b main
```

## 2. Put your name on it

```sh
dev/set-github-user.sh <your-github-username> "Your Full Name"
# replaces YOUR_GITHUB_USER (README links, install.sh, config template, Cargo.toml, CI badge)
# and YOUR_NAME (LICENSE) everywhere
git add -A && git commit -m "fore v0.4.0"
```

## 3. Create the repository

Either in the browser — github.com → **New repository** → name `fore`, public, **no** README /
.gitignore / license (we have them) — or with the CLI:

```sh
gh repo create fore --public --source=. --description \
  "Local-first shell copilot for zsh: ghost text from your history, Ctrl-/ fixes the last error, English→command, pre-flight checks before Enter, undoable rm. Rust daemon, any OpenAI-compatible model, secrets never leave the machine." \
  --push
```

If you created it in the browser:

```sh
git remote add origin git@github.com:<you>/fore.git   # or https://github.com/<you>/fore.git
git push -u origin main
```

## 4. Make it findable (this is what actually gets stars)

On the repo page → ⚙ next to **About**:

- **Description**: the sentence from step 3.
- **Website**: leave empty for now, or your blog post about it.
- **Topics** (add all of these): `zsh` `zsh-plugin` `shell` `terminal` `cli` `rust` `ai`
  `llm` `ollama` `developer-tools` `productivity` `copilot` `command-line` `local-first`
  `openai` `natural-language`

Then **Settings → General**: enable *Discussions* (people ask questions there instead of opening
issues), and under *Features* keep *Issues* on.

## 5. Watch CI go green

**Actions** tab → the "CI" workflow runs automatically on the push: builds on Ubuntu + macOS,
runs 52 unit tests, drives a real zsh in a pty for 18 end-to-end checks, and lints. The README
badge turns green when it passes. If macOS fails on something you can't reproduce, open an issue on
yourself with the log — it's useful to have visible.

## 6. Cut a release (gives you downloadable binaries)

```sh
git tag v0.4.0 && git push origin v0.4.0
```

The "Release" workflow builds `fore` for Linux x86-64 / arm64 and macOS Intel / Apple Silicon,
packs each with `install.sh`, `shell/` and `docs/`, and publishes them under **Releases** with
auto-generated notes. Users who don't want Rust can then download a tarball and run
`./install.sh` — it detects the prebuilt binary next to it and skips the build entirely.

## 7. Tell people

In order of payoff for a tool like this:

1. **A 90-second demo is already in the README.** Keep the GIF near the top; it's what people
   look at before reading anything.
2. **Hacker News → "Show HN: fore – local-first shell copilot for zsh (Rust, works with Ollama)"**.
   Post at ~14:00 UTC on a weekday. Answer every comment for the first two hours. Lead with the
   numbers (1.8 ms ghost text, 4 MB daemon, undo for rm) — HN is allergic to "AI" without
   engineering behind it, and fore has the engineering.
3. **r/zsh, r/commandline, r/rust** (r/rust likes the `Redacted` type-enforced redaction story).
4. **Awesome lists**: open PRs adding fore to `unixorn/awesome-zsh-plugins` (section "Plugins"),
   `agarrharr/awesome-cli-apps`, and `rust-unofficial/awesome-rust` (section "Applications →
   Utilities"). Each is a permanent trickle of visitors.
5. **A short write-up** on dev.to / your blog: "Why I built a terminal copilot as a daemon instead of
   a terminal" — the architecture decision is the interesting part.
6. Later: publish to crates.io (`cargo publish` — the name `fore` is free as of Sep 2026) so
   `cargo install fore` works, and a Homebrew tap (`brew tap <you>/fore`).

## 8. What "more recognised" looks like afterwards

- Respond to the first issues within a day. Early responsiveness is the strongest signal on GitHub.
- Label 3–5 small, well-defined tasks `good first issue` (e.g. "add a preset for Fireworks AI",
  "pre-flight: warn on `chmod -R 777`", "fish plugin"). CONTRIBUTING.md already tells people where
  each of those lives.
- Pin the demo GIF issue-free: if someone reports a bug that the GIF shows working, re-record with
  `python3 dev/record_demo.py` after the fix.
