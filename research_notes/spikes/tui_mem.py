"""What a ratatui app of this family costs on screen: tuitube --demo in a
pseudo-terminal of 160x50 cells, sampled once a second for 20 s after 10 s.
CPU and footprint as pepper-play/bench/measure.py measures them.

  SCRATCH=DIR python3 -I tui_mem.py OUT.jsonl
"""

import ctypes
import fcntl
import json
import os
import pty
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

LIBC = ctypes.CDLL("/usr/lib/libSystem.B.dylib")


class _Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = _Timebase()
LIBC.mach_timebase_info(ctypes.byref(_tb))
TICK = _tb.numer / _tb.denom / 1e9


def rusage(pid):
    buf = (ctypes.c_uint64 * 64)()
    if LIBC.proc_pid_rusage(pid, 2, ctypes.byref(buf)) != 0:
        return None
    return ((buf[2] + buf[3]) * TICK, buf[9])


tuitube = shutil.which("tuitube")
master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 50, 160, 1600, 1700))
data = tempfile.mkdtemp(prefix="tuitok-tt-", dir=os.environ["SCRATCH"])
proc = subprocess.Popen([tuitube, "--demo"], stdin=slave, stdout=slave, stderr=slave,
                        env=dict(os.environ, TT_DATA_DIR=data), start_new_session=True)
os.close(slave)


def drain():
    while True:
        try:
            if not os.read(master, 65536):
                return
        except OSError:
            return


threading.Thread(target=drain, daemon=True).start()
time.sleep(10)
samples = []
for _ in range(21):
    samples.append((time.monotonic(), rusage(proc.pid)))
    time.sleep(1)
proc.terminate()
proc.wait(5)
shutil.rmtree(data, ignore_errors=True)
ok = [(t, u) for t, u in samples if u]
r = {"name": "tuitube --demo (ratatui, kitty images)",
     "cpu_mean": round(100 * (ok[-1][1][0] - ok[0][1][0]) / (ok[-1][0] - ok[0][0]), 2),
     "footprint_peak_mb": round(max(u[1] for _, u in ok) / 2**20, 1)}
with open(sys.argv[1], "a") as f:
    f.write(json.dumps(r) + "\n")
print(json.dumps(r))
