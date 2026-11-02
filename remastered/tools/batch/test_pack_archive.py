"""Tests for pack_archive.py. Run: python test_pack_archive.py (or pytest)."""
import json
import os
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import pack_archive as pa  # noqa: E402


class Pack:
    def __init__(self):
        self.dir = Path(tempfile.mkdtemp(prefix="gw2rem_pack_"))
        self.lines = {}

    def loose(self, i, data, src=100, width=64):
        (self.dir / f"{i}.dds.zst").write_bytes(data)
        self.lines[i] = {"file_id": i, "src_bytes": src, "width": width}
        (self.dir / "index.jsonl").write_text("".join(json.dumps(v) + "\n" for v in self.lines.values()))

    def run(self, *args):
        pa.game_running = lambda: False
        pa.batch_running = lambda: False
        cmd, *rest = args
        pa.main([cmd, str(self.dir), *rest])

    def read(self, i):
        ar = pa.Archive(self.dir)
        r = ar.entries[i]
        return ar.read(r[1], r[5], r[2])

    def entries(self):
        return pa.Archive(self.dir).entries

    def close(self):
        shutil.rmtree(self.dir)


def blob(n, seed=1):
    return bytes((seed * i + i // 7) % 251 for i in range(n))


def test_add_keeps_order_and_deletes_only_with_flag():
    p = Pack()
    p.loose(1, blob(5000), 77, 512)
    p.loose(2, blob(900, 3))
    p.run("add")
    assert (p.dir / "1.dds.zst").exists()
    assert p.read(1) == blob(5000) and p.read(2) == blob(900, 3)
    assert p.entries()[1][3:5] == (77, 512)
    p.run("add", "--delete")
    assert not (p.dir / "1.dds.zst").exists() and not (p.dir / "2.dds.zst").exists()
    assert p.read(1) == blob(5000)
    assert "file_id" in (p.dir / "index.jsonl").read_text()
    p.close()


def test_new_textures_are_appended():
    p = Pack()
    p.loose(1, blob(1000))
    p.run("add", "--delete")
    size = os.path.getsize(p.dir / pa.volume_name(0))
    p.loose(2, blob(2000, 3))
    p.run("add", "--delete")
    assert os.path.getsize(p.dir / pa.volume_name(0)) == size + 2000
    assert p.read(1) == blob(1000) and p.read(2) == blob(2000, 3)
    p.close()


def test_volumes_split_at_the_limit_and_for_updates():
    p = Pack()
    for i in range(5):
        p.loose(i, blob(1000, i + 1))
    p.run("add", "--max-gb", str(2500 / 2**30))
    assert len(pa.Archive(p.dir).volumes()) == 3
    for i in range(5):
        assert p.read(i) == blob(1000, i + 1)
    p.loose(9, blob(10, 9))
    p.run("add", "--new-volume")
    e = p.entries()
    assert e[9][1] == 3 and all(e[i][1] < 3 for i in range(5))
    p.close()


def test_new_volume_with_nothing_to_add_leaves_no_empty_file():
    p = Pack()
    p.loose(1, blob(100))
    p.run("add", "--delete")
    p.run("add", "--new-volume")
    assert pa.Archive(p.dir).volumes() == [0]
    p.close()


def test_redo_then_add_replaces_and_old_version_serves_meanwhile():
    p = Pack()
    p.loose(1, blob(1000))
    p.loose(2, blob(1000, 3))
    p.run("add", "--delete")
    p.run("redo", "--ids", "1")
    assert '"file_id": 2' in (p.dir / "index.jsonl").read_text()
    assert '"file_id": 1,' not in (p.dir / "index.jsonl").read_text()
    assert p.read(1) == blob(1000)
    p.loose(1, blob(1500, 5), 200, 128)
    p.run("add", "--delete")
    assert p.read(1) == blob(1500, 5)
    assert p.entries()[1][3:5] == (200, 128)
    assert p.read(2) == blob(1000, 3)
    p.close()


def test_identical_loose_file_is_not_stored_twice():
    p = Pack()
    p.loose(1, blob(1000))
    p.run("add")
    size = os.path.getsize(p.dir / pa.volume_name(0))
    p.run("add", "--delete")
    assert os.path.getsize(p.dir / pa.volume_name(0)) == size
    assert not (p.dir / "1.dds.zst").exists()
    p.close()


def test_compact_frees_replaced_space_and_keeps_everything():
    p = Pack()
    p.loose(1, blob(1000))
    p.loose(2, blob(1000, 3))
    p.run("add", "--delete")
    p.loose(1, blob(1000, 5))
    p.run("add", "--delete")
    total = sum(os.path.getsize(p.dir / pa.volume_name(n)) for n in pa.Archive(p.dir).volumes())
    assert total == 3000
    p.run("compact")
    total = sum(os.path.getsize(p.dir / pa.volume_name(n)) for n in pa.Archive(p.dir).volumes())
    assert total == 2000
    assert p.read(1) == blob(1000, 5) and p.read(2) == blob(1000, 3)
    p.run("compact")  # nothing left to do
    assert p.read(1) == blob(1000, 5)
    p.close()


def test_interrupted_run_is_discarded():
    p = Pack()
    p.loose(1, blob(1000))
    p.run("add", "--delete")
    with open(p.dir / pa.volume_name(0), "ab") as f:  # bytes written, index never updated
        f.write(b"garbage")
    (p.dir / pa.volume_name(7)).write_bytes(b"orphan volume")
    p.loose(2, blob(500, 3))
    p.run("add", "--delete")
    assert p.read(1) == blob(1000) and p.read(2) == blob(500, 3)
    assert os.path.getsize(p.dir / pa.volume_name(0)) == 1500
    assert not (p.dir / pa.volume_name(7)).exists()
    p.close()


def test_first_version_is_migrated_without_copying():
    p = Pack()
    a, b = blob(800), blob(300, 3)
    (p.dir / "pack.bin").write_bytes(a + b)
    with open(p.dir / "pack.idx", "wb") as f:
        f.write(pa.OLD_MAGIC + pa.OLD_RECORD.pack(1, 800, 0, 11, 64) + pa.OLD_RECORD.pack(2, 300, 800, 22, 128))
    assert p.read(1) == a and p.read(2) == b
    assert p.entries()[2][3:5] == (22, 128)
    assert not (p.dir / "pack.bin").exists()
    p.close()


if __name__ == "__main__":
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_")]
    for t in tests:
        t()
        print("ok", t.__name__)
    print(len(tests), "passed")
