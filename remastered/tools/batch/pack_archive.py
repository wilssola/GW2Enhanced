"""A pack's HD textures as a few large files instead of one file per texture.

Opening one file per texture is what makes loading slow on a normal PC: every open can
be scanned by the antivirus. The DLL opens each volume once and reads by offset.

  pack_00000.bin, pack_00001.bin...  zstd frames, one after the other
  pack.idx  magic "GW2RPAK2", then per texture (little endian): u32 file id, u32 volume,
            u32 size, u32 original file size, u32 width, u64 offset

Commands (with the game closed; index.jsonl stays what the batch uses to know what is done):
  add <pack> [--delete] [--new-volume] [--max-gb N]
      Moves finished loose files in. A loose file for a texture already archived
      replaces it, unless identical. --new-volume keeps this update in its own files, so
      users only download those plus pack.idx.
  redo <pack> --ids 1,2,3 | --ids-file ids.txt
      Makes the batch produce these again. The archived version keeps being served until
      the new one is added.
  compact <pack>
      Removes the space left by replaced textures, one volume at a time.

Safe against power cuts: data is on disk before the index names it, the index is replaced
atomically, and anything no index names is from an interrupted run and gets discarded.
Loose files are deleted only after being read back from the archive and compared.
"""
import argparse
import json
import os
import re
import struct
import subprocess
import sys
from pathlib import Path

MAGIC = b"GW2RPAK2"
OLD_MAGIC = b"GW2RPAK1"
RECORD = struct.Struct("<IIIIIQ")  # id, volume, size, src_bytes, width, offset
OLD_RECORD = struct.Struct("<IIQII")  # id, size, offset, src_bytes, width
DEFAULT_MAX_GB = 1.9  # stays under the usual 2 GB limit per file on download hosts
COMMIT_BYTES = 1 << 30
VOLUME = re.compile(r"pack_(\d{5})\.bin$")


def volume_name(n: int) -> str:
    return f"pack_{n:05}.bin"


class Archive:
    def __init__(self, pack: Path, max_bytes: int = int(DEFAULT_MAX_GB * 2**30)):
        self.pack, self.max_bytes = pack, max_bytes
        self.idx = pack / "pack.idx"
        self.entries = {}  # id -> (id, volume, size, src_bytes, width, offset)
        self.out = None
        self.vol = -1
        self.pending = []  # (id, volume, offset, bytes, loose path or None) not yet committed
        self.pending_bytes = 0
        self.delete_loose = False
        self._migrate_old_format()
        if self.idx.exists():
            data = self.idx.read_bytes()
            if data[:8] != MAGIC or (len(data) - 8) % RECORD.size:
                raise SystemExit(f"{self.idx}: not a valid index")
            self.entries = {r[0]: r for r in RECORD.iter_unpack(data[8:])}
        self._discard_orphans()

    def _migrate_old_format(self):
        """One pack.bin from the first version becomes volume 0, without copying data."""
        if not self.idx.exists() or self.idx.read_bytes()[:8] != OLD_MAGIC:
            return
        old, data = self.pack / "pack.bin", self.idx.read_bytes()
        first = self.pack / volume_name(0)
        if not first.exists():
            os.link(old, first)
        self.entries = {r[0]: (r[0], 0, r[1], r[3], r[4], r[2]) for r in OLD_RECORD.iter_unpack(data[8:])}
        self._write_index()
        old.unlink()
        self.entries = {}

    def volumes(self):
        return sorted(int(m.group(1)) for f in os.scandir(self.pack) if (m := VOLUME.match(f.name)))

    def _ends(self):
        ends = {}
        for r in self.entries.values():
            ends[r[1]] = max(ends.get(r[1], 0), r[5] + r[2])
        return ends

    def _discard_orphans(self):
        ends = self._ends()
        for n in self.volumes():
            if n not in ends:
                (self.pack / volume_name(n)).unlink()
            elif os.path.getsize(self.pack / volume_name(n)) > ends[n]:
                with open(self.pack / volume_name(n), "r+b") as f:
                    f.truncate(ends[n])

    def _write_index(self):
        tmp = self.idx.with_suffix(".idx.tmp")
        with open(tmp, "wb") as f:
            f.write(MAGIC)
            for r in sorted(self.entries.values(), key=lambda r: r[0]):
                f.write(RECORD.pack(*r))
            f.flush()
            os.fsync(f.fileno())
        os.replace(tmp, self.idx)

    def _open_volume(self, n: int):
        if self.out:
            self.out.flush()
            os.fsync(self.out.fileno())
            self.out.close()
        path = self.pack / volume_name(n)
        self.out = open(path, "r+b" if path.exists() else "w+b")
        self.out.seek(0, 2)
        self.vol = n

    def start(self, new_volume: bool = False):
        vols = self.volumes()
        if not vols:
            self._open_volume(0)
        elif new_volume:
            self._open_volume(vols[-1] + 1)
        else:
            self._open_volume(vols[-1])

    def append(self, file_id: int, data: bytes, src_bytes: int, width: int, loose: Path | None = None):
        if self.out.tell() and self.out.tell() + len(data) > self.max_bytes:
            self.commit()
            self._open_volume(self.vol + 1)
        off = self.out.tell()
        self.out.write(data)
        self.entries[file_id] = (file_id, self.vol, len(data), src_bytes, width, off)
        self.pending.append((file_id, self.vol, off, data, loose))
        self.pending_bytes += len(data)
        if self.pending_bytes >= COMMIT_BYTES:
            self.commit()

    def commit(self):
        if self.out:
            self.out.flush()
            os.fsync(self.out.fileno())
        self._write_index()
        for file_id, vol, off, data, loose in self.pending:
            if self.read(vol, off, len(data)) != data:
                raise SystemExit(f"{file_id}: archive read-back differs, nothing else deleted")
            if loose and self.delete_loose:
                loose.unlink()
        self.pending, self.pending_bytes = [], 0

    def read(self, vol: int, off: int, size: int) -> bytes:
        if self.out and vol == self.vol:
            pos = self.out.tell()
            self.out.seek(off)
            data = self.out.read(size)
            self.out.seek(pos)
            return data
        with open(self.pack / volume_name(vol), "rb") as f:
            f.seek(off)
            return f.read(size)

    def close(self):
        self.commit()
        if self.out:
            self.out.close()
            self.out = None
        # Volumes nothing points to any more, and a new volume nothing was added to.
        self._discard_orphans()


