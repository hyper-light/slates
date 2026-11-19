#!/usr/bin/env python3
"""Makes slates-svn's ra_svn read-command fixtures (A-613; §4.17.4 "4. Reads", "5. The wire" and piece 7's second part).

Stock `svnserve` answering stock `svn` and scripted clients, each direction of each connection logged byte for byte by a
relay between the two before it forwards it:
- The repository is made from a dump this script writes (`svnadmin load`), with a fixed UUID (`svnadmin setuuid`) and
  every revision's author, date and log fixed, revision 0's date too (`svnadmin setrevprop`), so no transcript names a
  random value or the time it was made.
- The relay listens on 127.0.0.1:PORT, a fixed port, so every URL a transcript holds is the same on every run, and runs
  `svnserve -i` for each connection it accepts: inetd mode, the svn:// protocol over the process's standard input and
  output, so no second port is ever raced for. `anon-access = read` and no password database, so the server offers
  ANONYMOUS alone (`serve.c` `send_mechs`).
- Stock sessions run `svn` with an empty configuration directory, non-interactive, no authentication cache.
- Scripted sessions send the stock client's own greeting and ANONYMOUS answer (taken from the first stock session), then
  exact commands, the server's refusals among them, and close their sending half; the server answers each in turn.

Usage: python3 -I make-fixtures.py OUTDIR WORKDIR
Writes NAME.N.client.bin and NAME.N.server.bin per connection N of each session, sessions.txt (each session's command
or commands, its exit code and its standard error), repository.txt (each revision's rows, texts with their MD5 and
SHA-1, property lists and revision properties, as the tests rebuild the repository), subversion-version.txt and
SHA256SUMS. Every file is the same on every run. WORKDIR holds the repository and is removed at the end.

Sources: Subversion's `subversion/libsvn_ra_svn/protocol` §2 and §3.1.1, `svnserve/serve.c`, `svnserve/svnserve.c`
(`-i`), `notes/dump-load-format.txt`; all at `1.14.x` `d434dc14` as `docs/wip/research/vcs-servers-subversion.md`
§1 names them, the binaries stock 1.15.0.
"""

import hashlib
import os
import shutil
import socket
import subprocess
import sys
import threading

# The relay's port: fixed, so every URL in every transcript is the same on every run.
PORT = 36913
# The repository's name beneath the served root, and its fixed identity.
REPOSITORY = "repo"
UUID = "00000000-0000-4000-8000-00000000a613"
URL = "svn://127.0.0.1:%d/%s" % (PORT, REPOSITORY)
# Bytes one read takes from a socket or a pipe.
CHUNK = 65536
# How long any one step may take before the run is refused.
STEP_SECONDS = 60

# The repository, revision by revision: revision properties, then rows (path, kind, action, properties or None for
# unchanged, text or None for unchanged, copy source or None). Revision 0's date is set after the load.
REVISION_ZERO_DATE = "2026-01-01T00:00:00.000000Z"
REVISIONS = [
    ({"svn:author": "alice", "svn:date": "2026-01-01T01:00:00.000000Z", "svn:log": "the first trees"}, [
        ("trunk", "dir", "add", {}, None, None),
        ("branches", "dir", "add", {}, None, None),
        ("tags", "dir", "add", {}, None, None),
        ("trunk/a.txt", "file", "add", {"color": "blue"}, b"alpha\n", None),
        ("trunk/dir", "dir", "add", {"svn:ignore": "*.o\n"}, None, None),
        ("trunk/dir/b.txt", "file", "add", {}, b"beta\n", None),
    ]),
    ({"svn:author": "bob", "svn:date": "2026-01-02T02:00:00.000000Z", "svn:log": "more alpha"}, [
        ("trunk/a.txt", "file", "change", None, b"alpha\nmore\n", None),
    ]),
    ({"svn:author": "alice", "svn:date": "2026-01-03T03:00:00.000000Z", "svn:log": "branch b1", "x-ticket": "A-613"}, [
        ("branches/b1", "dir", "add", None, None, ("trunk", 2)),
    ]),
    ({"svn:author": "bob", "svn:date": "2026-01-04T04:00:00.000000Z", "svn:log": "branch work"}, [
        ("branches/b1/a.txt", "file", "change", None, b"alpha\nbranch\n", None),
    ]),
    ({"svn:author": "alice", "svn:date": "2026-01-05T05:00:00.000000Z", "svn:log": "merge b1"}, [
        ("trunk", "dir", "change", {"svn:mergeinfo": "/branches/b1:4"}, None, None),
        ("trunk/a.txt", "file", "change", None, b"alpha\nmore\nbranch\n", None),
    ]),
    ({"svn:author": "bob", "svn:date": "2026-01-06T06:00:00.000000Z", "svn:log": "drop b"}, [
        ("trunk/dir/b.txt", "file", "delete", None, None, None),
    ]),
    ({"svn:author": "alice", "svn:date": "2026-01-07T07:00:00.000000Z", "svn:log": "mergeinfo below"}, [
        ("trunk/dir", "dir", "change", {"svn:ignore": "*.o\n", "svn:mergeinfo": "/branches/b1/dir:3-4*"}, None, None),
        ("tags/t1", "dir", "add", None, None, ("trunk", 5)),
    ]),
]


