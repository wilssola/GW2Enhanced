"""Run: python test_diff.py (or pytest)."""
import json
import shutil
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import diff  # noqa: E402


def test_finds_changed_new_and_removed():
    d = Path(tempfile.mkdtemp(prefix="gw2rem_diff_"))
    try:
        old = {1: "a", 2: "b", 3: "c", 4: "d"}
        new = {1: "a", 2: "CHANGED", 4: "d", 5: "e"}
        (d / "old.jsonl").write_text("".join(json.dumps({"file_id": k, "xxh3_128": v}) + "\n" for k, v in old.items()))
        (d / "new.csv").write_text("file_id,xxh3_128\n" + "".join(f"{k},{v}\n" for k, v in new.items()))
        pack = d / "pack"
        pack.mkdir()
        (pack / "index.jsonl").write_text(
            '{"file_id":1,"src_xxh3_128":"a"}\n{"file_id":2,"src_xxh3_128":"b"}\n{"file_id":3,"src_xxh3_128":"c"}\n{"file_id":4}\nbroken\n'
        )
        assert diff.main([str(d / "new.csv"), str(d / "old.jsonl"), str(d / "out"), str(pack)]) == 0
        assert (d / "out" / "redo_pack.txt").read_text().split() == ["2"]
        assert (d / "out" / "new_ids.txt").read_text().split() == ["5"]
        assert (d / "out" / "removed_ids.txt").read_text().split() == ["3"]
    finally:
        shutil.rmtree(d)


if __name__ == "__main__":
    test_finds_changed_new_and_removed()
    print("1 passed")
