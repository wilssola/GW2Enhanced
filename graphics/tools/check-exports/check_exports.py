"""Regression guard for the wtsapi32.dll proxy: every export of the real DLL must exist in ours.

    python -I check_exports.py exports   <our.dll>
    python -I check_exports.py consumers <our.dll> <game_dir>
    python -I check_exports.py smoke     <our.dll>

Why: we are loaded as wtsapi32.dll, so every module of the game (libcef.dll!) that imports from it gets
our DLL. A missing export aborts the game with "Failed to load CEF DLL. Error code: 0x7f".
Needs `pip install pefile`.
"""
import ctypes
import os
import sys
from ctypes import wintypes
from pathlib import Path

import pefile

MODULE = "wtsapi32.dll"
MAX_DEPTH = 4


def system_dll() -> Path:
    buf = ctypes.create_unicode_buffer(260)
    ctypes.windll.kernel32.GetSystemDirectoryW(buf, 260)
    return Path(buf.value) / MODULE


def export_names(path) -> set:
    pe = pefile.PE(str(path), fast_load=True)
    try:
        pe.parse_data_directories([pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_EXPORT"]])
        return {e.name.decode() for e in pe.DIRECTORY_ENTRY_EXPORT.symbols if e.name}
    finally:
        pe.close()


def imports_from(pe, module: str) -> set:
    """Names imported (by name) from `module` by an already parsed PE."""
    names = set()
    for entry in getattr(pe, "DIRECTORY_ENTRY_IMPORT", []):
        if entry.dll.decode(errors="replace").lower() == module.lower():
            names.update(i.name.decode() for i in entry.imports if i.name)
    return names


def missing_exports(our_dll) -> list:
    return sorted(export_names(system_dll()) - export_names(our_dll))


def cmd_exports(our_dll) -> int:
    missing = missing_exports(our_dll)
    for name in missing:
        print(f"FAIL missing export: {name}")
    print(f"{'FAIL' if missing else 'PASS'} exports: {len(export_names(system_dll())) - len(missing)} of "
          f"{len(export_names(system_dll()))} real exports present")
    return 1 if missing else 0


def scan_consumers(game_dir: Path):
    """Yield (path, imported names) for every exe/dll under game_dir (depth <= 4) importing from wtsapi32."""
    for root, dirs, files in os.walk(game_dir):
        depth = len(Path(root).relative_to(game_dir).parts)
        if depth >= MAX_DEPTH:
            dirs.clear()
        for f in files:
            if f.lower().endswith((".exe", ".dll")):
                p = Path(root) / f
                try:
                    pe = pefile.PE(str(p), fast_load=True)
                    pe.parse_data_directories([pefile.DIRECTORY_ENTRY["IMAGE_DIRECTORY_ENTRY_IMPORT"]])
                except Exception:
                    continue
                names = imports_from(pe, MODULE)
                pe.close()
                if names:
                    yield p, names


def cmd_consumers(our_dll, game_dir) -> int:
    ours = export_names(our_dll)
    bad = 0
    for path, names in scan_consumers(Path(game_dir)):
        missing = sorted(names - ours)
        print(f"{path.relative_to(game_dir)}: imports {', '.join(sorted(names))}")
        for m in missing:
            print(f"FAIL {path.relative_to(game_dir)}: our DLL does not export {m}")
        bad += len(missing)
    print(f"{'FAIL' if bad else 'PASS'} consumers: {bad} missing import(s)")
    return 1 if bad else 0


def cmd_smoke(our_dll) -> int:
    k32 = ctypes.WinDLL("kernel32", use_last_error=True)
    k32.GetProcAddress.restype = ctypes.c_void_p
    k32.GetProcAddress.argtypes = [wintypes.HMODULE, ctypes.c_char_p]
    dll = ctypes.WinDLL(str(Path(our_dll).resolve()))
    results = []

    def check(label, ok):
        results.append(ok)
        print(f"{'PASS' if ok else 'FAIL'} {label}")

    unresolved = [n for n in sorted(export_names(system_dll())) if not k32.GetProcAddress(dll._handle, n.encode())]
    check(f"GetProcAddress resolves all real names (unresolved: {unresolved or 'none'})", not unresolved)

    ptr, size = ctypes.c_void_p(), wintypes.DWORD()
    q = dll.WTSQuerySessionInformationW
    q.argtypes = [wintypes.HANDLE, wintypes.DWORD, ctypes.c_int, ctypes.POINTER(ctypes.c_void_p),
                  ctypes.POINTER(wintypes.DWORD)]
    q.restype = wintypes.BOOL
    # WTS_CURRENT_SERVER_HANDLE=0, WTS_CURRENT_SESSION=0xFFFFFFFF, WTSSessionId=4: 5 args, one on the stack
    ok = q(None, 0xFFFFFFFF, 4, ctypes.byref(ptr), ctypes.byref(size))
    check(f"WTSQuerySessionInformationW(WTSSessionId) -> {ok}, buffer {'set' if ptr.value else 'NULL'}, len {size.value}",
          bool(ok) and bool(ptr.value) and size.value == 4)
    if ptr.value:
        dll.WTSFreeMemory.argtypes = [ctypes.c_void_p]
        dll.WTSFreeMemory(ptr)
        check("WTSFreeMemory(ptr) returned", True)

    dll.WTSRegisterSessionNotification.argtypes = [wintypes.HWND, wintypes.DWORD]
    dll.WTSUnRegisterSessionNotification.argtypes = [wintypes.HWND]
    r1 = dll.WTSRegisterSessionNotification(None, 0)  # FALSE is fine for a NULL hwnd
    r2 = dll.WTSUnRegisterSessionNotification(None)
    check(f"WTSRegisterSessionNotification(NULL,0)={r1} / WTSUnRegisterSessionNotification(NULL)={r2} returned", True)
    return 0 if all(results) else 1


def main(argv) -> int:
    usage = "usage: check_exports.py exports <dll> | consumers <dll> <game_dir> | smoke <dll>"
    try:
        cmd, args = argv[1], argv[2:]
        fn = {"exports": (cmd_exports, 1), "consumers": (cmd_consumers, 2), "smoke": (cmd_smoke, 1)}[cmd]
        if len(args) != fn[1]:
            raise IndexError
    except (IndexError, KeyError):
        print(usage)
        return 2
    return fn[0](*args)


if __name__ == "__main__":
    sys.exit(main(sys.argv))
