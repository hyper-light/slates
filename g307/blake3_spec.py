"""BLAKE3 (hash mode, 32-byte output) written from the BLAKE3 specification (O'Connor, Aumasson, Neves, Wilcox-O'Hearn,
"BLAKE3: one function, fast everywhere", 2020): the compression function, chunk chaining and the binary tree of parent
nodes. Pure Python, no dependency: an independent hash for golden vectors (A-626 part 3), checked against the empty
input, the official test vectors (input byte i is i % 251) and A-646's ops-document golden vector before use."""

import sys

IV = [0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A, 0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19]
MSG_PERMUTATION = [2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8]
CHUNK_START, CHUNK_END, PARENT, ROOT = 1, 2, 4, 8
BLOCK_LEN, CHUNK_LEN = 64, 1024
MASK = 0xFFFFFFFF


def rotr(x, n):
    return ((x >> n) | (x << (32 - n))) & MASK


def g(s, a, b, c, d, mx, my):
    s[a] = (s[a] + s[b] + mx) & MASK
    s[d] = rotr(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & MASK
    s[b] = rotr(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b] + my) & MASK
    s[d] = rotr(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & MASK
    s[b] = rotr(s[b] ^ s[c], 7)


def one_round(s, m):
    g(s, 0, 4, 8, 12, m[0], m[1])
    g(s, 1, 5, 9, 13, m[2], m[3])
    g(s, 2, 6, 10, 14, m[4], m[5])
    g(s, 3, 7, 11, 15, m[6], m[7])
    g(s, 0, 5, 10, 15, m[8], m[9])
    g(s, 1, 6, 11, 12, m[10], m[11])
    g(s, 2, 7, 8, 13, m[12], m[13])
    g(s, 3, 4, 9, 14, m[14], m[15])


def compress(cv, words, counter, block_len, flags):
    s = list(cv[:8]) + IV[:4] + [counter & MASK, (counter >> 32) & MASK, block_len, flags]
    m = list(words)
    for r in range(7):
        one_round(s, m)
        if r < 6:
            m = [m[i] for i in MSG_PERMUTATION]
    for i in range(8):
        s[i] ^= s[i + 8]
        s[i + 8] ^= cv[i]
    return s


def words(block):
    return [int.from_bytes(block[4 * i:4 * i + 4], "little") for i in range(16)]


class Output:
    def __init__(self, cv, block_words, counter, block_len, flags):
        self.cv, self.block_words, self.counter, self.block_len, self.flags = cv, block_words, counter, block_len, flags

    def chaining_value(self):
        return compress(self.cv, self.block_words, self.counter, self.block_len, self.flags)[:8]

    def root_bytes(self, length):
        out = bytearray()
        counter = 0
        while len(out) < length:
            for w in compress(self.cv, self.block_words, counter, self.block_len, self.flags | ROOT):
                out += w.to_bytes(4, "little")
            counter += 1
        return bytes(out[:length])


class Chunk:
    def __init__(self, counter):
        self.cv, self.counter, self.block, self.compressed = list(IV), counter, bytearray(), 0

    def length(self):
        return BLOCK_LEN * self.compressed + len(self.block)

    def start(self):
        return CHUNK_START if self.compressed == 0 else 0

    def update(self, data):
        while data:
            if len(self.block) == BLOCK_LEN:
                self.cv = compress(self.cv, words(bytes(self.block)), self.counter, BLOCK_LEN, self.start())[:8]
                self.compressed += 1
                self.block = bytearray()
            take = data[:BLOCK_LEN - len(self.block)]
            self.block += take
            data = data[len(take):]

    def output(self):
        block = bytes(self.block) + bytes(BLOCK_LEN - len(self.block))
        return Output(self.cv, words(block), self.counter, len(self.block), self.start() | CHUNK_END)


def parent(left, right):
    return Output(list(IV), left + right, 0, BLOCK_LEN, PARENT)


def blake3(data):
    chunk, stack = Chunk(0), []
    while data:
        if chunk.length() == CHUNK_LEN:
            cv, total = chunk.output().chaining_value(), chunk.counter + 1
            while total & 1 == 0:
                cv = parent(stack.pop(), cv).chaining_value()
                total >>= 1
            stack.append(cv)
            chunk = Chunk(chunk.counter + 1)
        take = data[:CHUNK_LEN - chunk.length()]
        chunk.update(take)
        data = data[len(take):]
    output = chunk.output()
    for left in reversed(stack):
        output = parent(left, output.chaining_value())
    return output.root_bytes(32).hex()


def official(length):
    return blake3(bytes(i % 251 for i in range(length)))


if __name__ == "__main__":
    data = sys.stdin.buffer.read()
    print(len(data), blake3(data))
