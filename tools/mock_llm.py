#!/usr/bin/env python3
"""
Mock OpenAI-compatible /v1/chat/completions server for testing fore end-to-end
without a real model. Answers are rule-based. Swap in a real model by setting
FORE_LLM_BASE_URL to Ollama / OpenAI / etc. — nothing in fore changes.

It also logs every prompt it receives to stderr, so you can *see* what left the
daemon (i.e. verify that redaction worked).
"""
import json, re, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer

def fix_for(user: str) -> str:
    m = re.search(r"failed command: (.*)", user)
    cmd = m.group(1).strip() if m else ""
    err = user.split("stderr (last lines):", 1)[-1].lower()

    if "cargo" in cmd and ("no such command" in err or "tset" in cmd):
        return "WHY: `tset` is a typo of the `test` subcommand.\nFIX: cargo test"
    if "gti " in cmd:
        return "WHY: `gti` is a typo of `git`.\nFIX: " + cmd.replace("gti ", "git ", 1)
    if "permission denied" in err:
        return "WHY: The file or directory is not writable by your user.\nFIX: sudo " + cmd
    if "command not found" in err or "not found" in err:
        prog = cmd.split()[0] if cmd else "it"
        return f"WHY: `{prog}` is not installed or not on PATH.\nFIX: sudo apt-get install -y {prog}"
    if "no such file" in err:
        return "WHY: The path does not exist in the current directory.\nFIX: ls -la"
    if "did you mean" in err:
        m2 = re.search(r"did you mean[^\n]*?[`'\"]([^`'\"]+)[`'\"]", err)
        if m2 and cmd:
            parts = cmd.split()
            return f"WHY: Typo in the subcommand.\nFIX: {parts[0]} {m2.group(1)} " + " ".join(parts[2:])
    return "WHY: Could not determine the cause from stderr.\nFIX: " + (cmd if cmd else "ls")

def nl_for(user: str) -> str:
    m = re.search(r"request: (.*)", user)
    req = (m.group(1) if m else "").lower().strip()
    if "larger than" in req or "bigger than" in req:
        size = re.search(r"(\d+)\s*(m|mb|g|gb|k|kb)", req)
        s = (size.group(1) + size.group(2)[0].upper()) if size else "100M"
        return f"CMD: find . -type f -size +{s} -exec ls -lh {{}} +\nNOTE: -"
    if "delete" in req and "node_modules" in req:
        return "CMD: find . -name node_modules -type d -prune -exec rm -rf {} +\nNOTE: recursive delete; review the list first"
    if "port" in req and ("using" in req or "listening" in req or "on port" in req):
        p = re.search(r"\d{2,5}", req)
        return f"CMD: lsof -i :{p.group(0) if p else 8080} -sTCP:LISTEN\nNOTE: -"
    if "commits" in req and ("today" in req or "last"):
        return "CMD: git log --since=midnight --oneline\nNOTE: -"
    if "disk" in req or "space" in req:
        return "CMD: du -sh * | sort -rh | head -20\nNOTE: -"
    return "CMD: echo 'not sure how to do that'\nNOTE: mock model has no rule for this"

class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            body = json.dumps({"object": "list", "data": [{"id": "mock-1", "object": "model"}]}).encode()
            self.send_response(200); self.send_header("Content-Type", "application/json"); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
        else:
            self.send_response(404); self.end_headers()

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(n) or b"{}")
        msgs = body.get("messages", [])
        system = next((m["content"] for m in msgs if m["role"] == "system"), "")
        user = next((m["content"] for m in msgs if m["role"] == "user"), "")
        print("\n" + "=" * 70 + f"\nPROMPT RECEIVED ({time.strftime('%H:%M:%S')}) model={body.get('model')}\n" + "-" * 70 + f"\n{user}" + "=" * 70, file=sys.stderr, flush=True)
        content = fix_for(user) if "WHY:" in system else nl_for(user)
        time.sleep(0.35)  # pretend to think, so "precomputed" vs "live" is observable
        out = json.dumps({"id": "mock", "object": "chat.completion", "model": body.get("model"),
                          "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}]}).encode()
        self.send_response(200); self.send_header("content-type", "application/json"); self.send_header("content-length", str(len(out))); self.end_headers(); self.wfile.write(out)

if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 11434
    print(f"mock llm on http://127.0.0.1:{port}/v1", file=sys.stderr, flush=True)
    HTTPServer(("127.0.0.1", port), H).serve_forever()
