"""Builds the whole GW2 Remastered HD pack from your own copy of the game. One command, resumable.

    python tools/build_pack.py --game "<Guild Wars 2 folder>" [--data <work folder>]

What it does (each step is skipped when its result already exists, so a rerun continues where it stopped):

  tools      build the Rust tools (gw2tex, texhash, texpack, texrdo)         needs Rust (rustup.rs)
  model      download the 4x-UltraSharp upscaler (checked against its SHA-256)
  survey     list every texture of Gw2.dat, hash them, find who uses them
  jobs       decide what gets upscaled and how far (tools/batch/policy.py)
  pack       upscale + encode: the main pack, then the 4x character packs  needs an NVIDIA GPU (CUDA)
  archive    move the finished textures into a few big volumes
  finish     pair_skip.txt and reasons.bin (consistency lists the mod reads)
  verify     compare the job lists with the reference (data/reference.json)

The pack is not shipped with the mod: it is generated from the textures of the game you own. The default work folder
holds about 100 GB of result and some temporary space; the game's own Gw2.dat (about 90 GB) is read in place, so
close the game first. Expect days of GPU time for the full pack; `--only`/`--skip` and Ctrl+C are safe.

Result: `<data>/pack` (point the mod's overlay at it, or set `pack_dir` in GW2Remastered/config.ini).
"""

import argparse
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
BATCH = REPO / "tools" / "batch"
EXE = ".exe" if sys.platform == "win32" else ""
MODEL_URL = "https://huggingface.co/Kim2091/UltraSharp/resolve/main/4x-UltraSharp.safetensors"
MODEL_SHA256 = "36a340b5509b699d2c06cb445ddc1d3d39199ac734d889ed6d7915f60e05bcbc"
STEPS = ["tools", "model", "survey", "jobs", "pack", "archive", "finish", "verify"]
NEED_FREE_GB = 130  # pack ~95 GB + temporary files while upscaling
RUST_TOOLS = ["gw2tex", "texhash", "texpack", "texrdo"]


def say(msg):
    print(f"\n=== {msg}", flush=True)


def run(cmd, retries=0, **kw):
    """Runs a command; `retries` covers files that are briefly locked right after being written (antivirus)."""
    print("  $", " ".join(str(c) for c in cmd), flush=True)
    for attempt in range(retries + 1):
        try:
            subprocess.run([str(c) for c in cmd], check=True, **kw)
            return
        except subprocess.CalledProcessError:
            if attempt == retries:
                raise
            print("  failed, trying again in 30 s", flush=True)
            time.sleep(30)


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for block in iter(lambda: f.read(1 << 20), b""):
            h.update(block)
    return h.hexdigest()


def game_dir(arg):
    cands = [arg, os.environ.get("GW2_DIR")] + [
        r"C:\Program Files\Guild Wars 2",
        r"C:\Program Files (x86)\Guild Wars 2",
        r"C:\Program Files (x86)\Steam\steamapps\common\Guild Wars 2",
        r"C:\Program Files\Steam\steamapps\common\Guild Wars 2",
    ]
    for c in cands:
        if c and (Path(c) / "Gw2-64.exe").is_file() and (Path(c) / "Gw2.dat").is_file():
            return Path(c)
    raise SystemExit("Guild Wars 2 not found: pass --game \"<folder with Gw2-64.exe and Gw2.dat>\" (or set GW2_DIR)")


def game_running():
    if sys.platform != "win32":
        return False
    out = subprocess.run(["tasklist", "/FI", "IMAGENAME eq Gw2-64.exe", "/NH"], capture_output=True, text=True).stdout
    return "Gw2-64.exe" in out


def step_tools(ctx):
    say("Rust tools")
    target = ctx.data / "cargo-target"
    if all((target / "release" / f"{t}{EXE}").exists() for t in RUST_TOOLS) and not ctx.rebuild:
        print("  already built")
        return
    if not shutil.which("cargo"):
        raise SystemExit("Rust is needed to build the tools: install it from https://rustup.rs and run again")
    env = dict(os.environ, CARGO_TARGET_DIR=str(target))
    for t in RUST_TOOLS:
        run(["cargo", "build", "--release", "--manifest-path", REPO / "tools" / t / "Cargo.toml"], env=env)


def step_model(ctx):
    say("Upscaler model (4x-UltraSharp, about 64 MB)")
    dst = ctx.data / "models" / "4x-UltraSharp.safetensors"
    dst.parent.mkdir(parents=True, exist_ok=True)
    if dst.exists() and sha256(dst) == MODEL_SHA256:
        print("  already there")
        return
    part = dst.with_suffix(".part")
    print(f"  downloading {MODEL_URL}")
    urllib.request.urlretrieve(MODEL_URL, part)
    if sha256(part) != MODEL_SHA256:
        part.unlink()
        raise SystemExit("the downloaded model does not match the expected SHA-256; not using it")
    part.replace(dst)


