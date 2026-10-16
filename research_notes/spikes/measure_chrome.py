"""What TikTok's For You costs in Chrome, four ways:

  window    a Chrome window, as a person watches it
  headless  headless Chrome, everything loaded (videos play unseen)
  lean      headless Chrome with media, images and fonts refused
  batch     lean, open only while it fetches a batch, then closed; then a
            second batch with the same profile and one with a new profile,
            to see whether the feed goes on or repeats

CPU and memory are measured as ../../../pepper/pepper-play/bench/measure.py
does: CPU time from proc_pid_rusage (as a share of one core) and memory as
the footprint Activity Monitor shows, here summed over Chrome's whole
process tree, sampled once a second. WindowServer's CPU and the graphics
chip's use too. In window, headless and lean, ArrowDown goes to the next
video every 10 s, as someone scrolling would.

  uv run --no-project --with playwright python -I measure_chrome.py OUT.jsonl [NAME...]

Profiles go in $TUITOK_PROFILES (else the temp folder) and are deleted after.
"""

import asyncio
import ctypes
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time

from playwright.async_api import async_playwright

URL = "https://www.tiktok.com/foryou"
SECONDS = int(os.environ.get("TUITOK_SECONDS", 60))
WARMUP = 5
PRESS_EVERY = 10
PROFILES = os.environ.get("TUITOK_PROFILES") or tempfile.gettempdir()
VIDEOS = "() => [...document.querySelectorAll('video')].map(v => [v.paused, v.currentTime, v.videoWidth, v.videoHeight])"

# --- as in pepper-play/bench/measure.py ------------------------------------

LIBC = ctypes.CDLL("/usr/lib/libSystem.B.dylib")


class _Timebase(ctypes.Structure):
    _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]


_tb = _Timebase()
LIBC.mach_timebase_info(ctypes.byref(_tb))
TICK = _tb.numer / _tb.denom / 1e9
GPU = re.compile(rb'"Device Utilization %"=(\d+)')


def rusage(pid):
    """(cpu seconds, footprint bytes, start time), or None."""
    buf = (ctypes.c_uint64 * 64)()
    if LIBC.proc_pid_rusage(pid, 2, ctypes.byref(buf)) != 0:
        return None
    return ((buf[2] + buf[3]) * TICK, buf[9], buf[10])


def ps_cpu(pid):
    out = subprocess.run(["/bin/ps", "-o", "time=", "-p", str(pid)],
                         capture_output=True, text=True).stdout.strip()
    if not out:
        return None
    total = 0.0
    for part in out.split(":"):
        total = total * 60 + float(part)
    return total


def gpu_percent():
    out = subprocess.run(["/usr/sbin/ioreg", "-r", "-d", "1", "-w", "0", "-c", "IOAccelerator"],
                         capture_output=True).stdout
    found = GPU.findall(out)
    return float(found[0]) if found else None


def windowserver():
    ws = subprocess.run(["/usr/bin/pgrep", "-x", "-u", "_windowserver", "WindowServer"],
                        capture_output=True, text=True).stdout.split()
    return int(ws[0]) if len(ws) == 1 else None


# --- Chrome's processes ------------------------------------------------------

def tree(roots):
    out = subprocess.run(["/bin/ps", "-A", "-o", "pid=,ppid="], capture_output=True, text=True).stdout
    kids = {}
    for line in out.splitlines():
        pid, ppid = map(int, line.split())
        kids.setdefault(ppid, []).append(pid)
    found, todo = set(), list(roots)
    while todo:
        pid = todo.pop()
        if pid not in found:
            found.add(pid)
            todo.extend(kids.get(pid, []))
    return found


def roots_of(profile):
    """The processes started with this profile whose parent isn't one of them."""
    out = subprocess.run(["/bin/ps", "-A", "-o", "pid=,ppid=,args="], capture_output=True, text=True).stdout
    rows = {}
    for line in out.splitlines():
        parts = line.split(None, 2)
        if len(parts) == 3 and profile in parts[2]:
            rows[int(parts[0])] = int(parts[1])
    return [pid for pid, ppid in rows.items() if ppid not in rows]


