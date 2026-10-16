"""The feed through Lightpanda with raw DevTools calls, no Playwright: the
exact sequence tuitok's cdp.rs/browser.rs make, to see which calls
Lightpanda 1.0.1 answers. Prints each call's result or error.

  LIGHTPANDA=PATH uv run --no-project --with websockets python -I lp_cdp.py
"""

import asyncio
import json
import os
import socket
import subprocess
import time
import urllib.request

import websockets

URL = "https://www.tiktok.com/foryou"
WANT = 30


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def main():
    port = free_port()
    proc = subprocess.Popen([os.environ["LIGHTPANDA"], "serve", "--host", "127.0.0.1", "--port", str(port),
                             "--disable-metrics"], stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, env={"LIGHTPANDA_DISABLE_TELEMETRY": "true",
                                                            "HOME": os.environ["HOME"]})
    t0 = time.monotonic()
    for _ in range(100):
        try:
            version = json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/version", timeout=1))
            break
        except OSError:
            await asyncio.sleep(0.05)
    print(f"{time.monotonic() - t0:.2f}s up:", version.get("Browser"), version.get("webSocketDebuggerUrl"))
    ids, pending, events = iter(range(1, 10**6)), {}, asyncio.Queue()
    async with websockets.connect(version["webSocketDebuggerUrl"], max_size=64 << 20) as ws:
        async def reader():
            async for raw in ws:
                msg = json.loads(raw)
                if "id" in msg and msg["id"] in pending:
                    pending.pop(msg["id"]).set_result(msg)
                else:
                    await events.put(msg)
        task = asyncio.create_task(reader())

        async def call(method, params=None, session=None, show=True):
            i = next(ids)
            fut = asyncio.get_running_loop().create_future()
            pending[i] = fut
            msg = {"id": i, "method": method, "params": params or {}}
            if session:
                msg["sessionId"] = session
            await ws.send(json.dumps(msg))
            try:
                r = await asyncio.wait_for(fut, 15)
            except asyncio.TimeoutError:
                r = {"error": {"message": "no answer in 15 s"}}
            if show:
                print(f"{time.monotonic() - t0:6.2f}s {method}:", "ERROR " + json.dumps(r["error"]) if "error" in r
                      else json.dumps(r.get("result"))[:120])
            return r.get("result"), r.get("error")

        target, _ = await call("Target.createTarget", {"url": "about:blank"})
        attached, _ = await call("Target.attachToTarget", {"targetId": target["targetId"], "flatten": True})
        s = attached["sessionId"]
        await call("Page.enable", session=s)
        await call("Network.enable", session=s)
        await call("Page.addScriptToEvaluateOnNewDocument",
                   {"source": "HTMLMediaElement.prototype.play = function () { return Promise.resolve(); };"}, s)
        await call("Fetch.enable", {"patterns": [{"urlPattern": "*", "resourceType": t} for t in ("Image", "Font", "Media")]
                                    + [{"urlPattern": "*", "resourceType": t, "requestStage": "Response"} for t in ("Fetch", "XHR")]}, s)
        await call("Page.navigate", {"url": URL}, s)
        feed, items, refused, passed, kinds = set(), {}, 0, 0, {}
        deadline = time.monotonic() + 40
        pressed = None
        while len(items) < WANT and time.monotonic() < deadline:
            try:
                ev = await asyncio.wait_for(events.get(), 2)
            except asyncio.TimeoutError:
                if pressed is None:
                    _, pressed = await call("Input.dispatchKeyEvent", {"type": "keyDown", "key": "ArrowDown", "code": "ArrowDown", "windowsVirtualKeyCode": 40}, s)
                continue
            m, p = ev.get("method"), ev.get("params", {})
            kinds[m] = kinds.get(m, 0) + 1
            if m == "Fetch.requestPaused":
                ctype = next((h["value"] for h in p.get("responseHeaders") or [] if h["name"].lower() == "content-type"), "")
                if "responseStatusCode" not in p or ctype.startswith(("video/", "audio/")):
                    refused += 1
                    await call("Fetch.failRequest", {"requestId": p["requestId"], "errorReason": "BlockedByClient"}, s, show=False)
                else:
                    passed += 1
                    await call("Fetch.continueRequest", {"requestId": p["requestId"]}, s, show=False)
            elif m == "Network.responseReceived" and "item_list" in p["response"]["url"] and (
                    "recommend" in p["response"]["url"] or "preload" in p["response"]["url"]):
                feed.add(p["requestId"])
            elif m == "Network.loadingFinished" and p["requestId"] in feed:
                body, err = await call("Network.getResponseBody", {"requestId": p["requestId"]}, s, show=False)
                if err:
                    print("getResponseBody error:", err)
                    continue
                data = json.loads(body["body"]) if not body.get("base64Encoded") else {}
                for it in data.get("itemList") or []:
                    items[it["id"]] = it
                print(f"{time.monotonic() - t0:6.2f}s feed answer: {len(data.get('itemList') or [])} items, {len(items)} in all")
        cookies, err = await call("Network.getCookies", {"urls": ["https://www.tiktok.com"]}, s)
        if err:
            cookies, err = await call("Storage.getCookies", {}, None)
        names = sorted(c["name"] for c in (cookies or {}).get("cookies", []))
        print("cookies:", names)
        print("events:", kinds)
        print(f"refused {refused}, passed {passed}; {len(items)} videos in {time.monotonic() - t0:.1f}s; keys: {'none' if pressed is None else 'refused: ' + json.dumps(pressed)}")
        await call("Target.closeTarget", {"targetId": target["targetId"]})
        task.cancel()
    proc.terminate()
    proc.wait(5)


asyncio.run(main())
