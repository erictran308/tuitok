import asyncio
from TikTokApi import TikTokApi

async def main():
    async with TikTokApi() as api:
        await api.create_sessions(num_sessions=1, sleep_after=3, browser="webkit", headless=True)
        n = 0
        async for v in api.trending.videos(count=5):
            d = v.as_dict
            print("-", d.get("id"), "@" + d.get("author", {}).get("uniqueId", "?"), "|", (d.get("desc") or "")[:50])
            n += 1
        print("got", n, "videos")

asyncio.run(main())