def step_survey(ctx):
    say("Survey of Gw2.dat (texture list, hashes, usage)")
    T = ctx.textures
    T.mkdir(parents=True, exist_ok=True)
    tool = lambda n: ctx.data / "cargo-target" / "release" / f"{n}{EXE}"  # noqa: E731
    dat, exe = ctx.dat, ctx.game / "Gw2-64.exe"
    if not (T / "survey.jsonl").exists():
        run([tool("gw2tex"), "--dat", dat, "survey", "--out", T / "survey.jsonl"])
    if not (T / "texture_hashes.jsonl").exists():
        run([tool("texhash"), dat, T / "survey.jsonl", T / "texture_hashes.jsonl", "8"])
    if not (T / "models.jsonl").exists():
        run([tool("gw2tex"), "--dat", dat, "models", "--exe", exe, "--out", T / "models.jsonl"])
    if not (T / "refs.jsonl").exists():
        run([tool("gw2tex"), "--dat", dat, "refs", "--exe", exe, "--kinds", T / "mft_kinds.jsonl", "--out", T / "refs.jsonl"])
    if not (T / "texture_class_hints.jsonl").exists():
        run([sys.executable, "-I", REPO / "tools" / "gw2tex" / "scripts" / "texture_hints.py", T], retries=2)


def step_jobs(ctx):
    say("Job lists (what to upscale)")
    run([sys.executable, BATCH / "build_jobs.py", ctx.textures, ctx.textures / "jobs.jsonl"], env=ctx.env)


def step_pack(ctx):
    say("Upscaling (the long step; stop with Ctrl+C and run again to continue)")
    try:
        import torch  # noqa: F401
    except ImportError:
        raise SystemExit("Python packages missing: run  pip install -e \"" + str(REPO) + "[batch]\"  (PyTorch with CUDA: pytorch.org)")
    import torch

    if not torch.cuda.is_available():
        raise SystemExit("no CUDA GPU found: the upscaler needs an NVIDIA GPU with a CUDA build of PyTorch (pytorch.org)")
    T = ctx.textures
    extra = ["--duty", str(ctx.duty)] + (["--limit", str(ctx.limit)] if ctx.limit else [])
    for jobs, pack in (
        ("jobs.jsonl", ctx.pack),
        ("jobs_composite_4x.jsonl", ctx.pack / "composite_4x"),
        ("jobs_character_4x.jsonl", ctx.pack / "character_4x"),
    ):
        if jobs != "jobs.jsonl" and ctx.skip_4x:
            continue
        say(f"pack: {jobs} -> {pack}")
        run([sys.executable, BATCH / "run_batch.py", T / jobs, pack, "--chunk", "400", *extra], env=ctx.env)


def step_archive(ctx):
    say("Archive into volumes (the game must be closed)")
    for p in (ctx.pack, ctx.pack / "composite_4x"):
        if (p / "index.jsonl").exists():
            run([sys.executable, BATCH / "pack_archive.py", "add", p, "--delete"], env=ctx.env)
    # character_4x stays as loose files: it is small (about 2,000 textures)


def step_finish(ctx):
    say("Consistency lists")
    run([sys.executable, BATCH / "check_groups.py"], env=ctx.env)
    run([sys.executable, BATCH / "build_reasons.py"], env=ctx.env)


def count_lines(p: Path) -> int:
    return sum(1 for _ in open(p, encoding="utf-8")) if p.exists() else 0


def step_verify(ctx):
    say("Verify against the reference")
    ref = json.loads((REPO / "data" / "reference.json").read_text())
    T = ctx.textures
    ok = True
    exe_hash = sha256(ctx.game / "Gw2-64.exe")
    same_build = exe_hash == ref["gw2_exe_sha256"]
    print(f"  game build: {'same as the reference' if same_build else 'DIFFERENT from the reference (a patch changed things; counts may differ)'}")
    for name, key in (("jobs.jsonl", "jobs"), ("jobs_composite_4x.jsonl", "composite_4x"), ("jobs_character_4x.jsonl", "character_4x")):
        n = count_lines(T / name)
        good = n == ref["job_counts"][key]
        ok &= good or not same_build
        print(f"  {name}: {n} jobs (reference {ref['job_counts'][key]}) {'ok' if good else 'DIFFERENT'}")
    if same_build and (T / "jobs.jsonl").exists():
        h = hashlib.sha256()
        for line in sorted(open(T / "jobs.jsonl", encoding="utf-8")):  # the order of the lines is not significant
            h.update(line.encode())
        good = h.hexdigest() == ref["jobs_sorted_sha256"]
        ok &= good
        print(f"  jobs.jsonl content: {'identical to the reference' if good else 'DIFFERENT from the reference'}")
    for label, p in (("pack", ctx.pack), ("composite_4x", ctx.pack / "composite_4x"), ("character_4x", ctx.pack / "character_4x")):
        idx = p / "pack.idx"
        n = (idx.stat().st_size - 8) // 28 if idx.exists() else 0
        n += count_lines(p / "index.jsonl")
        print(f"  {label}: {n} textures (reference ~{ref['pack_textures'][label]})")
    print("  Note: the AI upscale runs on your GPU; pixel values can differ in the last bits between GPUs/drivers.")
    if not ok:
        raise SystemExit("verification found differences (see above)")
    print("  ok")


