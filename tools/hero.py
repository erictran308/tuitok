# Usage: cargo test -- --ignored export_hero_screen
#        uv run --no-project --with pillow --with fonttools \
#          python -I tools/hero.py target/hero docs/hero.png FONTS_DIR
# FONTS_DIR holds JetBrainsMonoNoNF-Regular.ttf and JetBrainsMonoNerdFont-
# Regular.ttf and -Bold.ttf, Ghostty's (and Pepper's) own: ../pepper/src/font/res.
"""Draws one frame of `tuitok --demo` in Pepper, as exported by the
`export_hero_screen` test, the way Pepper shows it: every cell's character
in JetBrains Mono, box lines and blocks drawn to the cell's edges as Ghostty
draws them, emoji in Apple's, and in the video's box a frame of the made-up
video: the demo's cover with a cat and the words on it, as TikTok videos
have them. tuitube's tools/hero.py, cut down."""

import json
import os
import sys

from fontTools.ttLib import TTFont
from PIL import Image, ImageDraw, ImageFilter, ImageFont

SCREEN, OUT, FONTS = sys.argv[1], sys.argv[2], sys.argv[3]
PAD = 28
RADIUS = 26

screen = json.load(open(os.path.join(SCREEN, "screen.json")))
COLS, ROWS = screen["cols"], screen["rows"]
CW, CH = screen["cell"]
BG, FG = screen["bg"], screen["fg"]
cells = screen["cells"]

REGULAR = os.path.join(FONTS, "JetBrainsMonoNerdFont-Regular.ttf")
regular = ImageFont.truetype(REGULAR, 26.6)
bold = ImageFont.truetype(os.path.join(FONTS, "JetBrainsMonoNerdFont-Bold.ttf"), 26.6)
# What Ghostty's own JetBrains Mono (without Nerd Font icons) doesn't have
# (♪ ♫ ♥) comes from a system font, as Ghostty falls back; emoji from
# Apple's, whose pictures come at 160 px.
covered = set(TTFont(os.path.join(FONTS, "JetBrainsMonoNoNF-Regular.ttf")).getBestCmap())
fallback = ImageFont.truetype("/System/Library/Fonts/Apple Symbols.ttf", 30)
EMOJI = "/System/Library/Fonts/Apple Color Emoji.ttc"
ascent, descent = regular.getmetrics()
BASELINE = (CH - (ascent + descent)) // 2 + ascent

width, height = COLS * CW + 2 * PAD, ROWS * CH + 2 * PAD
canvas = Image.new("RGB", (width, height), BG)
draw = ImageDraw.Draw(canvas)


def cell_box(x, y):
    left, top = PAD + x * CW, PAD + y * CH
    return left, top, left + CW, top + CH


def rect(box, fill):
    """A rectangle, unless it's empty (a corner's arc can reach the cell's
    edge on its own)."""
    if box[2] >= box[0] and box[3] >= box[1]:
        draw.rectangle(box, fill=fill)


