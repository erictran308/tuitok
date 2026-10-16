# tuitok: TikTok's For You feed in the terminal, inside Pepper

Research of 2026-10-16. Everything marked **tested** was run on this Mac that day; the scripts are in `research_notes/spikes/` (run with `uv run --no-project --with TikTokApi python -I <script>`).

## What works and what doesn't

| Way to get the feed | Result |
| --- | --- |
| Plain GET of `tiktok.com/api/recommend/item_list/?from_page=fyp` | **Tested.** HTTP 200, but `{"status_code": 0}` and no videos: the request needs signatures (`X-Bogus`, `msToken`) that only TikTok's own JavaScript makes. |
| TikTokApi (davidteather, Python), `trending.videos()` | **Tested.** 0 videos, no error, headless WebKit. It calls the same endpoint (`from_page: "fyp"`). |
| A headless browser opens `tiktok.com/foryou`, and we read the `recommend/item_list` answers the page asks for itself | **Tested, works.** 40 videos in WebKit, 37 in Chrome (even with `HeadlessChrome` in its user agent). Each has `video.playAddr`. Pressing ↓ in the page loads more. No login. |
| yt-dlp | Plays one TikTok video by its link (**tested**). No For You feed; its `tiktok:tag` and `tiktok:sound` are marked broken. |
| ProxiTok and other mirror sites | Most instances are gone; not worth building on. |
| TikTok's official APIs | Research API is for approved academics, Display API only shows your own videos. Neither has the feed. |

Downloading a video (**tested**):

- `playAddr` with no cookies: **403**.
- With only the `tt_chain_token` cookie from the browser, a referer of `https://www.tiktok.com/` and a Chrome user agent: **200**, from a plain HTTP client, **after the browser has closed**. 2–2.5 MB, 0.2–0.5 s each.
- The files are H.264 576×1024 with AAC, 5–12 s long.

The feed without login is TikTok's logged-out For You: popular videos for the country of your IP address, not personal.

## The plan

tuitok is a Rust TUI like tuitube, tuigram, tuimail and tuimeta: ratatui 0.30, crossterm, tokio, reqwest with rustls, the same nine themes.

1. **The feed: Lightpanda, else Chrome.** Both speak the DevTools protocol, so one client does both: Lightpanda over a WebSocket on a random 127.0.0.1 port (it has no pipe), Chrome over `--remote-debugging-pipe` as pepper-agent's `cdp.rs` does (hand-written, no crates). Turn on `Network`, note `responseReceived` events whose URL holds `recommend/item_list`, and on their `loadingFinished` ask `Network.getResponseBody`. Lightpanda loads more by itself (its page has no real layout, so every item looks visible); Chrome needs `Input.dispatchKeyEvent` ArrowDown. Refuse images, fonts, and fetch/XHR answers holding video or sound; make `play()` do nothing. A profile in a 0700 folder, kept between batches of one session (fewer repeats), deleted on exit. pepper-agent's local-only lock lives in `viewer/web.rs`, not `cdp.rs`, so the copy reaches TikTok.
2. **Downloads, ahead of time.** Keep `tt_chain_token` in memory only. Of `video.bitrateInfo`, take the smallest H.265 (`h265_hvc1`) rendition at least as tall as the box in pixels, else H.264; never `bytevc2` or a `www.tiktok.com` address. Download the next 3 whole (most are under 3 MB) with reqwest into a 0700 cache folder, capped in size, so j/k never waits.
3. **Playing: one pepper-play for the whole session.** App mode on a socketpair (fd 3), the token sent over IPC (`script-message-to main pepper-token`), started with `--idle=yes --keep-open=yes --loop-file=inf`. `j`/`k` send `loadfile <cached file> replace`, as pepper-agent does when it opens something new in the same box. A video too long to download ahead is streamed instead, its `Cookie` header set over IPC (`set_property http-header-fields`) just before its `loadfile`: never on a command line.
4. **The box: upright.** tuitube's `pepper::video_area` is always 16:9. tuitok works out 9:16 from the cell's pixel size (`cols = rows × cell_h × 9 / 16 / cell_w`), centred, with the author, caption, sound and counts beside it. Pepper keeps a picture's shape in any box (`contentsGravity = .resizeAspect`), so nothing in Pepper needs to change.
5. **Outside Pepper.** `PEPPER_PORT` missing, or `TMUX`/`STY`/`ZELLIJ` set: play in mpv's own window, as tuitube does, with the cover picture in the TUI.