def props_block(props):
    """A property list as the dump format writes it, `PROPS-END` closing it."""
    out = b""
    for name in sorted(props):
        key = name.encode()
        value = props[name].encode()
        out += b"K %d\n%s\nV %d\n%s\n" % (len(key), key, len(value), value)
    return out + b"PROPS-END\n"


def dump():
    """The repository's dump, format 2, every length computed."""
    out = b"SVN-fs-dump-format-version: 2\n\nUUID: %s\n\n" % UUID.encode()
    for number, (revprops, rows) in enumerate(REVISIONS, start=1):
        block = props_block(revprops)
        out += b"Revision-number: %d\nProp-content-length: %d\nContent-length: %d\n\n%s\n" % (
            number, len(block), len(block), block)
        for path, kind, action, props, text, copy in rows:
            out += b"Node-path: %s\n" % path.encode()
            if action == "delete":
                out += b"Node-action: delete\n\n\n"
                continue
            out += b"Node-kind: %s\nNode-action: %s\n" % (kind.encode(), action.encode())
            if copy:
                out += b"Node-copyfrom-rev: %d\nNode-copyfrom-path: %s\n" % (copy[1], copy[0].encode())
            body = b""
            if props is not None:
                block = props_block(props)
                out += b"Prop-content-length: %d\n" % len(block)
                body += block
            if text is not None:
                out += b"Text-content-length: %d\n" % len(text)
                body += text
            out += b"Content-length: %d\n\n%s\n\n" % (len(body), body)
    return out


def repository_record():
    """What the tests rebuild the repository from: each revision's properties and rows, each text's digests."""
    lines = ["revision 0 svn:date %s" % REVISION_ZERO_DATE]
    for number, (revprops, rows) in enumerate(REVISIONS, start=1):
        for name in sorted(revprops):
            lines.append("revision %d %s %s" % (number, name, revprops[name].encode().hex()))
        for path, kind, action, props, text, copy in rows:
            row = "row %d /%s %s %s" % (number, path, kind, action)
            if copy:
                row += " copy /%s %d" % copy
            if props is not None:
                row += " props " + ",".join("%s=%s" % (n.encode().hex(), props[n].encode().hex()) for n in sorted(props))
            if text is not None:
                row += " text %s md5 %s sha1 %s" % (
                    text.hex(), hashlib.md5(text).hexdigest(), hashlib.sha1(text).hexdigest())
            lines.append(row)
    return "\n".join(lines) + "\n"


def run(arguments, **keywords):
    """A stock tool run to completion, refused loudly when it fails."""
    done = subprocess.run(arguments, capture_output=True, timeout=STEP_SECONDS, **keywords)
    if done.returncode != 0:
        sys.exit("%s failed (%d): %s" % (arguments, done.returncode, done.stderr.decode(errors="replace")))
    return done


def make_repository(root):
    """The repository at ROOT/REPOSITORY: created, its identity fixed, the dump loaded, revision 0's date fixed."""
    path = os.path.join(root, REPOSITORY)
    run(["svnadmin", "create", path])
    run(["svnadmin", "load", "--quiet", path], input=dump())
    run(["svnadmin", "setuuid", path, UUID])
    date = os.path.join(root, "r0-date")
    with open(date, "w") as handle:
        handle.write(REVISION_ZERO_DATE)
    run(["svnadmin", "setrevprop", path, "-r", "0", "svn:date", date])
    with open(os.path.join(path, "conf", "svnserve.conf"), "w") as handle:
        handle.write("[general]\nanon-access = read\nauth-access = write\n")


