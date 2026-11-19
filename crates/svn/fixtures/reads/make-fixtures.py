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
  `get-file-revs-svndiff0` sends the greeting without `svndiff1` and `accepts-svndiff2`, so its deltas are svndiff 0.
- A-613 part 2 added revisions 8 to 11 (a 10,000-byte text and its change, an empty text, a name in capitals, a path
  added again after its deletion, a file replaced by a copy, a revision without an author, mergeinfo taken away) and
  the sessions of get-file, list, get-location-segments, get-deleted-rev, log (with merged revisions) and
  get-file-revs, scripted and stock (`svn cat`, `ls`, `log`, `blame`, `mergeinfo --show-revs`).

Usage: python3 -I make-fixtures.py OUTDIR WORKDIR
Writes NAME.N.client.bin and NAME.N.server.bin per connection N of each session, sessions.txt (each session's command
or commands, its exit code and its standard error), repository.txt (each revision's rows, texts with their MD5 and
SHA-1, property lists and revision properties, as the tests rebuild the repository), subversion-version.txt and
SHA256SUMS. Every file is the same on every run but those holding a property list of more than one property:
`svnserve` writes a property list in its hash table's order, which APR seeds per table (`apr_hash_make` since APR 1.5),
so three recordings gave revision 3's four properties three orders (`revision-properties.1.server.bin`,
`svn-revprop-list.1.server.bin`), and the tests compare property lists as sets. WORKDIR holds the repository and is
removed at the end.

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

