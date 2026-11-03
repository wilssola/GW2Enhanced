"""Resumable remaster batch: jobs.jsonl -> <pack_dir> (DDS + RDO16 + zstd).

Per chunk of N jobs:
  1. gw2tex extract (ATEX -> DDS, original blocks)        [CPU]
  2. color: AI 4x (spandrel, tiles in batches) -> Lanczos to the job factor  [GPU]
     mask: Lanczos per channel; normal: Lanczos RG + renormalize
  3. texpack: original format + level rule                 [CPU, parallel]
  4. texrdo apply 16: block-reuse RDO + zstd-19            [CPU, parallel]
  5. move *.dds.zst into the pack, append index lines, delete temp files
Done ids = ids present in pack/index.jsonl, so a rerun continues where it stopped.

Usage: run_batch.py <jobs.jsonl> <pack_dir> [--chunk 400] [--limit N] [--group composite]
"""
import argparse
from collections import deque
import concurrent.futures
import json
import os
from concurrent.futures import ProcessPoolExecutor, ThreadPoolExecutor
from concurrent.futures.process import BrokenProcessPool
import faulthandler
import re
import shutil
import subprocess
import sys
import threading
import time
from pathlib import Path

import numpy as np
from PIL import Image
import torch

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "upscale"))
sys.path.insert(0, str(Path(__file__).resolve().parent))
from policy import already_high  # noqa: E402
from upscale import load_model, renormalize, run_tiled  # noqa: E402

if "GW2REM_DATA" not in os.environ:
    raise SystemExit("set GW2REM_DATA to the data folder (see README)")
ROOT = Path(os.environ["GW2REM_DATA"])
GW2TEX = ROOT / "cargo-target/release/gw2tex.exe"
TEXPACK = ROOT / "cargo-target/release/texpack.exe"
TEXRDO = ROOT / "cargo-target/release/texrdo.exe"
DAT = ROOT / "Gw2.dat"
TEX = ROOT / "textures"
MODEL = ROOT / "models/4x-UltraSharp.safetensors"
RDO = "16"
TILE, PAD = 512, 16
BATCH_MPIX = 0.11  # pixels per GPU batch with padding. Measured: small tiles gain up to 4x
# from batching (64 px: 8 at a time), but from ~256 px up larger batches only lose speed.
EXTRACT_AHEAD = 2  # chunks extracted ahead of the GPU
DECODE_AHEAD = 32  # textures decoded ahead of the GPU
PACK_WORKERS = 2  # chunks packing at once (each tool is multi-threaded)
MAX_PACKING = 3  # GPU thread waits above this many chunks packing/queued
DISK_LOW = 20 << 30  # below this, one chunk at a time on disk
DISK_STOP = 5 << 30  # below this, stop cleanly


def lanczos_ch(a, size):
    return np.stack([np.asarray(Image.fromarray(a[..., c]).resize(size, Image.LANCZOS)) for c in range(a.shape[2])], -1)


def finish_color(rgb8, alpha, size, dst: Path):
    """CPU tail of a colour job (resize to the job size + PNG), run off the GPU thread."""
    if rgb8.shape[1] != size[0] or rgb8.shape[0] != size[1]:
        rgb8 = lanczos_ch(rgb8, size)
    res = np.concatenate([rgb8, lanczos_ch(alpha, size)], -1)
    Image.fromarray(res.astype(np.uint8), "RGBA").save(dst, compress_level=1)


def process_cpu(job, src: Path, dst: Path):
    """Mask/normal job: no AI, pure CPU (runs in the process pool)."""
    a = np.asarray(Image.open(src).convert("RGBA"))
    h, w = a.shape[:2]
    f = job["factor"]
    size = (w * f, h * f)
    if job["role"] == "normal":
        rg = lanczos_ch(a[..., :2], size).astype(np.float32) / 255
        rg = (renormalize(rg) * 255 + 0.5).astype(np.uint8)
        res = np.concatenate([rg, lanczos_ch(a[..., 2:], size)], -1)
    else:
        res = lanczos_ch(a, size)
    Image.fromarray(res.astype(np.uint8), "RGBA").save(dst, compress_level=1)


def is_ai(job, src: Path) -> bool:
    if job["role"] != "color":
        return False
    with Image.open(src) as im:
        return min(im.size) >= 8


