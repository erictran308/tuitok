<div align="center">

# tuitok

**TikTok's For You, at the speed of your keyboard.**

A terminal TikTok client (TUI): one upright video at a time, `j` / `k` to scroll, playing inside [Pepper](https://github.com/erictran308/pepper).<br>
No account, no login.

</div>

> [!WARNING]
> **Early.** It plays: `tuitok` in a Pepper tab shows TikTok's For You one video at a time in an upright box beside its author, caption, sound and counts, the next few downloaded ahead so they start at once (elsewhere, videos play in mpv's window). Still to come: cover pictures, `--demo`, and polish ([the plan](reports/TikTok%20terminal%20client.md#the-plan)).

> [!IMPORTANT]
> **For personal use only.** tuitok isn't made, endorsed or allowed by TikTok or ByteDance. It reads TikTok's web feed through a headless browser, which TikTok's terms don't permit. It never logs in. See the [disclaimer](#disclaimer).

## How it works

- **The feed.** A light headless browser ([Lightpanda](https://github.com/lightpanda-io/browser), else Chrome) opens TikTok's For You for a few seconds, and tuitok keeps the videos it lists. Then the browser is closed: it never runs while you watch.
- **The videos.** Downloaded a few ahead, as H.265 where TikTok has it (sharper and smaller than its default), so the next one starts at once.
- **Playing.** Inside Pepper, in an upright box beside the caption, through Pepper's own player. In other terminals, in mpv's window.

Why a terminal: measured on an M5 Pro, TikTok in a Chrome window used 49% of a core and 1.5 GB. The parts of tuitok, measured the same way: Pepper's player 10% and 220 MB, a ratatui app 2.5% and 40 MB, and Lightpanda 3.4 s of CPU and 290 MB for 14 s to fetch ~27 videos. The [report](reports/TikTok%20terminal%20client.md) has every number and how it was measured.

## Keys

`j` or `↓` the next video, `k` or `↑` the one before, `space` pause, `m` mute, `h` `l` or `←` `→` back and on 5 s, `r` fetch more, `q` quit.

## Get started (for now)

```sh
brew install mpv lightpanda-io/browser/lightpanda
cargo run -- --check   # what tuitok found
cargo run              # in a Pepper tab
```

## Disclaimer

tuitok is an unofficial, independent hobby project. It is **not** affiliated with, endorsed by, sponsored by, or connected to TikTok or ByteDance. Reading TikTok this way breaks TikTok's terms of service. **You use tuitok entirely at your own risk**; it is provided "as is", and its authors are **not liable** for anything that comes of using it. For personal use only: do not use tuitok to download or redistribute videos, for automation, scraping, or any commercial purpose.

## License

MIT