def read_meta(pack: Path) -> dict:
    meta = {}
    for l in open(pack / "index.jsonl"):
        try:
            j = json.loads(l)
            meta[j["file_id"]] = (j.get("src_bytes", 0), j.get("width", 0))
        except (ValueError, KeyError):
            pass
    return meta


def cmd_add(a):
    ar = Archive(a.pack, int(a.max_gb * 2**30))
    ar.delete_loose = a.delete
    meta = read_meta(a.pack)
    loose = {int(f.name.split(".")[0]) for f in os.scandir(a.pack) if f.name.endswith(".dds.zst")}
    todo = sorted(loose & set(meta))
    print(f"{a.pack}: {len(ar.entries)} in archive, {len(todo)} loose", flush=True)
    ar.start(a.new_volume)
    added = replaced = same = 0
    for n, i in enumerate(todo, 1):
        path = a.pack / f"{i}.dds.zst"
        data = path.read_bytes()
        old = ar.entries.get(i)
        if old and old[2] == len(data) and ar.read(old[1], old[5], old[2]) == data:
            same += 1
            if a.delete:
                path.unlink()
            continue
        replaced += bool(old)
        added += not old
        ar.append(i, data, *meta[i], loose=path)
        if n % 20000 == 0:
            print(f"  {n}/{len(todo)}", flush=True)
    ar.close()
    print(f"done: +{added} new, {replaced} replaced, {same} already there; {len(ar.entries)} textures in {len(ar.volumes())} volumes", flush=True)


def batch_running() -> bool:
    if sys.platform != "win32":
        return False
    r = subprocess.run(
        ["powershell", "-NoProfile", "-Command", "Get-CimInstance Win32_Process -Filter \"Name like 'python%'\" | ForEach-Object CommandLine"],
        capture_output=True, text=True,
    )
    return "run_batch" in r.stdout


def cmd_redo(a):
    if batch_running():
        raise SystemExit("the batch is running and appends to index.jsonl: wait for it or stop it")
    ids = {int(x) for x in a.ids.split(",") if x} if a.ids else {int(x) for x in Path(a.ids_file).read_text().split()}
    path = a.pack / "index.jsonl"
    lines, removed = [], 0
    for l in open(path):
        try:
            if json.loads(l)["file_id"] in ids:
                removed += 1
                continue
        except (ValueError, KeyError):
            pass
        lines.append(l)
    tmp = path.with_suffix(".jsonl.tmp")
    with open(tmp, "w") as f:
        f.writelines(lines)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)
    print(f"{removed} of {len(ids)} textures will be redone by the next batch run", flush=True)


def cmd_compact(a):
    ar = Archive(a.pack, int(a.max_gb * 2**30))
    live = {}
    for r in ar.entries.values():
        live.setdefault(r[1], []).append(r)
    dirty = [n for n in ar.volumes() if os.path.getsize(a.pack / volume_name(n)) > sum(r[2] for r in live.get(n, []))]
    if not dirty:
        print("nothing to compact", flush=True)
        ar.close()
        return
    before = sum(os.path.getsize(a.pack / volume_name(n)) for n in ar.volumes())
    ar.start(new_volume=True)
    for n in dirty:
        for r in sorted(live.get(n, []), key=lambda r: r[5]):
            ar.append(r[0], ar.read(n, r[5], r[2]), r[3], r[4])
        # On disk and indexed elsewhere before the old volume goes away.
        ar.commit()
        (a.pack / volume_name(n)).unlink()
    ar.close()
    after = sum(os.path.getsize(a.pack / volume_name(n)) for n in ar.volumes())
    print(f"compacted {len(dirty)} volumes: {(before - after) >> 20} MB freed", flush=True)


def game_running() -> bool:
    if sys.platform != "win32":
        return False
    r = subprocess.run(["tasklist", "/FI", "IMAGENAME eq Gw2-64.exe", "/NH"], capture_output=True, text=True)
    return "Gw2-64.exe" in r.stdout


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("add")
    p.add_argument("pack", type=Path)
    p.add_argument("--delete", action="store_true", help="remove loose files once verified in the archive")
    p.add_argument("--new-volume", action="store_true", help="keep this update in its own files")
    p.add_argument("--max-gb", type=float, default=DEFAULT_MAX_GB)
    p.set_defaults(fn=cmd_add)
    p = sub.add_parser("redo")
    p.add_argument("pack", type=Path)
    p.add_argument("--ids", default="")
    p.add_argument("--ids-file", default="")
    p.set_defaults(fn=cmd_redo)
    p = sub.add_parser("compact")
    p.add_argument("pack", type=Path)
    p.add_argument("--max-gb", type=float, default=DEFAULT_MAX_GB)
    p.set_defaults(fn=cmd_compact)
    a = ap.parse_args(argv)
    if game_running():
        raise SystemExit("close the game first: it reads these files")
    a.fn(a)


if __name__ == "__main__":
    main()
