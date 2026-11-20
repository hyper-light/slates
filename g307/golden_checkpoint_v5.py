"""The golden version 5 checkpoints of A-626 part 3, continued (1), laid out field by field from the format's description
(crates/merge/src/checkpoint.rs): the copies' section and the identity's sum each behind a presence byte. Hashed by
blake3_spec.py: each one's length and BLAKE3 identity."""
import importlib.util, struct

spec = importlib.util.spec_from_file_location("b3", "/Users/adalundhe/.local/state/slates-fanout/g307/blake3_spec.py")
b3 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b3)

def u32(n): return struct.pack("<I", n)
def u64(n): return struct.pack("<Q", n)
def blob(b): return u32(len(b)) + b
PRESENT, ABSENT = b"\x01", b"\x00"

# The checkpoint touching every table, a copy's record and its copied keys.
g = bytearray(b"GCKP") + u32(5) + u64(7)
g += u32(1) + bytes([1]) * 32 + u64(5)                       # accepted
g += u32(1) + u64(5) + bytes([1, 2]) + bytes([3]) * 32         # signatures
g += u32(2) + blob(b"a") + u64(3) + ABSENT + blob(b"b") + u64(5) + PRESENT + blob(b"bytes")   # files
g += u32(1) + blob(b"d") + u64(2) + PRESENT                                                     # dirs
g += u32(1) + blob(b"b") + u64(6) + PRESENT + u32(0o640)                                         # modes
g += u32(1) + blob(b"s") + u64(4) + PRESENT + blob(b"b")                                         # symlinks
g += u32(1) + blob(b"h") + u64(7) + ABSENT                                                      # hardlinks
g += u32(0)                                                                                      # specials
g += u32(1) + blob(b"b") + blob(b"user.k") + u64(6) + PRESENT + blob(bytes([9]))                 # xattrs
g += PRESENT                                                                                     # the copies' section
g += u32(1) + blob(b"tags/1") + u64(6) + blob(b"trunk") + u64(5)                                 # the copies' records
g += u32(1) + blob(b"tags/1/f") + u64(6) + PRESENT + blob(b"f")                                  # copied files
g += u32(1) + blob(b"tags/1") + u64(6) + PRESENT                                                 # copied dirs
g += u32(0) + u32(0) + u32(0) + u32(0)                                                           # modes, symlinks, hardlinks, specials
g += u32(1) + blob(b"tags/1/f") + blob(b"user.k") + u64(6) + ABSENT                              # copied xattrs
g += PRESENT + bytes(j % 251 for j in range(2048))                                               # the sum
g += PRESENT + u64(9) + bytes([4]) * 32                                                          # the head check
print("copied", len(g), b3.blake3(bytes(g)))

# A green that never copied: one accepted increment that set file f to "v1" at version 1.
n = bytearray(b"GCKP") + u32(5) + u64(1)
n += u32(1) + bytes([2]) * 32 + u64(1)                       # accepted
n += u32(0)                                                   # signatures
n += u32(1) + blob(b"f") + u64(1) + PRESENT + blob(b"v1")     # files
n += u32(0) * 6                                               # dirs, modes, symlinks, hardlinks, specials, xattrs
n += ABSENT                                                   # no copies' section
n += ABSENT                                                   # no sum
n += ABSENT                                                   # no head check
print("never_copied", len(n), b3.blake3(bytes(n)))
