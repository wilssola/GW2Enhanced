"""Run: python test_check_groups.py (or pytest)."""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import check_groups as cg  # noqa: E402


def test_half_served_groups_are_listed():
    survey = {i: {} for i in range(1, 9)}
    via = {2: "hires-variant of 1 (+1)", 4: "hires-variant of 3 (+1)", 6: "hires-variant of 5 (+1)", 7: "hires-variant of 5 (+2)"}
    # group {1,2}: fully served; {3,4}: only 4 served; {5,6,7}: 5 and 6 served, 7 not; 8 alone and served
    served = {1, 2, 4, 5, 6, 8}
    assert cg.half_served_ids(survey, via, served) == {4, 5, 6}


if __name__ == "__main__":
    test_half_served_groups_are_listed()
    print("1 passed")
