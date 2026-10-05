#!/usr/bin/env python3
"""Bakes the desktop's art into flat binary blobs under kernel/assets/.

The kernel has no PNG/JPEG/SVG/TrueType decoders, and doesn't need them:
like the cursor (see kernel/src/cursor.rs), everything is decoded once,
offline, into raw pixels or alpha masks, then pulled in with include_bytes!.

Inputs (pass paths on the command line, or let the defaults find them):
  --wallpaper  any image Pillow can read (cover-scaled to 1280x800)
  --logo       the Inkscape SVG of the three-bar "K" boot logo
  --assets     a folder holding the Fluent System Icons fonts + JSON maps
               and the Inter / JetBrains Mono .ttf files (see README of
               this script's output section below)

Outputs:
  kernel/assets/wallpaper.rgb   "KWAL" u16 w, u16 h, then RGB888 rows
  kernel/assets/logo_k.a8       "KLGO" u16 w, u16 h, then A8 rows (boot size)
  kernel/assets/logo_k_mid.a8   same, sized for the About window
  kernel/assets/logo_k_small.a8 same, sized for the Start button
  kernel/assets/icons.kico      "KICO" u16 count, entries (u16 size, u32 off), A8
  kernel/assets/font_*.kfnt     "KFNT" glyph atlas, see write_font()
  kernel/src/ui/icon_ids.rs     generated constants naming each icon
"""

import argparse
import json
import re
import struct
from pathlib import Path

from PIL import Image, ImageDraw, ImageFont

ROOT = Path(__file__).resolve().parent.parent
ASSETS_OUT = ROOT / "kernel" / "assets"
IDS_OUT = ROOT / "kernel" / "src" / "ui" / "icon_ids.rs"

SCREEN_W, SCREEN_H = 1280, 800

# (constant name, fluent base name, size, variant). Taskbar art uses the
# 32px grid (crisp, hand-hinted at that size); window chrome and lists use 20.
ICONS = [
    ("TERMINAL_32", "code_block", 32, "regular"),
    ("TERMINAL_32_FILLED", "code_block", 32, "filled"),
    ("FILES_32", "folder", 32, "regular"),
    ("FILES_32_FILLED", "folder", 32, "filled"),
    ("MONITOR_32", "data_usage", 32, "regular"),
    ("MONITOR_32_FILLED", "data_usage", 32, "filled"),
    ("DOOM_32", "games", 32, "regular"),
    ("DOOM_32_FILLED", "games", 32, "filled"),
    ("ABOUT_32", "info", 32, "regular"),
    ("ABOUT_32_FILLED", "info", 32, "filled"),
    ("TERMINAL_20", "window_console", 20, "regular"),
    ("FILES_20", "folder", 20, "regular"),
    ("MONITOR_20", "data_usage", 20, "regular"),
    ("DOOM_20", "games", 20, "regular"),
    ("ABOUT_20", "info", 20, "regular"),
    ("CLOSE_16", "dismiss", 16, "regular"),
    ("MINIMIZE_16", "subtract", 16, "regular"),
    ("MAXIMIZE_16", "maximize", 16, "regular"),
    ("RESTORE_16", "square_multiple", 16, "regular"),
    ("CPU_20", "developer_board", 20, "regular"),
    ("RAM_20", "ram", 20, "regular"),
    ("FOLDER_20_FILLED", "folder", 20, "filled"),
    ("DOCUMENT_20", "document", 20, "regular"),
    ("DOCUMENT_TEXT_20", "document_text", 20, "regular"),
    ("APP_20", "app_generic", 20, "regular"),
    ("ARROW_UP_20", "arrow_up", 20, "regular"),
    ("HARD_DRIVE_20", "hard_drive", 20, "regular"),
    ("POWER_20", "power", 20, "regular"),
]

# (output name, ttf path relative to --assets, pixel size)
FONTS = [
    ("font_ui", "inter/extras/ttf/Inter-Regular.ttf", 14),
    ("font_ui_bold", "inter/extras/ttf/Inter-SemiBold.ttf", 14),
    ("font_small", "inter/extras/ttf/Inter-Medium.ttf", 12),
    ("font_display", "inter/extras/ttf/InterDisplay-SemiBold.ttf", 28),
    ("font_mono", "jbm/fonts/ttf/JetBrainsMono-Regular.ttf", 14),
]


def write_wallpaper(src: Path) -> None:
    im = Image.open(src).convert("RGB")
    scale = max(SCREEN_W / im.width, SCREEN_H / im.height)
    w, h = round(im.width * scale), round(im.height * scale)
    im = im.resize((w, h), Image.LANCZOS)
    left, top = (w - SCREEN_W) // 2, (h - SCREEN_H) // 2
    im = im.crop((left, top, left + SCREEN_W, top + SCREEN_H))
    data = b"KWAL" + struct.pack("<HH", SCREEN_W, SCREEN_H) + im.tobytes()
    (ASSETS_OUT / "wallpaper.rgb").write_bytes(data)


def parse_matrix(transform: str):
    if not transform:
        return (1, 0, 0, 1, 0, 0)
    nums = [float(n) for n in re.findall(r"-?[\d.]+(?:e-?\d+)?", transform)]
    return tuple(nums)


