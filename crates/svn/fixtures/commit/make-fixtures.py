#!/usr/bin/env python3
"""Makes slates-svn's commit fixtures (A-617; §4.17.4 item 3 and piece 7's commit editor drive).

Stock `svn` 1.15 commits against stock `svnserve`, every byte of every ra_svn session logged in each direction:
- Every session is an `svn+fixture://` URL, whose tunnel command (the client configuration's `[tunnels]`) is this
  script in its tunnel role, running `svnserve -t --tunnel-user=USER -r ROOT` as sshd would run `svnserve -t` for a
  logged-in user, and logging the client's bytes and the server's before forwarding them. So each session's principal
  is fixed (EXTERNAL; no random CRAM-MD5 challenge), and the client's bytes are the same on every run.
- Each `svn` command names its session (SVN_FIXTURE_SESSION) and its user (SVN_FIXTURE_USER); a command that opens
  more than one ra session gets NAME.1, NAME.2 and so on, in the order they open.
- One commit is raced (SVN_FIXTURE_RACE): its tunnel forwards the client's bytes up to `close-edit`, waits for the
  server to have begun the transaction, runs another user's commit of the same file, and only then forwards
  `close-edit`, so the first commit meets FSFS's merge conflict at the verdict (§4.17.4 item 3.3), as a busy server
  would make it.

Usage: python3 -I make-fixtures.py OUTDIR
Writes NAME.client.bin and NAME.server.bin per session, sessions.txt (each session's command, user, exit code and the
client's standard error), subversion-version.txt and SHA256SUMS. The client's bytes are the same on every run; the
server's carry each revision's `svn:date`.

Sources: Subversion's `subversion/libsvn_ra_svn/protocol` §3.1.1 (`commit`) and §3.1.2 (the editor commands),
`svnserve/serve.c` (`commit`), `libsvn_ra_svn/editorp.c` (`svn_ra_svn_drive_editor2`), `libsvn_ra_svn/client.c`
(`find_tunnel_agent`, `open_session`).
"""

import hashlib
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time

# Bytes one read takes from a pipe.
CHUNK = 65536
# How long any one step may take before the run is refused.
STEP_SECONDS = 60
# How long the raced session waits, after forwarding the drive up to `close-edit`, for the server to have read it.
RACE_SETTLE_SECONDS = 1.0
# The fixed identity of the repository (`svnadmin setuuid`), so the transcripts name no random UUID.
UUID = "00000000-0000-4000-8000-00000000a617"
# The repository's name under the server's root, and its URL as every session names it.
REPOSITORY = "repo"
URL = "svn+fixture://fixture/" + REPOSITORY
# The users: alice commits; bob takes a lock, commits against it and races alice.
ALICE = "alice"
BOB = "bob"
# The marker the raced tunnel waits for in the client's bytes (protocol §3.1.2).
CLOSE_EDIT = b"( close-edit ( ) ) "
# One line of the large file, and how many make it: past one svndiff window (102,400 bytes, `notes/svndiff`), and
# repetitive, so its LZ4-compressed windows stay small.
LARGE_LINE = b"a line of the large file, which passes one svndiff window\n"
LARGE_LINES = 4096
# A small binary file: bytes that are no text, so the client sets `svn:mime-type` on it.
BINARY = bytes(range(256)) * 4


def log(message):
    print(message, flush=True)


def write_all_fd(fd, data):
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        view = view[written:]


def record_path(directory, session, side, index):
    return os.path.join(directory, "%s.%d.%s.bin" % (session, index, side))


def claim_prefix(directory, session):
    """The first index whose client file this session can create, so concurrent sessions never share a file."""
    index = 1
    while True:
        path = record_path(directory, session, "client", index)
        try:
            fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o644)
            return fd, index
        except FileExistsError:
            index += 1


def pump_up(server_stdin, client_record, race):
    """The client's bytes to the server, each chunk recorded first; a raced session holds `close-edit` until its race ran."""
    seen = b""
    raced = race is None
    while True:
        data = os.read(0, CHUNK)
        if not data:
            server_stdin.close()
            return
        os.write(client_record, data)
        if not raced:
            seen += data
            at = seen.find(CLOSE_EDIT)
            if at >= 0:
                # The marker may have begun in an earlier read, already forwarded: hold all of this one then.
                before = max(0, len(data) - (len(seen) - at))
                write_all_fd(server_stdin.fileno(), data[:before])
                server_stdin.flush()
                time.sleep(RACE_SETTLE_SECONDS)
                subprocess.run(race, shell=True, check=True, timeout=STEP_SECONDS)
                raced = True
                write_all_fd(server_stdin.fileno(), data[before:])
                continue
        write_all_fd(server_stdin.fileno(), data)


