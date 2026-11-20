# An independent implementation of the landing manifest's canonical encoding (crates/land/src/manifest.rs `encode`),
# from its reading, writing the golden vectors' text for A-469's fixed manifests. The identities are BLAKE3, which this
# machine lacks: they are taken from the encoder's own run and passed in.
import struct, sys

PREAMBLE = """# The landing manifest's golden vectors (A-469): the canonical encoding a grant binds, one for each layout the encoder
# writes, over the fixed manifests of crates/land/src/manifest_vectors.rs. A line is `<layout> <part> <hex>`: the
# header (magic, layout, entry count), each entry by its path in landing order, then the manifest's BLAKE3 identity,
# the hash a grant names. Republish deliberately after a layout change:
#   cargo test -p slates-land --lib -- --ignored publish_the_manifest_vectors
"""

DEV = 0x0807_0605_0403_0201
MTIME_NS = 1_700_000_000_000_000_001
CTIME_NS = 1_700_000_000_000_000_002
OVERLAY_MTIME_NS = 1_700_000_000_000_000_010
FILE_MODE = 0o100_644
EXECUTABLE_MODE = 0o100_755
DIR_MODE = 0o040_755
READONLY, HIDDEN, SYSTEM, LINK_DIRECTORY, ARCHIVE = 0x1, 0x2, 0x4, 0x10, 0x20
OFFLINE, NOT_INDEXED = 0x1000, 0x2000
KEPT = READONLY | HIDDEN | SYSTEM | ARCHIVE | OFFLINE | NOT_INDEXED

def kept(bits, kind):
    if kind == "dir":
        mask = KEPT
    elif kind == "symlink":
        mask = (KEPT & ~READONLY) | LINK_DIRECTORY
    else:
        mask = KEPT & ~READONLY
    return bits & mask

TAGS = {"create": 0, "replace": 1, "delete": 2, "rename": 3, "mkdir": 4, "rmdir": 5, "symlink": 6, "clear": 7, "chmod": 8, "chmod_dir": 9}
CLASS = {"mkdir": 0, "clear": 0, "create": 1, "symlink": 1, "replace": 2, "chmod": 2, "rename": 3, "delete": 4, "rmdir": 5, "chmod_dir": 6}
PRIVATE_DIR_MODE = 0o040_700
BOTTOM_UP = ("delete", "rmdir", "chmod_dir")

def u16(v): return struct.pack("<H", v)
def u32(v): return struct.pack("<I", v)
def u64(v): return struct.pack("<Q", v)
def i64(v): return struct.pack("<q", v)
def put_str(s):
    b = s.encode()
    return u16(min(len(b), 0xFFFF)) + b
def put_meta(permissions, windows): return u32(permissions) + u16(windows)

def fingerprint(ino, mode, windows):
    return dict(dev=DEV, ino=ino, size=3, mtime=MTIME_NS, ctime=CTIME_NS, mode=mode, windows=windows)
def witness(ino, mode, windows, identity):
    return dict(fp=fingerprint(ino, mode, windows), identity=bytes([identity]) * 32)
def content_witness(identity, permissions, windows):
    return dict(fp=dict(dev=0, ino=0, size=0, mtime=0, ctime=0, mode=permissions, windows=windows), identity=bytes([identity]) * 32)
def overlay(h, size, mode): return dict(hash=bytes([h]) * 32, size=size, mode=mode, mtime=OVERLAY_MTIME_NS)
def made_directory(): return overlay(0, 0, DIR_MODE)

def entry(path, action, witnessed, ov, windows, **extra):
    return dict(path=path, action=action, witnessed=witnessed, overlay=ov, windows=windows, **extra)

