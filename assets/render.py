#!/usr/bin/env python3
"""Rasterise `assets/icon.svg` into every format the packaging needs.

Usage
-----
    python assets/render.py

With no arguments this regenerates the whole set from the master SVG:

    assets/icon-128.png   assets/icon-256.png   assets/icon.ico   assets/icon.icns

and then re-opens each result and prints what is actually in it. That is the
only supported way to change the raster files — edit `assets/icon.svg` and run
this. Editing a PNG, the `.ico` or the `.icns` by hand puts them out of step
with the master and with each other.

For one-off renders of some other SVG (a mark being tried out) pass files and
sizes explicitly:

    python assets/render.py path/to/mark.svg --sizes 16 32 256 [--outdir DIR]

Why resvg
---------
The script is rudbman's `assets/render.py` with the rasteriser swapped out.
That one renders through GdkPixbuf because GTK is already a hard dependency
of that repository; this one has no GTK anywhere, and its icon is maintained
on Windows where none is installed. `resvg` is a standalone SVG rasteriser
(`cargo install resvg`), which makes it the cheapest correct tool for a Rust
workspace to ask for — and its output is what this icon was proofed against,
warnings included: it refuses filters on zero-sized shapes, which is why the
master draws its horizontal highlight as a rect rather than a stroked line.

Every size is drawn from the vector
-----------------------------------
`resvg --width N` re-renders the vector at the requested pixel size instead
of scaling a large bitmap down, and every size in every container below is
produced that way — the 16 px frame in the `.ico` is a 16 px drawing, not a
shrunk 256. The difference is plain at that size: a downscaled 256 smears the
glyph's translucent bevels into haze, while a fresh 16 px render lets them
dissolve and keeps the silhouette clean. Pillow would happily resample for us
in both container writers, so each one is handed a complete set of exact-size
frames through ``append_images`` and never gets the chance.
"""

from __future__ import annotations

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from PIL import Image

ASSETS = Path(__file__).resolve().parent
MASTER = ASSETS / "icon.svg"

#: Standalone PNGs. These are the sizes the Linux `.desktop` entry installs;
#: see `packaging/`.
PNG_SIZES = (128, 256)

#: Frames in the Windows `.ico`. Explorer, the taskbar, the window title bar
#: and Alt-Tab each pick a different one out of the same file, so leaving a
#: size out means Windows scales a neighbouring frame and shows its artefacts.
#: `crates/rulogman-app/build.rs` embeds this file as resource ID 1.
ICO_SIZES = (16, 24, 32, 48, 64, 128, 256)

#: Frames in the macOS `.icns`. There is no 16 px entry in the container's
#: chunk table: its smallest type, `ic11`, is the 16 pt slot at 2x, which is
#: 32 px. macOS derives the 1x 16 px image from that itself.
ICNS_SIZES = (32, 64, 128, 256, 512, 1024)


def rasterise(svg: Path, size: int) -> Image.Image:
    """Render *svg* at *size*x*size* pixels and return it as a Pillow image."""
    resvg = shutil.which("resvg")
    if resvg is None:
        raise RuntimeError("resvg is not on PATH; install it with `cargo install resvg`")
    # resvg only writes files, so the hand-off goes through a temporary one.
    # `delete=False` because Windows will not let resvg open a file this
    # process is still holding; it is removed once Pillow has read it.
    with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as handle:
        target = Path(handle.name)
    try:
        subprocess.run(
            [resvg, "--width", str(size), "--height", str(size), str(svg), str(target)],
            check=True,
            capture_output=True,
        )
        with Image.open(target) as image:
            image.load()
            return image.convert("RGBA")
    finally:
        target.unlink(missing_ok=True)


def render(svg: Path, size: int, outdir: Path | None = None) -> Path:
    """Render *svg* at *size* and write `<stem>-<size>.png`; return that path."""
    target = (outdir or svg.parent) / f"{svg.stem}-{size}.png"
    target.parent.mkdir(parents=True, exist_ok=True)
    rasterise(svg, size).save(target, "png")
    return target


def build_all(svg: Path = MASTER, outdir: Path | None = None) -> list[Path]:
    """Produce the full shipped set from *svg*. Returns the files written."""
    out = outdir or svg.parent
    out.mkdir(parents=True, exist_ok=True)
    written: list[Path] = []

    for size in PNG_SIZES:
        target = out / f"icon-{size}.png"
        rasterise(svg, size).save(target, "png")
        written.append(target)

    # Pillow's ICO writer looks through `[im] + append_images` for a frame
    # whose size matches each requested one and only resamples when it finds
    # none, so handing it every size means it resamples nothing. The base image
    # has to be the largest, because the writer skips any requested size larger
    # than the base.
    largest = max(ICO_SIZES)
    ico_frames = {size: rasterise(svg, size) for size in ICO_SIZES}
    ico = out / "icon.ico"
    ico_frames[largest].save(
        ico,
        "ico",
        sizes=[(s, s) for s in ICO_SIZES],
        append_images=[ico_frames[s] for s in ICO_SIZES if s != largest],
    )
    written.append(ico)

    # The ICNS writer keys `append_images` by width and resizes only for a
    # width it was not given, so the same rule applies: supply them all.
    icns_frames = [rasterise(svg, size) for size in ICNS_SIZES]
    icns = out / "icon.icns"
    icns_frames[-1].save(icns, "icns", append_images=icns_frames)
    written.append(icns)

    return written


def verify(paths: list[Path]) -> None:
    """Re-open each generated file and print what it really contains."""
    for path in paths:
        with Image.open(path) as im:
            fmt = im.format
            if fmt == "ICO":
                sizes = sorted(im.ico.sizes())
                detail = f"{len(sizes)} frames: " + ", ".join(
                    f"{w}x{h}" for w, h in sizes
                )
            elif fmt == "ICNS":
                sizes = sorted({w * scale for w, _, scale in im.info["sizes"]})
                detail = f"{len(sizes)} sizes: " + ", ".join(str(s) for s in sizes)
            else:
                detail = f"{im.size[0]}x{im.size[1]} {im.mode}"
        print(f"  {path.name:<14} {fmt:<5} {detail}  ({path.stat().st_size} bytes)")


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description="Rasterise the application icon from its master SVG."
    )
    parser.add_argument(
        "svg", nargs="*", type=Path, help="SVGs to render (default: the full set)"
    )
    parser.add_argument(
        "--sizes", nargs="+", type=int, help="pixel sizes, for one-off renders"
    )
    parser.add_argument(
        "--outdir", type=Path, default=None, help="write here instead of alongside"
    )
    args = parser.parse_args(argv)

    if not args.svg and not args.sizes:
        if not MASTER.is_file():
            parser.error(f"no master SVG at {MASTER}")
        written = build_all(MASTER, args.outdir)
        print(f"regenerated from {MASTER.name}:")
        verify(written)
        return 0

    svgs = args.svg or [MASTER]
    sizes = args.sizes or list(PNG_SIZES)
    for svg in svgs:
        if not svg.is_file():
            parser.error(f"no such file: {svg}")
        for size in sizes:
            print(render(svg, size, args.outdir))
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