class Relay:
    """Accepts connections on PORT, runs `svnserve -i` for each, and logs both directions of each."""

    def __init__(self, root):
        self.root = root
        self.listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", PORT))
        self.listener.listen(8)
        self.connections = []
        self.lock = threading.Lock()
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        while True:
            try:
                peer, _ = self.listener.accept()
            except OSError:
                return
            record = {"client": bytearray(), "server": bytearray(), "done": threading.Event()}
            with self.lock:
                self.connections.append(record)
            threading.Thread(target=self.serve, args=(peer, record), daemon=True).start()

    def serve(self, peer, record):
        server = subprocess.Popen(["svnserve", "-i", "-r", self.root], stdin=subprocess.PIPE, stdout=subprocess.PIPE)

        def upward():
            while True:
                data = peer.recv(CHUNK)
                if not data:
                    break
                record["client"].extend(data)
                server.stdin.write(data)
                server.stdin.flush()
            server.stdin.close()

        sender = threading.Thread(target=upward, daemon=True)
        sender.start()
        while True:
            data = server.stdout.read1(CHUNK)
            if not data:
                break
            record["server"].extend(data)
            peer.sendall(data)
        server.wait(timeout=STEP_SECONDS)
        sender.join(timeout=STEP_SECONDS)
        peer.close()
        record["done"].set()

    def take(self):
        """The connections since the last take, each waited for."""
        with self.lock:
            taken, self.connections = self.connections, []
        for record in taken:
            if not record["done"].wait(STEP_SECONDS):
                sys.exit("a connection did not end")
        return taken


def item(*parts):
    """An ra_svn list of words, numbers (ints), strings (bytes) and lists (tuples), each followed by a space."""
    out = b"( "
    for part in parts:
        if isinstance(part, tuple):
            out += item(*part)
        elif isinstance(part, int):
            out += b"%d " % part
        elif isinstance(part, bytes):
            out += b"%d:%s " % (len(part), part)
        else:
            out += part.encode() + b" "
    return out + b") "


def command(name, *params):
    """One command: its name and its parameter list."""
    return item(name, tuple(params))


# Scripted sessions: each a list of commands sent after the stock client's greeting and ANONYMOUS answer.
SCRIPTED = {
    "latest-and-dated": [
        command("get-latest-rev"),
        command("get-dated-rev", b"2026-01-03T03:00:00.000000Z"),
        command("get-dated-rev", b"2026-01-03T02:59:59.999999Z"),
        command("get-dated-rev", b"2025-12-31T23:59:59.000000Z"),
        command("get-dated-rev", b"2027-01-01T00:00:00.000000Z"),
        command("get-dated-rev", b"2026-01-04"),
        command("get-dated-rev", b"not a date"),
    ],
    "revision-properties": [
        command("rev-proplist", 0),
        command("rev-proplist", 3),
        command("rev-prop", 1, b"svn:log"),
        command("rev-prop", 3, b"x-ticket"),
        command("rev-prop", 2, b"x-missing"),
        command("rev-proplist", 99),
        command("rev-prop", 99, b"svn:log"),
    ],
    "check-path": [
        command("check-path", b"", ()),
        command("check-path", b"trunk/a.txt", ()),
        command("check-path", b"trunk/dir", (1,)),
        command("check-path", b"trunk/missing", ()),
        command("check-path", b"trunk/dir/b.txt", (5,)),
        command("check-path", b"trunk/dir/b.txt", (6,)),
        command("check-path", b"/trunk//dir/./", ()),
        command("check-path", b"trunk", (99,)),
    ],
    "stat": [
        command("stat", b"", ()),
        command("stat", b"trunk/a.txt", ()),
        command("stat", b"trunk/a.txt", (1,)),
        command("stat", b"trunk/dir", (7,)),
        command("stat", b"branches/b1/a.txt", (3,)),
        command("stat", b"trunk/missing", ()),
        command("stat", b"trunk", (99,)),
    ],
    "get-mergeinfo": [
        command("get-mergeinfo", (b"trunk", b"trunk/a.txt", b"branches/b1"), (), "inherited", "false"),
        command("get-mergeinfo", (b"trunk/a.txt",), (5,), "explicit", "false"),
        command("get-mergeinfo", (b"trunk",), (), "nearest-ancestor", "false"),
        command("get-mergeinfo", (b"trunk/dir",), (), "nearest-ancestor", "false"),
        command("get-mergeinfo", (b"",), (), "inherited", "true"),
        command("get-mergeinfo", (b"tags/t1/dir",), (), "inherited", "false"),
        command("get-mergeinfo", (), (), "inherited", "false"),
        command("get-mergeinfo", (b"trunk",), (4,), "other-word", "false"),
        command("get-mergeinfo", (b"trunk/missing",), (), "inherited", "false"),
    ],
    "get-iprops": [
        command("get-iprops", b"trunk/a.txt", (7,)),
        command("get-iprops", b"trunk/dir", (7,)),
        command("get-iprops", b"tags/t1/a.txt", (7,)),
        command("get-iprops", b"", (7,)),
        command("get-iprops", b"trunk/missing", (7,)),
        command("get-iprops", b"trunk/a.txt", ()),
    ],
    "hostile-malformed": [
        command("check-path", 5, ()),
        command("get-latest-rev"),
    ],
    "unknown-command": [
        command("no-such-command"),
        command("get-latest-rev"),
    ],
}

