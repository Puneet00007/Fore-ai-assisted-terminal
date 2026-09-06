#!/usr/bin/env python3
"""Record a real fore session: pty → zsh → pyte terminal emulator → PNG frames → MP4/GIF.

Nothing is faked: the keystrokes go to a live zsh with the fore plugin loaded, talking to
the real daemon. Screenshots are taken at the interesting moments.

Usage: python3 dev/record_demo.py  [--out /home/user/fore-demo]
"""
import os, pty, re, select, sys, time, shutil, subprocess, textwrap
import pyte
from PIL import Image, ImageDraw, ImageFont

OUT = "/home/user/fore-demo"
for i, a in enumerate(sys.argv):
    if a == "--out": OUT = sys.argv[i + 1]
FRAMES = f"{OUT}/frames"
SHOTS = f"{OUT}/screenshots"
shutil.rmtree(OUT, ignore_errors=True)
os.makedirs(FRAMES); os.makedirs(SHOTS)

COLS, ROWS = 100, 30
FONT = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"
FONT_B = "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf"
FS = 17
font = ImageFont.truetype(FONT, FS)
font_b = ImageFont.truetype(FONT_B, FS)
CW = font.getlength("M")
CH = int(FS * 1.35)
PAD = 18
TITLE_H = 40
W = int(COLS * CW + 2 * PAD)
H = int(ROWS * CH + 2 * PAD + TITLE_H)

# One Dark-ish palette
BG = (17, 19, 24); FG = (220, 223, 228)
ANSI16 = {
    "black": (40, 44, 52), "red": (224, 108, 117), "green": (152, 195, 121), "brown": (229, 192, 123),
    "blue": (97, 175, 239), "magenta": (198, 120, 221), "cyan": (86, 182, 194), "white": (171, 178, 191),
    "brightblack": (92, 99, 112), "brightred": (224, 108, 117), "brightgreen": (152, 195, 121),
    "brightbrown": (229, 192, 123), "brightblue": (97, 175, 239), "brightmagenta": (198, 120, 221),
    "brightcyan": (86, 182, 194), "brightwhite": (255, 255, 255), "default": None,
}

def color(c, default):
    if c in ANSI16:
        return ANSI16[c] or default
    if isinstance(c, str) and len(c) == 6:
        try: return tuple(int(c[i:i + 2], 16) for i in (0, 2, 4))
        except ValueError: pass
    return default

screen = pyte.Screen(COLS, ROWS)
stream = pyte.ByteStream(screen)
frames = []          # (path, duration)
frame_no = 0
elapsed = 0.0


def render(caption=None):
    img = Image.new("RGB", (W, H), BG)
    d = ImageDraw.Draw(img)
    # title bar
    d.rounded_rectangle([0, 0, W, TITLE_H], radius=0, fill=(30, 33, 40))
    for i, c in enumerate([(255, 95, 86), (255, 189, 46), (39, 201, 63)]):
        d.ellipse([PAD + i * 22, 13, PAD + i * 22 + 13, 26], fill=c)
    title = "fore — zsh"
    d.text(((W - font.getlength(title)) / 2, 10), title, fill=(160, 165, 175), font=font)
    y0 = TITLE_H + PAD
    for y in range(ROWS):
        line = screen.buffer[y]
        for x in range(COLS):
            ch = line[x]
            fg = color(ch.fg, FG); bg = color(ch.bg, BG)
            if ch.reverse: fg, bg = bg, fg
            px = PAD + x * CW; py = y0 + y * CH
            if bg != BG:
                d.rectangle([px, py, px + CW, py + CH], fill=bg)
            if ch.data and ch.data != " ":
                f = font_b if ch.bold else font
                if ch.italics: fg = tuple(min(255, int(v * 0.8 + 50)) for v in fg)
                d.text((px, py + 2), ch.data, fill=fg, font=f)
    # cursor
    cx, cy = screen.cursor.x, screen.cursor.y
    if not screen.cursor.hidden:
        px = PAD + cx * CW; py = y0 + cy * CH
        d.rectangle([px, py + 2, px + CW, py + CH], fill=(245, 197, 66))
        ch = screen.buffer[cy][cx]
        if ch.data and ch.data != " ":
            d.text((px, py + 2), ch.data, fill=BG, font=font)
    if caption:
        tw = font_b.getlength(caption) + 28
        d.rounded_rectangle([W - tw - 16, H - 46, W - 16, H - 12], radius=8, fill=(97, 175, 239))
        d.text((W - tw - 2, H - 40), caption, fill=(17, 19, 24), font=font_b)
    return img


