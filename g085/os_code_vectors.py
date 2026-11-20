# An independent encoder of A-539's wire forms (crates/wire's canonical encoding, from its reading: an enum's tag a
# little-endian u32, integers little-endian, a string a u32 length then its bytes), writing os_code_vectors.txt.
import struct, sys

PREAMBLE = """# The wire form of a host's refusal code (A-539): an errno in the errno variants, a Win32 error code in the Win32
# variants, for numbers both numberings name. A line is `<value> <hex>`, the value as the test builds it. Republish
# deliberately after a layout change:
#   cargo test -p slates-ipc --lib -- --ignored publish_the_os_code_vectors
"""

def tag(n): return struct.pack("<I", n)
def i32(n): return struct.pack("<i", n)
def u32(n): return struct.pack("<I", n)
def string(s):
    b = s.encode()
    return u32(len(b)) + b

TAGS = {"HostAnswer::Errno": 4, "HostAnswer::Win32": 5, "LandingDegradation::Crashed": 2,
        "LandingDegradation::CrashedWin32": 8, "TargetRefusal::Host": 7, "TargetRefusal::HostWin32": 18,
        "Refusal::BaseUnavailable": 12, "Refusal::BaseUnavailableWin32": 75}

rows = [
    ("HostAnswer::Errno{errno:17}", tag(TAGS["HostAnswer::Errno"]) + i32(17)),
    ("HostAnswer::Win32{code:17}", tag(TAGS["HostAnswer::Win32"]) + u32(17)),
    ("LandingDegradation::Crashed{errno:5}", tag(TAGS["LandingDegradation::Crashed"]) + i32(5)),
    ("LandingDegradation::CrashedWin32{code:5}", tag(TAGS["LandingDegradation::CrashedWin32"]) + u32(5)),
    ("TargetRefusal::Host{errno:5}", tag(TAGS["TargetRefusal::Host"]) + i32(5)),
    ("TargetRefusal::HostWin32{code:5}", tag(TAGS["TargetRefusal::HostWin32"]) + u32(5)),
    ("Refusal::BaseUnavailable{path:/base,errno:2}", tag(TAGS["Refusal::BaseUnavailable"]) + string("/base") + i32(2)),
    ("Refusal::BaseUnavailableWin32{path:/base,code:2}", tag(TAGS["Refusal::BaseUnavailableWin32"]) + string("/base") + u32(2)),
]
out = PREAMBLE + "".join(f"{name} {b.hex()}\n" for name, b in rows)
sys.stdout.write(out)
