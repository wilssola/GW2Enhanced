"""GW2 Remastered — AI texture upscaler.

Input: DDS files written by `gw2tex extract` (mip 0, original BC blocks) and
the survey CSV (file id -> kind/format/size). Output: PNG per texture at the
target size plus manifest.jsonl (params, hashes) for reproducibility.

Policy per texture class (by ANet format):
  color  (DXT1/2/3/4/5, BC7X, DXTL) -> AI model on RGB; alpha upscaled with
                                       the model too (as grey) or Lanczos.
  normal (DXTN, 3DCX)               -> Lanczos + renormalize (no AI: models
                                       hallucinate and break vector data).
  mask   (DXTA)                     -> Lanczos.
Textures already >= --max-side are skipped. Scale is chosen so the long side
reaches at most --max-side (2048 by default), using the model's native scale
and a final Lanczos resize when needed.
"""
from __future__ import annotations

import argparse
import csv
import hashlib
import json
import math
import os
import sys
import time
from pathlib import Path

import numpy as np
import torch
from PIL import Image

COLOR = {"DXT1", "DXT2", "DXT3", "DXT4", "DXT5", "BC7X", "DXTL"}
NORMAL = {"DXTN", "3DCX"}
MASK = {"DXTA"}


def load_model(path: Path, half: bool):
    import spandrel

    desc = spandrel.ModelLoader().load_from_file(str(path))
    m = desc.model.eval().cuda()
    if half and desc.supports_half:
        m = m.half()
    return m, desc.scale, (half and desc.supports_half)


@torch.inference_mode()
def run_tiled(model, scale: int, half: bool, img: np.ndarray, tile: int, pad: int, wrap: bool) -> np.ndarray:
    """img HxWx3 float32 [0,1] -> (H*s)x(W*s)x3. Tiles overlap by `pad` on
    each side; only the tile core is kept (no seams). `wrap` pads with the
    opposite edge so tileable textures stay tileable."""
    h, w, _ = img.shape
    mode = "wrap" if wrap else "reflect"
    src = np.pad(img, ((pad, pad), (pad, pad), (0, 0)), mode=mode)
    out = np.zeros((h * scale, w * scale, 3), np.float32)
    dt = torch.float16 if half else torch.float32
    t = tile
    while True:
        try:
            for y in range(0, h, t):
                for x in range(0, w, t):
                    th, tw = min(t, h - y), min(t, w - x)
                    patch = src[y : y + th + 2 * pad, x : x + tw + 2 * pad]
                    ten = torch.from_numpy(patch).permute(2, 0, 1)[None].to("cuda", dt)
                    res = model(ten)[0].float().clamp(0, 1).permute(1, 2, 0).cpu().numpy()
                    core = res[pad * scale : (pad + th) * scale, pad * scale : (pad + tw) * scale]
                    out[y * scale : (y + th) * scale, x * scale : (x + tw) * scale] = core
            return out
        except torch.OutOfMemoryError:
            torch.cuda.empty_cache()
            if t <= 64:
                raise
            t //= 2
            print(f"  OOM, tile -> {t}", file=sys.stderr)


def lanczos(arr: np.ndarray, size: tuple[int, int]) -> np.ndarray:
    chans = [np.asarray(Image.fromarray(arr[..., c].astype(np.float32), "F").resize(size, Image.LANCZOS)) for c in range(arr.shape[2])]
    return np.stack(chans, -1)


def renormalize(rg: np.ndarray) -> np.ndarray:
    """2-channel tangent normal in [0,1] -> renormalized x,y (z implied)."""
    v = rg * 2 - 1
    n = np.sqrt(np.clip((v**2).sum(-1, keepdims=True), 1e-8, None))
    v = np.where(n > 1, v / n, v)
    return (v + 1) / 2


