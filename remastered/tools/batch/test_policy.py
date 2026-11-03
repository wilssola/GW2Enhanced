"""Run: python test_policy.py (or pytest)."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import policy  # noqa: E402


def job(group, w, h=None):
    return {"group": group, "width": w, "height": h or w}


def test_only_the_group_target_is_left_alone():
    assert policy.already_high(job("world", 1024))
    assert not policy.already_high(job("world", 512))
    assert policy.already_high(job("world", 512, 2048))
    assert not policy.already_high(job("weapon_armor", 1024))  # weapons go up to 2048
    assert policy.already_high(job("weapon_armor", 2048))
    assert policy.already_high(job("effect", 1024)) and not policy.already_high(job("effect", 512))
    # pieces: fixed slots, always scaled
    assert not policy.already_high(job("composite", 1024))
    assert not policy.already_high(job("composite_4x", 2048))


if __name__ == "__main__":
    test_only_the_group_target_is_left_alone()
    print("1 passed")