def tunnel(argv):
    """The tunnel role: `[tunnels]` runs `make-fixtures.py --tunnel ROOT HOST svnserve -t`."""
    root = argv[0]
    directory = os.environ["SVN_FIXTURE_DIR"]
    session = os.environ["SVN_FIXTURE_SESSION"]
    user = os.environ["SVN_FIXTURE_USER"]
    race = os.environ.get("SVN_FIXTURE_RACE")
    client_record, index = claim_prefix(directory, session)
    server = subprocess.Popen(
        ["svnserve", "-t", "--tunnel-user=" + user, "-r", root],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    )
    with open(record_path(directory, session, "server", index), "wb", buffering=0) as server_record:
        upward = threading.Thread(target=pump_up, args=(server.stdin, client_record, race), daemon=True)
        upward.start()
        while True:
            data = os.read(server.stdout.fileno(), CHUNK)
            if not data:
                break
            server_record.write(data)
            write_all_fd(1, data)
        os.close(1)
        upward.join(STEP_SECONDS)
    os.close(client_record)
    return server.wait(STEP_SECONDS)


class Fixture:
    """The repository, the client's configuration and the commands' log."""

    def __init__(self, out, work):
        self.out = out
        self.work = work
        self.root = os.path.join(work, "root")
        self.home = os.path.join(work, "home")
        self.config = os.path.join(work, "config")
        for directory in (self.root, self.home, self.config):
            os.makedirs(directory)
        self.env = dict(os.environ, HOME=self.home, LC_ALL="C", LANG="C", TZ="UTC", SVN_FIXTURE_DIR=out)
        self.env.pop("SVN_FIXTURE_RACE", None)
        self.sessions = []

    def setup(self):
        repository = os.path.join(self.root, REPOSITORY)
        self.tool(["svnadmin", "create", repository])
        self.tool(["svnadmin", "setuuid", repository, UUID])
        with open(os.path.join(repository, "conf", "svnserve.conf"), "w") as conf:
            conf.write("[general]\nanon-access = none\nauth-access = write\n")
        script = os.path.abspath(__file__)
        with open(os.path.join(self.config, "config"), "w") as config:
            config.write(
                '[auth]\npassword-stores =\n[tunnels]\nfixture = "%s" -I "%s" --tunnel "%s"\n'
                "[miscellany]\nenable-auto-props = yes\n[auto-props]\n*.txt = svn:eol-style=native\n"
                % (sys.executable, script, self.root)
            )
        with open(os.path.join(self.config, "servers"), "w") as servers:
            servers.write("[global]\nstore-passwords = no\nstore-auth-creds = no\n")

    def tool(self, argv):
        subprocess.run(argv, check=True, timeout=STEP_SECONDS, env=self.env, capture_output=True)

    def svn(self, session, user, args, expect_failure=False, race=None):
        """Runs `svn ARGS` as USER, its ra sessions recorded under SESSION."""
        env = dict(self.env, SVN_FIXTURE_SESSION=session, SVN_FIXTURE_USER=user)
        if race is not None:
            env["SVN_FIXTURE_RACE"] = race
        argv = ["svn", "--config-dir", self.config, "--non-interactive"] + args
        done = subprocess.run(argv, env=env, capture_output=True, timeout=STEP_SECONDS)
        self.sessions.append((session, user, args, done.returncode, done.stderr.decode("utf-8", "replace").strip()))
        failed = done.returncode != 0
        if failed != expect_failure:
            raise RuntimeError(
                "%s: svn %s exited %d, expected %s: %s"
                % (session, " ".join(args), done.returncode, "failure" if expect_failure else "success",
                   done.stderr.decode("utf-8", "replace"))
            )
        log("%s: exit %d" % (session, done.returncode))

    def race_command(self, user, args):
        """A shell command the raced tunnel runs: `svn ARGS` as USER, recorded as its own session."""
        quoted = " ".join("'" + arg.replace("'", "'\\''") + "'" for arg in args)
        return ("SVN_FIXTURE_SESSION=race-commit SVN_FIXTURE_USER=%s svn --config-dir '%s' --non-interactive %s"
                % (user, self.config, quoted))


