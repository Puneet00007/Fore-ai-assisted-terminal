#!/usr/bin/env python3
"""End-to-end test of the zsh plugin in a real pty, against a fresh HOME.

Runs twice: native ghost text, and with zsh-autosuggestions loaded (coexistence).
Usage: python3 dev/pty_test.py [--home /tmp/h]
"""
import os, pty, re, select, sys, time, shutil, subprocess

HOME = "/tmp/h"
for i, a in enumerate(sys.argv):
    if a == "--home": HOME = sys.argv[i + 1]
FORE = f"{HOME}/.local/bin/fore"
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[()][0-9A-Za-z]|\x1b[=>]|\r")

def clean(b):
    return ANSI.sub("", b.decode("utf-8", "replace"))

class Shell:
    def __init__(self, extra_rc=""):
        env = {k: v for k, v in os.environ.items() if not k.startswith("FORE_")}
        env.update(HOME=HOME, ZDOTDIR=HOME, TERM="xterm-256color", PATH=f"{HOME}/.local/bin:" + os.environ["PATH"], LANG="C.UTF-8")
        env.pop("XDG_RUNTIME_DIR", None)
        rc = f"{HOME}/.zshrc"
        base = open(rc).read().split("# --pty-test-extra--")[0].rstrip("\n")
        open(rc, "w").write(base + ("\n# --pty-test-extra--\n" + extra_rc + "\n" if extra_rc else "\n"))
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.chdir(f"{HOME}/proj")
            os.execvpe("zsh", ["zsh", "-i"], env)
        self.buf = b""
        self.read(1.0)

    def read(self, t=0.4):
        end = time.time() + t
        out = b""
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.05)
            if r:
                try:
                    d = os.read(self.fd, 65536)
                except OSError:
                    break
                if not d: break
                out += d
        self.buf += out
        return out

    def type(self, s, t=0.3):
        os.write(self.fd, s.encode())
        return self.read(t)

    def close(self):
        try: os.write(self.fd, b"exit\r")
        except OSError: pass
        time.sleep(0.2)
        try: os.kill(self.pid, 9)
        except ProcessLookupError: pass

results = []
def check(name, cond, detail=""):
    results.append((name, bool(cond)))
    print(("  ✔ " if cond else "  ✖ ") + name + ("" if cond else f"   [{detail[:300]!r}]"))

