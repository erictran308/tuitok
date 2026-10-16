#!/bin/sh
# pepper-play playing a TikTok clip (576x1024, looped) in a 9:16 box, in a
# Pepper of its own, measured by Pepper's own bench (measure.py --no-window),
# the way pepper-play/bench/run.sh --new-pepper runs it. The Pepper you work
# in isn't measured or touched.
#
#   SCRATCH=DIR bench_pepper.sh CLIP OUT-PREFIX
set -eu
clip=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")
out=$2
app=/Applications/Pepper.app
measure=/Users/eric/projects/Personal/pepper/pepper-play/bench/measure.py
q() {
    printf "'%s'" "$(printf %s "$1" | sed "s/'/'\\\\''/g")"
}
dir=$(mktemp -d "${SCRATCH:?}/tuitok-bench.XXXXXX")
trap 'rm -rf "$dir"' EXIT
{
    echo '#!/bin/sh'
    printf 'export PATH=%s\n' "$(q "$PATH")"
    printf 'exec >%s 2>&1\n' "$(q "$out.log")"
    printf 'python3 -I %s --seconds 60 --box 608x1080 --no-window --label tiktok-9x16 --player %s --out %s %s --loop-file=inf\n' \
        "$(q "$measure")" "$(q "$app/Contents/MacOS/pepper-play")" "$(q "$out.jsonl")" "$(q "$clip")"
    printf 'echo $? >%s\n' "$(q "$out.done")"
} >"$dir/bench"
chmod 700 "$dir/bench"
rm -f "$out.done"
uid=$(id -u)
exe="$app/Contents/MacOS/ghostty"
before=" $(pgrep -u "$uid" -f "^$exe( |$)" | tr '\n' ' ' || true) "
open -n "$app" --args --command="direct:$dir/bench" --window-save-state=never \
    --quit-after-last-window-closed=true --confirm-close-surface=false \
    --window-width=160 --window-height=50
pid=
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
    for new in $(pgrep -u "$uid" -f "^$exe( |$)" || true); do
        case "$before" in *" $new "*) ;; *) pid=$new ;; esac
    done
    [ -n "$pid" ] && break
    sleep 0.5
done
[ -n "$pid" ] || { echo "bench_pepper: Pepper didn't start" >&2 && exit 1; }
sleep 1
osascript -l JavaScript -e "ObjC.import('AppKit');
    \$.NSRunningApplication.runningApplicationWithProcessIdentifier($pid).activateWithOptions(2)" >/dev/null
waited=0
while [ ! -f "$out.done" ] && kill -0 "$pid" 2>/dev/null && [ "$waited" -lt 200 ]; do
    sleep 2
    waited=$((waited + 2))
done
python3 -I "$measure" --table "$out.jsonl"
