import unittest
import dxbc_dump as d

EXE = os.environ.get("GW2_EXE", "Gw2-64.exe")


class T(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.idx, cls.data = d.build_index(EXE)

    def test_fnv_known(self):
        self.assertEqual(d.fnv1a(b""), 2166136261)
        self.assertEqual(d.fnv1a(b"a"), 0xE40C292C)

    def test_known_blobs(self):
        by = {e["hash"]: e for e in self.idx}
        self.assertEqual(by["4340ca88"]["size"], 504)
        self.assertEqual(by["37c5610f"]["size"], 2060)
        self.assertEqual(by["4340ca88"]["type"], "pixel")

    def test_count(self):
        self.assertGreater(len(self.idx), 2000)

    def test_disasm(self):
        e = next(e for e in self.idx if e["hash"] == "4340ca88")
        self.assertIn("ps_", d.disassemble(self.data[e["offset"]:e["offset"] + e["size"]]))


if __name__ == "__main__":
    unittest.main()