def process(job, src: Path, dst: Path, model, scale, half):
    """Single-threaded path (kept for repair_rdo and tests)."""
    if not is_ai(job, src):
        return process_cpu(job, src, dst)
    a = np.asarray(Image.open(src).convert("RGBA"))
    h, w = a.shape[:2]
    size = (w * job["factor"], h * job["factor"])
    rgb = run_tiled(model, scale, half, a[..., :3].astype(np.float32) / 255, 512, 16, True)
    finish_color((np.clip(rgb, 0, 1) * 255 + 0.5).astype(np.uint8), a[..., 3:4], size, dst)


BELOW_NORMAL = 0x4000  # Windows priority class
TMP_NAME = re.compile(r"batch\d+$")


def run(cmd):
    # CPU tools yield to the thread feeding the GPU, but still take every idle core.
    r = subprocess.run([str(c) for c in cmd], capture_output=True, text=True, creationflags=WORKER_FLAGS)
    if r.returncode != 0:
        raise RuntimeError(f"{cmd[0]} failed: {r.stderr[-500:]}")
    return r


WORKER_FLAGS = 0


def lower_priority():
    if sys.platform == "win32":
        import ctypes
        ctypes.windll.kernel32.SetPriorityClass(ctypes.windll.kernel32.GetCurrentProcess(), BELOW_NORMAL)


def worker_init(lower: bool):
    """Pool worker: native crashes leave a trace (a dead worker says nothing otherwise)."""
    faulthandler.enable(open(ROOT / "tmp" / "worker_faults.txt", "a"))
    if lower:
        lower_priority()


def decode(job, src: Path, pad: int):
    """Worker thread: RGBA -> (job, wrap-padded RGB uint8, alpha, w, h), None = no AI."""
    if not is_ai(job, src):
        return job, None, None, 0, 0
    a = np.asarray(Image.open(src).convert("RGBA"))
    h, w = a.shape[:2]
    return job, np.pad(a[..., :3], ((pad, pad), (pad, pad), (0, 0)), mode="wrap"), a[..., 3:4].copy(), w, h


