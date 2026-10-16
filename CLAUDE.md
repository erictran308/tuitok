# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

tuitok: TikTok's For You feed in the terminal, one upright video at a time, for one person's own use, playing inside Pepper (`../pepper`: Ghostty with video boxes). Rust, ratatui, tokio, MIT. A sibling of tuitube (`../tuitube`, YouTube), tuigram and tuimeta: the same security posture, themes and way of starting programs. When in doubt, do what tuitube does; its CLAUDE.md is the long version of most rules here. The research behind every choice, with measured numbers, is `reports/TikTok terminal client.md`: read it before changing how the feed is fetched or how videos play. The scripts that measured it are in `research_notes/spikes/` (Python: `uv run --no-project --with playwright python -I <script>`; raw numbers in `results/`).

## Decided

- **Logged out.** No TikTok account, login or cookie import. The feed is TikTok's logged-out For You (popular videos for the IP's country); the only cookie is the guest `tt_chain_token` the browser is given, which the video addresses need (403 without).
- **Own repository.** Pepper's README gets a paragraph about tuitok later, as it has for tuitube; nothing in Pepper changes for it.
- **A browser only while fetching.** The feed (`/api/recommend/item_list/`) needs signatures only TikTok's own JavaScript makes (a plain request gets HTTP 200 and no videos), so a headless browser opens `tiktok.com/foryou` and tuitok reads the `item_list` answers the page asks for, then closes it. Lightpanda first (0.13 s of CPU per video, ~290 MB, one process), Chrome as the fallback (~0.8 s, ~930 MB). The browser is the expensive part of tuitok: it never stays open while you watch.
- **No change to mpv or Pepper.** Videos are single MP4 files, not HLS or DASH; pepper-play loads Homebrew's libmpv, which plays them (H.265 up to 1080×1920 on the graphics chip).

## Status

Steps 1 and 2 of 5 done: the project, and the feed. Next, in order (the report's "The plan" has each in detail):

3. **Downloads.** `cache.rs`: `Video::best` for the box's size in pixels, the next 3 downloaded ahead with reqwest into a 0700 folder, capped in size, with `tt_chain_token` (`Batch.token`, not kept by `App` yet). Some videos are long (up to an hour, 500 MB seen): above a size, stream instead (step 4).
4. **Playing.** `player.rs` and the rest of `pepper.rs` (token, `place`/`remove`) from tuitube: one pepper-play for the session, `loadfile replace` on j/k, `loop-file=inf`; mpv's window outside Pepper.
5. **The panel, cover pictures, `--demo`, README.**

**The feed, as built (step 2).** `feed::fetch_any` tries `feed::engines` in order (Lightpanda, then Chrome) and returns the first `Batch` with videos, its `note` saying why any before failed. `feed::fetch` starts a `browser::Browser`, reads one batch and closes it. `browser.rs`: Lightpanda (`lightpanda serve`, a free 127.0.0.1 port, `LIGHTPANDA_DISABLE_TELEMETRY=true`) reached at `ws://127.0.0.1:PORT/` (the WebSocket straight away works; Playwright's `ws://` connect hung, its `http://` one didn't), Chrome with `CHROME_FLAGS` on two pipes as fds 3 and 4 (`as_fd3_and_4`, made under `tools::starting`); both in their own process group, `killpg` on close and in `Drop` (before the child is waited for, so its pid can't be reused), `kill_on_drop` too. `cdp.rs`: one client for both (`Cdp::websocket`, `Cdp::pipe`), `call` waits `CALL_WAIT`, `send` doesn't wait, messages capped at `MAX_MESSAGE`. `feed::read`: `Target.createTarget` a new page (Lightpanda's own first page doesn't navigate), attach with `flatten`, `Network.enable`, Chrome's user agent without `HeadlessChrome`, `NO_PLAY`, `Fetch.enable` with `refused()`, navigate; then each `Fetch.requestPaused` answered (`answer_paused`), feed answers (`is_feed`) read on `loadingFinished`, until `WANT` videos, or `ENOUGH` and `QUIET`, or `LIMIT`; Chrome gets ArrowDown every `PRESS_AFTER` (Lightpanda loads more by itself and takes no keys); then `Network.getCookies` for `tt_chain_token` (`chain_token` checks it fits a header). `video.rs`: `Video::parse` (lenient; `VideoId` 1–25 digits; text through `text::one_line` / `text::lines`, capped; renditions from `bitrateInfo`, H.264 and H.265 only, addresses through `video_url_allowed`, covers through `image_url_allowed`) and `Video::best`. Measured: Lightpanda 1.0.1 got 28–30 videos in 26–41 s; Chrome 12 in 41 s.

## Commands

```sh
cargo build
cargo test                    # offline tests
cargo clippy --all-targets    # keep it warning-free
cargo fmt
cargo run -- --check          # what tuitok found: Pepper, pepper-play, Lightpanda, Chrome, mpv
cargo run -- --feed           # one batch of the feed, listed (--feed lightpanda | chrome for one)
cargo test -- --ignored live  # fetches a batch with each browser found
```

UI tests render into ratatui's `TestBackend` (`ui.rs`). Tests never start programs or write Pepper's sequences. Tests that will talk to TikTok or run a browser are `#[ignore]`d (`cargo test -- --ignored live`).

At run time: `brew install mpv lightpanda-io/browser/lightpanda`; Chrome is the fallback. `tools::Tools::find` resolves each program once to an absolute path (`TUITOK_PEPPER_PLAY`, `TUITOK_LIGHTPANDA`, `TUITOK_CHROME`, `TUITOK_MPV`, else `PEPPER_PLAY`, `PATH`'s absolute folders, Homebrew's, `/Applications`); a program is never started by bare name. `main` cuts `PATH` to its absolute folders before any thread starts.

## Security

As tuitube's, where it applies:

- Everything from TikTok is untrusted: captions, names, the feed's JSON (read leniently, every field optional), image bytes, addresses. Text is cleaned of control characters and capped before it's shown or stored; ids are checked where they come in; paths and errors printed go through `tools::shown`.
- Nothing private goes on a command line (`ps` shows them to every user). `tt_chain_token` stays in tuitok's memory. It goes to pepper-play or mpv only over their socket, as `http-header-fields`, for a video streamed rather than downloaded.
- Video and image addresses are fetched only over https from TikTok's hosts (seen: `v16-webapp-prime.tiktok.com`, `v19-webapp-prime.tiktok.com`); `www.tiktok.com` addresses in `UrlList` are skipped, as yt-dlp does.
- The browser: a profile in a 0700 folder in tuitok's data folder, kept between batches of a session, deleted on exit; images, fonts, and fetch/XHR answers holding video or sound refused (TikTok streams through MediaSource, so refusing "Media" alone stops nothing), and `play()` a no-op. Lightpanda runs with `LIGHTPANDA_DISABLE_TELEMETRY=true` and serves its DevTools on a random 127.0.0.1 port, which any local program can reach while it runs: it runs only for a batch. Chrome is driven over `--remote-debugging-pipe` (no port). Lightpanda is AGPL-3.0: a separate program, never linked or bundled.
- pepper-play and mpv are controlled over a socketpair passed as fd 3 (`--input-ipc-client=fd://3`): no socket file (and macOS caps those paths at 104 bytes), and mpv's IPC can run programs. Pepper's token goes to pepper-play only over that socket; `OSC 777;pepper` sequences are written only when `pepper::port()` is set (Pepper, not under tmux, screen or zellij).
- What's typed is never echoed back raw (`main` doesn't repeat unknown arguments).
