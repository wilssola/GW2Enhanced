import contextlib
import io
import os
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import fx_summary as fx  # noqa: E402


def g(pass_, blend, quad, draws, vs="00000001", ps="00000002", max_idx=6, tex="-"):
    return (f"G pass={pass_} blend={blend} quad={int(quad)} draws={draws} vs={vs} ps={ps} kinds=DrawIndexed:{draws} "
            f"indices={max_idx * draws} max_idx={max_idx} vertices=0 max_vtx=0 instances={draws} vcb=0..0 vcbs=0,0,0,0 "
            f"pcbs=0,0,0,0 layouts=1 strides=0 texptrs=0 tex={tex}")


LOG = "\n".join([
    "# gw2g-draws-fx v1",
    "# unix=7 frame=8 ctx=0x10 screen=3440x1440",
    "# opaque_pass=4 how=classifier passes=2 captured=121 dropped=0 groups=4",
    "# fxpass=9 rt0=R16G16B16A16_FLOAT depth=none pass_draws=63 captured=63 groups=2",
    "# fxpass=10 rt0=R16G16B16A16_FLOAT depth=D24_UNORM_S8_UINT pass_draws=556 captured=58 groups=2",
    g(9, "ADD:ONE:ONE", False, 40, max_idx=3000, vs="0000000a"),
    g(9, "off", True, 23, vs="0000000b"),
    g(10, "ADD:SRC_ALPHA:ONE|ADD:ONE:INV_SRC_ALPHA", True, 8, tex="0:BC3_UNORM:64x64m7"),
    g(10, "off", False, 50, max_idx=900),
    "",
])


class ParseTests(unittest.TestCase):
    def test_parses_headers_passes_and_groups(self):
        h, passes, groups = fx.parse_fx_log(LOG)
        self.assertEqual((h["frame"], h["opaque_pass"], h["dropped"]), ("8", "4", "0"))
        self.assertEqual(sorted(passes), [9, 10])
        self.assertEqual(passes[10]["depth"], "D24_UNORM_S8_UINT")
        self.assertEqual(passes[9]["pass_draws"], 63)
        self.assertEqual(len(groups), 4)
        self.assertEqual((groups[0]["pass"], groups[0]["blend"], groups[0]["quad"], groups[0]["max_idx"]), (9, "ADD:ONE:ONE", False, 3000))
        self.assertTrue(groups[1]["quad"])
        self.assertEqual(groups[2]["tex"], "0:BC3_UNORM:64x64m7")

    def test_a_bad_line_is_reported(self):
        with self.assertRaises(ValueError):
            fx.parse_fx_log("G pass=1 draws=2\n")
        with self.assertRaises(ValueError):
            fx.parse_fx_log("# fxpass=x pass_draws=y\n")

    def test_other_lines_are_skipped(self):
        h, passes, groups = fx.parse_fx_log("\nnoise\n# gw2g-draws-fx v1\n# unix=5\n")
        self.assertEqual((h["unix"], passes, groups), ("5", {}, []))


class RankTests(unittest.TestCase):
    def test_quad_like_first_then_by_draw_count(self):
        _, _, groups = fx.parse_fx_log(LOG)
        r = fx.rank([x for x in groups if x["pass"] == 10])
        self.assertEqual([(x["quad"], x["draws"]) for x in r], [(True, 8), (False, 50)])
        r = fx.rank(groups)
        self.assertEqual([x["draws"] for x in r], [23, 8, 50, 40])

    def test_order_does_not_depend_on_input_order(self):
        _, _, groups = fx.parse_fx_log(LOG)
        self.assertEqual(fx.rank(groups), fx.rank(list(reversed(groups))))

    def test_additive_detection(self):
        self.assertTrue(fx.is_additive("ADD:ONE:ONE"))
        self.assertTrue(fx.is_additive("ADD:SRC_ALPHA:ONE|ADD:ONE:INV_SRC_ALPHA"))
        self.assertFalse(fx.is_additive("off"))
        self.assertFalse(fx.is_additive("ADD:SRC_ALPHA:INV_SRC_ALPHA"))
        self.assertFalse(fx.is_additive("unknown"))


class ReportTests(unittest.TestCase):
    def test_report_lists_each_pass_with_quads_first(self):
        text = fx.report(*fx.parse_fx_log(LOG), top=8)
        lines = text.splitlines()
        self.assertIn("opaque_pass=4", lines[0])
        self.assertTrue(lines[1].startswith("pass #9  rt0=R16G16B16A16_FLOAT depth=none draws=63 captured=63 quad-like=23 groups=2"), lines[1])
        self.assertTrue(lines[2].lstrip().startswith("QUAD draws=23 blend=off"), lines[2])
        self.assertIn("ADDITIVE draws=40 blend=ADD:ONE:ONE", lines[3])
        self.assertTrue(lines[4].startswith("pass #10"))
        self.assertIn("QUAD ADDITIVE draws=8", lines[5])

    def test_top_limits_the_groups_per_pass(self):
        text = fx.report(*fx.parse_fx_log(LOG), top=1)
        self.assertEqual(text.count("... 1 more groups"), 2)

    def test_cli(self):
        with tempfile.TemporaryDirectory() as d:
            p = os.path.join(d, "draws-fx-1.log")
            with open(p, "w", encoding="utf-8") as f:
                f.write(LOG)
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(fx.main([p, "--top", "2"]), 0)
            self.assertIn("pass #10", out.getvalue())
            bad = os.path.join(d, "bad.log")
            with open(bad, "w", encoding="utf-8") as f:
                f.write("G pass=1\n")
            err = io.StringIO()
            with contextlib.redirect_stderr(err):
                self.assertEqual(fx.main([bad]), 2)
            self.assertIn("error:", err.getvalue())


if __name__ == "__main__":
    unittest.main()
