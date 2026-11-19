#!/usr/bin/env python3
"""Makes slates-svn's reporter fixtures (A-616; §4.17.4 "4. Reads", the reporter, and piece 7's third part).

Stock Subversion end to end: a repository built from history.txt with svnmucc over file://, one commit a revision, each
revision's svn:date set with svnadmin setrevprop; then stock `svn` checkouts, updates, switches, `status -u` and diffs
against stock `svnserve -d`, every byte of every connection each way logged by a relay on 127.0.0.1:3690 before it is
forwarded; and hand-made clients for the refusals of a report.

Usage: python3 -I make-fixtures.py OUTDIR
Writes, per stock session, NAME.K.client.bin and NAME.K.server.bin for the K-th connection its command opened; per
hand-made session NAME.1.client.bin and NAME.1.server.bin; changed.txt (`svnlook changed --copy-info` of every
revision) and tree.txt (`svnlook tree --full-paths --show-ids` of every revision), which hold the repository the tests
model to FSFS's own record; sessions.txt (each session's command, exit code and standard error), subversion-version.txt
and SHA256SUMS. Every file is the same on every run but those holding a lock token (svn lock draws one at random) and
the node offsets in tree.txt, which are FSFS's own.

Sources: Subversion's `subversion/libsvn_ra_svn/protocol` §3.1.1 and §3.1.3, `svnserve/serve.c` (`update`,
`switch_cmd`, `status`, `diff`, `accept_report` and the report commands), `libsvn_repos/reporter.c`,
`libsvn_ra_svn/editorp.c`.
"""

import hashlib
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time

# Bytes one read takes from a socket.
CHUNK = 65536
# svn://'s port (protocol §2), on which the relay listens so the client's URLs carry none.
RELAY_PORT = 3690
# How long any one step may take before the run is refused.
STEP_SECONDS = 120
# How long the relay waits on its listener between looks at whether its command has ended.
ACCEPT_POLL_SECONDS = 0.1
# The repository's fixed identity.
UUID = "00000000-0000-4000-8000-0000000a0616"
# The text of every line of a `lines:` file, and of an edited one.
LINE = "line %d of a text long enough to need several windows\n"
EDITED_LINE = "line %d, edited on the branch of history\n"
# svnserve.conf: anonymous users read; alice writes (a lock needs write); the authorization file hides /trunk/secret
# from all but alice.
SVNSERVE_CONF = "[general]\nanon-access = read\nauth-access = write\npassword-db = passwd\nauthz-db = authz\n"
PASSWD = "[users]\nalice = secret\n"
AUTHZ = "[/]\n* = r\nalice = rw\n[/trunk/secret]\n* =\nalice = rw\n"


def unescape(text):
    out = bytearray()
    i = 0
    while i < len(text):
        c = text[i]
        if c == "\\" and i + 1 < len(text):
            n = text[i + 1]
            if n == "n":
                out.append(10); i += 2; continue
            if n == "t":
                out.append(9); i += 2; continue
            if n == "\\":
                out.append(92); i += 2; continue
            if n == "x":
                out.append(int(text[i + 2:i + 4], 16)); i += 4; continue
        out.extend(c.encode()); i += 1
    return bytes(out)


def content(spec):
    """The bytes a `put` names."""
    if spec.startswith("bytes:"):
        return bytes(range(int(spec[len("bytes:"):])))
    if spec.startswith("lines:"):
        parts = spec.split(":")
        count = int(parts[1])
        edited = int(parts[3]) if len(parts) == 4 and parts[2] == "edit" else None
        return "".join((EDITED_LINE if i == edited else LINE) % i for i in range(1, count + 1)).encode()
    return unescape(spec)


def history(path):
    """history.txt as a list of (author, date, [actions]); each action a list of words, a put's text its last."""
    revisions = []
    for raw in open(path, encoding="utf-8"):
        line = raw.rstrip("\n")
        if not line.strip() or line.startswith("#"):
            continue
        words = line.split(" ")
        if words[0] == "revision":
            revisions.append((words[1], words[2], []))
        elif words[0] == "put":
            revisions[-1][2].append(["put", words[1], " ".join(words[2:])])
        else:
            revisions[-1][2].append(words)
    return revisions


def run(argv, **kwargs):
    return subprocess.run(argv, check=True, timeout=STEP_SECONDS, **kwargs)