def logo_polygons(svg: Path):
    """The logo is three <rect>s, two of them under a matrix() transform;
    returns each one as a 4-point polygon in SVG user units."""
    text = svg.read_text()
    polys = []
    for m in re.finditer(r"<rect(.*?)/>", text, re.S):
        attrs = dict(re.findall(r'(\w+)="([^"]*)"', m.group(1)))
        x, y = float(attrs["x"]), float(attrs["y"])
        w, h = float(attrs["width"]), float(attrs["height"])
        a, b, c, d, e, f = parse_matrix(attrs.get("transform", ""))
        pts = [(x, y), (x + w, y), (x + w, y + h), (x, y + h)]
        polys.append([(a * px + c * py + e, b * px + d * py + f) for px, py in pts])
    return polys


def write_logo(svg: Path, height: int, name: str) -> None:
    polys = logo_polygons(svg)
    xs = [p[0] for poly in polys for p in poly]
    ys = [p[1] for poly in polys for p in poly]
    min_x, max_x, min_y, max_y = min(xs), max(xs), min(ys), max(ys)
    ss = 8  # supersampling factor for smooth diagonal edges
    pad = max(1, height // 24)
    scale = (height - 2 * pad) / (max_y - min_y)
    width = round((max_x - min_x) * scale) + 2 * pad
    big = Image.new("L", (width * ss, height * ss), 0)
    draw = ImageDraw.Draw(big)
    for poly in polys:
        draw.polygon(
            [((px - min_x) * scale * ss + pad * ss, (py - min_y) * scale * ss + pad * ss) for px, py in poly],
            fill=255,
        )
    mask = big.resize((width, height), Image.BOX)
    (ASSETS_OUT / name).write_bytes(b"KLGO" + struct.pack("<HH", width, height) + mask.tobytes())


def render_icon(font_path: Path, codepoint: int, size: int) -> bytes:
    font = ImageFont.truetype(str(font_path), size)
    im = Image.new("L", (size, size), 0)
    ImageDraw.Draw(im).text((0, 0), chr(codepoint), font=font, fill=255)
    return im.tobytes()


def write_icons(assets: Path) -> None:
    maps = {
        v: json.loads((assets / f"FluentSystemIcons-{v.title()}.json").read_text())
        for v in ("regular", "filled")
    }
    fonts = {v: assets / f"FluentSystemIcons-{v.title()}.ttf" for v in ("regular", "filled")}
    header = b"KICO" + struct.pack("<H", len(ICONS))
    table = b""
    blob = b""
    base = len(header) + 6 * len(ICONS)
    for _, fluent, size, variant in ICONS:
        key = f"ic_fluent_{fluent}_{size}_{variant}"
        table += struct.pack("<HI", size, base + len(blob))
        blob += render_icon(fonts[variant], maps[variant][key], size)
    (ASSETS_OUT / "icons.kico").write_bytes(header + table + blob)

    lines = [
        "//! Generated by tools/gen_desktop_assets.py -- indices into",
        "//! `assets/icons.kico`. Re-run the script instead of editing by hand.",
        "",
    ]
    for i, (const, fluent, size, variant) in enumerate(ICONS):
        lines.append(f"pub const {const}: usize = {i}; // {fluent} {size}px {variant}")
    IDS_OUT.parent.mkdir(parents=True, exist_ok=True)
    IDS_OUT.write_text("\n".join(lines) + "\n")


def write_font(ttf: Path, size: int, name: str) -> None:
    """KFNT layout: magic, u16 size, u16 ascent, u16 descent, u16 first,
    u16 count, then per glyph {i16 left, i16 top, u16 w, u16 h,
    u16 advance (1/16 px), u16 pad, u32 offset}, then A8 coverage data.
    `top` is measured down from the line's top (ascent line)."""
    font = ImageFont.truetype(str(ttf), size)
    ascent, descent = font.getmetrics()
    first, count = 32, 95
    header = b"KFNT" + struct.pack("<HHHHH", size, ascent, descent, first, count)
    table, blob = b"", b""
    base = len(header) + 16 * count
    for code in range(first, first + count):
        ch = chr(code)
        advance = round(font.getlength(ch) * 16)
        l, t, r, b = font.getbbox(ch)
        w, h = max(0, r - l), max(0, b - t)
        data = b""
        if w and h:
            im = Image.new("L", (w, h), 0)
            ImageDraw.Draw(im).text((-l, -t), ch, font=font, fill=255)
            # Slight coverage boost: blending happens in gamma space, which
            # otherwise makes light-on-dark text look thin and washed out.
            im = im.point(lambda v: round(255 * (v / 255) ** 0.8))
            data = im.tobytes()
        table += struct.pack("<hhHHHHI", l, t, w, h, advance, 0, base + len(blob))
        blob += data
    (ASSETS_OUT / f"{name}.kfnt").write_bytes(header + table + blob)


def main() -> None:
    downloads = Path.home() / "Downloads"
    ap = argparse.ArgumentParser()
    ap.add_argument("--wallpaper", type=Path, default=downloads / "magicpattern-87PP9Zd7MNo-unsplash.jpg")
    ap.add_argument("--logo", type=Path, default=downloads / "IconsForKonjac" / "BootupLogo.png")
    ap.add_argument("--assets", type=Path, default=ROOT / ".cache" / "assets")
    args = ap.parse_args()

    ASSETS_OUT.mkdir(parents=True, exist_ok=True)
    write_wallpaper(args.wallpaper)
    write_logo(args.logo, 160, "logo_k.a8")
    write_logo(args.logo, 64, "logo_k_mid.a8")
    write_logo(args.logo, 22, "logo_k_small.a8")
    write_icons(args.assets)
    for name, rel, size in FONTS:
        write_font(args.assets / rel, size, name)
    print("assets written to", ASSETS_OUT)


if __name__ == "__main__":
    main()