def target_size(w: int, h: int, max_side: int, max_factor: int = 4) -> tuple[int, int] | None:
    side = max(w, h)
    if side >= max_side:
        return None
    # power of two keeps mip chains clean; never invent more than max_factor
    f = min(2 ** int(math.log2(max_side / side)), max_factor)
    return (w * f, h * f) if f > 1 else None


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dds-dir", type=Path, required=True)
    ap.add_argument("--survey", type=Path, required=True)
    ap.add_argument("--out", type=Path, required=True)
    ap.add_argument(
        "--model",
        type=Path,
        default=Path(os.environ["GW2REM_DATA"]) / "models/4x-UltraSharp.safetensors" if "GW2REM_DATA" in os.environ else None,
        help="spandrel weights; 4x-UltraSharp is the one every texture of the pack is made with",
    )
    ap.add_argument("--max-side", type=int, default=2048)
    ap.add_argument("--max-factor", type=int, default=4)
    ap.add_argument("--min-side", type=int, default=64, help="skip tiny textures (icons/LUTs)")
    ap.add_argument("--tile", type=int, default=512)
    ap.add_argument("--pad", type=int, default=16)
    ap.add_argument("--no-wrap", action="store_true", help="reflect instead of wrap at borders")
    ap.add_argument("--fp32", action="store_true")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--ids", type=str, default="", help="comma-separated file ids")
    a = ap.parse_args()

    meta = {}
    with open(a.survey, newline="") as f:
        rows = (json.loads(l) for l in f) if a.survey.suffix == ".jsonl" else csv.DictReader(f)
        for r in rows:
            meta[int(r["file_id"])] = r
    ids = [int(x) for x in a.ids.split(",") if x] or sorted(int(p.stem) for p in a.dds_dir.glob("*.dds"))
    a.out.mkdir(parents=True, exist_ok=True)
    model = scale = None
    half = False
    model_sha = None
    if a.model:
        model, scale, half = load_model(a.model, not a.fp32)
        model_sha = hashlib.sha256(a.model.read_bytes()).hexdigest()
    done = {json.loads(l)["file_id"] for l in open(a.out / "manifest.jsonl")} if (a.out / "manifest.jsonl").exists() else set()
    man = open(a.out / "manifest.jsonl", "a")
    n = 0
    for fid in ids:
        if fid in done:
            continue
        m = meta.get(fid)
        src = a.dds_dir / f"{fid}.dds"
        if not m or not src.exists():
            continue
        w, h, fmt = int(m["width"]), int(m["height"]), m["format"]
        if max(w, h) < a.min_side:
            continue
        ts = target_size(w, h, a.max_side, a.max_factor)
        if not ts:
            continue
        t0 = time.time()
        im = Image.open(src)
        im.load()
        rgba = np.asarray(im.convert("RGBA"), np.float32) / 255.0
        if fmt in NORMAL:
            cls = "normal"
            rg = renormalize(lanczos(rgba[..., :2], ts))
            res = np.concatenate([rg, np.zeros_like(rg[..., :1]), np.ones_like(rg[..., :1])], -1)
        elif fmt in MASK:
            cls = "mask"
            res = lanczos(rgba, ts)
        elif fmt in COLOR and model is not None:
            cls = "color"
            rgb = run_tiled(model, scale, half, np.ascontiguousarray(rgba[..., :3]), a.tile, a.pad, not a.no_wrap)
            alpha = rgba[..., 3]
            if alpha.min() < 0.999:
                al = run_tiled(model, scale, half, np.repeat(alpha[..., None], 3, -1), a.tile, a.pad, not a.no_wrap).mean(-1, keepdims=True)
            else:
                al = np.ones(rgb.shape[:2] + (1,), np.float32)
            res = np.concatenate([rgb, al], -1)
            if res.shape[1] != ts[0] or res.shape[0] != ts[1]:
                res = lanczos(res, ts)
        else:
            continue
        png = a.out / f"{fid}.png"
        Image.fromarray((np.clip(res, 0, 1) * 255 + 0.5).astype(np.uint8), "RGBA").save(png)
        rec = {
            "file_id": fid, "class": cls, "format": fmt, "src": [w, h], "dst": list(ts),
            "model": a.model.name if (cls == "color" and a.model) else "lanczos", "model_sha256": model_sha if cls == "color" else None,
            "tile": a.tile, "pad": a.pad, "wrap": not a.no_wrap, "fp16": half, "seconds": round(time.time() - t0, 2),
            "src_sha256": hashlib.sha256(src.read_bytes()).hexdigest(),
        }
        man.write(json.dumps(rec) + "\n")
        man.flush()
        n += 1
        print(f"{fid} {cls} {w}x{h} -> {ts[0]}x{ts[1]} {rec['seconds']}s", flush=True)
        if a.limit and n >= a.limit:
            break


if __name__ == "__main__":
    main()
