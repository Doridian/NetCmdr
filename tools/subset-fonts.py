#!/usr/bin/env python3
# Copyright (C) 2026 Doridian
# SPDX-License-Identifier: AGPL-3.0-or-later
"""Regenerate the viewer's subsetted fonts in crates/netcmdr-ui/fonts.

egui rasterises glyphs itself, so the web client has to carry real font files —
a browser's installed fonts are not reachable from WebAssembly. Carrying
egui's four stock fonts whole costs 1.3 MiB, about a quarter of the module, so
this cuts them down to the characters the viewer can actually draw.

Needs fontTools and the upstream fonts from the epaint_default_fonts crate:

    pip install fonttools
    cargo fetch
    tools/subset-fonts.py

Run it after bumping egui, in case upstream moved to different faces.
"""

import glob
import os
import sys

try:
    from fontTools import subset
    from fontTools.ttLib import TTFont
except ImportError:
    sys.exit("fontTools not found. Install it with: pip install fonttools")

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = os.path.join(ROOT, "crates", "netcmdr-ui", "fonts")

# Keep in step with egui, and re-run on bumps.
EPAINT_FONTS = "epaint_default_fonts-0.36.2"


def upstream() -> str:
    """Find the vendored fonts in the cargo registry."""
    home = os.environ.get("CARGO_HOME") or os.path.expanduser("~/.cargo")
    hits = glob.glob(os.path.join(home, "registry", "src", "*", EPAINT_FONTS, "fonts"))
    if not hits:
        sys.exit(
            f"{EPAINT_FONTS} not in the cargo registry.\n"
            "Run `cargo fetch` first, or update EPAINT_FONTS if egui moved on."
        )
    return hits[0]


def codepoints(*ranges) -> list:
    """Flatten a mix of single codepoints and inclusive (lo, hi) pairs."""
    out = []
    for item in ranges:
        if isinstance(item, tuple):
            out.extend(range(item[0], item[1] + 1))
        else:
            out.append(item)
    return out


# Characters the viewer, device names and egui widgets may draw.
TEXT = codepoints(
    (0x0020, 0x007E),  # printable ASCII
    (0x00A0, 0x00FF),  # Latin-1: device names, users and targets are not all ASCII
    (0x2010, 0x2015),  # hyphens and dashes — the viewer uses U+2014
    0x2018, 0x2019, 0x201C, 0x201D,  # curly quotes
    0x2009, 0x202F,  # thin and narrow spaces — egui's DragValue uses these
    0x2022,  # bullet
    0x2026,  # ellipsis — "Connecting…"
    0x2039, 0x203A,  # single angle quotes
    0x20AC,  # euro
    0x2212,  # minus sign — egui's DragValue draws negatives with this
    0x21E7, 0x2303, 0x2318, 0x2325,  # ⇧ ⌃ ⌘ ⌥, egui's keyboard-shortcut names
)

# The icon glyphs egui's widgets draw as text (e.g. SubMenuButton's "⏵").
ICONS = codepoints((0x23F4, 0x23F7), 0x26A0)

# The Ubuntu Font Licence §2(c) requires a modified font to be distinguished
# by name; applied to all three.
DERIVATIVE = "derivative NetCmdr"

# (upstream file, output file, wanted codepoints, licence to carry alongside)
JOBS = [
    ("Ubuntu-Light.ttf", "Ubuntu-Light-subset.ttf", TEXT, "UFL.txt"),
    ("Hack-Regular.ttf", "Hack-Regular-subset.ttf", TEXT, "Hack-Regular.txt"),
    ("emoji-icon-font.ttf", "emoji-icon-subset.ttf", ICONS,
     "emoji-icon-font-mit-license.txt"),
]


def rename(path: str) -> str:
    """Mark the face as a derivative, in the name table rather than the filename.

    epaint looks fonts up by the keys `fonts.rs` gives them, never by what is
    inside the file, so this is free of consequences beyond the licence it
    settles. Returns the new family name, for the log.
    """
    font = TTFont(path)
    name = font["name"]
    family = None
    for record in name.names:
        # 1 family, 3 unique id, 4 full name, 6 PostScript, 16/17 typographic.
        if record.nameID not in (1, 3, 4, 6, 16):
            continue
        text = record.toUnicode()
        if DERIVATIVE in text:
            continue
        # PostScript names take no spaces, and the unique id is free-form.
        suffix = DERIVATIVE.replace(" ", "") if record.nameID == 6 else DERIVATIVE
        joiner = "-" if record.nameID == 6 else " "
        record.string = f"{text}{joiner}{suffix}"
        if record.nameID == 1:
            family = record.toUnicode()
    font.save(path)
    return family or "?"


def main() -> None:
    src = upstream()
    os.makedirs(OUT, exist_ok=True)
    total_before = total_after = 0

    for name, out_name, wanted, licence in JOBS:
        src_path = os.path.join(src, name)
        out_path = os.path.join(OUT, out_name)

        have = set(TTFont(src_path, fontNumber=0).getBestCmap())
        keep = sorted(set(wanted) & have)
        missing = len(set(wanted)) - len(keep)

        subset.main([
            src_path,
            "--output-file=" + out_path,
            "--unicodes=" + ",".join(f"{c:04X}" for c in keep),
            # epaint doesn't use layout tables.
            "--layout-features=",
            "--no-hinting",
            "--desubroutinize",
            "--drop-tables+=DSIG,GSUB,GPOS,GDEF,kern,morx,TTFA,FFTM",
        ])

        family = rename(out_path)

        with open(os.path.join(src, licence), "rb") as fh:
            blob = fh.read()
        with open(os.path.join(OUT, licence), "wb") as fh:
            fh.write(blob)

        before, after = os.path.getsize(src_path), os.path.getsize(out_path)
        total_before += before
        total_after += after
        note = f", {missing} not in the face" if missing else ""
        print(f"{name:22} {before:7} -> {after:6} bytes  "
              f"({len(keep)} glyphs{note})  as {family!r}")

    print(f"{'total':22} {total_before:7} -> {total_after:6} bytes "
          f"({100 * (total_before - total_after) / total_before:.1f}% smaller)")


if __name__ == "__main__":
    main()