class Sampler:
    def __init__(self, roots, ws, extra=None):
        self.roots, self.ws, self.extra = roots, ws, extra
        self.rows = []  # (time, {(pid, start): (cpu, footprint)}, windowserver cpu, gpu)
        self.done = threading.Event()
        self.take()
        self.thread = threading.Thread(target=self.loop, daemon=True)
        self.thread.start()

    def take(self):
        procs = {}
        for pid in tree(self.roots) | (self.extra() if self.extra else set()):
            u = rusage(pid)
            if u:
                procs[(pid, u[2])] = (u[0], u[1])
        self.rows.append((time.monotonic(), procs, ps_cpu(self.ws) if self.ws else None, gpu_percent()))

    def loop(self):
        while not self.done.wait(1.0):
            self.take()

    def stop(self):
        self.done.set()
        self.thread.join()
        self.take()
        return self.summary()

    def summary(self):
        rows = self.rows
        wall = rows[-1][0] - rows[0][0]
        per_s, total = [], 0.0
        for (t0, a, _, _), (t1, b, _, _) in zip(rows, rows[1:]):
            # A process new since the last sample: all its CPU time is new.
            d = sum(c - a[k][0] if k in a else c for k, (c, _) in b.items())
            total += d
            per_s.append(100 * d / (t1 - t0))
        foot = [sum(f for _, f in procs.values()) / 2**20 for _, procs, _, _ in rows]
        ws = [w for _, _, w, _ in rows if w is not None]
        gpu = [g for *_, g in rows if g is not None]
        return {
            "wall_s": round(wall, 1),
            "cpu_mean": round(100 * total / wall, 1),
            "cpu_p95": round(sorted(per_s)[int(0.95 * (len(per_s) - 1))], 1) if per_s else None,
            "footprint_mean_mb": round(sum(foot) / len(foot)),
            "footprint_peak_mb": round(max(foot)),
            "processes": max(len(p) for _, p, _, _ in rows),
            "windowserver_cpu": round(100 * (ws[-1] - ws[0]) / wall, 1) if len(ws) > 1 else None,
            "gpu_mean": round(sum(gpu) / len(gpu), 1) if gpu else None,
            "cpu_s_since_launch": round(sum(c for c, _ in rows[-1][1].values()), 1),
        }


# --- runs --------------------------------------------------------------------

async def launch(p, profile, headless, lean, items, blocked):
    args = ["--mute-audio", "--autoplay-policy=no-user-gesture-required"]
    kw = {}
    if headless:
        kw["viewport"] = {"width": 1280, "height": 900}
    else:
        args += ["--window-size=1280,900", "--window-position=80,60"]
        kw["no_viewport"] = True
    ctx = await p.chromium.launch_persistent_context(profile, channel="chrome", headless=headless,
                                                     args=args, **kw)
    page = ctx.pages[0] if ctx.pages else await ctx.new_page()

    async def on_response(r):
        if "item_list" in r.url and ("recommend" in r.url or "preload" in r.url):
            try:
                items.extend((await r.json()).get("itemList") or [])
            except Exception:
                pass

    page.on("response", on_response)
    if lean:
        # TikTok streams its videos with fetch/XHR into MediaSource (blob:
        # URLs), so refusing "Media" isn't enough: fetch and XHR answers are
        # looked at, and those holding video or sound refused. play() does
        # nothing, so whatever gets through is never decoded.
        await ctx.add_init_script("HTMLMediaElement.prototype.play = function () { return Promise.resolve(); };")
        cdp = await ctx.new_cdp_session(page)

        async def paused(ev):
            kind = next((h["value"] for h in ev.get("responseHeaders") or []
                         if h["name"].lower() == "content-type"), "")
            refuse = "responseStatusCode" not in ev or kind.startswith(("video/", "audio/"))
            try:
                if refuse:
                    blocked[0] += 1
                    await cdp.send("Fetch.failRequest", {"requestId": ev["requestId"],
                                                         "errorReason": "BlockedByClient"})
                else:
                    await cdp.send("Fetch.continueRequest", {"requestId": ev["requestId"]})
            except Exception:
                pass

        cdp.on("Fetch.requestPaused", lambda ev: asyncio.ensure_future(paused(ev)))
        await cdp.send("Fetch.enable", {"patterns": [
            *({"urlPattern": "*", "resourceType": t} for t in ("Media", "Image", "Font")),
            *({"urlPattern": "*", "resourceType": t, "requestStage": "Response"} for t in ("Fetch", "XHR")),
        ]})
    return ctx, page


async def wait_items(items, seconds):
    deadline = time.monotonic() + seconds
    while not items and time.monotonic() < deadline:
        await asyncio.sleep(0.25)
    return bool(items)


def unique(items):
    return list(dict.fromkeys(it["id"] for it in items if it.get("id")))


