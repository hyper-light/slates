#!/usr/bin/env python3
"""Makes slates-svn's fold fixture (A-613 part 2, step 7): names folded as `svn_utf__fuzzy_glob_match` folds them before
`svn_repos_list` matches a pattern, by the utf8proc library stock `svnserve` links, called as Subversion calls it.

Subversion 1.15.0's `libsvn_subr/utf8proc.c` (`normalize_cstring`, through `svn_utf__xfrm` with case and accents
ignored): `utf8proc_decompose` with UTF8PROC_DECOMPOSE | UTF8PROC_STABLE | UTF8PROC_CASEFOLD | UTF8PROC_STRIPMARK, then
`utf8proc_reencode` with UTF8PROC_COMPOSE | UTF8PROC_STABLE. This script loads the library with ctypes and makes the
same two calls; the option values are read from the library's own header.

Usage: python3 -I make-fold.py LIBRARY HEADER OUTDIR
Writes OUTDIR/fold.txt and OUTDIR/SHA256SUMS. fold.txt's first lines name the library's version and its Unicode
version; then `every` lines: each Unicode scalar value alone whose fold is not itself, `CODE:FOLDED`, FOLDED the fold's
code points (empty where the value folds to nothing), all in hexadecimal; then `name` lines: names of several code points
(a precomposed and a decomposed letter, marks around letters, Hangul, a final sigma), `CODES:FOLDED`. Every value not
listed folds to itself. The same file on every run with the same library.
"""

import ctypes
import hashlib
import os
import re
import sys

# Names of several code points, each a string; their folds exercise what single values cannot: a mark stripped
# between letters, canonical order and composition across code points (Hangul jamo composed again), case folds that
# expand, and the order utf8proc applies its options in (a mark stripped before it would fold).
NAMES = [
    "Ärger.txt",
    "Ärger.txt",
    "STRASSE",
    "Straße",
    "ΣΊΣΥΦΟΣ",
    "σίσυφος",
    "ȩ́",
    "ȩ́",
    "각",
    "가",
    "각",
    "ᾳ",
    "ᾳ",
    "İstanbul",
    "Ångström",
    "ﬃx",
    "ẛ̣",
    "q̣̇",
    "กิ่",
    "क़ि",
    "\U0001d400BC",
    "\U00010400\U00010428",
    "Ꭰꭰ",
    "Å̊",
    "Ω",
    "x⃝",
    "️❤️",
    "a‍b",
]


def options(header):
    """The option values the header defines, `UTF8PROC_NAME = (1<<N)`."""
    text = open(header, encoding="utf-8").read()
    found = {name: 1 << int(shift) for name, shift in re.findall(r"UTF8PROC_(\w+)\s*=\s*\(1\s*<<\s*(\d+)\)", text)}
    for name in ("STABLE", "COMPOSE", "DECOMPOSE", "CASEFOLD", "STRIPMARK"):
        if name not in found:
            sys.exit("the header defines no UTF8PROC_%s" % name)
    return found


def folder(library, values):
    """A function folding UTF-8 bytes as `normalize_cstring` does."""
    lib = ctypes.CDLL(library)
    lib.utf8proc_decompose.argtypes = [ctypes.c_char_p, ctypes.c_ssize_t, ctypes.POINTER(ctypes.c_int32),
                                       ctypes.c_ssize_t, ctypes.c_int]
    lib.utf8proc_decompose.restype = ctypes.c_ssize_t
    lib.utf8proc_reencode.argtypes = [ctypes.POINTER(ctypes.c_int32), ctypes.c_ssize_t, ctypes.c_int]
    lib.utf8proc_reencode.restype = ctypes.c_ssize_t
    decompose = values["DECOMPOSE"] | values["STABLE"] | values["CASEFOLD"] | values["STRIPMARK"]
    reencode = values["COMPOSE"] | values["STABLE"]

    def fold(data):
        size = 4 * len(data) + 4
        while True:
            buffer = (ctypes.c_int32 * (size + 1))()
            length = lib.utf8proc_decompose(data, len(data), buffer, size, decompose)
            if length < 0:
                raise RuntimeError("utf8proc_decompose refused %r: %d" % (data, length))
            if length <= size:
                break
            size = length
        written = lib.utf8proc_reencode(buffer, length, reencode)
        if written < 0:
            raise RuntimeError("utf8proc_reencode refused %r: %d" % (data, written))
        return ctypes.string_at(buffer, written)

    lib.utf8proc_version.restype = ctypes.c_char_p
    lib.utf8proc_unicode_version.restype = ctypes.c_char_p
    return fold, lib.utf8proc_version().decode(), lib.utf8proc_unicode_version().decode()


def hexadecimal(text):
    return " ".join("%04X" % ord(character) for character in text)


def main():
    library, header, outdir = sys.argv[1:4]
    fold, version, unicode_version = folder(library, options(header))
    lines = ["utf8proc %s" % version, "unicode %s" % unicode_version]
    for code in range(0x110000):
        if 0xD800 <= code <= 0xDFFF:
            continue
        data = chr(code).encode("utf-8")
        folded = fold(data)
        if folded != data:
            lines.append("every %04X:%s" % (code, hexadecimal(folded.decode("utf-8"))))
    for name in NAMES:
        lines.append("name %s:%s" % (hexadecimal(name), hexadecimal(fold(name.encode("utf-8")).decode("utf-8"))))
    os.makedirs(outdir, exist_ok=True)
    data = ("\n".join(lines) + "\n").encode()
    with open(os.path.join(outdir, "fold.txt"), "wb") as handle:
        handle.write(data)
    with open(os.path.join(outdir, "SHA256SUMS"), "w") as handle:
        handle.write("%s  fold.txt\n" % hashlib.sha256(data).hexdigest())
    print("utf8proc %s, Unicode %s: %d values fold to another, %d names" % (
        version, unicode_version, len(lines) - 2 - len(NAMES), len(NAMES)))
    return 0


if __name__ == "__main__":
    sys.exit(main())
