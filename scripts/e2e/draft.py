"""Draft hold through a real attach (Linux and macOS: needs a pty).

usage: draft.py <h0x-core> <terminal>

Attaches to the terminal the way a person does, types an unfinished line, then
sends a note from outside. The core must hold the note while the line is a
fresh draft (H0X_CORE_DRAFT_MS), then land it. Prints `hold=<seconds>`.
"""
import os
import pty
import subprocess
import sys
import threading
import time

hc, term = sys.argv[1], sys.argv[2]

pid, fd = pty.fork()
if pid == 0:
    os.execv(hc, [hc, "attach", term])


def drain():
    try:
        while os.read(fd, 4096):
            pass
    except OSError:
        pass


threading.Thread(target=drain, daemon=True).start()
time.sleep(2)
os.write(fd, b"hello")
time.sleep(0.5)
start = time.time()
done = subprocess.run([hc, "send", term, "NOTE"], capture_output=True, text=True, timeout=60)
print(f"hold={time.time() - start:.1f} rc={done.returncode}")
os.write(fd, b"\x1c")
time.sleep(0.5)
try:
    os.kill(pid, 9)
except OSError:
    pass
