import asyncio, time, sys
from playwright.async_api import async_playwright

OUT = sys.argv[1]

async def main():
    items = []
    async with async_playwright() as p:
        b = await p.webkit.launch(headless=True)
        ctx = await b.new_context(viewport={"width": 1280, "height": 900})
        page = await ctx.new_page()
        async def on_resp(r):
            if "recommend/item_list" in r.url:
                try: items.extend((await r.json()).get("itemList") or [])
                except Exception: pass
        page.on("response", on_resp)
        await page.goto("https://www.tiktok.com/foryou", wait_until="domcontentloaded", timeout=45000)
        await page.wait_for_timeout(8000)
        for it in items[:3]:
            url = it["video"]["playAddr"]
            t = time.time()
            r = await ctx.request.get(url, headers={"Referer": "https://www.tiktok.com/"})
            body = await r.body()
            path = f"{OUT}/{it['id']}.mp4"
            open(path, "wb").write(body)
            print(it["id"], "HTTP", r.status, f"{len(body)/1e6:.1f}MB", f"{time.time()-t:.1f}s", r.headers.get("content-type"))
        # same URL without browser cookies
        import urllib.request
        try:
            urllib.request.urlopen(urllib.request.Request(items[0]["video"]["playAddr"], headers={"User-Agent": "Mozilla/5.0", "Referer": "https://www.tiktok.com/"}), timeout=15)
            print("no-cookie fetch: OK")
        except Exception as e:
            print("no-cookie fetch:", e)
        await b.close()

asyncio.run(main())