def snap(dur, caption=None):
    global frame_no, elapsed
    img = render(caption)
    path = f"{FRAMES}/f{frame_no:05d}.png"
    img.save(path)
    frames.append((path, dur))
    frame_no += 1
    elapsed += dur
    return img


class Term:
    def __init__(self):
        env = {k: v for k, v in os.environ.items() if not k.startswith("FORE_")}
        env.update(ZDOTDIR="/home/user/fore/demo", TERM="xterm-256color", LANG="C.UTF-8",
                   COLUMNS=str(COLS), LINES=str(ROWS), PATH=os.path.expanduser("~/.local/bin") + ":" + os.environ["PATH"])
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            os.execvpe("zsh", ["zsh", "-i"], env)
        import fcntl, termios, struct
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def pump(self, t):
        end = time.time() + t
        while time.time() < end:
            r, _, _ = select.select([self.fd], [], [], 0.02)
            if r:
                try: data = os.read(self.fd, 65536)
                except OSError: return
                stream.feed(data)

    def type(self, text, cps=0.055, caption=None):
        """Type like a human: one char at a time, frame per char."""
        for ch in text:
            os.write(self.fd, ch.encode())
            self.pump(cps)
            snap(cps, caption)

    def key(self, seq, wait=0.6, caption=None, frames_=6):
        os.write(self.fd, seq.encode() if isinstance(seq, str) else seq)
        for _ in range(frames_):
            self.pump(wait / frames_)
            snap(wait / frames_, caption)

    def hold(self, secs, caption=None):
        for _ in range(int(secs * 5)):
            self.pump(0.2)
            snap(0.2, caption)

    def enter(self, wait=1.0, caption=None):
        self.key("\r", wait, caption)

    def shot(self, name, caption=None):
        self.pump(0.3)
        img = snap(1.6, caption)
        img.save(f"{SHOTS}/{name}.png")

    def clear(self):
        os.write(self.fd, b"\x15")   # Ctrl-U
        self.pump(0.2)


ENTER, CTRL_U, CTRL_SLASH, CTRL_SPACE, RIGHT = "\r", "\x15", "\x1f", "\x00", "\x1b[C"

t = Term()
t.hold(1.6)                                   # welcome card
t.shot("00-welcome", "fore — live demo")
t.hold(1.0)

# ---- 1. ghost text --------------------------------------------------------
t.type("clear", cps=0.03); t.enter(0.4)
# seed a harmless command so accepting it is instant
t.type("git log --oneline -5", cps=0.02); t.enter(0.8)
t.type("clear", cps=0.03); t.enter(0.4)
t.type("car", caption="1 · ghost text from your own history")
t.hold(1.2, "1 · ghost text from your own history")
t.shot("01-ghost-text", "1 · ghost text — grey = suggestion")
t.key(CTRL_U, 0.3)
t.type("git l", caption="1 · ghost text from your own history")
t.hold(1.0, "1 · ghost text from your own history")
t.key(RIGHT, 0.5, "→ accepts")
t.shot("02-accepted", "→ accepted the whole command")
t.enter(1.4, "runs it")
t.hold(0.8)