# Stock sessions: each a list of `svn` argument lists, run one after another.
STOCK = {
    "svn-info": [["info", URL]],
    "svn-info-file": [["info", "-r", "2", URL + "/trunk/a.txt"]],
    "svn-revprop-get": [["propget", "--revprop", "-r", "1", "svn:log", URL]],
    "svn-revprop-list": [["proplist", "--revprop", "-v", "-r", "3", URL]],
    "svn-info-dated": [["info", "-r", "{2026-01-03T12:00:00Z}", URL]],
    "svn-proplist-inherited": [["proplist", "-v", "--show-inherited-props", URL + "/trunk/dir"]],
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
    make_repository(root)
    relay = Relay(root)
    sessions = []
    written = []
    greeting = None
    for name, commands in STOCK.items():
        for arguments in commands:
            done = subprocess.run(
                ["svn", "--config-dir", config, "--non-interactive", "--no-auth-cache"] + arguments,
                capture_output=True, timeout=STEP_SECONDS)
            sessions.append("%s: svn %s -> %d %r" % (name, " ".join(arguments), done.returncode,
                                                     done.stderr.decode(errors="replace").strip()))
        for number, record in enumerate(relay.take(), start=1):
            if greeting is None:
                greeting = bytes(record["client"])
            written += write_pair(outdir, "%s.%d" % (name, number), record)
    opening = opening_items(greeting)
    for name, commands in SCRIPTED.items():
        peer = socket.create_connection(("127.0.0.1", PORT), timeout=STEP_SECONDS)
        peer.sendall(opening + b"".join(commands))
        peer.shutdown(socket.SHUT_WR)
        while peer.recv(CHUNK):
            pass
        peer.close()
        sessions.append("%s: scripted, %d commands" % (name, len(commands)))
        for number, record in enumerate(relay.take(), start=1):
            written += write_pair(outdir, "%s.%d" % (name, number), record)
    relay.listener.close()
    write(outdir, "sessions.txt", ("\n".join(sessions) + "\n").encode(), written)
    write(outdir, "repository.txt", repository_record().encode(), written)
    versions = run(["svn", "--version", "--quiet"]).stdout + run(["svnserve", "--version", "--quiet"]).stdout
    write(outdir, "subversion-version.txt", versions, written)
    sums = "".join("%s  %s\n" % (digest, name) for name, digest in sorted(written))
    with open(os.path.join(outdir, "SHA256SUMS"), "w") as handle:
        handle.write(sums)
    shutil.rmtree(workdir, ignore_errors=True)


def item_end(data, start):
    """Where the ra_svn item starting at `start` (after any spaces) ends, its trailing space included."""
    position, depth = start, 0
    while position < len(data):
        byte = data[position:position + 1]
        if byte in (b" ", b"\n"):
            position += 1
        elif byte == b"(":
            depth += 1
            position += 1
        elif byte == b")":
            depth -= 1
            position += 1
            if depth == 0:
                return position + 1 if data[position:position + 1] in (b" ", b"\n") else position
        elif byte.isdigit():
            digits = position
            while data[position:position + 1].isdigit():
                position += 1
            if data[position:position + 1] == b":":
                position += 1 + int(data[digits:position])
            if depth == 0:
                return position + 1
        else:
            while position < len(data) and data[position:position + 1] not in (b" ", b"\n", b"(", b")"):
                position += 1
            if depth == 0:
                return position + 1
    sys.exit("an item ran past the data: %r" % data[start:start + 200])


def opening_items(greeting):
    """The stock client's first two items, its greeting answer and its authentication answer, as it sent them."""
    if not greeting:
        sys.exit("no stock session recorded the client's greeting")
    first = item_end(greeting, 0)
    second = item_end(greeting, first)
    opening = greeting[:second]
    if b"ANONYMOUS" not in opening[first:]:
        sys.exit("the stock client's second item is not ANONYMOUS: %r" % opening)
    return opening


def write_pair(outdir, stem, record):
    written = []
    write(outdir, stem + ".client.bin", bytes(record["client"]), written)
    write(outdir, stem + ".server.bin", bytes(record["server"]), written)
    return written


def write(outdir, name, data, written):
    with open(os.path.join(outdir, name), "wb") as handle:
        handle.write(data)
    written.append((name, hashlib.sha256(data).hexdigest()))


if __name__ == "__main__":
    main()