def build(repository, revisions, work, env):
    url = "file://" + repository
    for number, (author, date, actions) in enumerate(revisions, start=1):
        argv = ["svnmucc", "-U", url, "-u", author, "-m", "r%d" % number, "--non-interactive"]
        for index, action in enumerate(actions):
            if action[0] == "put":
                source = os.path.join(work, "put-%d-%d" % (number, index))
                with open(source, "wb") as put:
                    put.write(content(action[2]))
                argv += ["put", source, action[1]]
            else:
                argv += action
        run(argv, env=env, capture_output=True)
        date_file = os.path.join(work, "date-%d" % number)
        with open(date_file, "w") as dated:
            dated.write(date)
        run(["svnadmin", "setrevprop", repository, "-r", str(number), "svn:date", date_file], env=env)


class Relay:
    """Relays every connection to svnserve while a command runs, recording each direction of the K-th connection in
    NAME.K.client.bin and NAME.K.server.bin before forwarding it."""

    def __init__(self, upstream_port, out):
        self.upstream_port = upstream_port
        self.out = out
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", RELAY_PORT))
        self.listener.listen(8)
        self.listener.settimeout(ACCEPT_POLL_SECONDS)

    def session(self, name, command):
        stop = threading.Event()
        pumps = []
        failures = []
        acceptor = threading.Thread(target=self.accept, args=(name, stop, pumps, failures), daemon=True)
        acceptor.start()
        try:
            return command()
        finally:
            stop.set()
            acceptor.join(STEP_SECONDS)
            for pump in pumps:
                pump.join(STEP_SECONDS)
                if pump.is_alive():
                    failures.append("a pump of %s did not end" % name)
            if failures:
                raise RuntimeError("relay for %s: %s" % (name, failures))

    def accept(self, name, stop, pumps, failures):
        count = 0
        while not stop.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError as error:
                failures.append(repr(error))
                return
            count += 1
            try:
                server = socket.create_connection(("127.0.0.1", self.upstream_port), timeout=STEP_SECONDS)
            except OSError as error:
                failures.append(repr(error))
                client.close()
                return
            client.settimeout(STEP_SECONDS)
            prefix = os.path.join(self.out, "%s.%d" % (name, count))
            client_record = open(prefix + ".client.bin", "wb", buffering=0)
            server_record = open(prefix + ".server.bin", "wb", buffering=0)
            for source, sink, record in ((client, server, client_record), (server, client, server_record)):
                pump = threading.Thread(target=pump_sockets, args=(source, sink, record), daemon=True)
                pump.start()
                pumps.append(pump)


def pump_sockets(source, sink, record):
    try:
        while True:
            data = source.recv(CHUNK)
            if not data:
                break
            record.write(data)
            sink.sendall(data)
    except OSError:
        pass
    try:
        sink.shutdown(socket.SHUT_WR)
    except OSError:
        pass
    record.close()


def free_port():
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    return port


def start_daemon(root, log_file, env):
    port = free_port()
    daemon = subprocess.Popen(
        ["svnserve", "-d", "--foreground", "--listen-host", "127.0.0.1", "--listen-port", str(port), "-r", root,
         "--log-file", log_file],
        env=env,
    )
    deadline = time.monotonic() + STEP_SECONDS
    while True:
        try:
            socket.create_connection(("127.0.0.1", port), timeout=STEP_SECONDS).close()
            return daemon, port
        except OSError:
            if daemon.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("svnserve did not listen on %d" % port)
            time.sleep(0.05)