Reused from tuitube: `pepper.rs` (detection, token, place/remove), `player.rs` (process, IPC, quitting), `images.rs` (covers and avatars through ratatui-image), `tools.rs` (programs by absolute path only), `theme.rs`, the `--demo` and `--check` patterns, its security rules.

### Rough time, by step

| Step | Time |
| --- | --- |
| Project, themes, `--check`, copied modules | 30 min |
| Feed through Chrome (`cdp.rs` + response bodies + paging) | 2–3 h |
| Downloads and cache | 1 h |
| Player, j/k, the 9:16 box | 1–2 h |
| Side panel, cover pictures, `--demo`, README | 2–3 h |
| **First version that scrolls** | **about 1–1.5 days** |

## What it costs, measured

Measured on 2026-10-16 on this Mac (Apple M5 Pro, 24 GB, macOS 27.2, Chrome 156), the way Pepper's own bench measures (`pepper-play/bench/measure.py`): CPU time from the kernel as a share of one core, and memory as the footprint Activity Monitor shows, summed over all of Chrome's processes. 60 s each, sampled every second, with ArrowDown to the next video every 10 s. Scripts: `research_notes/spikes/measure_chrome.py`, `bench_pepper.sh`, `tui_mem.py`; raw numbers in `research_notes/spikes/results/`.

| What runs | CPU, mean (95th percentile) | Memory, mean / peak |
| --- | --- | --- |
| **TikTok in a Chrome window**, videos playing | **49.1%** (74.9%) | **1,524 / 2,263 MB** |
| Headless Chrome as it is (videos play unseen) | 34.1% (60.2%) | 1,011 / 1,226 MB |
| Headless Chrome, lean: video, sound, images and fonts refused, `play()` does nothing | 13.5% (41.3%) | 654 / 876 MB |
| pepper-play, a 576×1024 TikTok clip in a 612×1058 px box (VideoToolbox) | 10.1% (11.0%) | 219 MB |
| Pepper itself while that plays | 5.1% | the terminal you have open anyway |
| A ratatui app on screen (`tuitube --demo`, kitty images) | 2.5% | 40 MB |

Fetching a batch with lean Chrome, then closing it (ArrowDown every 2 s, until 30 videos or 40 s):

| Batch | Open | CPU time | Peak memory | Videos |
| --- | --- | --- | --- | --- |
| 1, new profile | 42 s | 11.0 s | 1,015 MB | 11 |
| 2, same profile | 44 s | 8.9 s | 965 MB | 4, none repeated from batch 1 |
| 3, another new profile | 42 s | 10.5 s | 991 MB | 12, 5 repeated from batch 1 |

**So:** about 1 s of CPU per video fetched. tuitok watching, with Chrome closed between batches: player + TUI + Pepper ≈ **13–18% of a core and ~260 MB**. With fetching averaged in (a video every 20 s): **~18–23%**, plus ~1 GB for ~40 s per ~11 videos. That's a third to two-fifths of the Chrome window's CPU, and a sixth of its memory most of the time. With the lean browser left open instead: ~27–31% and ~910 MB, all the time. **Chrome is the expensive part: open it only to fetch, and keep its profile between batches.**

Caveats:

- The Chrome window played some videos at 1080×1920; the pepper-play run was 576×1024. Pepper's README measures pepper-play at 1080p30 H.264 at 10.0% and 305 MB, so a sharper video costs tuitok little more.
- WindowServer (42–44%) and the graphics chip (38–55%) were busy in every run, the idle one too (44% and 38% with no Chrome), so they're left out for both sides. A Chrome window's compositing isn't in its numbers, which favours Chrome.
- A fresh Chrome profile, no extensions, one tab: the Chrome people use is heavier.
- One run each. Fetching was slower than hoped (~11 videos per 40 s, and only 4 in batch 2): worth a better way to page than ArrowDown.

## Lighter browsers, measured

Same machine and method as above. Each engine opened TikTok with images, fonts, video and sound refused and `play()` doing nothing, scrolled (ArrowDown every 2 s) until 20 videos or 40 s, then closed. Two rounds, order turned; both rounds shown. Script: `research_notes/spikes/measure_engines.py`, numbers in `results/engines.jsonl`. Every engine's `tt_chain_token` downloaded a video (HTTP 200).

