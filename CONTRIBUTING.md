# Contributing to fore

Thanks for looking. fore is a small codebase with three hard rules; everything else is negotiable.

## The three rules

1. **Latency is a feature.** Anything on the keystroke path (ghost text) must stay under 2 ms p99,
   anything on the Enter path (pre-flight) under 5 ms, with a hard 400 ms timeout in the plugin.
   If your change touches `predict.rs`, `preflight.rs`, `daemon.rs` or `shell/fore.zsh`, include
   before/after numbers from `fore doctor` (latency section) or `fore ping`.
2. **Nothing unredacted reaches a model.** `llm::Llm::complete` only accepts `Redacted`, whose
   constructor is private to `redact.rs`. Don't add a second constructor, don't add a `From<String>`.
   New context you want to send to a model goes through `Redactor::wrap`.
3. **Proposals are never executed.** `Ctrl-/` and `Ctrl-Space` insert text into the line editor.
   That is where it stops. DESTRUCTIVE proposals are inserted as a `# comment`.

## Setup

```sh
git clone https://github.com/YOUR_GITHUB_USER/fore && cd fore
cargo build --release
cargo test                             # unit tests
sudo apt-get install -y zsh sqlite3    # (Linux) for the e2e suite
python3 dev/pty_test.py                # drives a real zsh in a pty against a throwaway $HOME
python3 tools/mock_llm.py 11434 &      # OpenAI-compatible mock; prints every prompt it gets
```

Iterating on the daemon: `cargo build --release && fore restart`. Iterating on the plugin: open a
new shell (the plugin is baked into the binary via `include_str!`, so rebuild first).

## Adding things

- **A pre-flight check**: `src/preflight.rs`, one function returning `Option<Finding>`, add its id to
  the list in the module doc, add a unit test with a fake `EnvSnapshot`. Keep filesystem walks under
  the existing budget helpers.
- **A model provider**: `src/providers.rs` → `PRESETS`. Only OpenAI-compatible endpoints; if a
  provider needs a header quirk, put it in `llm::apply_auth` (see Anthropic).
- **A redaction pattern**: `src/redact.rs`, add the regex to the right layer and a test that shows
  the *masked* output. False negatives are bugs; false positives on obvious non-secrets are too.
- **A shell**: `shell/<name>.<ext>` + a `fore init <name>` arm. bash and fish are wanted.

## Pull requests

- One topic per PR. Tests for behaviour changes. `cargo test` and `python3 dev/pty_test.py` green.
- Commit messages: what changed and why, in the imperative (“preflight: warn on `chmod -R 777`”).
- If it changes what users see, update `README.md` (and `docs/` if there's a walkthrough).

## Reporting bugs

`fore doctor` output plus the last lines of `~/.local/state/fore/daemon.log` answer most questions
up front. Redact anything you don't want public — the log never contains command *arguments*, but
does contain directory names.
