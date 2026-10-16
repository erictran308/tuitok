"""Runs tuitok in a pseudo-terminal of 150x45 cells, presses keys, and prints
what the screen shows after each (read through pyte, a terminal emulator:
ratatui redraws only the cells that change, so the raw output can't be
searched). Outside Pepper; with TUITOK_MPV at tools/mpv-silent nothing shows
or plays aloud.

  TUITOK_MPV="$PWD/tools/mpv-silent" \
    uv run --no-project --with pyte python -I tools/drive.py target/debug/tuitok [WAIT] [KEY:SECONDS ...]

WAIT is how long to let it fetch first (40 s); each KEY is pressed, then
SECONDS waited (`j:6 space:2`; `space` is a space). `q` is pressed last.
"""

import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time

import pyte

COLS, ROWS = 150, 45
KEYS = {"space": b" ", "down": b"\x1b[B", "up": b"\x1b[A"}


def main():
    program, rest = sys.argv[1], sys.argv[2:]
    wait = float(rest.pop(0)) if rest and rest[0].replace(".", "").isdigit() else 40.0
    screen = pyte.Screen(COLS, ROWS)
    stream = pyte.ByteStream(screen)
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, COLS * 9, ROWS * 18))
    env = {k: v for k, v in os.environ.items() if k != "PEPPER_PORT"}
    child = subprocess.Popen([program], stdin=slave, stdout=slave, stderr=slave, env=env,
                             start_new_session=True)
    os.close(slave)

    def pump(seconds):
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            ready, _, _ = select.select([master], [], [], 0.2)
            if ready:
                try:
                    stream.feed(os.read(master, 65536))
                except OSError:
                    return

    def show(label):
        print(f"--- {label}")
        for line in screen.display:
            if line.strip():
                print("  " + line.rstrip())

    pump(wait)
    show(f"after {wait:.0f} s")
    for step in rest:
        key, _, seconds = step.partition(":")
        os.write(master, KEYS.get(key, key.encode()))
        pump(float(seconds or 2))
        show(f"after {key}")
    os.write(master, b"q")
    pump(3)
    print("exit code:", child.wait(timeout=10))


if __name__ == "__main__":
    main()
