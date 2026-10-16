<div align="center">

# tuitok

**TikTok's For You, at the speed of your keyboard.**

A terminal TikTok client (TUI): one upright video at a time, `j` / `k` to scroll, playing inside [Pepper](https://github.com/erictran308/pepper).<br>
No account, no login, nothing kept after you quit.

</div>

> [!IMPORTANT]
> **For personal use only.** tuitok isn't made, endorsed or allowed by TikTok or ByteDance. It reads TikTok's web feed through a headless browser, which TikTok's terms don't permit. It never logs in. See the [disclaimer](#disclaimer).

## What it does

- **For You, one at a time.** The video in an upright box, its cover while it loads, and beside it the author, caption, sound, plays, likes, comments and shares, where it's at and a bar of how far. Each video loops, as on TikTok.
- **Starts at once.** The next few videos are downloaded while you watch, so `j` plays the next one straight away. Long ones stream instead.
- **In Pepper, inside the terminal.** In a [Pepper](https://github.com/erictran308/pepper) tab the video plays in the box itself, through Pepper's player; `f` fills the tab with it. In other terminals it plays in mpv's window, with the cover in the box.
- **Light.** A small headless browser ([Lightpanda](https://github.com/lightpanda-io/browser), else Chrome) opens TikTok's For You for a few seconds to fetch a batch, then closes: it never runs while you watch.
- **Keyboard only.** Arrows or vim keys.

## Why a terminal

Measured on an M5 Pro: TikTok in a Chrome window used 49% of a core and 1.5 GB. tuitok's parts, measured the same way: Pepper's player 10% and 220 MB, the app itself 2.5% and 40 MB, and Lightpanda 3.4 s of CPU and 290 MB for 14 s to fetch about 27 videos. The [report](reports/TikTok%20terminal%20client.md) has every number and how it was measured.

## Keys

| Key | |
| --- | --- |
| `j` or `↓` | the next video |
| `k` or `↑` | the one before |
| `space` | pause |
| `m` | mute |
| `h` `l` or `←` `→` | back or on 5 s |
| `f` | the video fills the tab (Pepper), or mpv's window the screen |
| `r` | fetch more now |
| `q` | quit |

## Get started

tuitok runs on macOS (and Linux, without Pepper). It needs mpv, which Pepper's player uses too, and Lightpanda for the feed (Chrome works as well):

```sh
brew install mpv lightpanda-io/browser/lightpanda
cargo install --path .     # from this folder
tuitok --check             # what it found: Pepper, its player, a browser, mpv
tuitok                     # in a Pepper tab, not inside tmux or herdr
```

`tuitok --demo` shows the screen with made-up videos: nothing is fetched or played. `tuitok --feed` fetches one batch and lists it.

Inside a multiplexer (tmux, screen, zellij, herdr) Pepper can't be told where its box goes, so videos play in mpv's window there; the panel says so.

## Privacy and security

- **No account.** tuitok never logs in. The feed is TikTok's logged-out For You: popular videos for your country, not yours.
- **Nothing kept.** Videos are downloaded into a folder of that run's only, readable only by you, and it's deleted when tuitok quits (or by the next run, if it crashed); covers stay in memory. No history, no settings file yet.
- **The browser runs only while it fetches**, with a profile of that run's, pictures, video and sound refused, and Lightpanda's telemetry off.
- **Only TikTok's own hosts** are fetched from, over https; redirects elsewhere are refused. Text from TikTok is cleaned before it's shown, pictures are decoded only as JPEG within size limits, and downloaded videos must be MP4s.
- **Nothing private on a command line.** The guest cookie TikTok's videos need stays in tuitok, and goes to the player only over its private socket.

## Disclaimer

tuitok is an unofficial, independent hobby project. It is **not** affiliated with, endorsed by, sponsored by, or connected to TikTok or ByteDance. Reading TikTok this way breaks TikTok's terms of service. **You use tuitok entirely at your own risk**; it is provided "as is", and its authors are **not liable** for anything that comes of using it. For personal use only: do not use tuitok to download or redistribute videos, for automation, scraping, or any commercial purpose.

## License

MIT
