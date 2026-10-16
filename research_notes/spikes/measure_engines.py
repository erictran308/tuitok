"""Which headless browser fetches TikTok's For You for the least: Chrome
(new headless), Chrome Headless Shell, WebKit, Firefox and Lightpanda.

Each run: start the engine, open tiktok.com/foryou with images, fonts,
video and sound refused and play() doing nothing, press ArrowDown every 2 s
until WANT videos or LIMIT s, then close. Measured as measure_chrome.py
does: CPU time and footprint of every process the engine started (its
process tree, and for Playwright's own builds any process run from their
folder, which catches WebKit's XPC services), once a second. ROUNDS rounds,
the engines' order turned each round.

The first videos' `video` fields (shape only, no addresses) go to
OUT-video-fields.json; with $TUITOK_SAMPLE set, one item's addresses and
the tt_chain_token go there (0600), for the streaming test.

  LIGHTPANDA=PATH uv run --no-project --with playwright python -I measure_engines.py OUT.jsonl [ENGINE...]
"""

import asyncio
import ctypes
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from urllib.parse import urlsplit

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from measure_chrome import LIBC, PROFILES, URL, Sampler, roots_of, unique, windowserver  # noqa: E402
from playwright.async_api import async_playwright  # noqa: E402

WANT = 20
LIMIT = 40
ROUNDS = int(os.environ.get("TUITOK_ROUNDS", 2))
VIEW = {"width": 1280, "height": 900}
CACHE = os.path.expanduser("~/Library/Caches/ms-playwright")
VIDEO_URL = re.compile(r"/video/tos/|mime_type=(video|audio)")
NO_PLAY = "HTMLMediaElement.prototype.play = function () { return Promise.resolve(); };"
WS = windowserver()


def pid_path(pid):
    buf = ctypes.create_string_buffer(4096)
    n = LIBC.proc_pidpath(pid, buf, ctypes.sizeof(buf))
    return buf.raw[:n].decode(errors="replace") if n > 0 else ""


def all_pids():
    out = subprocess.run(["/bin/ps", "-A", "-o", "pid="], capture_output=True, text=True).stdout
    return {int(x) for x in out.split()}


def from_folder(prefix, before):
    return lambda: {p for p in all_pids() - before if pid_path(p).startswith(prefix)}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


# --- engines: each returns (context, close, roots or None, folder or None) ---

async def chrome(p, profile):
    ctx = await p.chromium.launch_persistent_context(profile, channel="chrome", headless=True,
                                                     viewport=VIEW, args=["--mute-audio"])
    return ctx, ctx.close, None, None


async def shell(p, profile):
    # headless=True without a channel is Playwright's chromium-headless-shell.
    ctx = await p.chromium.launch_persistent_context(profile, headless=True, viewport=VIEW,
                                                     args=["--mute-audio"])
    return ctx, ctx.close, None, f"{CACHE}/chromium_headless_shell-"


async def webkit(p, profile):
    ctx = await p.webkit.launch_persistent_context(profile, headless=True, viewport=VIEW)
    return ctx, ctx.close, None, f"{CACHE}/webkit-"


async def firefox(p, profile):
    ctx = await p.firefox.launch_persistent_context(profile, headless=True, viewport=VIEW)
    return ctx, ctx.close, None, f"{CACHE}/firefox-"


async def lightpanda(p, profile):
    port = free_port()
    log = open(os.path.join(profile, "lightpanda.log"), "wb")
    proc = subprocess.Popen([os.environ["LIGHTPANDA"], "serve", "--host", "127.0.0.1",
                             "--port", str(port), "--disable-metrics"],
                            stdin=subprocess.DEVNULL, stdout=log, stderr=log,
                            env=dict(os.environ, LIGHTPANDA_DISABLE_TELEMETRY="true"))
    for _ in range(100):
        try:
            socket.create_connection(("127.0.0.1", port), 0.1).close()
            break
        except OSError:
            await asyncio.sleep(0.1)
    # http:// (it reads /json/version first): ws:// straight away hangs.
    browser = await p.chromium.connect_over_cdp(f"http://127.0.0.1:{port}")
    ctx = browser.contexts[0] if browser.contexts else await browser.new_context()

    async def close():
        try:
            await browser.close()
        finally:
            proc.terminate()
            proc.wait(5)

    return ctx, close, [proc.pid], None


