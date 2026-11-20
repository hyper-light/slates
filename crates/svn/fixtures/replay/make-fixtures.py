#!/usr/bin/env python3
"""Makes slates-svn's ra_svn replay fixtures (A-703; §4.17.4 "5. The wire"): stock `svnserve` answering stock `svnsync`
and `svnrdump`, the oracle for `replay` and `replay-range`.

The repository is the reads fixtures' (`../reads/make-fixtures.py`, imported whole: its dump, its fixed UUID, dates and
authors, its relay), so the tests rebuild it from `../reads/repository.txt` and nothing here re-records the reads
transcripts. Each direction of each connection is logged byte for byte by the reads recorder's relay before it
forwards it; `svnserve -i` serves each connection, `anon-access = read`.

Sessions:
- `svnrdump-dump`: `svnrdump dump URL`, the whole history (`replay-range` from revision 0 with deltas sent); its
  output is kept as `svnrdump-dump.dump`, what a client writes from the answers.
- `svnsync`: `svnsync initialize` of a new local repository (whose pre-revprop-change hook allows every change) from
  URL, then `svnsync synchronize` of it (`replay-range` revision by revision; its revision properties set after);
  `svnadmin dump` of the synced repository is kept as `svnsync.dump`.
- Scripted, after the stock client's greeting and ANONYMOUS answer: `replay` of single revisions with and without
  deltas, at low-water-marks below and above the revision's copy sources, and `replay-range` of a part of the history.

Usage: python3 -I make-fixtures.py OUTDIR WORKDIR
Writes NAME.N.client.bin and NAME.N.server.bin per connection N of each session, the two dumps, sessions.txt (each
command, its exit code and its standard error), subversion-version.txt and SHA256SUMS. WORKDIR is removed at the end.
"""

import importlib.util
import os
import shutil
import socket
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
SPEC = importlib.util.spec_from_file_location("reads", os.path.join(HERE, "..", "reads", "make-fixtures.py"))
READS = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(READS)

URL = READS.URL
# The last item of a replay's and a replay-range's answer: `finish-replay`, then the command's success. A scripted
# session waits for it before it closes.
DONE = b"( finish-replay ( ) ) ( success ( ) ) "
# The mirror's fixed identity, so its dump is the same on every run (`svnadmin setuuid`).
MIRROR_UUID = "00000000-0000-4000-8000-00000000a703"


def command(name, *params):
    """One ra_svn command item, as the reads recorder writes one."""
    return READS.command(name, *params)


# Scripted replays: (name, commands). Revision 3 copies /trunk@2 to /branches/b1 and revision 9 replaces
# /branches/b1/a.txt by a copy of /trunk/a.txt@5 (../reads/repository.txt).
SCRIPTED = {
    "replay-copy": [b"( replay ( 3 0 true ) ) "],
    "replay-copy-no-deltas": [b"( replay ( 3 0 false ) ) "],
    "replay-copy-below-water": [b"( replay ( 3 3 true ) ) "],
    "replay-replace": [b"( replay ( 9 0 true ) ) "],
    "replay-zero": [b"( replay ( 0 0 true ) ) "],
    "replay-range-part": [b"( replay-range ( 2 5 0 true ) ) "],
}


def main():
    if len(sys.argv) != 3:
        sys.exit(__doc__)
    outdir, workdir = sys.argv[1], sys.argv[2]
    os.makedirs(outdir, exist_ok=True)
    shutil.rmtree(workdir, ignore_errors=True)
    os.makedirs(workdir)
    root = os.path.join(workdir, "root")
    os.makedirs(root)
    config = os.path.join(workdir, "config")
    os.makedirs(config)
    READS.make_repository(root)
    relay = READS.Relay(root)
    sessions, written = [], []
    greeting = None

    def stock(name, tool, arguments, keep=None):
        nonlocal greeting
        done = subprocess.run([tool, "--config-dir", config, "--non-interactive", "--no-auth-cache"] + arguments,
                              capture_output=True, timeout=READS.STEP_SECONDS * 10)
        sessions.append("%s: %s %s -> %d %r" % (name, tool, " ".join(arguments), done.returncode,
                                                done.stderr.decode(errors="replace").strip()))
        if keep:
            READS.write(outdir, keep, done.stdout, written)
        for number, record in enumerate(relay.take(), start=1):
            if greeting is None:
                greeting = bytes(record["client"])
            written.extend(READS.write_pair(outdir, "%s.%d" % (name, number), record))

    stock("svnrdump-dump", "svnrdump", ["dump", URL], keep="svnrdump-dump.dump")
    mirror = os.path.join(workdir, "mirror")
    READS.run(["svnadmin", "create", mirror])
    READS.run(["svnadmin", "setuuid", mirror, MIRROR_UUID])
    hook = os.path.join(mirror, "hooks", "pre-revprop-change")
    with open(hook, "w") as handle:
        handle.write("#!/bin/sh\nexit 0\n")
    os.chmod(hook, 0o755)
    mirror_url = "file://" + mirror
    stock("svnsync-initialize", "svnsync", ["initialize", mirror_url, URL])
    stock("svnsync-synchronize", "svnsync", ["synchronize", mirror_url])
    READS.write(outdir, "svnsync.dump", READS.run(["svnadmin", "dump", "--quiet", mirror]).stdout, written)
    opening = READS.opening_items(greeting)
    for name, commands in SCRIPTED.items():
        # The connection stays open while svnserve answers: a replay's editor watches its client's input, and an
        # input that ends mid-drive is answered `abort-edit` and "Network connection closed unexpectedly" (210002).
        peer = socket.create_connection(("127.0.0.1", READS.PORT), timeout=READS.STEP_SECONDS)
        peer.sendall(opening + b"".join(commands))
        answered = bytearray()
        while not answered.endswith(DONE):
            data = peer.recv(READS.CHUNK)
            if not data:
                break
            answered.extend(data)
        peer.shutdown(socket.SHUT_WR)
        while peer.recv(READS.CHUNK):
            pass
        peer.close()
        sessions.append("%s: scripted, %d commands" % (name, len(commands)))
        for number, record in enumerate(relay.take(), start=1):
            written.extend(READS.write_pair(outdir, "%s.%d" % (name, number), record))
    relay.listener.close()
    READS.write(outdir, "sessions.txt", ("\n".join(sessions) + "\n").encode(), written)
    versions = READS.run(["svnsync", "--version", "--quiet"]).stdout + READS.run(["svnserve", "--version", "--quiet"]).stdout
    READS.write(outdir, "subversion-version.txt", versions, written)
    sums = "".join("%s  %s\n" % (digest, name) for name, digest in sorted(written))
    with open(os.path.join(outdir, "SHA256SUMS"), "w") as handle:
        handle.write(sums)
    shutil.rmtree(workdir, ignore_errors=True)


if __name__ == "__main__":
    main()
