import asyncio
from playwright.async_api import async_playwright

async def main():
    items, hits = [], []
    async with async_playwright() as p:
        b = await p.chromium.launch(channel="chrome", headless=True)
        ctx = await b.new_context(viewport={"width": 1280, "height": 900})
        page = await ctx.new_page()
        async def on_resp(r):
            if "item_list" in r.url:
                try:
                    got = (await r.json()).get("itemList") or []
                    hits.append((r.url.split("?")[0].split("/api/")[1], r.status, len(got))); items.extend(got)
                except Exception as e:
                    hits.append((r.url.split("?")[0], r.status, e.__class__.__name__))
        page.on("response", on_resp)
        await page.goto("https://www.tiktok.com/foryou", wait_until="domcontentloaded", timeout=45000)
        await page.wait_for_timeout(8000)
        for _ in range(3):
            await page.keyboard.press("ArrowDown"); await page.wait_for_timeout(2500)
        ok = 0
        for it in [i for i in items if i.get("video", {}).get("playAddr")][:2]:
            r = await ctx.request.get(it["video"]["playAddr"], headers={"Referer": "https://www.tiktok.com/"})
            ok += r.status == 200
        print("UA:", await page.evaluate("navigator.userAgent"))
        await b.close()
    for h in hits: print("hit:", h)
    print("total items:", len(items), "| downloads ok:", ok)

asyncio.run(main())
