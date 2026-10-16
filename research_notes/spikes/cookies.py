import asyncio, time, urllib.request
from playwright.async_api import async_playwright

async def main():
    items = []
    async with async_playwright() as p:
        b = await p.chromium.launch(channel="chrome", headless=True)
        ctx = await b.new_context()
        page = await ctx.new_page()
        async def on_resp(r):
            if "recommend/item_list" in r.url:
                try: items.extend((await r.json()).get("itemList") or [])
                except Exception: pass
        page.on("response", on_resp)
        await page.goto("https://www.tiktok.com/foryou", wait_until="domcontentloaded", timeout=45000)
        await page.wait_for_timeout(8000)
        for _ in range(2):
            await page.keyboard.press("ArrowDown"); await page.wait_for_timeout(2500)
        cookies = await ctx.cookies("https://www.tiktok.com")
        ua = await page.evaluate("navigator.userAgent")
        await b.close()   # browser gone: only the copied cookies are left
    print("cookie names:", sorted(c["name"] for c in cookies))
    jar = "; ".join(f"{c['name']}={c['value']}" for c in cookies)
    vids = [i for i in items if i.get("video", {}).get("playAddr")]
    for label, hdrs in [("all cookies", {"Cookie": jar}),
                        ("tt_chain_token only", {"Cookie": "; ".join(f"{c['name']}={c['value']}" for c in cookies if c["name"] == "tt_chain_token")}),
                        ("no cookies", {})]:
        for it in vids[:2]:
            t = time.time()
            req = urllib.request.Request(it["video"]["playAddr"], headers={"User-Agent": ua.replace("HeadlessChrome", "Chrome"), "Referer": "https://www.tiktok.com/", **hdrs})
            try:
                with urllib.request.urlopen(req, timeout=20) as r:
                    n = len(r.read()); print(f"{label:22} HTTP {r.status} {n/1e6:.1f}MB {time.time()-t:.1f}s")
            except Exception as e:
                print(f"{label:22} {e}")
    print("videos:", len(vids))

asyncio.run(main())
