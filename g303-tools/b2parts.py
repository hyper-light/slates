import struct, sys
def parts(b):
    assert b[:4] == b"HG20", b[:4]
    n = struct.unpack(">i", b[4:8])[0]; pos = 8 + n
    out = []
    while True:
        hs = struct.unpack(">i", b[pos:pos+4])[0]; pos += 4
        if hs == 0: break
        h = b[pos:pos+hs]; pos += hs
        tl = h[0]; kind = h[1:1+tl]; q = 1 + tl
        pid = struct.unpack(">I", h[q:q+4])[0]; q += 4
        mc, ac = h[q], h[q+1]; q += 2
        sizes = [(h[q+2*i], h[q+2*i+1]) for i in range(mc + ac)]; q += 2 * (mc + ac)
        params = []
        for ks, vs in sizes:
            params.append((h[q:q+ks], h[q+ks:q+ks+vs])); q += ks + vs
        payload = b""
        while True:
            cs = struct.unpack(">i", b[pos:pos+4])[0]; pos += 4
            if cs == 0: break
            payload += b[pos:pos+cs]; pos += cs
        out.append((kind, pid, mc, params, payload))
    return out
data = open(sys.argv[1], "rb").read()
if b"\r\n\r\n" in data[:2000] and data.startswith((b"POST", b"GET")):
    data = data[data.index(b"\r\n\r\n") + 4:]
for kind, pid, mc, params, payload in parts(data):
    print(kind, pid, "mandatory params", mc, params, len(payload), payload[:200] if kind.lower() != b"changegroup" else payload[:60].hex())