def golden_entries(basis):
    executable = (0o755, HIDDEN | ARCHIVE)
    directory, private, hidden = (0o755, 0), (0o700, READONLY), (0o755, HIDDEN)
    es = [
        entry("/src/new.rs", "create", None, overlay(0x11, 5, FILE_MODE), kept(HIDDEN | ARCHIVE, "file")),
        entry("/made", "mkdir", None, made_directory(), kept(READONLY | HIDDEN, "dir")),
        entry("/link", "symlink", None, None, kept(LINK_DIRECTORY | HIDDEN, "symlink"), target="src/lib.rs"),
    ]
    if basis == "witness":
        was = (0o644, 0)
        es += [
            entry("/src/lib.rs", "replace", witness(2, FILE_MODE, ARCHIVE, 0x22), overlay(0x33, 7, FILE_MODE), 0),
            entry("/run.sh", "chmod", witness(5, FILE_MODE, 0, 0x55), overlay(0x55, 9, EXECUTABLE_MODE), kept(HIDDEN | ARCHIVE, "file"), meta_from=was, meta_to=executable),
            entry("/src/gone.rs", "delete", witness(3, FILE_MODE, 0, 0x44), None, 0),
            entry("/old", "rmdir", witness(4, DIR_MODE, 0, 0), None, 0),
            entry("/moved", "rename", witness(6, DIR_MODE, 0, 0), None, 0, origin="/origin"),
            entry("/cleared", "clear", witness(10, DIR_MODE, 0, 0), made_directory(), 0),
            entry("/src", "chmod_dir", witness(7, DIR_MODE, 0, 0), overlay(0, 0, PRIVATE_DIR_MODE), kept(READONLY, "dir"), meta_from=directory, meta_to=private),
            entry("/src/deep", "chmod_dir", witness(8, DIR_MODE, 0, 0), made_directory(), kept(HIDDEN, "dir"), meta_from=directory, meta_to=hidden),
        ]
    else:
        was = (0o644, ARCHIVE)
        es += [
            entry("/src/lib.rs", "replace", content_witness(0x22, *was), overlay(0x33, 7, FILE_MODE), kept(ARCHIVE, "file")),
            entry("/run.sh", "chmod", content_witness(0x55, *was), overlay(0x55, 9, EXECUTABLE_MODE), kept(HIDDEN | ARCHIVE, "file"), meta_from=was, meta_to=executable),
            entry("/src/gone.rs", "delete", content_witness(0x44, 0, 0), None, 0),
            entry("/old", "rmdir", content_witness(0, 0, 0), None, 0),
            entry("/src", "chmod_dir", content_witness(0, *directory), overlay(0, 0, PRIVATE_DIR_MODE), kept(READONLY, "dir"), meta_from=directory, meta_to=private),
            entry("/src/deep", "chmod_dir", content_witness(0, *directory), made_directory(), kept(HIDDEN, "dir"), meta_from=directory, meta_to=hidden),
        ]
    return es

def landing_order(es):
    es = sorted(es, key=lambda e: (CLASS[e["action"]], e["path"].encode()))
    keep = [e for e in es if e["action"] not in BOTTOM_UP]
    removals = [e for e in es if e["action"] in BOTTOM_UP]
    removals.sort(key=lambda e: (CLASS[e["action"]], -e["path"].count("/"), e["path"].encode()))
    return keep + removals

def encode_entry(e, basis):
    out = put_str(e["path"]) + bytes([TAGS[e["action"]]])
    if e["action"] == "rename":
        out += put_str(e["origin"])
    elif e["action"] == "symlink":
        out += put_str(e["target"])
    elif e["action"] in ("chmod", "chmod_dir"):
        out += put_meta(*e["meta_from"]) + put_meta(*e["meta_to"])
    w = e["witnessed"]
    if w is None:
        out += b"\x00"
    elif basis == "witness":
        fp = w["fp"]
        out += b"\x01" + u64(fp["dev"]) + u64(fp["ino"]) + u64(fp["size"]) + i64(fp["mtime"]) + i64(fp["ctime"]) + u32(fp["mode"]) + w["identity"]
    else:
        fp = w["fp"]
        out += b"\x02" + w["identity"] + put_meta(fp["mode"] & 0o7777, kept(fp["windows"], "file"))
    o = e["overlay"]
    if o is None:
        out += b"\x00"
    else:
        out += b"\x01" + o["hash"] + u64(o["size"]) + u32(o["mode"]) + i64(o["mtime"])
    out += u16(e["windows"])
    return out

def vector_text(identities):
    text = PREAMBLE
    for basis, layout in (("witness", 3), ("content", 5)):
        es = landing_order(golden_entries(basis))
        header = b"SLMF" + u16(layout) + u32(len(es))
        text += f"{layout} header {header.hex()}\n"
        for e in es:
            text += f"{layout} {e['path']} {encode_entry(e, basis).hex()}\n"
        text += f"{layout} identity {identities[layout]}\n"
    return text

if __name__ == "__main__":
    identities = {3: sys.argv[1], 5: sys.argv[2]}
    sys.stdout.write(vector_text(identities))
