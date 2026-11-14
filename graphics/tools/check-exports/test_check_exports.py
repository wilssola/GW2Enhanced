import os
import unittest
from pathlib import Path

import check_exports as ce


class FakeEntry:
    def __init__(self, dll, names):
        self.dll = dll
        self.imports = [type("I", (), {"name": n})() for n in names]


class FakePE:
    DIRECTORY_ENTRY_IMPORT = [
        FakeEntry(b"KERNEL32.dll", [b"Sleep"]),
        FakeEntry(b"WTSAPI32.dll", [b"WTSFreeMemory", None, b"WTSQuerySessionInformationW"]),
    ]


class ImportTable(unittest.TestCase):
    def test_imports_from_filters_module_case_insensitively(self):
        self.assertEqual(ce.imports_from(FakePE(), "wtsapi32.dll"),
                         {"WTSFreeMemory", "WTSQuerySessionInformationW"})

    def test_no_import_directory(self):
        self.assertEqual(ce.imports_from(object(), "wtsapi32.dll"), set())


class BuiltDll(unittest.TestCase):
    def test_exports_are_superset_of_system(self):
        dll = os.environ.get("GW2G_DLL")
        if not dll or not Path(dll).exists():
            self.skipTest("set GW2G_DLL to the built wtsapi32.dll")
        self.assertEqual(ce.missing_exports(dll), [])


if __name__ == "__main__":
    unittest.main()
