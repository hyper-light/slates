#!/usr/bin/env bash
# Inflates the deltas of the packs make-fixtures.sh captured (A-582 part 3): for each entry `git verify-pack -v` lists
# with a base (ofs.verify.txt, ref.verify.txt), the delta its compressed data holds, written to
# deltas/<object id>.delta. The same object's delta must be the same bytes in both packs, and each must inflate whole
# (the zlib stream ends exactly where the listing says the entry ends) to the size the listing gives it.
#
# Python's zlib inflates them: slates-git has no inflater yet (§4.17.2 piece 3 chooses one by measurement). The crate's
# tests do not trust this script: they tie each file back to its entry in both packs by the zlib stream's Adler-32
# trailer (RFC 1950 §2.2), and apply it to a base whose name git listed, so a file that is not the pack's own delta
# fails them.
#
# Usage: make-delta-fixtures.sh FIXTURES_DIR (crates/git/fixtures). Deterministic: it reads only the committed packs and
# listings, and records the inflater's version in deltas/inflater-version.txt.
set -euo pipefail
DIR=$(cd "$1" && pwd)
mkdir -p "$DIR/deltas"
python3 -I - "$DIR" <<'PY'
import hashlib
import platform
import sys
import zlib

directory = sys.argv[1]
# The packs are SHA-1 (make-fixtures.sh): a REF_DELTA's base name takes twenty bytes.
NAME_LEN = 20


def listed_deltas(name):
    """Each delta `git verify-pack -v` lists: id, type, size, size in pack, offset, depth, base."""
    with open(f"{directory}/{name}.verify.txt") as listing:
        for line in listing:
            fields = line.split()
            if len(fields) == 7 and len(fields[0]) == 2 * NAME_LEN:
                yield fields[0], int(fields[2]), int(fields[3]), int(fields[4])


def inflated_delta(pack, at, end, size):
    """The delta the entry at `at` holds, its compressed data ending at `end`."""
    byte = pack[at]
    kind, shift, used = (byte >> 4) & 7, 4, at + 1
    named = byte & 15
    while byte & 0x80:
        byte = pack[used]
        used += 1
        named |= (byte & 0x7F) << shift
        shift += 7
    if kind == 6:
        byte = pack[used]
        used += 1
        while byte & 0x80:
            byte = pack[used]
            used += 1
    elif kind == 7:
        used += NAME_LEN
    else:
        sys.exit(f"the entry at {at} is no delta (type {kind})")
    inflater = zlib.decompressobj()
    data = inflater.decompress(pack[used:end])
    if not inflater.eof or inflater.unused_data or len(data) != size or named != size:
        sys.exit(f"the entry at {at} does not inflate whole to its {size} bytes")
    return data


deltas = {}
for name in ("ofs", "ref"):
    with open(f"{directory}/{name}.pack", "rb") as file:
        pack = file.read()
    for object_id, size, stored, at in listed_deltas(name):
        data = inflated_delta(pack, at, at + stored, size)
        if deltas.setdefault(object_id, data) != data:
            sys.exit(f"{object_id}: the two packs hold different deltas")
if len(deltas) != 2:
    sys.exit(f"expected the two deltas make-fixtures.sh's history makes, found {len(deltas)}")
for object_id, data in sorted(deltas.items()):
    with open(f"{directory}/deltas/{object_id}.delta", "wb") as out:
        out.write(data)
    print(hashlib.sha256(data).hexdigest(), f"deltas/{object_id}.delta", data.hex())
with open(f"{directory}/deltas/inflater-version.txt", "w") as out:
    out.write(f"Python {platform.python_version()}, zlib {zlib.ZLIB_RUNTIME_VERSION}\n")
PY