def line_shape(ch, x, y, color):
    """Box lines and blocks, drawn to the cell's edges as Ghostty does, so
    they join up; True if `ch` is one."""
    left, top, right, bottom = cell_box(x, y)
    mx, my = left + CW // 2, top + CH // 2
    if ch == "─":
        rect([left, my - 1, right - 1, my], fill=color)
    elif ch == "━":
        rect([left, my - 2, right - 1, my + 1], fill=color)
    elif ch == "│":
        rect([mx - 1, top, mx, bottom - 1], fill=color)
    elif ch in "╭╮╰╯":
        r = CW // 2
        cx = mx + r if ch in "╭╰" else mx - r
        cy = my + r if ch in "╭╮" else my - r
        start = {"╭": 180, "╮": 270, "╰": 90, "╯": 0}[ch]
        draw.arc([cx - r, cy - r, cx + r, cy + r], start, start + 90, fill=color, width=2)
        if ch in "╭╮":
            rect([mx - 1, cy, mx, bottom - 1], fill=color)
        else:
            rect([mx - 1, top, mx, cy], fill=color)
        if ch in "╭╰":
            rect([cx, my - 1, right - 1, my], fill=color)
        else:
            rect([left, my - 1, cx, my], fill=color)
    elif ch == "▀":
        rect([left, top, right - 1, top + CH // 2 - 1], fill=color)
    elif ch == "▄":
        rect([left, top + CH // 2, right - 1, bottom - 1], fill=color)
    elif ch == "█":
        rect([left, top, right - 1, bottom - 1], fill=color)
    else:
        return False
    return True


def emoji(ch, size):
    """`ch` in Apple's emoji, scaled to `size` pixels tall."""
    font = ImageFont.truetype(EMOJI, 160)
    picture = Image.new("RGBA", (220, 220), (0, 0, 0, 0))
    ImageDraw.Draw(picture).text((10, 10), ch, font=font, embedded_color=True)
    picture = picture.crop(picture.getbbox())
    scale = size / picture.height
    return picture.resize((round(picture.width * scale), size), Image.LANCZOS)


def is_emoji(ch):
    return ord(ch[0]) >= 0x1F000


def draw_cell(x, y, cell):
    ch, fg, bg, heavy = cell
    left, top, right, bottom = cell_box(x, y)
    if bg:
        rect([left, top, right - 1, bottom - 1], fill=bg)
    if ch in ("", " "):
        return
    color = fg or FG
    if line_shape(ch, x, y, color):
        return
    if is_emoji(ch):
        # Two cells wide, as the layout counts it.
        picture = emoji(ch, round(CH * 0.72))
        canvas.paste(picture, (left + (2 * CW - picture.width) // 2, top + (CH - picture.height) // 2), picture)
        return
    font = bold if heavy else regular
    if ord(ch[0]) not in covered:
        font = fallback
    draw.text((left, top + BASELINE), ch, font=font, fill=color, anchor="ls")


def video_frame(w, h):
    """A frame of the made-up video, `w`×`h` pixels: its cover, a cat, and
    the words on it."""
    frame = Image.open(os.path.join(SCREEN, "cover.png")).convert("RGB")
    scale = max(w / frame.width, h / frame.height)
    frame = frame.resize((round(frame.width * scale), round(frame.height * scale)), Image.LANCZOS)
    left, top = (frame.width - w) // 2, (frame.height - h) // 2
    frame = frame.crop((left, top, left + w, top + h)).convert("RGBA")
    cat = emoji("🐈", round(h * 0.26))
    shadow = Image.new("RGBA", frame.size, (0, 0, 0, 0))
    shadow.paste((0, 0, 0, 90), (0, 0), cat)
    shadow = shadow.transform(frame.size, Image.AFFINE, (1, 0, -(w - cat.width) // 2 - 14, 0, 1, -round(h * 0.47) - 22))
    frame.alpha_composite(shadow.filter(ImageFilter.GaussianBlur(18)))
    frame.alpha_composite(cat, ((w - cat.width) // 2, round(h * 0.47)))
    # The words, as TikTok's classic text: white, bold, a dark edge.
    pen = ImageDraw.Draw(frame)
    font = ImageFont.truetype("/System/Library/Fonts/Supplemental/Arial Rounded Bold.ttf", round(w * 0.105))
    y = round(h * 0.22)
    for line in ["3am zoomies", "have begun"]:
        pen.text((w / 2, y), line, font=font, fill="#ffffff", anchor="mt", stroke_width=5, stroke_fill="#1b1b1b")
        y += round(font.size * 1.18)
    return frame.convert("RGB")


cover = screen["cover"]
in_cover = {(x, y) for x in range(cover[0], cover[0] + cover[2]) for y in range(cover[1], cover[1] + cover[3])}
for y in range(ROWS):
    for x in range(COLS):
        if (x, y) not in in_cover:
            draw_cell(x, y, cells[y * COLS + x])
cx, cy, cw, ch = cover
canvas.paste(video_frame(cw * CW, ch * CH), (PAD + cx * CW, PAD + cy * CH))

# Rounded corners, like a window.
mask = Image.new("L", canvas.size, 0)
ImageDraw.Draw(mask).rounded_rectangle([0, 0, width - 1, height - 1], RADIUS, fill=255)
out = Image.new("RGBA", canvas.size, (0, 0, 0, 0))
out.paste(canvas, (0, 0), mask)
os.makedirs(os.path.dirname(OUT) or ".", exist_ok=True)
out.save(OUT, optimize=True)
print(f"wrote {OUT} ({width}×{height})")