ENGINES = {"chrome": chrome, "shell": shell, "webkit": webkit, "firefox": firefox,
           "lightpanda": lightpanda}


# --- a run -------------------------------------------------------------------

async def lean(ctx, blocked):
    await ctx.add_init_script(NO_PLAY)

    async def handle(route):
        req = route.request
        if req.resource_type in ("media", "image", "font") or (
                req.resource_type in ("fetch", "xhr") and VIDEO_URL.search(req.url)):
            blocked[0] += 1
            await route.abort()
        else:
            await route.continue_()

    await ctx.route("**/*", handle)


CHROME_UA = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
             "(KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36")


def play_url(item):
    pa = (item.get("video") or {}).get("playAddr")
    if isinstance(pa, list):
        pa = pa[0].get("src") if pa else None
    return pa


def try_download(url, token):
    """Whether this engine's tt_chain_token gets the video from a plain HTTP
    client, as tuitok's would: (status, bytes)."""
    if not url:
        return ("no playAddr", 0)
    req = urllib.request.Request(url, headers={"User-Agent": CHROME_UA, "Referer": "https://www.tiktok.com/",
                                               "Cookie": f"tt_chain_token={token}"})
    try:
        with urllib.request.urlopen(req, timeout=20) as resp:
            return (resp.status, len(resp.read()))
    except urllib.error.HTTPError as e:
        return (e.code, 0)
    except Exception as e:  # noqa: BLE001
        return (type(e).__name__, 0)


def host(url):
    return urlsplit(url).hostname if url else None


def video_shape(it):
    v = it.get("video") or {}
    raw = json.dumps(it)
    out = {"keys": sorted(v), "size": [v.get("width"), v.get("height")], "duration": v.get("duration"),
           "format": v.get("format"), "codecType": v.get("codecType"),
           "playAddr_host": host(v.get("playAddr")), "downloadAddr_host": host(v.get("downloadAddr")),
           "mentions": {w: w in raw for w in ("m3u8", ".mpd", "dash", "hls")}, "bitrateInfo": []}
    for b in v.get("bitrateInfo") or []:
        pa = b.get("PlayAddr") or {}
        urls = pa.get("UrlList") or []
        out["bitrateInfo"].append({"GearName": b.get("GearName"), "Bitrate": b.get("Bitrate"),
                                   "CodecType": b.get("CodecType"), "QualityType": b.get("QualityType"),
                                   "size": [pa.get("Width"), pa.get("Height")], "DataSize": pa.get("DataSize"),
                                   "urls": len(urls), "hosts": sorted({host(u) for u in urls})})
    return out


def step(name, what, t0=[time.monotonic()]):  # noqa: B006 - start time kept between calls
    if os.environ.get("TUITOK_DEBUG"):
        print(f"  {time.monotonic() - t0[0]:6.1f}s {name}: {what}", file=sys.stderr, flush=True)


