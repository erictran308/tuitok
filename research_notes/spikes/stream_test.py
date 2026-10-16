"""Can mpv (and so pepper-play, which loads the same Homebrew libmpv) stream
TikTok's videos itself, and how fast does each start? One mpv, idle, no
picture or sound (--vo=null --ao=null), each target sent with `loadfile
replace` as tuitok would, timed from loadfile to mpv's `playback-restart`
(the first frame ready). Headers go over IPC (`set_property`), never on the
command line.

  local     a video downloaded first (as tuitok's cache would hold it)
  stream    its address, streamed, tt_chain_token as a Cookie header
  bare      the same address with no cookie (expected: refused)
  each bitrateInfo rendition (h264 / bytevc1 / bytevc2), streamed
  hls       Apple's HLS test stream (m3u8)

  python3 -I stream_test.py SAMPLE.json WORKDIR
"""

import json
import os
import socket
import subprocess
import sys
import time
import urllib.request

MPV = "/opt/homebrew/bin/mpv"
UA = ("Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 "
      "(KHTML, like Gecko) Chrome/156.0.0.0 Safari/537.36")
REFERER = "https://www.tiktok.com/"
HLS = "https://devstreaming-cdn.apple.com/videos/streaming/examples/bipbop_4x3/bipbop_4x3_variant.m3u8"
PROPS = ["file-format", "video-codec", "width", "height", "duration", "hwdec-current", "audio-codec-name"]


class Mpv:
    def __init__(self):
        # A socketpair, mpv's end as an inherited fd, as tuitube does: no
        # socket file (whose path macOS caps at 104 bytes) and mpv quits
        # when this end closes.
        self.sock, theirs = socket.socketpair()
        self.proc = subprocess.Popen(
            [MPV, "--no-config", "--idle=yes", "--keep-open=yes", "--vo=null", "--ao=null",
             "--terminal=no", "--hwdec=auto-safe", "--ytdl=no", f"--input-ipc-client=fd://{theirs.fileno()}"],
            stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            pass_fds=[theirs.fileno()])
        theirs.close()
        self.buf = b""
        self.rid = 0

    def send(self, *command, rid=None):
        msg = {"command": list(command)}
        if rid is not None:
            msg["request_id"] = rid
        self.sock.sendall(json.dumps(msg).encode() + b"\n")

    def read(self, deadline):
        while b"\n" not in self.buf:
            left = deadline - time.monotonic()
            if left <= 0:
                return None
            self.sock.settimeout(left)
            try:
                chunk = self.sock.recv(65536)
            except socket.timeout:
                return None
            if not chunk:
                return None
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def get(self, name):
        self.rid += 1
        self.send("get_property", name, rid=self.rid)
        deadline = time.monotonic() + 5
        while (msg := self.read(deadline)) is not None:
            if msg.get("request_id") == self.rid:
                return msg.get("data")
        return None

    def play(self, target, headers, timeout=20):
        self.send("set_property", "http-header-fields", headers)
        self.send("set_property", "user-agent", UA)
        self.send("set_property", "referrer", REFERER)
        t0 = time.monotonic()
        self.send("loadfile", target, "replace")
        deadline = t0 + timeout
        while (msg := self.read(deadline)) is not None:
            if msg.get("event") == "playback-restart":
                took = round(time.monotonic() - t0, 3)
                return {"result": "playing", "first_frame_s": took, **{p: self.get(p) for p in PROPS}}
            if msg.get("event") == "end-file" and msg.get("reason") == "error":
                return {"result": f"error: {msg.get('file_error')}", "after_s": round(time.monotonic() - t0, 3)}
        return {"result": "no frame in time"}

    def quit(self):
        self.send("quit")
        self.proc.wait(5)


def play_url(video):
    pa = video.get("playAddr")
    return pa[0].get("src") if isinstance(pa, list) and pa else pa


def main():
    sample, work = json.load(open(sys.argv[1])), sys.argv[2]
    cookie = [f"Cookie: tt_chain_token={sample['tt_chain_token']}"]
    video = sample["items"][0]["video"]
    url = play_url(video)
    local = os.path.join(work, "local.mp4")
    req = urllib.request.Request(url, headers={"User-Agent": UA, "Referer": REFERER, "Cookie": cookie[0][8:]})
    t0 = time.monotonic()
    with urllib.request.urlopen(req, timeout=20) as resp, open(local, "wb") as f:
        f.write(resp.read())
    download = round(time.monotonic() - t0, 3)
    targets = [("local", local, []), ("stream", url, cookie), ("bare", url, [])]
    for b in video.get("bitrateInfo") or []:
        urls = (b.get("PlayAddr") or {}).get("UrlList") or []
        if urls:
            targets.append((f"{b.get('CodecType')} {b.get('GearName')}", urls[0], cookie))
    targets.append(("hls", HLS, []))
    mpv = Mpv()
    results = [{"case": "download first", "seconds": download, "bytes": os.path.getsize(local)}]
    try:
        for name, target, headers in targets:
            r = {"case": name, **mpv.play(target, headers)}
            results.append(r)
            print(json.dumps(r), flush=True)
    finally:
        mpv.quit()
    print(json.dumps(results[0]))


if __name__ == "__main__":
    main()
