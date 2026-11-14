"""Offline DXBC finder/disassembler for the GW2 exe. Read-only.

  python -I dxbc_dump.py index  <exe> [--out index.json]
  python -I dxbc_dump.py disasm <exe> <hash>...
"""
import argparse, ctypes, json, struct, sys

MAGIC = b"DXBC"
TYPES = {0: "pixel", 1: "vertex", 2: "geometry", 3: "hull", 4: "domain", 5: "compute"}


def fnv1a(data: bytes) -> int:
    h = 2166136261
    for b in data:
        h = ((h ^ b) * 16777619) & 0xFFFFFFFF
    return h


def shader_type(blob: bytes) -> str:
    """Type from the version token of the SHDR/SHEX chunk."""
    count = struct.unpack_from("<I", blob, 28)[0]
    for i in range(count):
        off = struct.unpack_from("<I", blob, 32 + 4 * i)[0]
        if blob[off:off + 4] in (b"SHDR", b"SHEX"):
            tok = struct.unpack_from("<I", blob, off + 8)[0]
            return TYPES.get(tok >> 16, "unknown")
    return "none"


def find_blobs(data: bytes):
    """Yield (offset, blob) for every well-formed DXBC container."""
    pos = data.find(MAGIC)
    while pos != -1:
        if pos + 36 <= len(data):
            ver, size, count = struct.unpack_from("<III", data, pos + 20)
            if 36 <= size <= 1 << 22 and ver == 1 and 0 < count < 64 and pos + size <= len(data):
                blob = data[pos:pos + size]
                ok = all(struct.unpack_from("<I", blob, 32 + 4 * i)[0] < size for i in range(count))
                if ok:
                    yield pos, blob
        pos = data.find(MAGIC, pos + 1)


def build_index(path):
    with open(path, "rb") as f:
        data = f.read()
    return [{"offset": o, "size": len(b), "hash": f"{fnv1a(b):08x}", "type": shader_type(b)}
            for o, b in find_blobs(data)], data


def disassemble(blob: bytes) -> str:
    d3d = ctypes.WinDLL("d3dcompiler_47.dll")
    out = ctypes.c_void_p()
    fn = d3d.D3DDisassemble
    fn.argtypes = [ctypes.c_char_p, ctypes.c_size_t, ctypes.c_uint, ctypes.c_char_p, ctypes.POINTER(ctypes.c_void_p)]
    fn.restype = ctypes.c_long
    hr = fn(blob, len(blob), 0, None, ctypes.byref(out))
    if hr < 0:
        raise OSError(f"D3DDisassemble failed 0x{hr & 0xFFFFFFFF:08x}")
    vt = ctypes.cast(ctypes.cast(out, ctypes.POINTER(ctypes.c_void_p))[0], ctypes.POINTER(ctypes.c_void_p))
    get_ptr = ctypes.CFUNCTYPE(ctypes.c_void_p, ctypes.c_void_p)(vt[3])
    get_size = ctypes.CFUNCTYPE(ctypes.c_size_t, ctypes.c_void_p)(vt[4])
    release = ctypes.CFUNCTYPE(ctypes.c_ulong, ctypes.c_void_p)(vt[2])
    try:
        return ctypes.string_at(get_ptr(out), get_size(out)).decode("latin-1")
    finally:
        release(out)


def main(argv=None):
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("index"); p.add_argument("exe"); p.add_argument("--out")
    p = sub.add_parser("disasm"); p.add_argument("exe"); p.add_argument("hashes", nargs="+")
    a = ap.parse_args(argv)
    idx, data = build_index(a.exe)
    if a.cmd == "index":
        if a.out:
            with open(a.out, "w") as f:
                json.dump(idx, f, indent=1)
        kinds = {}
        for e in idx:
            kinds[e["type"]] = kinds.get(e["type"], 0) + 1
        print(f"{len(idx)} blobs {kinds}")
        if not a.out:
            for e in idx:
                print(f'{e["offset"]:#010x} {e["size"]:6d} {e["hash"]} {e["type"]}')
        return 0
    byhash = {}
    for e in idx:
        byhash.setdefault(e["hash"], e)
    for h in a.hashes:
        e = byhash.get(h.lower())
        if not e:
            print(f"// {h}: not found"); continue
        blob = data[e["offset"]:e["offset"] + e["size"]]
        print(f'// ===== {h} {e["type"]} size={e["size"]} offset={e["offset"]:#x}')
        print(disassemble(blob))
    return 0


if __name__ == "__main__":
    sys.exit(main())