| Engine | Videos | Open | CPU time | Peak memory | Processes |
| --- | --- | --- | --- | --- | --- |
| **Lightpanda** 1.1.0 nightly | **28, 27** | **14 s** | **3.4, 3.5 s** | **288, 286 MB** | 1 |
| Chrome Headless Shell 156 (old headless) | 10, 13 | 44, 42 s | 8.6, 8.6 s | 530, 495 MB | 4 |
| Chrome 156 (new headless) | 13, 13 | 42 s | 9.9, 11.0 s | 924, 928 MB | 7 |
| WebKit (Playwright's) | 13, 13 | 43 s | 11.0, 11.2 s | 832, 950 MB | 5 |
| Firefox 157 (Playwright's) | didn't start here: "Could not find profile folder" | | | | |

- **Lightpanda** costs about 0.13 s of CPU per video against Chrome's 0.8, and a third of its memory, and it got its videos in 14 s without a key press. At a video every 20 s, fetching with it averages under 1% of a core (Chrome's: ~5%), with ~290 MB for 14 s per ~27 videos. TikTok serves it although it calls itself `Lightpanda/1.0` (it can't pretend otherwise). It's AGPL-3.0 (fine as a separate program, never linked), young (1.0 shipped 2026-10-02), sends telemetry unless `LIGHTPANDA_DISABLE_TELEMETRY=true`, and serves its DevTools on a 127.0.0.1 port that any local program can reach while it runs. Installed with `brew install lightpanda-io/browser/lightpanda`.
- **Chrome Headless Shell** is the next lightest, but it's a separate download (Chrome for Testing) where Chrome is usually installed already.
- **Firefox** has had no DevTools protocol since 141 (WebDriver BiDi instead), and its published numbers aren't lighter.
- **No browser at all** (computing X-Bogus, X-Gnarly and msToken in Rust or Python): no maintained project gets the logged-out For You that way; msToken comes from the browser's canvas, WebGL and audio fingerprint, and the signer changes every 1–3 months.
- Chrome flags worth passing when it's the fallback: `--blink-settings=imagesEnabled=false`, `--mute-audio`, `--disable-features=site-per-process,IsolateOrigins` with `--renderer-process-limit=1`, `--in-process-gpu`. Not `--single-process` (crashes) or `--js-flags=--jitless`.

## How TikTok sends videos, and what mpv and Pepper need

**Progressive MP4, not chunks.** A For You video is one MP4 file in several renditions (`video.bitrateInfo`), on `v16-webapp-prime.tiktok.com` and `v19-webapp-prime.tiktok.com`; no HLS (m3u8) or DASH anywhere in the feed's answer. TikTok's own web player fetches pieces of that file into MediaSource, which is why the page shows `blob:` addresses, but the file itself is whole. TikTok LIVE is different (FLV and HLS pull addresses), and so are YouTube (separate video and sound, through yt-dlp), Instagram Reels (MP4 or DASH) and Twitch (HLS).

One video's renditions (its default `playAddr` is the H.264 one):

| Rendition | Codec | Size | File |
| --- | --- | --- | --- |
| `normal_540_0` | H.264 | 576×1024 | 2.52 MB |
| `adapt_lowest_1080_1` | H.265 | 1080×1920 | 1.48 MB |
| `adapt_lower_720_1` | H.265 | 720×1280 | 0.78 MB |
| `adapt_540_1` | H.265 | 576×1024 | 0.64 MB |
| `comet_adapt_lowest_540_1` | H.265 | 576×1024 | 0.43 MB |

So H.265 is sharper and lighter at once: 1080p in 60% of the H.264 file's bytes.

**mpv plays all of it** (tested with mpv 0.41, one idle mpv and `loadfile replace`, timed to its first frame; `results/stream.jsonl`):

| Played | First frame |
| --- | --- |
| A file downloaded first | 0.23 s |
| Streamed, `tt_chain_token` as a `Cookie` header set over IPC | 0.89 s (first), 0.2–0.5 s (the other renditions) |
| Streamed with no cookie | refused ("loading failed") |
| Every H.265 rendition, 540p to 1080p | 0.2–0.4 s, decoded on the graphics chip (VideoToolbox) |
| Apple's HLS test stream (m3u8) | plays (11.8 s, mpv weighing its many variants) |

**Nothing to update in mpv or Pepper.** pepper-play loads Homebrew's `libmpv.2.dylib` at run time (`pepper-play/src/mpv.rs`), the same mpv 0.41 and FFmpeg as `mpv`, so it plays whatever mpv plays: MP4 over HTTPS, H.265, HLS, FLV. Pepper only shows pepper-play's frames and never sees where they came from. The one format missing is DASH (`.mpd`): Homebrew's FFmpeg is built without libxml2, so it has no `dash` demuxer. TikTok doesn't use it, and YouTube's separate streams reach mpv through yt-dlp as EDL, not as `.mpd`.

**Streaming or downloading?** Downloading the next 3 ahead makes every j/k start in 0.23 s, and a TikTok rendition is usually under 1 MB as H.265. Streaming costs 0.2–0.9 s a video and needs the cookie in the player. So: download ahead, and stream only a video too long to download (minutes-long TikToks exist), its header set over IPC.

## Decided

- **Own repo**, like tuitube. The Pepper PR comes later and only advertises tuitok in Pepper's README, as tuitube's paragraph does. Nothing in Pepper changes.
- **Logged out** (2026-10-16): TikTok's logged-out For You, no account, nothing to flag.
- **Lightpanda first, Chrome as the fallback**, both through one DevTools client; the browser runs only while a batch is fetched.
- **No change to mpv or Pepper**: videos are MP4 files, downloaded ahead as H.265 where TikTok has it.
- **Terms**: reading TikTok this way is against its terms. The README carries tuimeta's kind of warning and tuitube's personal-use disclaimer.

## Risks

- **TikTok changes its web app.** The endpoint names (`recommend/item_list`) and the page's keys can change. Match by URL substring, never rebuild the requests ourselves, and keep a live `#[ignore]`d test that fails loudly.
- **Headless detection gets stricter.** Today headless Chrome works as is. If it stops: replace `HeadlessChrome` in the user agent (pepper-agent already does), then a real Chrome window kept off screen.
- **TikTok stops serving Lightpanda.** It names itself in every request. When a Lightpanda batch comes back empty, fetch with Chrome and say so on the status line.
- **Rate limits.** Load more only when the reader is near the end of what's downloaded.

## Prior art

- tuitube (`../tuitube`): the model for everything but the feed. Its Shorts are a 16:9 grid from your subscriptions' RSS; no one-at-a-time scroller, no prefetch.
- [irevenko/tiktik](https://github.com/irevenko/tiktik): a Go TikTok terminal client from 2021, scraping pages. Old.
- [davidteather/TikTok-Api](https://github.com/davidteather/TikTok-Api): Python, Playwright inside. Got 0 videos here.
- [remorses/tuitube](https://github.com/remorses/tuitube): a different tuitube, a yt-dlp downloader on Termcast. Not related.

## Sources

- [TikTok-Api](https://github.com/davidteather/TikTok-Api) and its [trending.py](https://github.com/davidteather/TikTok-Api/blob/main/TikTokApi/api/trending.py) (`recommend/item_list`, `from_page: "fyp"`)
- [yt-dlp supported sites](https://github.com/yt-dlp/yt-dlp/blob/master/supportedsites.md)
- [Reverse Engineering TikTok's Web Protection](https://autodev.blog/posts/tiktok-research-article/) (X-Bogus made by `byted_acrawler.frontierSign()`)
- [gallery-dl #8969](https://github.com/mikf/gallery-dl/issues/8969) (removing parameters from `item_list` returns nothing)
- [TikHub docs](https://tikhub.apifox.cn/186826073e0) (`tt_chain_token` or 403)
- [ProxiTok](https://github.com/pablouser1/ProxiTok), [Techlore thread](https://discuss.techlore.tech/t/looking-for-alternatives-for-social-media-frontend-services/10161)
- [mpv video outputs](https://github.com/mpv-player/mpv/blob/master/DOCS/man/vo.rst) (kitty, sixel, tct: what we'd use outside Pepper if not mpv's window)
- `../pepper/pepper-play/README.md`, `../pepper/pepper-agent/src/cdp.rs`, `../tuitube/src/pepper.rs`, `../tuitube/src/player.rs`