# ---- 2. Ctrl-/ fix ----------------------------------------------------------
t.type("clear", cps=0.03); t.enter(0.4)
t.type("cargo tset", caption="2 · a typo…")
t.enter(1.6, "2 · a typo… fails")
t.shot("03-typo-failed", "2 · command failed")
t.key(CTRL_SLASH, 1.8, "Ctrl-/ → the fix, precomputed", frames_=9)
t.shot("04-ctrl-slash-fix", "Ctrl-/ → fix placed in the line, never run")
t.hold(1.5, "Ctrl-/ → fix placed in the line, never run")
t.key(CTRL_U, 0.3)

# ---- 3. Ctrl-Space English → command -----------------------------------------
t.type("clear", cps=0.03); t.enter(0.4)
t.type("files bigger than 50mb", caption="3 · plain English…")
t.key(CTRL_SPACE, 1.8, "Ctrl-Space → command", frames_=9)
t.shot("05-ctrl-space-english", "3 · English → command, with risk badge")
t.hold(1.5, "3 · English → command, with risk badge")
t.key(CTRL_U, 0.3)
t.type("delete all node_modules folders", caption="…and something dangerous")
t.key(CTRL_SPACE, 1.8, "DESTRUCTIVE → arrives as a # comment", frames_=9)
t.shot("06-destructive-commented", "DESTRUCTIVE → arrives as a # comment")
t.hold(1.8, "DESTRUCTIVE → arrives as a # comment")
t.key(CTRL_U, 0.3)

# ---- 4. pre-flight preview + undo ---------------------------------------------
t.type("clear", cps=0.03); t.enter(0.4)
t.type("cd ~/playground", cps=0.04); t.enter(0.5)
t.type("ls", cps=0.04); t.enter(0.8)
t.type("rm -rf build", caption="4 · rm on a 300 MB build dir")
t.enter(2.0, "pre-flight: counts files + size first")
t.shot("07-rm-preview-trash", "4 · rm → preview, goes to trash, not deleted")
t.hold(1.2, "4 · rm → preview, goes to trash, not deleted")
t.type("ls", cps=0.04); t.enter(0.8, "it's gone…")
t.type("fore undo", caption="5 · fore undo")
t.enter(1.6, "5 · fore undo")
t.type("ls", cps=0.04); t.enter(0.8, "…and it's back")
t.shot("08-undo-restored", "5 · fore undo — restored")
t.hold(1.5, "5 · fore undo — restored")

# ---- 5. guards -----------------------------------------------------------------
t.type("clear", cps=0.03); t.enter(0.4)
t.type("git add -A", caption="6 · guards: staging a .env file")
t.enter(1.6, "6 · guards: .env about to be committed")
t.shot("09-guard-secret-file", "6 · guard — .env is not git-ignored")
t.hold(1.2, "6 · guard — .env is not git-ignored")
t.key(CTRL_U, 0.4, "Ctrl-U: never mind")
t.type("git push --force origin main", caption="force-push to main…")
t.enter(1.6, "blocked — Enter again to confirm, any edit resets")
t.shot("10-guard-force-push-block", "force-push to main — blocked until 2nd Enter")
t.hold(1.5, "force-push to main — blocked until 2nd Enter")
t.key(CTRL_U, 0.3)
t.type("echo hi > .gitignore", caption="overwriting a file with >")
t.enter(1.4, "warns: did you mean >> ?")
t.shot("11-guard-truncate", "`>` on an existing file — did you mean `>>`?")
t.hold(1.0)

# ---- 6. privacy ----------------------------------------------------------------
t.type("clear", cps=0.03); t.enter(0.4)
t.type("export STRIPE_KEY=sk_live_51HxQ7qL9zT3mNbV2", caption="7 · a secret in a command")
t.enter(0.8)
t.type("fore redact -- \"curl -H 'Authorization: Bearer abc123' https://api.x.io\"", cps=0.03, caption="what a model would see")
t.enter(1.2, "redacted before anything leaves the machine")
t.type("sqlite3 ~/.local/share/fore/history.db \"select cmd from history where cmd like 'export STRIPE%'\"", cps=0.025, caption="what the history DB stored")
t.enter(1.4, "stored redacted too")
t.shot("12-privacy-redaction", "7 · secrets never reach the DB or the model")
t.hold(1.8, "7 · secrets never reach the DB or the model")

