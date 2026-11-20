"""Independent golden for A-692: HMAC-MD5's keyed contexts in Dovecot's layout
(hmac_md5_get_cram_context: the outer MD5 state's words a,b,c,d then the inner's, each little-endian),
from an MD5 compression written from RFC 1321 3.3-3.4, checked against hashlib's MD5 and HMAC."""
import hashlib, hmac, math, struct

T = [int(abs(math.sin(i + 1)) * 2**32) & 0xffffffff for i in range(64)]
S = [7, 12, 17, 22] * 4 + [5, 9, 14, 20] * 4 + [4, 11, 16, 23] * 4 + [6, 10, 15, 21] * 4
INIT = (0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476)

def rotl(x, c):
    return ((x << c) | (x >> (32 - c))) & 0xffffffff

def compress(state, block):
    x = struct.unpack('<16I', block)
    a, b, c, d = state
    for i in range(64):
        if i < 16:
            f, g = (b & c) | (~b & d), i
        elif i < 32:
            f, g = (d & b) | (~d & c), (5 * i + 1) % 16
        elif i < 48:
            f, g = b ^ c ^ d, (3 * i + 5) % 16
        else:
            f, g = c ^ (b | (~d & 0xffffffff)), (7 * i) % 16
        f &= 0xffffffff
        a, d, c, b = d, c, b, (b + rotl((a + f + T[i] + x[g]) & 0xffffffff, S[i])) & 0xffffffff
    return tuple((s + v) & 0xffffffff for s, v in zip(state, (a, b, c, d)))

def finish(state, absorbed, message):
    total = absorbed + len(message)
    padded = message + b'\x80' + b'\x00' * ((55 - len(message)) % 64) + struct.pack('<Q', (total * 8) & (2**64 - 1))
    for at in range(0, len(padded), 64):
        state = compress(state, padded[at:at + 64])
    return struct.pack('<4I', *state)

for message in [b'', b'a', b'abc', b'x' * 55, b'y' * 56, b'z' * 64, b'w' * 200]:
    assert finish(INIT, 0, message) == hashlib.md5(message).digest(), message

def contexts(secret):
    key = hashlib.md5(secret).digest() if len(secret) > 64 else secret
    key = key.ljust(64, b'\x00')
    inner = compress(INIT, bytes(k ^ 0x36 for k in key))
    outer = compress(INIT, bytes(k ^ 0x5c for k in key))
    return outer, inner

def digest(pair, challenge):
    outer, inner = pair
    return finish(outer, 64, finish(inner, 64, challenge))

for secret, challenge in [(b'Jefe', b'what do ya want for nothing?'), (b'\xaa' * 80, b'Test Using Larger Than Block-Size Key - Hash Key First'), (b's' * 64, b'<1.2@h>'), (b't' * 70, b'')]:
    assert digest(contexts(secret), challenge) == hmac.new(secret, challenge, hashlib.md5).digest(), secret

outer, inner = contexts(b'Jefe')
print('Jefe contexts (outer then inner, LE words):', struct.pack('<4I', *outer).hex() + struct.pack('<4I', *inner).hex())
token = b'slates_' + bytes(range(32)).hex().encode()
outer, inner = contexts(token)
print('token', token.decode(), 'contexts:', struct.pack('<4I', *outer).hex() + struct.pack('<4I', *inner).hex())
print('hmac(token, <1.2@h>):', hmac.new(token, b'<1.2@h>', hashlib.md5).hexdigest())
