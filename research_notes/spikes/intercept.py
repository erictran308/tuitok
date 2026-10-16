import asyncio, json
from playwright.async_api import async_playwright

async def main():
    items, hits = [], []
    async with async_playwright() as p:
        b = await p.webkit.launch(headless=True)
        page = await b.new_page(viewport={"width": 1280, "height": 900})
        async def on_resp(r):
            if "/api/" in r.url and ("item_list" in r.url or "recommend" in r.url):
                try:
                    body = await r.json()
                    got = body.get("itemList") or []
                    hits.append((r.url.split("?")[0], r.status, len(got)))
                    items.extend(got)
                except Exception as e:
                    hits.append((r.url.split("?")[0], r.status, f"non-json: {e.__class__.__name__}"))
        page.on("response", on_resp)
        await page.goto("https://www.tiktok.com/foryou", wait_until="domcontentloaded", timeout=45000)
        await page.wait_for_timeout(8000)
        for _ in range(3):
            await page.keyboard.press("ArrowDown"); await page.wait_for_timeout(2500)
        print("title:", await page.title())
        await b.close()
    for h in hits: print("hit:", h)
    for it in items[:6]:
        print("-", it.get("id"), "@" + it.get("author", {}).get("uniqueId", "?"), "|", (it.get("desc") or "")[:50],
              "| playAddr" if it.get("video", {}).get("playAddr") else "")
    print("total items:", len(items))

asyncio.run(main())