class TileBatcher:
    """Groups tiles of the same shape, from any texture, into GPU batches.

    Same tiling as upscale.run_tiled (wrap padding, only the tile core is kept), so
    results match it; the model has no batch-dependent layers. A texture is handed to
    `done(item, rgb8)` when its last tile returns. `limit_px` follows free VRAM: an
    out-of-memory error (e.g. the game takes the card) halves it and retries.
    """

    def __init__(self, model, scale, half, pad, tile, limit_px, done, fail, say):
        self.m, self.s, self.half, self.pad, self.tile = model, scale, half, pad, tile
        self.limit_px, self.done, self.fail, self.say = limit_px, done, fail, say
        self.buckets = {}
        self.live = {}

    def add(self, item, src, w, h):
        st = self.s
        t, pad = self.tile, self.pad
        # Full tile count up front: a batch can return before the last tile is queued.
        count = ((h + t - 1) // t) * ((w + t - 1) // t)
        slot = [item, np.empty((h * st, w * st, 3), np.uint8), count]
        self.live[id(item)] = slot
        for y in range(0, h, t):
            for x in range(0, w, t):
                th, tw = min(t, h - y), min(t, w - x)
                b = self.buckets.setdefault((th, tw), [])
                b.append((slot, y, x, src[y : y + th + 2 * pad, x : x + tw + 2 * pad]))
                if len(b) * (th + 2 * pad) * (tw + 2 * pad) >= self.limit_px:
                    self.flush((th, tw))

    def flush(self, key):
        b = self.buckets.pop(key, None)
        while b:
            th, tw = key
            n = max(1, self.limit_px // ((th + 2 * self.pad) * (tw + 2 * self.pad)))
            self.infer(key, b[:n])
            b = b[n:]

    def flush_all(self):
        for k in list(self.buckets):
            self.flush(k)

    @torch.inference_mode()
    def forward(self, patches, pad_px, th, tw):
        x = torch.from_numpy(np.stack(patches)).permute(0, 3, 1, 2).to("cuda", non_blocking=True).float().div_(255)
        if self.half:
            x = x.half()
        y = self.m(x).float().clamp_(0, 1).mul_(255).add_(0.5).to(torch.uint8)
        return y[:, :, pad_px : pad_px + th * self.s, pad_px : pad_px + tw * self.s].permute(0, 2, 3, 1).cpu().numpy()

    def infer(self, key, items):
        th, tw = key
        try:
            res = self.forward([i[3] for i in items], self.pad * self.s, th, tw)
        except torch.OutOfMemoryError:
            torch.cuda.empty_cache()
            if len(items) > 1:
                self.limit_px = max(20000, self.limit_px // 2)
                self.say(f"  GPU memory full: batch -> {self.limit_px / 1e6:.2f} MPix")
                mid = len(items) // 2
                self.infer(key, items[:mid])
                self.infer(key, items[mid:])
                return
            time.sleep(5)  # one tile does not fit: the game holds the VRAM, give it a moment
            try:
                res = self.forward([i[3] for i in items], self.pad * self.s, th, tw)
            except torch.OutOfMemoryError:
                torch.cuda.empty_cache()
                self.drop(items[0][0])
                return
        st = self.s
        for (slot, y, x, _), core in zip(items, res):
            slot[1][y * st : (y + th) * st, x * st : (x + tw) * st] = core
            slot[2] -= 1
            if slot[2] == 0 and self.live.pop(id(slot[0]), None) is not None:
                self.done(slot[0], slot[1])

    def drop(self, slot):
        if self.live.pop(id(slot[0]), None) is not None:
            self.fail(slot[0])


def main():
    global WORKER_FLAGS
    ap = argparse.ArgumentParser()
    ap.add_argument("jobs", type=Path)
    ap.add_argument("pack", type=Path)
    ap.add_argument("--chunk", type=int, default=400)
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--group", default="")
    # GPU duty cycle (0..1]: after each AI batch, sleep so the GPU is busy only this
    # fraction of the time; leaves room for the game when the user plays meanwhile.
    ap.add_argument("--duty", type=float, default=1.0)
    ap.add_argument("--low", action="store_true", help="idle CPU priority (inherited by the tools it runs)")
    ap.add_argument("--unattended", action="store_true", help="nobody at the PC: CPU tools at normal priority")
    ap.add_argument("--batch-mpix", type=float, default=BATCH_MPIX, help="pixels per GPU batch, millions (shrinks on out-of-memory)")
    a = ap.parse_args()
    if a.low and sys.platform == "win32":
        import ctypes
        ctypes.windll.kernel32.SetPriorityClass(ctypes.windll.kernel32.GetCurrentProcess(), 0x40)  # IDLE_PRIORITY_CLASS
    elif sys.platform == "win32" and not a.unattended:
        WORKER_FLAGS = BELOW_NORMAL
    if not a.low and sys.platform == "win32":
        import ctypes
        # ES_CONTINUOUS | ES_SYSTEM_REQUIRED: no idle sleep; ends with this process.
        ctypes.windll.kernel32.SetThreadExecutionState(0x80000000 | 0x00000001)

    a.pack.mkdir(parents=True, exist_ok=True)
    index = a.pack / "index.jsonl"
    done = set()
    if index.exists():
        for l in open(index):
            try:
                done.add(json.loads(l)["file_id"])
            except Exception:
                pass
    if index.exists() and index.stat().st_size:
        with open(index, "rb+") as f:  # an interrupted append must not glue onto the next line
            f.seek(-1, 2)
            if f.read(1) != bytes([10]):
                f.write(bytes([10]))
    jobs = [json.loads(l) for l in open(a.jobs)]
    todo = [j for j in jobs if j["file_id"] not in done and (not a.group or j["group"] == a.group)]
    high = [j for j in todo if already_high(j)]
    todo = [j for j in todo if not already_high(j)]  # never upscale what is already high resolution
    if a.limit:
        todo = todo[: a.limit]
    log = open(a.pack / "batch_log.txt", "a", encoding="utf-8")
    log_lock = threading.Lock()

    def say(msg):
        line = f"[{time.strftime('%Y-%m-%d %H:%M:%S')}] {msg}"
        with log_lock:
            print(line, flush=True)
            log.write(line + "\n")
            log.flush()

    # lists the DLL uses: composite pieces and paired device textures
    (a.pack / "composite_ids.txt").write_text("\n".join(str(j["file_id"]) for j in jobs if j["group"] == "composite"))
    (a.pack / "device_ids.txt").write_text("\n".join(str(j["file_id"]) for j in jobs if j["group"] not in ("composite", "terrain")))
    # terrain tiles (ATEP) are CPU textures of a terrain atlas, not device textures: kept apart
    (a.pack / "terrain_ids.txt").write_text("\n".join(str(j["file_id"]) for j in jobs if j["group"] == "terrain"))

    say(f"start: {len(todo)} jobs to do ({len(done)} already in pack)" + (f", {len(high)} skipped: source already at the target size" if high else ""))
    if not todo:
        return
    for d in (ROOT / "tmp").iterdir():  # leftovers of an interrupted run
        if d.is_dir() and TMP_NAME.match(d.name):
            shutil.rmtree(d, ignore_errors=True)
    model, scale, half = load_model(MODEL, True)
    # Channels-last weights: ~1.35x faster on this GPU with identical output (the batches
    # are already built channel-last, so no layout conversion happens per tile either).
    model = model.to(memory_format=torch.channels_last)
    # Pipeline with bounded queues, so neither side waits for the other: the GPU thread
    # only ever waits when nothing is decoded yet or too much is waiting to be packed.
    # Decoding, extraction, masks/normals, PNG and packing fill every free CPU core at
    # below-normal priority, which keeps the GPU fed first.
    ncpu = os.cpu_count() or 8
    pool_lock = threading.Lock()
    cpu_pool = [ProcessPoolExecutor(ncpu, initializer=worker_init, initargs=(not (a.low or a.unattended),))]

    def submit_cpu(*args):
        """A dead worker breaks the whole pool: replace it and carry on."""
        with pool_lock:
            try:
                return cpu_pool[0].submit(process_cpu, *args)
            except (BrokenProcessPool, RuntimeError, OSError):
                say("  worker pool broke: starting a new one")
                cpu_pool[0].shutdown(wait=False, cancel_futures=True)
                cpu_pool[0] = ProcessPoolExecutor(ncpu, initializer=worker_init, initargs=(not (a.low or a.unattended),))
                return cpu_pool[0].submit(process_cpu, *args)
    tail_pool = ThreadPoolExecutor(ncpu // 2)
    decode_pool = ThreadPoolExecutor(ncpu // 2)
    extract_pool = ThreadPoolExecutor(1)
    pack_pool = ThreadPoolExecutor(PACK_WORKERS)
    chunks = [todo[c : c + a.chunk] for c in range(0, len(todo), a.chunk)]
    t0 = time.time()
    state = {"done": 0, "left": len(todo)}
    index_lock = threading.Lock()

    def tmpdir(k):
        return ROOT / "tmp" / f"batch{k}"

    def extract(k):
        d = tmpdir(k)
        shutil.rmtree(d, ignore_errors=True)
        for sub in ("dds", "png", "pack", "zst"):
            (d / sub).mkdir(parents=True)
        run([GW2TEX, "--dat", DAT, "extract", "--ids", ",".join(str(j["file_id"]) for j in chunks[k]), "--out", d / "dds"])

    def finalize(k, futs):
        d = tmpdir(k)
        ok = 0
        for j, fu in futs:
            try:
                try:
                    fu.result()
                except BrokenProcessPool:  # lost with the pool, not this texture's fault
                    submit_cpu(j, d / "dds" / f"{j['file_id']}.dds", d / "png" / f"{j['file_id']}.png").result()
                ok += 1
            except Exception as e:
                say(f"  skip {j['file_id']}: {e}")
        try:
            run([TEXPACK, d / "png", d / "pack", TEX / "texture_hashes.jsonl", TEX / "survey.jsonl"])
            run([TEXRDO, "apply", d / "pack", d / "zst", RDO])
            lines = (d / "zst" / "index.jsonl").read_text().splitlines() if (d / "zst" / "index.jsonl").exists() else []
            for zf in (d / "zst").glob("*.dds.zst"):
                # On disk for real before the index names it (power cuts).
                with open(zf, "r+b") as f:
                    os.fsync(f.fileno())
                shutil.move(str(zf), a.pack / zf.name)
            with index_lock, open(index, "a") as f:
                for l in lines:
                    f.write(json.dumps(json.loads(l), separators=(",", ":")) + "\n")
                f.flush()
                os.fsync(f.fileno())
        except Exception as e:
            say(f"chunk {k} failed: {e}")
            shutil.rmtree(d, ignore_errors=True)
            return
        with index_lock:
            state["done"] += len(lines)
            state["left"] -= len(chunks[k])
            rate = state["done"] / (time.time() - t0)
            left = state["left"]
            total = state["done"]
        say(f"chunk {k + 1}/{len(chunks)}: +{len(lines)} (ok {ok}/{len(chunks[k])}), "
            f"{total} done, {rate:.1f}/s, ETA {left / rate / 3600 if rate else 0:.1f} h")
        shutil.rmtree(d, ignore_errors=True)

    pending_extract = {}
    packing = []

    def lookahead(k):
        for n in range(k, min(k + EXTRACT_AHEAD + 1, len(chunks))):
            if n not in pending_extract:
                pending_extract[n] = extract_pool.submit(extract, n)

    def limit_backlog():
        """Bounds temp files and RAM; with the disk nearly full, packs everything first."""
        while True:
            packing[:] = [f for f in packing if not f.done()]
            free = shutil.disk_usage(ROOT).free
            if free < DISK_STOP:
                concurrent.futures.wait(packing)
                raise SystemExit(say(f"stopped: only {free >> 30} GB free on the data disk"))
            if len(packing) < MAX_PACKING and (free > DISK_LOW or not packing):
                return
            concurrent.futures.wait(packing, return_when=concurrent.futures.FIRST_COMPLETED)

    for k in range(len(chunks)):
        limit_backlog()
        lookahead(k)
        try:
            pending_extract.pop(k).result()
        except Exception as e:
            say(f"chunk {k} extract failed: {e}")
            continue
        d = tmpdir(k)
        futs = []
        by_id = {}

        def on_done(item, rgb8):
            j, alpha, w, h = item
            by_id[j["file_id"]].set_result(
                tail_pool.submit(finish_color, rgb8, alpha, (w * j["factor"], h * j["factor"]), d / "png" / f"{j['file_id']}.png")
            )

        def on_fail(item):
            by_id[item[0]["file_id"]].set_exception(RuntimeError("GPU out of memory"))

        def wrap(j):
            # Resolved by the batcher with the tail's future.
            f = concurrent.futures.Future()
            by_id[j["file_id"]] = f
            futs.append((j, _Chained(f)))

        batcher = TileBatcher(model, scale, half, PAD, TILE, int(a.batch_mpix * 1e6), on_done, on_fail, say)
        ready = deque()

        def consume(fu):
            try:
                j, src, alpha, w, h = fu.result()
            except Exception as e:
                say(f"  skip: {e}")
                return
            if src is None:
                futs.append((j, submit_cpu(j, d / "dds" / f"{j['file_id']}.dds", d / "png" / f"{j['file_id']}.png")))
                return
            wrap(j)
            t_ai = time.time()
            batcher.add((j, alpha, w, h), src, w, h)
            if a.duty < 1.0:
                time.sleep((time.time() - t_ai) * (1.0 / max(a.duty, 0.05) - 1.0))

        for j in chunks[k]:
            src = d / "dds" / f"{j['file_id']}.dds"
            if not src.exists():
                continue
            ready.append(decode_pool.submit(decode, j, src, PAD))
            if len(ready) >= DECODE_AHEAD:
                consume(ready.popleft())
        while ready:
            consume(ready.popleft())
        batcher.flush_all()
        packing.append(pack_pool.submit(finalize, k, futs))
    concurrent.futures.wait(packing)
    for pool in (cpu_pool[0], tail_pool, decode_pool, extract_pool, pack_pool):
        pool.shutdown()
    for d in (ROOT / "tmp").iterdir():
        if d.is_dir() and TMP_NAME.match(d.name):
            shutil.rmtree(d, ignore_errors=True)
    say("finished")


class _Chained:
    """Waits for the batcher's future, then for the tail job it resolved to."""

    def __init__(self, f):
        self.f = f

    def result(self):
        return self.f.result().result()


if __name__ == "__main__":
    main()
