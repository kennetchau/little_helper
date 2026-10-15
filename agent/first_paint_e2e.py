#!/usr/bin/env python3
"""E2E regression: the TUI must PAINT WITHOUT ANY INPUT.

Launches `little_helper tui` in a pty and reads for ~2 s without sending
a single byte. The first real frame (header/status bar) must appear well
before the deadline — historically startup hung in the drop of the
throwaway tokio runtime used by the kitty probe, so the screen stayed
black on the alternate screen until the user's first keystroke.

Usage: agent/first_paint_e2e.py <dir-containing-little_helper>
Exit 0 = frame painted with no input; exit 1 = black screen.
"""
import fcntl
import os
import pty
import shutil
import struct
import sys
import termios
import time

root = sys.argv[1]
state = "/tmp/first-paint-state"
ws = "/tmp/first-paint-ws"
for d in (state, ws):
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)

bin_ = os.path.join(root, "little_helper")
pid, fd = pty.fork()
if pid == 0:
    os.execve(
        bin_,
        [bin_, "tui"],
        dict(os.environ, HARNESS_STATE=state, HARNESS_WORKSPACE=ws),
    )
fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
os.set_blocking(fd, False)

START = time.time()
out = b""
painted_at = None

# Phase 1: 3 s of pure reading — NO input of any kind.
while time.time() - START < 3.0:
    try:
        chunk = os.read(fd, 65536)
    except (BlockingIOError, OSError):
        chunk = b""
    if chunk:
        out += chunk
        if painted_at is None and b"model" in out:
            painted_at = time.time() - START

if painted_at is not None:
    print(f"ok: first frame painted at t={painted_at:.2f}s with no input")
else:
    print("FAIL: no frame painted within 3 s of NO input — black screen")
    os.kill(pid, 9)
    sys.exit(1)

# Phase 2: clean /quit still works afterwards.
os.write(fd, b"/quit\r")
deadline = time.time() + 3.0
exited = False
while time.time() < deadline:
    try:
        wpid, _ = os.waitpid(pid, os.WNOHANG)
        if wpid == pid:
            exited = True
            break
    except ChildProcessError:
        exited = True
        break
    time.sleep(0.05)

if exited:
    print("ok: clean exit after /quit")
    sys.exit(0)
print("FAIL: process did not exit after /quit")
try:
    os.kill(pid, 9)
except ProcessLookupError:
    pass
sys.exit(1)