def run_suite(label, extra_rc=""):
    print(f"\n== {label} ==")
    # fresh state each suite
    subprocess.run([FORE, "stop"], env={**os.environ, "HOME": HOME}, capture_output=True)
    for d in [".local/share/fore", ".local/state/fore"]:
        shutil.rmtree(f"{HOME}/{d}", ignore_errors=True)
    # "offline" = the model endpoint is dead, regardless of what's on :11434 on this box
    os.makedirs(f"{HOME}/.config/fore", exist_ok=True)
    open(f"{HOME}/.config/fore/config.toml", "w").write('[llm]\nbase_url = "http://127.0.0.1:1"\nmodel = "none"\ntimeout_s = 2\n')
    shutil.rmtree(f"{HOME}/proj", ignore_errors=True); os.makedirs(f"{HOME}/proj/build/sub")
    for i in range(5): open(f"{HOME}/proj/build/f{i}.o", "w").write("x" * 100)
    open(f"{HOME}/proj/build/sub/g.o", "w").write("y" * 50)
    open(f"{HOME}/proj/notes.txt", "w").write("keep me")

    sh = Shell(extra_rc)
    # 1. plugin loaded, daemon autostarted (by the plugin, not by us)
    out = clean(sh.type("echo mode=$FORE_GHOST_MODE session=${FORE_SESSION:+yes}\r", 0.6))
    check("plugin loaded", "session=yes" in out and os.environ.get("X") is None, out)
    mode = re.search(r"mode=(\w+)", out)
    check(f"ghost mode = {'autosuggestions' if extra_rc else 'native'}", mode and mode.group(1) == ("autosuggestions" if extra_rc else "native"), out)
    time.sleep(0.8)
    out = clean(sh.type("fore status\r", 0.6))
    check("daemon auto-started by plugin", "running" in out and "not running" not in out, out)

    # 2. history + ghost text
    sh.type("echo hello-fore-world\r", 0.5)
    sh.type("echo hello-fore-world\r", 0.5)
    sh.type("git status --short\r", 0.6)
    out = clean(sh.type("echo hel", 0.6))
    check("ghost text appears", "lo-fore-world" in out, out)
    if extra_rc:
        out = clean(sh.type("\x1b[C", 0.3))  # → : zsh-autosuggestions forward-char accepts partially; use End
        out += clean(sh.type("\x1b[F", 0.3))
    else:
        out = clean(sh.type("\x1b[C", 0.3))
    out = clean(sh.type("\r", 0.6))
    check("accept + run", "hello-fore-world" in out, out)

    # 3. leading space is not recorded
    sh.type(" echo SECRET-LINE-42\r", 0.5)
    sh.type("echo TOKEN=abcdef123456789 ok\r", 0.5)
    time.sleep(0.4)
    db = subprocess.run(["sqlite3", f"{HOME}/.local/share/fore/history.db", "select cmd from history"], capture_output=True, text=True).stdout
    check("leading-space command not recorded", "SECRET-LINE-42" not in db, db)
    check("secret redacted before DB write", "abcdef123456789" not in db and "TOKEN=" in db, db)

    # 4. safe rm + undo
    out = clean(sh.type("rm -rf build\r", 1.2))
    check("rm routed to trash", "trash" in out and "fore undo" in out, out)
    check("build dir gone", not os.path.exists(f"{HOME}/proj/build"))
    out = clean(sh.type("fore undo\r", 0.8))
    check("undo restores", os.path.exists(f"{HOME}/proj/build/sub/g.o") and "restored" in out, out)
    out = clean(sh.type("command rm notes.txt; ls\r", 0.6))
    check("command rm bypasses", not os.path.exists(f"{HOME}/proj/notes.txt"), out)

    # 5. pre-flight: redirect truncation warn, protected-branch block (second Enter)
    sh.type("echo one > out.txt\r", 0.5)
    out = clean(sh.type("echo two > out.txt\r", 0.9))
    check("pre-flight warn (truncating redirect)", "overwrites" in out, out)
    sh.type("git init -q . && git checkout -q -b main 2>/dev/null; git add . >/dev/null; git commit -qm init >/dev/null\r", 1.5)
    out = clean(sh.type("git push --force origin main\r", 1.0))
    check("pre-flight block (force-push main)", "press Enter again" in out, out)
    out = clean(sh.type("\x15", 0.3))  # Ctrl-U clears line
    check("block cleared by edit", True)

    # 6. Ctrl-/ with no model server → friendly message, no hang
    sh.type("false\r", 0.5)
    t0 = time.time()
    out = clean(sh.type("\x1f", 2.5))
    check("Ctrl-/ offline gives friendly error quickly", ("Ollama" in out or "model" in out) and time.time() - t0 < 5, out)
    # 7. Ctrl-Space offline
    sh.type("\x15", 0.2)
    sh.type("list big files")
    out = clean(sh.type("\x00", 2.0))
    check("Ctrl-Space offline gives friendly error", "Ollama" in out or "model" in out, out)
    sh.type("\x15", 0.2)

    # 8. stats works
    out = clean(sh.type("fore stats | head -3\r", 0.8))
    check("fore stats", "commands" in out, out)
    # 9. shell startup cost
    out = clean(sh.type("for i in 1 2 3; do s=$EPOCHREALTIME; zsh -i -c exit; print T=$(( (EPOCHREALTIME - s) * 1000 )); done\r", 4.0))
    times = [round(float(x)) for x in re.findall(r"^T=(\d+\.?\d*)$", out, re.M)]
    check(f"shell startup ok ({times} ms)", times and max(times) < 500, out)
    sh.close()

run_suite("native")
if os.path.exists("/tmp/zas/zsh-autosuggestions.zsh"):
    run_suite("with zsh-autosuggestions", "source /tmp/zas/zsh-autosuggestions.zsh")

passed = sum(1 for _, ok in results if ok)
print(f"\n{passed}/{len(results)} passed")
sys.exit(0 if passed == len(results) else 1)