async def run(p, name, out):
    items, blocked = [], [0]
    profile = tempfile.mkdtemp(prefix=f"tuitok-{name}-", dir=PROFILES)
    before = all_pids()
    t0 = time.monotonic()
    r = {"engine": name}
    try:
        ctx, close, roots, folder = await ENGINES[name](p, profile)
    except Exception as e:  # noqa: BLE001 - reported
        shutil.rmtree(profile, ignore_errors=True)
        r["error"] = f"launch: {e}"[:300]
        return r
    r["launch_s"] = round(time.monotonic() - t0, 1)
    step(name, "launched")
    try:
        sampler = Sampler(roots or roots_of(profile), WS,
                          extra=from_folder(folder, before) if folder else None)
        try:
            await lean(ctx, blocked)
        except Exception as e:  # noqa: BLE001
            r["lean_error"] = str(e)[:200]
        step(name, f"lean set; pages already open: {len(ctx.pages)}")
        # Lightpanda's own first page doesn't navigate: a new one does.
        page = ctx.pages[0] if ctx.pages and name != "lightpanda" else await ctx.new_page()
        step(name, "page")

        async def on_response(resp):
            if "item_list" in resp.url and ("recommend" in resp.url or "preload" in resp.url):
                try:
                    items.extend((await resp.json()).get("itemList") or [])
                except Exception:  # noqa: BLE001
                    pass

        page.on("response", on_response)
        try:
            await page.goto(URL, wait_until="domcontentloaded", timeout=45000)
        except Exception as e:  # noqa: BLE001
            r["goto_error"] = str(e)[:200]
        step(name, f"goto returned {r.get('goto_error', '')[:80]}")
        deadline = time.monotonic() + 30
        while not items and time.monotonic() < deadline:
            await asyncio.sleep(0.25)
        if items:
            r["first_batch_s"] = round(time.monotonic() - t0, 1)
            deadline = time.monotonic() + LIMIT
            step(name, f"first items: {len(items)}")
            pressing = True
            while len(unique(items)) < WANT and time.monotonic() < deadline:
                if pressing:
                    try:
                        await asyncio.wait_for(page.keyboard.press("ArrowDown"), 5)
                    except Exception:  # noqa: BLE001 - Lightpanda takes no key events
                        pressing = False
                        r["no_keys"] = True
                await asyncio.sleep(2)
        else:
            try:
                r["title"] = (await asyncio.wait_for(page.title(), 5))[:80]
            except Exception:  # noqa: BLE001
                pass
        try:
            step(name, f"done waiting: {len(unique(items))} videos")
            cookies = await asyncio.wait_for(ctx.cookies("https://www.tiktok.com"), 10)
            step(name, f"cookies: {len(cookies)}")
        except Exception:  # noqa: BLE001
            cookies = []
        r.update(sampler.stop())
    finally:
        try:
            await asyncio.wait_for(close(), 15)
        except Exception:  # noqa: BLE001
            pass
        shutil.rmtree(profile, ignore_errors=True)
    token = next((c["value"] for c in cookies if c["name"] == "tt_chain_token"), None)
    r.update(videos=len(unique(items)), open_s=round(time.monotonic() - t0, 1), refused=blocked[0],
             chain_token=bool(token))
    if items and token:
        r["download"] = await asyncio.to_thread(try_download, play_url(items[0]), token)
    if items and not os.path.exists(out + "-video-fields.json"):
        with open(out + "-video-fields.json", "w") as f:
            json.dump([video_shape(it) for it in items[:3]], f, indent=1)
    sample = os.environ.get("TUITOK_SAMPLE")
    if items and sample and not os.path.exists(sample):
        token = next((c["value"] for c in cookies if c["name"] == "tt_chain_token"), None)
        fd = os.open(sample, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w") as f:
            json.dump({"tt_chain_token": token, "items": [{"id": it["id"], "video": it.get("video")}
                                                          for it in items[:5]]}, f)
    return r


async def main():
    out = sys.argv[1]
    names = sys.argv[2:] or list(ENGINES)
    results = []
    async with async_playwright() as p:
        for k in range(ROUNDS):
            order = names[k % len(names):] + names[:k % len(names)]
            for name in order:
                try:
                    r = await asyncio.wait_for(run(p, name, out), 150)
                except asyncio.TimeoutError:
                    r = {"engine": name, "error": "no result in 150 s"}
                r.update(round=k + 1, when=time.strftime("%Y-%m-%dT%H:%M:%S"))
                results.append(r)
                with open(out, "a") as f:
                    f.write(json.dumps(r) + "\n")
                print(json.dumps(r), flush=True)
                await asyncio.sleep(3)
    print("\nengine       videos  first(s)  open(s)  cpu-s  footprint peak/mean MB  procs")
    for r in results:
        if "error" in r:
            print(f"{r['engine']:<12} ERROR {r['error'][:90]}")
            continue
        print(f"{r['engine']:<12} {r['videos']:>6}  {r.get('first_batch_s') or '-':>8}  {r['open_s']:>7}"
              f"  {r.get('cpu_s_since_launch', 0):>5}  {r.get('footprint_peak_mb', 0):>9}/{r.get('footprint_mean_mb', 0):<6}"
              f"        {r.get('processes', 0)}")


if __name__ == "__main__":
    asyncio.run(main())