# ---- 7. offline + doctor + stats ---------------------------------------------
t.type("clear", cps=0.03); t.enter(0.4)
t.type("fore stop", caption="8 · daemon down?")
t.enter(1.0)
t.type("ls", cps=0.05, caption="shell keeps working, no lag, no errors"); t.enter(0.8, "shell keeps working, no lag, no errors")
t.type("fore start", caption="fore start"); t.enter(1.4)
t.type("fore status"); t.enter(1.2)
t.shot("13-stop-start", "8 · offline is a non-event")
t.hold(1.0)
t.type("clear", cps=0.03); t.enter(0.4)
t.type("fore doctor", caption="9 · fore doctor")
t.enter(2.2, "9 · fore doctor")
t.shot("14-doctor", "9 · fore doctor — every check has a fix")
t.hold(2.2, "9 · fore doctor — every check has a fix")
t.type("clear", cps=0.03); t.enter(0.4)
t.type("fore stats", caption="10 · fore stats")
t.enter(1.6, "10 · fore stats")
t.shot("15-stats", "10 · fore stats — keystrokes saved, failures, slowest")
t.hold(2.5, "10 · fore stats — keystrokes saved, failures, slowest")
t.type("clear", cps=0.03); t.enter(0.4)
t.type("fore aliases", caption="11 · alias miner")
t.enter(1.6, "11 · alias miner")
t.shot("16-aliases", "11 · fore aliases — shortcuts mined from what you repeat")
t.hold(2.5, "11 · fore aliases — shortcuts mined from what you repeat")
t.type("fore ping", caption="latency"); t.enter(1.4, "round trip in microseconds")
t.shot("17-ping", "round-trip latency")
t.hold(2.0, "that's fore.")

os.write(t.fd, b"exit\r")
time.sleep(0.3)

# ---- encode -------------------------------------------------------------------
with open(f"{OUT}/concat.txt", "w") as f:
    for path, dur in frames:
        f.write(f"file '{path}'\nduration {dur:.3f}\n")
    f.write(f"file '{frames[-1][0]}'\n")
import imageio_ffmpeg
ff = imageio_ffmpeg.get_ffmpeg_exe()
subprocess.run([ff, "-y", "-loglevel", "error", "-f", "concat", "-safe", "0", "-i", f"{OUT}/concat.txt",
                "-vf", "fps=15,scale=trunc(iw/2)*2:trunc(ih/2)*2", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "23", "-preset", "medium",
                f"{OUT}/fore-demo.mp4"], check=True)
# GIF at reduced size for quick viewing
# GIF (memory-light: palette from a 720px stream, 6 fps; two passes instead of split)
subprocess.run([ff, "-y", "-loglevel", "error", "-i", f"{OUT}/fore-demo.mp4",
                "-vf", "fps=6,scale=720:-1:flags=lanczos,palettegen=max_colors=64:stats_mode=diff", f"{OUT}/palette.png"], check=True)
subprocess.run([ff, "-y", "-loglevel", "error", "-i", f"{OUT}/fore-demo.mp4", "-i", f"{OUT}/palette.png",
                "-lavfi", "fps=6,scale=720:-1:flags=lanczos[x];[x][1:v]paletteuse=dither=none", f"{OUT}/fore-demo.gif"], check=True)
os.remove(f"{OUT}/palette.png")
shutil.rmtree(FRAMES)   # 47 MB of PNG frames; the video has them
print(f"frames: {len(frames)}  duration: {elapsed:.1f}s")
print(f"video: {OUT}/fore-demo.mp4  ({os.path.getsize(OUT + '/fore-demo.mp4') // 1024} KB)")
print(f"gif:   {OUT}/fore-demo.gif  ({os.path.getsize(OUT + '/fore-demo.gif') // 1024} KB)")
print(f"screenshots: {len(os.listdir(SHOTS))} in {SHOTS}")