def write_file(path, data):
    with open(path, "wb") as handle:
        handle.write(data)


def make_import_tree(directory):
    os.makedirs(os.path.join(directory, "sub", "deeper"))
    os.makedirs(os.path.join(directory, "gone"))
    write_file(os.path.join(directory, "a.txt"), b"alpha\nbeta\ngamma\n")
    write_file(os.path.join(directory, "c.txt"), b"the file that is replaced\n")
    write_file(os.path.join(directory, "empty.txt"), b"")
    write_file(os.path.join(directory, "binary.bin"), BINARY)
    write_file(os.path.join(directory, "large.txt"), LARGE_LINE * LARGE_LINES)
    write_file(os.path.join(directory, "sub", "b.txt"), b"in the subdirectory\n")
    write_file(os.path.join(directory, "sub", "deeper", "d.txt"), b"two levels down\n")
    write_file(os.path.join(directory, "gone", "g.txt"), b"in a directory that is deleted\n")


def record(fixture):
    trunk = URL + "/trunk"
    project = trunk + "/project"
    work = fixture.work
    wc_alice = os.path.join(work, "wc-alice")
    wc_stale = os.path.join(work, "wc-stale")
    wc_bob = os.path.join(work, "wc-bob")
    source = os.path.join(work, "import")
    make_import_tree(source)

    fixture.svn("mkdir", ALICE, ["mkdir", "-m", "the trunk", trunk])
    fixture.svn("import", ALICE, ["import", "-m", "the project", source, project])
    fixture.svn("checkout-alice", ALICE, ["checkout", project, wc_alice])
    fixture.svn("checkout-stale", ALICE, ["checkout", project, wc_stale])

    write_file(os.path.join(wc_alice, "a.txt"), b"alpha\nBETA\ngamma\ndelta\n")
    fixture.svn("edit", ALICE, ["commit", "-m", "an edit", wc_alice])

    large = LARGE_LINE * (LARGE_LINES // 2) + b"a changed line in the middle\n" + LARGE_LINE * (LARGE_LINES // 2)
    write_file(os.path.join(wc_alice, "large.txt"), large + b"an appended line\n")
    fixture.svn("edit-large", ALICE, ["commit", "-m", "an edit of the large file", wc_alice])

    fixture.svn("propset-dir", ALICE, ["propset", "colour", "blue", os.path.join(wc_alice, "sub")])
    fixture.svn("propset-file", ALICE, ["propset", "flavour", "sweet", os.path.join(wc_alice, "a.txt")])
    fixture.svn("propset", ALICE, ["commit", "-m", "properties", wc_alice])
    fixture.svn("propdel-file", ALICE, ["propdel", "flavour", os.path.join(wc_alice, "a.txt")])
    fixture.svn("propdel", ALICE, ["commit", "-m", "a property deleted", wc_alice])
    fixture.svn("propset-root", ALICE, ["propset", "rootprop", "on the session root", wc_alice])
    fixture.svn("root-prop", ALICE, ["commit", "-m", "a property of the root", wc_alice])

    write_file(os.path.join(wc_alice, "new.txt"), b"a new file\n")
    os.makedirs(os.path.join(wc_alice, "newdir"))
    write_file(os.path.join(wc_alice, "newdir", "inside.txt"), b"inside a new directory\n")
    fixture.svn("add-new", ALICE, ["add", os.path.join(wc_alice, "new.txt"), os.path.join(wc_alice, "newdir")])
    fixture.svn("rm-old", ALICE, ["rm", os.path.join(wc_alice, "sub", "b.txt"), os.path.join(wc_alice, "gone")])
    fixture.svn("add-delete", ALICE, ["commit", "-m", "additions and deletions", wc_alice])

    fixture.svn("rm-replaced", ALICE, ["rm", os.path.join(wc_alice, "c.txt")])
    write_file(os.path.join(wc_alice, "c.txt"), b"the replacement\n")
    fixture.svn("add-replacement", ALICE, ["add", os.path.join(wc_alice, "c.txt")])
    fixture.svn("replace", ALICE, ["commit", "-m", "a replacement", wc_alice])

    fixture.svn("copy-url", ALICE, ["copy", "-m", "a branch", "--parents", trunk, URL + "/branches/b1"])
    fixture.svn("copy-wc", ALICE, ["copy", os.path.join(wc_alice, "a.txt"), os.path.join(wc_alice, "a-copy.txt")])
    write_file(os.path.join(wc_alice, "a-copy.txt"), b"alpha\nBETA\ngamma\ndelta\nand a line of the copy\n")
    fixture.svn("copy-modify", ALICE, ["commit", "-m", "a copy changed as it is made", wc_alice])

    write_file(os.path.join(wc_stale, "a.txt"), b"alpha\nbeta\ngamma\nfrom a stale working copy\n")
    fixture.svn("out-of-date", ALICE, ["commit", "-m", "from a stale working copy", wc_stale], expect_failure=True)

    fixture.svn("checkout-bob", BOB, ["checkout", project, wc_bob])
    fixture.svn("lock-bob", BOB, ["lock", "-m", "bob's", os.path.join(wc_bob, "a.txt")])
    fixture.svn("update-alice", ALICE, ["update", wc_alice])
    write_file(os.path.join(wc_alice, "a.txt"), b"alpha\nBETA\ngamma\ndelta\nalice, against bob's lock\n")
    fixture.svn("locked-by-other", ALICE, ["commit", "-m", "against a lock", wc_alice], expect_failure=True)
    fixture.svn("revert-alice", ALICE, ["revert", os.path.join(wc_alice, "a.txt")])
    write_file(os.path.join(wc_bob, "a.txt"), b"alpha\nBETA\ngamma\ndelta\nbob, holding the lock\n")
    fixture.svn("commit-with-lock", BOB, ["commit", "-m", "with the lock's token", wc_bob])

    fixture.svn("update-alice-again", ALICE, ["update", wc_alice])
    fixture.svn("update-bob", BOB, ["update", wc_bob])
    write_file(os.path.join(wc_bob, "sub", "deeper", "d.txt"), b"two levels down, as bob changed it\n")
    write_file(os.path.join(wc_alice, "sub", "deeper", "d.txt"), b"two levels down, as alice changed it\n")
    race = fixture.race_command(BOB, ["commit", "-m", "the race's other commit", wc_bob])
    fixture.svn("conflict", ALICE, ["commit", "-m", "meets the race", wc_alice], expect_failure=True, race=race)


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "--tunnel":
        return tunnel(sys.argv[2:])
    out = os.path.abspath(sys.argv[1])
    os.makedirs(out, exist_ok=True)
    for name in os.listdir(out):
        if name.endswith(".bin") or name in ("sessions.txt", "subversion-version.txt", "SHA256SUMS"):
            os.remove(os.path.join(out, name))
    work = tempfile.mkdtemp(prefix="g268-a617-svnfix.", dir=os.path.expanduser("~/.cache"))
    try:
        fixture = Fixture(out, work)
        fixture.setup()
        record(fixture)
        versions = [
            subprocess.run([tool, "--version"], capture_output=True, text=True, timeout=STEP_SECONDS, env=fixture.env)
            .stdout.splitlines()[0]
            for tool in ("svn", "svnserve", "svnadmin")
        ]
        with open(os.path.join(out, "subversion-version.txt"), "w") as version:
            version.write("\n".join(versions) + "\n")
        with open(os.path.join(out, "sessions.txt"), "w") as sessions:
            for session, user, args, code, stderr in fixture.sessions:
                command = " ".join(arg.replace(work, "WORK") for arg in args)
                sessions.write("%s\tuser %s\texit %d\tsvn %s\n" % (session, user, code, command))
                if stderr:
                    sessions.write("\t%s\n" % stderr.replace(work, "WORK").replace("\n", "\n\t"))
        names = sorted(name for name in os.listdir(out) if name != "SHA256SUMS" and not name.endswith(".py"))
        with open(os.path.join(out, "SHA256SUMS"), "w") as sums:
            for name in names:
                with open(os.path.join(out, name), "rb") as handle:
                    sums.write("%s  %s\n" % (hashlib.sha256(handle.read()).hexdigest(), name))
    finally:
        shutil.rmtree(work)
    return 0


if __name__ == "__main__":
    sys.exit(main())
