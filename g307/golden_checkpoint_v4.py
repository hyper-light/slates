"""The golden version 4 checkpoint of A-626 part 3, laid out field by field from the format's description
(crates/merge/src/checkpoint.rs), hashed by blake3_spec.py: its length and BLAKE3 identity."""
import importlib.util, struct

spec = importlib.util.spec_from_file_location("b3", "/Users/adalundhe/.local/state/slates-fanout/g307/blake3_spec.py")
b3 = importlib.util.module_from_spec(spec)
spec.loader.exec_module(b3)

def u32(n): return struct.pack("<I", n)
def u64(n): return struct.pack("<Q", n)
def blob(b): return u32(len(b)) + b
PRESENT, ABSENT = b"\x01", b"\x00"

g = bytearray(b"GCKP") + u32(4) + u64(7)
g += u32(1) + bytes([1]) * 32 + u64(5)                       # accepted
g += u32(1) + u64(5) + bytes([1, 2]) + bytes([3]) * 32         # signatures
# the changed tables
g += u32(2) + blob(b"a") + u64(3) + ABSENT + blob(b"b") + u64(5) + PRESENT + blob(b"bytes")   # files
g += u32(1) + blob(b"d") + u64(2) + PRESENT                                                     # dirs
g += u32(1) + blob(b"b") + u64(6) + PRESENT + u32(0o640)                                         # modes
g += u32(1) + blob(b"s") + u64(4) + PRESENT + blob(b"b")                                         # symlinks
g += u32(1) + blob(b"h") + u64(7) + ABSENT                                                      # hardlinks
g += u32(0)                                                                                      # specials
g += u32(1) + blob(b"b") + blob(b"user.k") + u64(6) + PRESENT + blob(bytes([9]))                 # xattrs
# the copies' records
g += u32(1) + blob(b"tags/1") + u64(6) + blob(b"trunk") + u64(5)
# the copied tables
g += u32(1) + blob(b"tags/1/f") + u64(6) + PRESENT + blob(b"f")                                  # files
g += u32(1) + blob(b"tags/1") + u64(6) + PRESENT                                                 # dirs
g += u32(0) + u32(0) + u32(0) + u32(0)                                                           # modes, symlinks, hardlinks, specials
g += u32(1) + blob(b"tags/1/f") + blob(b"user.k") + u64(6) + ABSENT                              # xattrs
# the sum, then the head check
g += bytes(j % 251 for j in range(2048))
g += PRESENT + u64(9) + bytes([4]) * 32
print(len(g), b3.blake3(bytes(g)))
