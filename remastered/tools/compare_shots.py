"""Side-by-side pictures of original vs HD textures (for the README and the mod page).

    python compare_shots.py --dat <copy of Gw2.dat> --pack <pack folder> --gw2tex <gw2tex.exe> --out <folder>
        [--class scenery,terrain,creature,armor,weapon] [--per-class 6] [--ids 1,2,3] [--crop 256]

The original comes from Gw2.dat (gw2tex extract: mip 0, the game's own BC blocks), the HD version from the pack's
archive (pack.idx + pack_NNNNN.bin, a zstd frame holding a DDS). Both are decoded with Pillow. The same region is cut
from each (a `--crop`-pixel square of the HD texture, the matching smaller square of the original), the original is
enlarged with nearest-neighbour (what the game would show up close) and the two are put side by side, labelled.
Alpha is dropped (shown over black), since many textures keep masks there.
"""

import argparse
import io
import json
import random
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

import zstandard
from PIL import Image, ImageDraw, ImageFont

RECORD = struct.Struct("<IIIIIQ")  # id, volume, size, src_bytes, width, offset (see batch/pack_archive.py)


def load_archive(pack: Path) -> dict[int, tuple[int, int, int]]:
    idx = (pack / "pack.idx").read_bytes()
    assert idx[:8] == b"GW2RPAK2", "unknown pack.idx"
    out = {}
    for off in range(8, len(idx), RECORD.size):
        fid, vol, size, _src, _w, offset = RECORD.unpack_from(idx, off)
        out[fid] = (vol, offset, size)
    return out


def hd_image(pack: Path, loc: tuple[int, int, int]) -> Image.Image:
    vol, offset, size = loc
    with open(pack / f"pack_{vol:05}.bin", "rb") as f:
        f.seek(offset)
        frame = f.read(size)
    dds = zstandard.ZstdDecompressor().decompress(frame, max_output_size=256 << 20)
    return Image.open(io.BytesIO(dds)).convert("RGBA")


def flatten(img: Image.Image) -> Image.Image:
    bg = Image.new("RGB", img.size, (0, 0, 0))
    bg.paste(img.convert("RGB"))
    return bg


def detail_box(img: Image.Image, size: int) -> tuple[int, int, int, int]:
    """The `size` square with the most detail (variance of a small grey thumbnail), on a coarse grid."""
    g = img.convert("L")
    w, h = g.size
    size = min(size, w, h)
    best, box = -1.0, (0, 0, size, size)
    step = max(size // 2, 1)
    for y in range(0, h - size + 1, step):
        for x in range(0, w - size + 1, step):
            t = g.crop((x, y, x + size, y + size)).resize((32, 32))
            px = list(t.getdata())
            m = sum(px) / len(px)
            v = sum((p - m) ** 2 for p in px) / len(px)
            if v > best:
                best, box = v, (x, y, x + size, y + size)
    return box


def label(img: Image.Image, text: str) -> None:
    d = ImageDraw.Draw(img)
    try:
        font = ImageFont.truetype("arialbd.ttf", max(14, img.width // 18))
    except OSError:
        font = ImageFont.load_default()
    x0, y0, x1, y1 = d.textbbox((0, 0), text, font=font)
    pad = 6
    d.rectangle((0, 0, x1 - x0 + 2 * pad, y1 - y0 + 2 * pad), fill=(0, 0, 0))
    d.text((pad - x0, pad - y0), text, fill=(255, 255, 255), font=font)


def pair(orig: Image.Image, hd: Image.Image, crop: int) -> Image.Image:
    scale = hd.width / orig.width
    box = detail_box(hd, crop)
    hd_c = flatten(hd.crop(box))
    ob = tuple(round(v / scale) for v in box)
    or_c = flatten(orig.crop(ob)).resize(hd_c.size, Image.NEAREST)
    label(or_c, f"Original {orig.width}x{orig.height}")
    label(hd_c, f"GW2 Enhanced {hd.width}x{hd.height}")
    out = Image.new("RGB", (hd_c.width * 2 + 4, hd_c.height), (40, 40, 40))
    out.paste(or_c, (0, 0))
    out.paste(hd_c, (hd_c.width + 4, 0))
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dat", type=Path, required=True)
    ap.add_argument("--pack", type=Path, required=True)
    ap.add_argument("--gw2tex", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument("--hints", type=Path, help="texture_class_hints.jsonl (to pick by class)")
    ap.add_argument("--class", dest="classes", default="scenery,terrain,creature,armor,weapon")
    ap.add_argument("--per-class", type=int, default=6)
    ap.add_argument("--ids", default="")
    ap.add_argument("--crop", type=int, default=384)
    ap.add_argument("--seed", type=int, default=7)
    a = ap.parse_args()
    archive = load_archive(a.pack)
    picks: list[tuple[str, int]] = []
    if a.ids:
        picks = [("pick", int(i)) for i in a.ids.split(",")]
    else:
        by_class: dict[str, list[int]] = {}
        for line in open(a.hints, encoding="utf-8"):
            d = json.loads(line)
            if d["file_id"] in archive and d.get("width", 0) >= 256 and d.get("height", 0) >= 256:
                by_class.setdefault(d["hint"], []).append(d["file_id"])
        rnd = random.Random(a.seed)
        for c in a.classes.split(","):
            ids = by_class.get(c, [])
            picks += [(c, i) for i in rnd.sample(ids, min(a.per_class, len(ids)))]
    a.out.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory() as tmp:
        ids = ",".join(str(i) for _, i in picks)
        subprocess.run([str(a.gw2tex), "--dat", str(a.dat), "extract", "--out", tmp, "--ids", ids], check=True, capture_output=True)
        for cls, fid in picks:
            src = next(Path(tmp).glob(f"{fid}*.dds"), None)
            if src is None:
                print(f"{fid}: not extracted")
                continue
            try:
                orig = Image.open(src).convert("RGBA")
                hd = hd_image(a.pack, archive[fid])
            except Exception as e:  # an exotic format Pillow cannot decode: skip it
                print(f"{fid}: {e}")
                continue
            if hd.width <= orig.width:
                continue
            pair(orig, hd, a.crop).save(a.out / f"{cls}-{fid}.jpg", quality=90)
            print(f"{cls}-{fid}.jpg  {orig.width}x{orig.height} -> {hd.width}x{hd.height}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