# A text of 1,000 numbered lines, 10,000 bytes: more than two of get-file's 4,096-byte strings.
BIG_EIGHT = b"".join(b"line %04d\n" % number for number in range(1000))
# The same text with its 500th line changed and one line appended, 10,010 bytes.
BIG_NINE = BIG_EIGHT.replace(b"line 0500\n", b"LINE 0500\n") + b"line 1000\n"

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
    # A-613 part 2: a text past get-file's 4,096-byte strings, an empty text, a name in capitals for list's case
    # folding, and a path deleted in revision 6 added again as a new line of history.
    ({"svn:author": "carol", "svn:date": "2026-01-08T08:00:00.000000Z", "svn:log": "big, empty, again"}, [
        ("trunk/big.txt", "file", "add", {}, BIG_EIGHT, None),
        ("trunk/empty.txt", "file", "add", {}, b"", None),
        ("trunk/README.TXT", "file", "add", {}, b"read me\n", None),
        ("trunk/dir/b.txt", "file", "add", {}, b"beta again\n", None),
    ]),
    # A change inside a large text, for get-file-revs' delta between two texts; a file replaced by a copy, for log's
    # R and get-deleted-rev.
    ({"svn:author": "bob", "svn:date": "2026-01-09T09:00:00.000000Z", "svn:log": "big change, replace"}, [
        ("trunk/big.txt", "file", "change", None, BIG_NINE, None),
        ("branches/b1/a.txt", "file", "replace", None, None, ("trunk/a.txt", 5)),
    ]),
    # A revision without an author, changing a file's properties alone.
    ({"svn:date": "2026-01-10T10:00:00.000000Z", "svn:log": "no author, props only"}, [
        ("trunk/a.txt", "file", "change", {"color": "green"}, None, None),
    ]),
    # Mergeinfo taken away: log's merged revisions answer a reverse merge.
    ({"svn:author": "alice", "svn:date": "2026-01-11T11:00:00.000000Z", "svn:log": "unmerge on the tag"}, [
        ("tags/t1", "dir", "change", {}, None, None),
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
    "get-dir": [
        command("get-dir", b"", (7,), "true", "true"),
        command("get-dir", b"trunk", (3,), "false", "true", ("kind", "size")),
        command("get-dir", b"trunk", (), "true", "false", (), "true"),
        command("get-dir", b"branches/b1", (4,), "true", "true", ("kind", "size", "has-props", "created-rev", "time",
                                                                  "last-author", "no-such-field")),
        command("get-dir", b"trunk/a.txt", (), "false", "true"),
        command("get-dir", b"trunk/missing", (), "true", "false"),
        command("get-dir", b"trunk", (99,), "true", "true"),
    ],
    "get-locations": [
        command("get-locations", b"branches/b1/a.txt", 7, (7, 4, 3, 2, 1, 0)),
        command("get-locations", b"trunk/a.txt", 2, (7, 5, 2, 1)),
        command("get-locations", b"tags/t1/dir", 7, (7, 5, 3)),
        command("get-locations", b"trunk/dir/b.txt", 5, (5, 1)),
        command("get-locations", b"trunk/dir/b.txt", 5, (6, 5)),
        command("get-locations", b"trunk/missing", 7, (7,)),
        command("get-locations", b"trunk", 99, (1,)),
        command("get-locations", b"trunk", 7, ()),
    ],
    "hostile-locations": [
        command("get-locations", b"trunk", 7, (b"seven",)),
        command("get-latest-rev"),
    ],
    "hostile-malformed": [
        command("check-path", 5, ()),
        command("get-latest-rev"),
    ],
    "unknown-command": [
        command("no-such-command"),
        command("get-latest-rev"),
    ],
    # A-613 part 2.
    "get-file": [
        command("get-file", b"trunk/a.txt", (2,), "true", "true"),
        command("get-file", b"trunk/a.txt", (), "false", "true"),
        command("get-file", b"trunk/a.txt", (), "true", "false"),
        command("get-file", b"branches/b1/a.txt", (3,), "true", "true"),
        command("get-file", b"trunk/big.txt", (), "false", "true"),
        command("get-file", b"trunk/big.txt", (8,), "false", "true", "false"),
        command("get-file", b"trunk/empty.txt", (), "true", "true"),
        command("get-file", b"trunk/a.txt", (5,), "true", "true", "true"),
        command("get-file", b"trunk/dir/b.txt", (5,), "false", "false", "true"),
        command("get-file", b"trunk/a.txt", (5,), "false", "false", "false"),
        command("get-file", b"trunk/a.txt", (10,), "true", "false"),
        command("get-file", b"trunk", (5,), "true", "true"),
        command("get-file", b"trunk/missing", (), "true", "true"),
        command("get-file", b"trunk/a.txt", (99,), "true", "true"),
        command("get-file", b"trunk/dir/b.txt", (6,), "true", "true"),
        command("get-file", b"trunk/dir/b.txt", (8,), "true", "true"),
        command("get-file", b"/trunk//a.txt/./", (2,), "false", "true"),
        command("get-file", b"tags/t1/a.txt", (11,), "true", "true", "true"),
    ],
    "hostile-get-file": [
        command("get-file", b"trunk/a.txt", (), "true", "true", "maybe"),
        command("get-latest-rev"),
    ],
    "list": [
        command("list", b"", (), "infinity", ("kind", "size", "has-props", "created-rev", "time", "last-author")),
        command("list", b"", (7,), "immediates", ("kind",)),
        command("list", b"trunk", (), "files", ()),
        command("list", b"trunk", (), "infinity"),
        command("list", b"trunk/a.txt", (), "infinity", ("kind", "size")),
        command("list", b"trunk", (), "infinity", ("kind",), (b"a*", b"*.txt")),
        command("list", b"trunk", (), "infinity", ("kind",), (b"read*",)),
        command("list", b"trunk", (), "infinity", ("kind",), ()),
        command("list", b"trunk", (), "empty", ("kind", "time")),
        command("list", b"trunk/missing", (), "infinity", ("kind",)),
        command("list", b"", (99,), "immediates", ("kind",)),
        command("list", b"trunk", (), "unknown-word", ("kind",)),
        command("list", b"trunk", (), "exclude", ("kind",)),
        command("list", b"trunk", (10,), "immediates", ("last-author", "time")),
        command("list", b"", (0,), "infinity", ("kind", "size")),
        command("list", b"tags", (), "infinity", ("kind", "created-rev", "no-such-field")),
    ],
    "hostile-list": [
        command("list", b"trunk", (), "infinity", ("kind", b"size")),
        command("get-latest-rev"),
    ],
    "get-location-segments": [
        command("get-location-segments", b"branches/b1/a.txt", (11,), (11,), (0,)),
        command("get-location-segments", b"branches/b1/a.txt", (8,), (8,), (0,)),
        command("get-location-segments", b"tags/t1/a.txt", (), (), ()),
        command("get-location-segments", b"trunk", (), (), ()),
        command("get-location-segments", b"", (), (), ()),
        command("get-location-segments", b"trunk/dir/b.txt", (8,), (8,), (0,)),
        command("get-location-segments", b"trunk/dir/b.txt", (5,), (5,), (0,)),
        command("get-location-segments", b"trunk/a.txt", (11,), (6,), (3,)),
        command("get-location-segments", b"tags/t1/a.txt", (11,), (8,), (6,)),
        command("get-location-segments", b"tags/t1/dir", (11,), (11,), (0,)),
        command("get-location-segments", b"trunk/a.txt", (5,), (6,), (0,)),
        command("get-location-segments", b"trunk/a.txt", (5,), (2,), (4,)),
        command("get-location-segments", b"trunk/missing", (), (), ()),
        command("get-location-segments", b"trunk/a.txt", (99,), (), ()),
        command("get-location-segments", b"trunk/a.txt", (18446744073709551615,), (3,), ()),
        command("get-location-segments", b"trunk/a.txt", (8,), (99,), ()),
    ],
    "get-deleted-rev": [
        command("get-deleted-rev", b"trunk/dir/b.txt", 5, 11),
        command("get-deleted-rev", b"trunk/dir/b.txt", 1, 11),
        command("get-deleted-rev", b"trunk/a.txt", 1, 11),
        command("get-deleted-rev", b"branches/b1/a.txt", 4, 11),
        command("get-deleted-rev", b"trunk/dir/b.txt", 11, 5),
        command("get-deleted-rev", b"trunk/dir/b.txt", 8, 11),
        command("get-deleted-rev", b"trunk/missing", 1, 11),
        command("get-deleted-rev", b"trunk/a.txt", 1, 99),
        command("get-deleted-rev", b"trunk/a.txt", 99, 1),
        command("get-deleted-rev", b"trunk/a.txt", 18446744073709551615, 1),
        command("get-deleted-rev", b"trunk/a.txt", 1, 18446744073709551615),
        command("get-deleted-rev", b"tags/t1", 7, 11),
        command("get-deleted-rev", b"branches/b1/a.txt", 3, 9),
        command("get-deleted-rev", b"branches/b1/a.txt", 3, 8),
        command("get-deleted-rev", b"", 0, 11),
    ],
    "log": [
        command("log", (b"",), (11,), (0,), "true", "false", 0, "false", "revprops",
                (b"svn:author", b"svn:date", b"svn:log")),
        command("log", (b"trunk",), (11,), (0,), "true", "false", 0, "false", "all-revprops", ()),
        command("log", (b"branches/b1/a.txt",), (11,), (0,), "true", "false", 0, "false", "revprops", (b"svn:log",)),
        command("log", (b"branches/b1/a.txt",), (11,), (0,), "true", "true", 0, "false", "revprops", (b"svn:log",)),
        command("log", (b"trunk",), (0,), (11,), "false", "false", 2, "false", "revprops", (b"svn:author",)),
        command("log", (b"trunk/a.txt", b"trunk/dir"), (11,), (0,), "true", "false", 0, "false", "revprops", ()),
        command("log", (b"",), (11,), (0,), "false", "false", 0, "false", "revprops", (b"x-ticket", b"svn:log")),
        command("log", (b"trunk",), (11,), (0,), "false", "false"),
        command("log", (b"trunk",), (11,), (0,), "false", "false", 3),
        command("log", (b"trunk",), (), (), "false", "false", 0, "false", "all-revprops", ()),
        command("log", (), (3,), (1,), "false", "false", 0, "false", "all-revprops", ()),
        command("log", (b"trunk",), (99,), (0,), "false", "false", 0, "false", "all-revprops", ()),
        command("log", (b"trunk/missing",), (11,), (0,), "false", "false", 0, "false", "all-revprops", ()),
        command("log", (b"trunk",), (11,), (0,), "false", "false", 4294967296, "false", "all-revprops", ()),
        command("log", (b"tags/t1/dir",), (11,), (0,), "true", "false", 0, "false", "revprops", (b"svn:log",)),
        command("log", (b"trunk/dir/b.txt",), (11,), (0,), "true", "false", 0, "false", "revprops", (b"svn:log",)),
    ],
    "log-merged": [
        command("log", (b"trunk",), (11,), (0,), "false", "false", 0, "true", "all-revprops", ()),
        command("log", (b"trunk",), (0,), (11,), "true", "false", 0, "true", "revprops", (b"svn:log",)),
        command("log", (b"tags/t1",), (11,), (0,), "false", "false", 0, "true", "revprops", (b"svn:log",)),
        command("log", (b"trunk/a.txt",), (11,), (0,), "true", "false", 0, "true", "revprops", (b"svn:log",)),
        command("log", (b"",), (11,), (0,), "false", "false", 0, "true", "revprops", (b"svn:log",)),
        command("log", (b"trunk/dir",), (11,), (0,), "false", "false", 2, "true", "revprops", (b"svn:log",)),
        command("log", (b"trunk", b"tags/t1"), (11,), (0,), "false", "false", 0, "true", "revprops", (b"svn:log",)),
    ],
    "hostile-log-word": [
        command("log", (b"trunk",), (11,), (0,), "false", "false", 0, "false", "bogus-word", ()),
        command("get-latest-rev"),
    ],
    "hostile-log-path": [
        command("log", (5,), (11,), (0,), "false", "false", 0, "false", "all-revprops", ()),
        command("get-latest-rev"),
    ],
    "hostile-log-seven": [
        command("log", (b"trunk",), (11,), (0,), "false", "false", 0, "false"),
        command("get-latest-rev"),
    ],
    "get-file-revs": [
        command("get-file-revs", b"trunk/a.txt", (0,), (11,)),
        command("get-file-revs", b"trunk/a.txt", (0,), (11,), "true"),
        command("get-file-revs", b"branches/b1/a.txt", (0,), (8,)),
        command("get-file-revs", b"branches/b1/a.txt", (0,), (11,), "true"),
        command("get-file-revs", b"trunk/big.txt", (0,), (11,)),
        command("get-file-revs", b"trunk/empty.txt", (), ()),
        command("get-file-revs", b"trunk/a.txt", (11,), (0,)),
        command("get-file-revs", b"trunk/a.txt", (11,), (0,), "true"),
        command("get-file-revs", b"trunk", (0,), (11,)),
        command("get-file-revs", b"trunk/missing", (0,), (11,)),
        command("get-file-revs", b"trunk/a.txt", (3,), (5,)),
        command("get-file-revs", b"trunk/a.txt", (0,), (99,)),
        command("get-file-revs", b"tags/t1/a.txt", (0,), (11,), "true"),
    ],
}

# Scripted sessions of a client that takes svndiff 0 alone: the stock greeting without `svndiff1` and
# `accepts-svndiff2`, so `svnserve` writes every delta uncompressed (`svn_ra_svn__svndiff_version`) and the tests hold
# its strings byte for byte, `send_simple_insertion_window`'s among them.
SCRIPTED_SVNDIFF0 = {
    "get-file-revs-svndiff0": [
        command("get-file-revs", b"trunk/a.txt", (0,), (11,)),
        command("get-file-revs", b"trunk/a.txt", (0,), (11,), "true"),
        command("get-file-revs", b"trunk/big.txt", (0,), (11,)),
        command("get-file-revs", b"trunk/empty.txt", (), ()),
        command("get-file-revs", b"trunk/a.txt", (11,), (0,)),
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
    # A-613 part 2: what the stock client asks of get-file, list, log, get-file-revs and the location segments.
    "svn-cat": [["cat", "-r", "5", URL + "/trunk/a.txt"]],
    "svn-cat-big": [["cat", URL + "/trunk/big.txt"]],
    "svn-ls": [["ls", "-v", "-R", URL + "/trunk"]],
    "svn-log": [["log", "-v", URL + "/trunk"]],
    "svn-log-g": [["log", "-g", URL + "/trunk"]],
    "svn-blame": [["blame", URL + "/trunk/a.txt"]],
    "svn-blame-g": [["blame", "-g", URL + "/trunk/a.txt"]],
    "svn-mergeinfo-eligible": [["mergeinfo", "--show-revs", "eligible", URL + "/branches/b1", URL + "/trunk"]],
    "svn-mergeinfo-merged": [["mergeinfo", "--show-revs", "merged", URL + "/branches/b1", URL + "/trunk"]],
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
    scripted = [(opening, name, commands) for name, commands in SCRIPTED.items()]
    plain = svndiff0_opening(opening)
    scripted += [(plain, name, commands) for name, commands in SCRIPTED_SVNDIFF0.items()]
    for first, name, commands in scripted:
        peer = socket.create_connection(("127.0.0.1", PORT), timeout=STEP_SECONDS)
        peer.sendall(first + b"".join(commands))
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


def svndiff0_opening(opening):
    """The stock client's opening with `svndiff1` and `accepts-svndiff2` taken from its greeting's capability words."""
    first = item_end(opening, 0)
    greeting = opening[:first]
    for word in (b" svndiff1 ", b" accepts-svndiff2 "):
        if greeting.count(word) != 1:
            sys.exit("the stock greeting does not announce %r once: %r" % (word, greeting))
        greeting = greeting.replace(word, b" ")
    return greeting + opening[first:]


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