FUNCS = {
    "tools": step_tools, "model": step_model, "survey": step_survey, "jobs": step_jobs,
    "pack": step_pack, "archive": step_archive, "finish": step_finish, "verify": step_verify,
}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--game", help="Guild Wars 2 folder (with Gw2-64.exe and Gw2.dat); default: GW2_DIR or the usual places")
    ap.add_argument("--data", type=Path, help="work folder (default: GW2REM_DATA or ~/GW2RemasteredData)")
    ap.add_argument("--pack", type=Path, help="where the finished pack goes (default: <data>/pack)")
    ap.add_argument("--copy-dat", action="store_true", help="work on a copy of Gw2.dat (needs 90+ GB more; lets you play meanwhile)")
    ap.add_argument("--only", nargs="+", choices=STEPS, help="run only these steps")
    ap.add_argument("--skip", nargs="+", choices=STEPS, default=[], help="skip these steps")
    ap.add_argument("--skip-4x", action="store_true", help="do not build the 4x character packs (about 30 GB less)")
    ap.add_argument("--duty", type=float, default=1.0, help="GPU duty cycle 0..1 (lower to keep the PC usable meanwhile)")
    ap.add_argument("--rebuild-tools", dest="rebuild", action="store_true", help="rebuild the Rust tools even if present")
    ap.add_argument("--limit", type=int, default=0, help="test run: only this many textures per pack (0 = all)")
    ap.add_argument("--yes", "-y", action="store_true", help="do not ask for confirmation")
    a = ap.parse_args()

    ctx = argparse.Namespace()
    ctx.game = game_dir(a.game)
    ctx.data = (a.data or Path(os.environ.get("GW2REM_DATA") or Path.home() / "GW2RemasteredData")).resolve()
    ctx.pack = (a.pack or ctx.data / "pack").resolve()
    ctx.textures = ctx.data / "textures"
    ctx.duty, ctx.skip_4x, ctx.rebuild, ctx.limit = a.duty, a.skip_4x, a.rebuild, a.limit
    ctx.env = dict(os.environ, GW2REM_DATA=str(ctx.data), PYTHONUTF8="1")
    steps = [s for s in (a.only or STEPS) if s not in a.skip]

    ctx.data.mkdir(parents=True, exist_ok=True)
    (ctx.data / "tmp").mkdir(exist_ok=True)
    free_gb = shutil.disk_usage(ctx.data).free / 2**30
    print(f"game:  {ctx.game}\ndata:  {ctx.data}   ({free_gb:.0f} GB free)\npack:  {ctx.pack}\nsteps: {', '.join(steps)}")
    if {"pack", "archive"} & set(steps) and free_gb < NEED_FREE_GB and not (ctx.pack / "index.jsonl").exists():
        print(f"  WARNING: about {NEED_FREE_GB} GB are needed; only {free_gb:.0f} GB free")
    if not a.yes and input("Continue? [y/N] ").strip().lower() not in ("y", "yes", "s", "sim"):
        raise SystemExit("cancelled")

    if game_running() and ("archive" in steps or (not a.copy_dat and {"survey", "pack"} & set(steps))):
        raise SystemExit("close Guild Wars 2 first: it keeps Gw2.dat locked and the archive step edits files it reads"
                         " (--copy-dat lets you play while the upscale runs)")
    if a.copy_dat:
        ctx.dat = ctx.data / "Gw2.dat"
        if not ctx.dat.exists() or ctx.dat.stat().st_size != (ctx.game / "Gw2.dat").stat().st_size:
            say("Copying Gw2.dat")
            shutil.copyfile(ctx.game / "Gw2.dat", ctx.dat)
    else:
        ctx.dat = ctx.game / "Gw2.dat"
    ctx.env["GW2REM_DAT"] = str(ctx.dat)  # run_batch extracts from here
    for s in steps:
        FUNCS[s](ctx)
    say("done")


if __name__ == "__main__":
    main()
