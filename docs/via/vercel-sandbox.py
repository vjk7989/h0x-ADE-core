#!/usr/bin/env python3
"""Reach a Vercel Sandbox with hc: hc machine add --name box --via '/path/vercel-sandbox.py <sandbox>'

hc needs a connection that carries bytes both ways untouched, like ssh. `sandbox exec`
alone drops stdin, and `sandbox exec -i` wants a terminal and runs the command in a remote
pty. This makes `-i` a clean pipe:
- the CLI insists stdout is a terminal, so it gets a raw local pty that is copied out;
- the remote pty is switched to raw, then prints a marker; stdin is forwarded only after it;
- the CLI stays alive while its stdin is open, so close it once it reports the connection closed.
"""
import os, pty, subprocess, sys, threading, tty

MARK = b"\x1d__hc_ready__\x1d"
REMOTE = 'stty raw -echo; printf "\\035__hc_ready__\\035"; exec sh -c "$0"'

name, line = sys.argv[1], sys.argv[-1]
master, slave = pty.openpty()
tty.setraw(slave)
cli = subprocess.Popen(
    ["sandbox", "exec", "-i", name, "--", "sh", "-c", REMOTE, line],
    stdin=subprocess.PIPE, stdout=slave, stderr=subprocess.PIPE,
)
os.close(slave)


def close_stdin():
    try:
        cli.stdin.close()
    except (OSError, ValueError):
        pass


def forward_stdin():
    try:
        while data := os.read(sys.stdin.fileno(), 65536):
            cli.stdin.write(data)
            cli.stdin.flush()
    except (OSError, ValueError):
        pass


def watch_stderr():
    seen = b""
    for chunk in iter(lambda: cli.stderr.read1(4096), b""):
        seen = (seen + chunk)[-4096:]
        if b"closed." in seen or b"failed." in seen:
            return close_stdin()


threading.Thread(target=watch_stderr, daemon=True).start()
pending, ready = b"", False
while True:
    try:
        data = os.read(master, 65536)
    except OSError:
        break
    if not data:
        break
    if not ready:
        pending += data
        i = pending.find(MARK)
        if i < 0:
            continue
        ready, data = True, pending[i + len(MARK):]
        threading.Thread(target=forward_stdin, daemon=True).start()
    os.write(sys.stdout.fileno(), data)
sys.exit(cli.wait())