def stop(process):
    process.terminate()
    try:
        process.wait(STEP_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(STEP_SECONDS)


def item_string(data):
    return b"%d:%s " % (len(data), data)


# A stock 1.15 client's answer to the greeting without SASL's client string, and its anonymous authentication.
def opening(url):
    return (b"( 2 ( edit-pipeline svndiff1 accepts-svndiff2 absent-entries depth mergeinfo log-revprops ) "
            + item_string(url) + b"10:SVN/1.15.0 ( ) ) ( ANONYMOUS ( 0: ) ) ")


SET_ROOT = b"( set-path ( 0: 6 false ( ) infinity ) ) "
UPDATE_HEAD = b"( update ( ( 6 ) 0: true infinity false false ) ) "
FINISH = b"( finish-report ( ) ) "
# The client's answer to close-edit or abort-edit, which svnserve reads before it answers the report.
EDIT_DONE = b"( success ( ) ) "


def main():
    out = os.path.abspath(sys.argv[1])
    os.makedirs(out, exist_ok=True)
    here = os.path.dirname(os.path.abspath(__file__))
    work = tempfile.mkdtemp(prefix="g234-a616-svnfix.", dir=os.path.expanduser("~/.cache"))
    daemon = None
    relay = None
    try:
        env = dict(os.environ, HOME=os.path.join(work, "home"), LC_ALL="C", LANG="C", TZ="UTC")
        os.makedirs(env["HOME"])
        config = os.path.join(work, "config")
        os.makedirs(config)
        with open(os.path.join(config, "config"), "w") as client_config:
            client_config.write("[auth]\npassword-stores =\n[miscellany]\ninteractive-conflicts = no\n")
        with open(os.path.join(config, "servers"), "w") as servers:
            servers.write("[global]\nstore-passwords = no\nstore-auth-creds = no\n")
        root = os.path.join(work, "root")
        repository = os.path.join(root, "repo")
        os.makedirs(root)
        run(["svnadmin", "create", repository], env=env)
        run(["svnadmin", "setuuid", repository, UUID], env=env)
        for name, text in (("svnserve.conf", SVNSERVE_CONF), ("passwd", PASSWD), ("authz", AUTHZ)):
            with open(os.path.join(repository, "conf", name), "w") as conf:
                conf.write(text)
        revisions = history(os.path.join(here, "history.txt"))
        build(repository, revisions, work, env)
        with open(os.path.join(out, "changed.txt"), "w") as changed, open(os.path.join(out, "tree.txt"), "w") as tree:
            for number in range(1, len(revisions) + 1):
                listing = run(["svnlook", "changed", "--copy-info", "-r", str(number), repository], env=env,
                              capture_output=True, text=True).stdout
                changed.write("r%d\n%s" % (number, listing))
                listing = run(["svnlook", "tree", "--full-paths", "--show-ids", "-r", str(number), repository],
                              env=env, capture_output=True, text=True).stdout
                tree.write("r%d\n%s" % (number, listing))
        versions = [subprocess.run([tool, "--version"], capture_output=True, text=True, env=env).stdout.splitlines()[:2]
                    for tool in ("svn", "svnserve")]
        with open(os.path.join(out, "subversion-version.txt"), "w") as version_file:
            for lines in versions:
                version_file.write("\n".join(line.rstrip() for line in lines) + "\n")
        log_file = os.path.join(work, "svnserve.log")
        daemon, port = start_daemon(root, log_file, env)
        relay = Relay(port, out)
        svn = ["svn", "--config-dir", config, "--non-interactive", "--no-auth-cache"]
        trunk = "svn://127.0.0.1/repo/trunk"
        branch = "svn://127.0.0.1/repo/branches/b1"
        wc = lambda name: os.path.join(work, name)
        sessions = []

        def stock(name, argv):
            result = relay.session(name, lambda: subprocess.run(
                svn + argv, capture_output=True, text=True, timeout=STEP_SECONDS, env=env, cwd=work))
            sessions.append((name, " ".join(argv).replace(work + "/", ""), result))
            return result

        # A checkout of an old revision, every node added, /trunk/secret absent to an anonymous reader; then an update
        # to the head through every kind of change history.txt makes.
        stock("checkout-r1", ["checkout", "-r1", trunk, wc("wc-a")])
        stock("update-to-head", ["update", wc("wc-a")])
        stock("switch-to-branch", ["switch", branch, wc("wc-a")])
        # Depth: an empty checkout made infinite, and an immediate one asked for files.
        stock("checkout-depth-empty", ["checkout", "--depth", "empty", trunk, wc("wc-b")])
        stock("update-set-depth-infinity", ["update", "--set-depth", "infinity", wc("wc-b")])
        stock("checkout-depth-immediates", ["checkout", "--depth", "immediates", trunk, wc("wc-h")])
        stock("update-depth-files", ["update", "--depth", "files", wc("wc-h")])
        # Mixed revisions: one file taken back to r2 (an update whose target is that file), then the whole updated.
        stock("checkout-head", ["checkout", trunk, wc("wc-c")])
        stock("update-file-to-r2", ["update", "-r2", wc("wc-c/a.txt")])
        stock("update-mixed-revisions", ["update", wc("wc-c")])
        # status -u: a working copy at r2 with a local edit; no text deltas.
        stock("checkout-r2", ["checkout", "-r2", trunk, wc("wc-d")])
        with open(wc("wc-d/a.txt"), "ab") as edited:
            edited.write(b"a local edit\n")
        stock("status-u", ["status", "-u", wc("wc-d")])
        # A diff of two revisions of one URL.
        stock("diff-r1-r6", ["diff", "-r", "1:6", trunk])
        # A switched subtree, reported with link-path.
        stock("checkout-for-switch", ["checkout", trunk, wc("wc-e")])
        stock("switch-subtree", ["switch", branch + "/dir", wc("wc-e/dir")])
        stock("update-with-link", ["update", wc("wc-e")])
        # A subtree taken to a revision where it does not exist, then the whole updated.
        stock("checkout-for-not-present", ["checkout", trunk, wc("wc-f")])
        stock("update-subtree-to-r1", ["update", "-r1", wc("wc-f/dir/sub")])
        stock("update-with-not-present", ["update", wc("wc-f")])
        # A lock the working copy holds, reported with its token; then the lock broken and the token removed.
        stock("checkout-for-lock", ["checkout", trunk, wc("wc-g")])
        stock("lock-a", ["lock", "--username", "alice", "--password", "secret", "-m", "held", wc("wc-g/a.txt")])
        stock("update-with-lock", ["update", wc("wc-g")])
        run(["svnadmin", "rmlocks", repository, "/trunk/a.txt"], env=env, capture_output=True)
        stock("update-with-broken-lock", ["update", wc("wc-g")])
        # Hand-made reports: each sends its whole session at once, then half-closes.
        hostile = [
            ("hostile-report-unknown-command",
             UPDATE_HEAD + SET_ROOT + b"( bogus ( ) ) " + FINISH + EDIT_DONE),
            ("hostile-report-first-not-root",
             UPDATE_HEAD + b"( set-path ( 3:dir 6 false ( ) infinity ) ) " + FINISH),
            ("hostile-report-missing-path",
             UPDATE_HEAD + SET_ROOT + b"( set-path ( 7:missing 6 false ( ) infinity ) ) " + FINISH + EDIT_DONE),
            ("hostile-report-bad-depth",
             UPDATE_HEAD + b"( set-path ( 0: 6 false ( ) sideways ) ) " + FINISH),
            ("hostile-report-link-elsewhere",
             UPDATE_HEAD + SET_ROOT
             + b"( link-path ( 3:dir " + item_string(b"svn://example.org/other/dir") + b"6 false ( ) infinity ) ) "
             + FINISH),
            ("hostile-report-abort", UPDATE_HEAD + SET_ROOT + b"( abort-report ( ) ) "),
            ("hostile-update-depth-exclude",
             b"( update ( ( 6 ) 0: true exclude false false ) ) " + SET_ROOT + FINISH),
            ("hostile-report-delete-root", UPDATE_HEAD + SET_ROOT + b"( delete-path ( 0: ) ) " + FINISH),
            ("hostile-report-revision-past-head",
             UPDATE_HEAD + b"( set-path ( 0: 99 false ( ) infinity ) ) " + FINISH + EDIT_DONE),
            ("hostile-update-target-absent",
             b"( update ( ( 6 ) 7:missing true infinity false false ) ) "
             + b"( set-path ( 0: 6 false ( ) infinity ) ) " + FINISH + EDIT_DONE),
            ("hostile-report-malformed-set-path", UPDATE_HEAD + b"( set-path ( 0: six false ) ) " + FINISH),
        ]
        for name, commands in hostile:
            client_bytes = opening(b"svn://127.0.0.1/repo/trunk") + commands
            prefix = os.path.join(out, name + ".1")
            with socket.create_connection(("127.0.0.1", port), timeout=STEP_SECONDS) as connection:
                with open(prefix + ".client.bin", "wb") as client_record:
                    client_record.write(client_bytes)
                connection.sendall(client_bytes)
                connection.shutdown(socket.SHUT_WR)
                received = bytearray()
                while True:
                    data = connection.recv(CHUNK)
                    if not data:
                        break
                    received.extend(data)
            with open(prefix + ".server.bin", "wb") as server_record:
                server_record.write(received)
            sessions.append((name, "(written by this script)", None))
        with open(os.path.join(out, "sessions.txt"), "w") as summary:
            for name, command, result in sessions:
                summary.write("%s: %s\n" % (name, command))
                if result is not None:
                    summary.write("  exit %d\n" % result.returncode)
                    for line in result.stderr.splitlines():
                        summary.write("  stderr: %s\n" % line.replace(work + "/", ""))
        names = sorted(f for f in os.listdir(out) if f != "SHA256SUMS")
        with open(os.path.join(out, "SHA256SUMS"), "w") as sums:
            for file_name in names:
                with open(os.path.join(out, file_name), "rb") as fixture:
                    sums.write("%s  %s\n" % (hashlib.sha256(fixture.read()).hexdigest(), file_name))
        print("svnserve log:")
        print(open(log_file).read())
    finally:
        if relay is not None:
            relay.listener.close()
        if daemon is not None:
            stop(daemon)
        shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
