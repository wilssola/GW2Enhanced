"""Run: python test_build_reasons.py (or pytest)."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import build_reasons as br  # noqa: E402


def tex(w, h, fmt="DXT5", kind="ATEX"):
    return {"width": w, "height": h, "format": fmt, "kind": kind}


def test_every_reason():
    survey = {
        1: tex(256, 256),  # effect
        2: tex(256, 256), 3: tex(1024, 1024),  # pair: small member + big member (scenery)
        4: tex(1024, 1024),  # alone, already 1024
        5: tex(64, 64),  # tiny
        6: tex(512, 512),  # no job, none of the above
        7: tex(512, 512),  # has a job
        8: tex(256, 256, "BC7X"),  # not tintable
        9: tex(256, 256, kind="ATEU"),  # not ATEX
        10: tex(512, 512),  # weapon at 1024: below its 2048 target
        11: tex(256, 256), 12: tex(2048, 2048),  # weapon pair with a 2048 big member
        13: tex(256, 256),  # source of a character piece
    }
    hint = {i: "scenery" for i in survey}
    hint[1] = "effect"
    hint[10] = hint[11] = hint[12] = "weapon"
    via = {3: "hires-variant of 2 (+1)", 12: "hires-variant of 11 (+1)"}
    out = br.classify(survey, hint, via, jobs={7}, comp=frozenset({13}))
    assert out == {1: 1, 2: 2, 3: 3, 4: 3, 5: 4, 6: 5, 10: 5, 11: 2, 12: 3, 13: 6}, out


if __name__ == "__main__":
    test_every_reason()
    print("1 passed")
