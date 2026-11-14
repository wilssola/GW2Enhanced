import contextlib
import io
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import draw_diff as dd  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))


def fixture(name):
    with open(os.path.join(HERE, "fixtures", name), encoding="utf-8") as f:
        return f.read()


def g(vs, ps, tex, draws=1, max_idx=0, vcb=0, instances=None, stride=0):
    """One G line the way the mod writes it."""
    inst = draws if instances is None else instances
    texn = "-" if not tex else ",".join(f"{i}:{t}" for i, t in enumerate(tex))
    return (f"G draws={draws} vs={vs} ps={ps} kinds=DrawIndexed:{draws} indices={max_idx * draws} max_idx={max_idx} vertices=0 max_vtx=0 "
            f"instances={inst} vcb={vcb}..{vcb} vcbs={vcb},0,0,0 pcbs=64,0,0,0 layouts=1 strides={stride} texptrs={draws * len(tex)} tex={texn}")


T3 = ["BC3_UNORM:1024x1024m11", "BC1_UNORM:512x512m10", "BC3_UNORM:1024x1024m11"]


class ParseTests(unittest.TestCase):
    def test_parses_the_fixture_the_rust_test_pins(self):
        h, groups = dd.parse_log(fixture("sample_a.log"))
        self.assertEqual(h["frame"], "42")
        self.assertEqual(h["how"], "classifier")
        self.assertEqual(len(groups), 3)
        body = groups[1]
        self.assertEqual((body["vs"], body["ps"], body["draws"]), ("00001111", "00002222", 2))
        self.assertEqual((body["vcb_min"], body["vcb_max"], body["max_idx"]), (2048, 4096, 7200))
        self.assertEqual(body["vcbs"], [4096, 0, 0, 0])
        self.assertEqual(dd.tex_count(body), 3)
        self.assertEqual(dd.tex_count(groups[2]), 0)
        self.assertEqual(groups[0]["strides"], [32])

    def test_a_bad_line_is_reported_not_ignored(self):
        with self.assertRaises(ValueError):
            dd.parse_log("G draws=1 vs=1\n")

    def test_comments_and_blank_lines_are_skipped(self):
        h, groups = dd.parse_log("\n# gw2g-draws v1\n# unix=5 frame=1\nnoise\n")
        self.assertEqual((h["unix"], groups), ("5", []))


class DiffTests(unittest.TestCase):
    def test_lists_only_what_the_second_log_does_not_have(self):
        a = dd.parse_log(fixture("sample_a.log"))[1]
        b = dd.parse_log(fixture("sample_b.log"))[1]
        new = dd.only_in_first(a, b)
        self.assertEqual([x["vs"] for x in new], ["00001111"])  # the body; rock and quad are in both
        self.assertEqual([x["vs"] for x in dd.only_in_first(b, a)], ["00007777"])  # the reverse finds the tree

    def test_same_shaders_new_texture_size_counts_unless_shader_only(self):
        a = dd.parse_log(g("1", "2", ["BC3_UNORM:2048x2048m12"]) + "\n")[1]
        b = dd.parse_log(g("1", "2", ["BC3_UNORM:1024x1024m11"]) + "\n")[1]
        self.assertEqual(len(dd.only_in_first(a, b)), 1)
        self.assertEqual(len(dd.only_in_first(a, b, shader_only=True)), 0)

    def test_shader_only_merges_texture_sets_of_one_pair(self):
        a = dd.parse_log("\n".join([g("1", "2", ["BC3_UNORM:64x64m7"], draws=2, max_idx=100), g("1", "2", ["BC3_UNORM:128x128m8"], draws=3, max_idx=900)]) + "\n")[1]
        m = dd.only_in_first(a, [], shader_only=True)
        self.assertEqual(len(m), 1)
        self.assertEqual((m[0]["draws"], m[0]["max_idx"], len(m[0]["tex_sets"])), (5, 900, 2))


class RankTests(unittest.TestCase):
    def build(self):
        # The charr-like body: big vertex constant buffer, 3 textures, thousands of indices, few draws, stride 48.
        body = g("a1", "b1", T3, draws=3, max_idx=6000, vcb=3840, stride=48)
        banner = g("a2", "b2", ["BC1_UNORM:64x64m7"], draws=1, max_idx=12, vcb=128, stride=20)
        grass = g("a3", "b3", ["BC1_UNORM:64x64m7"], draws=200, max_idx=24, vcb=128, instances=4000, stride=16)
        # Big constant buffer but one texture and tiny: a batch of something else.
        batch = g("a4", "b4", ["BC1_UNORM:64x64m7"], draws=2, max_idx=6, vcb=4096, stride=16)
        # Rigid prop: 3 textures, thousands of indices, small constant buffer.
        prop = g("a5", "b5", T3, draws=4, max_idx=5000, vcb=256, stride=32)
        return "\n".join([banner, grass, batch, prop, body]) + "\n"

    def test_the_skinned_looking_group_ranks_first(self):
        rows = dd.ranked(dd.parse_log(self.build())[1])
        self.assertEqual(rows[0][2]["vs"], "a1")
        self.assertGreaterEqual(rows[0][0], 80)
        self.assertEqual(rows[1][2]["vs"], "a5")  # the rigid prop is second
        self.assertTrue(any("vertex constant buffer 3840" in w for w in rows[0][1]))
        scores = {r[2]["vs"]: r[0] for r in rows}
        self.assertTrue(all(0 <= s <= 100 for s in scores.values()))
        self.assertLess(scores["a3"], scores["a1"])
        self.assertLess(scores["a3"], scores["a2"] + 1)  # instanced grass is penalised

    def test_score_is_monotonic_in_the_constant_buffer_size(self):
        last = -1
        for vcb in (0, 600, 1100, 2100, 4200):
            s = dd.score(dd.parse_log(g("1", "2", T3, max_idx=3000, vcb=vcb) + "\n")[1][0])[0]
            self.assertGreaterEqual(s, last)
            last = s


class ReportTests(unittest.TestCase):
    def test_report_on_the_fixtures_names_the_body_group(self):
        text = dd.report(fixture("sample_a.log"), fixture("sample_b.log"))
        self.assertIn("only in the first (vs+ps+textures): 1 groups, 2 draws", text)
        self.assertIn("#1 score", text)
        self.assertIn("vs=00001111 ps=00002222", text)
        self.assertIn("tex 0:BC3_UNORM:1024x1024m11", text)
        self.assertNotIn("vs=00003333", text)

    def test_report_when_nothing_is_new_and_min_score_filter(self):
        a = fixture("sample_a.log")
        self.assertIn("nothing only in the first log", dd.report(a, a))
        self.assertIn("nothing only in the first log", dd.report(a, fixture("sample_b.log"), min_score=101))

    def test_a_fallback_pass_is_warned_about(self):
        a = fixture("sample_a.log").replace("how=classifier", "how=fallback_most_draws")
        self.assertIn("warning: the opaque pass was not found", dd.report(a, fixture("sample_b.log")))

    def test_cli_runs_on_files(self):
        buf = io.StringIO()
        with contextlib.redirect_stdout(buf):
            rc = dd.main([os.path.join(HERE, "fixtures", "sample_a.log"), os.path.join(HERE, "fixtures", "sample_b.log"), "--top", "3"])
        self.assertEqual(rc, 0)
        self.assertIn("#1 score", buf.getvalue())


if __name__ == "__main__":
    unittest.main()
