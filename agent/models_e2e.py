#!/usr/bin/env python3
"""E2E (Phase 8): /models modal shows per-model STATUS against the REAL
backend (set HARNESS_E2E_BACKEND, e.g. your self-hosted llama.cpp
router URL with the /v1 suffix); `r` refresh hits the root-level
management route; Esc + /quit are clean.

READ-ONLY: never presses l/u (would load/unload a live model on the
shared backend). A throwaway config in a temp state dir points the
default localhost base_url at that backend (like run.sh does on first
run) — the real /state is never touched.

Usage: HARNESS_E2E_BACKEND=https://<your-backend>/v1 \\
        agent/models_e2e.py <dir-containing-little_helper>
"""
import fcntl, os, pty, re, shutil, struct, sys, termios, time

root = sys.argv[1]
backend = os.environ.get("HARNESS_E2E_BACKEND", "https://llm.example.lan/v1")
state = "/tmp/models-e2e-state"
ws = "/tmp/models-e2e-ws"
for d in (state, ws):
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
with open(os.path.join(state, "config.toml"), "w") as f:
    f.write(f'[backend]\nbase_url = "{backend}"\n')

bin_ = os.path.join(root, "little_helper")
pid, fd = pty.fork()
if pid == 0:
    os.execve(
        bin_,
        [bin_, "tui"],
        dict(os.environ, HARNESS_STATE=state, HARNESS_WORKSPACE=ws,
             HARNESS_TLS_VERIFY="0"),
    )
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 110, 0, 0))
os.set_blocking(fd, False)

out = b""
START = time.time()
SENT = {"models": False, "r": False, "quit": False}


def read_loop():
    global out
    while True:
        try:
            chunk = os.read(fd, 65536)
        except (BlockingIOError, OSError):
            return
        if chunk:
            out += chunk


def text():
    s = re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", out.decode("utf-8", "replace"))
    s = re.sub(r"\x1b\][^\x07]*(\x07|\x1b\\)", "", s)
    return s.replace("\r\n", "\n").replace("\r", "\n")


def wait_for(needle, timeout):
    end = time.time() + timeout
    while time.time() < end:
        read_loop()
        if needle in text():
            return True
        time.sleep(0.05)
    return False


def check(name, cond, extra=""):
    print(f"  {'PASS' if cond else 'FAIL'}: {name}"
          + (f" — {extra}" if extra and not cond else ""))
    return cond


ok = True
ok &= check("first frame", wait_for("session", 10))

# open the model modal (real backend query)
os.write(fd, b"/models\r")
SENT["models"] = True
ok &= check("no connection error",
            not wait_for("model list failed", 3))
ok &= check("modal title", wait_for("models (current:", 20))
ok &= check("gemma-4-31B listed", wait_for("gemma-4-31B", 5))
ok &= check("qwen_3_8_27B listed", wait_for("qwen_3_8_27B", 5))
ok &= check("qwen_3_6_35B_Heretic_MTP listed",
            wait_for("qwen_3_6_35B_Heretic_MTP", 5))
ok &= check("[loaded] status tag", wait_for("[loaded]", 10))
ok &= check("[unloaded] status tag", wait_for("[unloaded]", 10))
ok &= check("[sleeping] status tag", wait_for("[sleeping]", 10))
ok &= check("modal hint line", wait_for("l load · u unload · r refresh", 5))

# r = re-scan the model sources (management route at the server root)
os.write(fd, b"r")
read_loop()
time.sleep(2.0)
read_loop()
ok &= check("r refresh keeps the modal", "models (current:" in text())

# Esc closes, /quit exits cleanly
os.write(fd, b"\x1b")
time.sleep(0.5)
read_loop()
os.write(fd, b"/quit\r")
deadline = time.time() + 8
gone = False
while time.time() < deadline:
    try:
        w, _ = os.waitpid(pid, os.WNOHANG)
        if w == pid:
            gone = True
            break
    except ChildProcessError:
        gone = True
        break
    time.sleep(0.1)
    read_loop()
ok &= check("clean exit after /quit", gone)

if not ok:
    print("---- screen tail ----")
    print(text()[-1500:])
    try:
        os.kill(pid, 9)
    except ProcessLookupError:
        pass

print("MODELS E2E:", "PASS" if ok else "FAIL")
sys.exit(0 if ok else 1)