async def steady(p, name, headless, lean):
    """Load the feed, then measure SECONDS of scrolling."""
    items, blocked = [], [0]
    profile = tempfile.mkdtemp(prefix=f"tuitok-{name}-", dir=PROFILES)
    t0 = time.monotonic()
    ctx, page = await launch(p, profile, headless, lean, items, blocked)
    try:
        roots = roots_of(profile)
        await page.goto(URL, wait_until="domcontentloaded", timeout=45000)
        got = await wait_items(items, 30)
        first = round(time.monotonic() - t0, 1)
        await asyncio.sleep(WARMUP)
        sampler = Sampler(roots, WS)
        startup_cpu = round(sum(c for c, _ in sampler.rows[0][1].values()), 1)
        checks = playing = 0
        sizes = set()
        for n in range(1, SECONDS + 1):
            await asyncio.sleep(1)
            if n % 5 == 0:
                vids = await page.evaluate(VIDEOS)
                checks += 1
                on = [v for v in vids if not v[0] and v[1] > 0]
                playing += bool(on)
                sizes.update(f"{v[2]}x{v[3]}" for v in on)
            if n % PRESS_EVERY == 0:
                await page.keyboard.press("ArrowDown")
        r = sampler.stop()
    finally:
        await ctx.close()
        shutil.rmtree(profile, ignore_errors=True)
    r.update(name=name, feed=got, first_batch_s=first, startup_cpu_s=startup_cpu,
             videos=len(unique(items)), playing_share=f"{playing}/{checks}",
             playing_sizes=sorted(sizes), refused=blocked[0])
    return r


async def one_batch(p, profile, want=30, limit=40):
    """Lean, from launch to closed: what one batch of videos costs."""
    items, blocked = [], [0]
    t0 = time.monotonic()
    ctx, page = await launch(p, profile, True, True, items, blocked)
    try:
        sampler = Sampler(roots_of(profile), WS)
        await page.goto(URL, wait_until="domcontentloaded", timeout=45000)
        await wait_items(items, 30)
        deadline = time.monotonic() + limit
        while len(unique(items)) < want and time.monotonic() < deadline:
            await page.keyboard.press("ArrowDown")
            await asyncio.sleep(2)
        cookies = await ctx.cookies("https://www.tiktok.com")
        r = sampler.stop()
    finally:
        await ctx.close()
    ids = unique(items)
    seen, viewing = set(), 0
    for it in items:
        if it.get("id") in ids and it["id"] not in seen:
            seen.add(it["id"])
            viewing += (it.get("video") or {}).get("duration") or 0
    r.update(videos=len(ids), open_s=round(time.monotonic() - t0, 1), refused=blocked[0],
             viewing_s=viewing, chain_token=any(c["name"] == "tt_chain_token" for c in cookies),
             avg_cpu_over_viewing=round(100 * r["cpu_s_since_launch"] / viewing, 2) if viewing else None)
    return r, ids


async def batches(p):
    kept = tempfile.mkdtemp(prefix="tuitok-batch-", dir=PROFILES)
    fresh = tempfile.mkdtemp(prefix="tuitok-batch2-", dir=PROFILES)
    try:
        b1, ids1 = await one_batch(p, kept)
        await asyncio.sleep(3)
        b2, ids2 = await one_batch(p, kept)
        await asyncio.sleep(3)
        b3, ids3 = await one_batch(p, fresh)
    finally:
        shutil.rmtree(kept, ignore_errors=True)
        shutil.rmtree(fresh, ignore_errors=True)
    b1["name"] = "batch-1"
    b2.update(name="batch-2-same-profile", repeats_of_batch_1=len(set(ids1) & set(ids2)))
    b3.update(name="batch-3-new-profile", repeats_of_batch_1=len(set(ids1) & set(ids3)))
    return [b1, b2, b3]


async def idle(p):
    """No Chrome: what WindowServer and the graphics chip do anyway."""
    sampler = Sampler([], WS)
    await asyncio.sleep(20)
    r = sampler.stop()
    r["name"] = "idle"
    return r


WS = windowserver()
RUNS = {
    "idle": idle,
    "window": lambda p: steady(p, "window", headless=False, lean=False),
    "headless": lambda p: steady(p, "headless", headless=True, lean=False),
    "lean": lambda p: steady(p, "lean", headless=True, lean=True),
    "batch": batches,
}


async def main():
    out = sys.argv[1]
    names = sys.argv[2:] or list(RUNS)
    async with async_playwright() as p:
        for name in names:
            got = await RUNS[name](p)
            for r in got if isinstance(got, list) else [got]:
                r["when"] = time.strftime("%Y-%m-%dT%H:%M:%S")
                with open(out, "a") as f:
                    f.write(json.dumps(r) + "\n")
                print(json.dumps(r), flush=True)
            await asyncio.sleep(5)


if __name__ == "__main__":
    asyncio.run(main())
