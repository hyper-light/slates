#!/usr/bin/env python3
"""Makes the Subversion hook fixtures (A-623; §4.17.4 "9. Hooks"): what stock `svnserve` 1.15 hands each of its nine
hooks, and what it tells its client when a hook refuses or fails.

Stock `svn` and `svnmucc` against stock `svnserve --foreground` started as a daemon starts its hooks: its directory
`/` and its standard input the null device (`apr_proc_detach`, which `-d` without `--foreground` calls, does both).
Every hook is one generated `/bin/sh` script that records its run and then does what the current case asks
(`control/<hook>`, sourced). A relay between the client and `svnserve` logs each direction of each connection byte for
byte, before it forwards it.

Usage: python3 -I make-fixtures.py OUTDIR
Writes:
- `runs/<case>.<n>-<hook>.args` (the arguments after the program, each NUL-ended), `.stdin`, `.env` (`env -0`) and
  `.cwd`: the `n`th hook run of the case, in order. `runs/control.1-pre-commit.*` is the script run by hand under
  `env -i`: what `/bin/sh` adds to an environment by itself.
- `told/<case>.client.bin` and `told/<case>.server.bin`: the case's connections, each direction, in connection order.
- `cases.txt`: each case's commands, with each one's exit code and standard error.
- `repository-path.txt`: the repository's directory as `svnserve` names it to its hooks.
- `subversion-version.txt`, and `SHA256SUMS` over every other file.
The tokens svn makes, the dates and the transactions' names differ from run to run; nothing else does but the
repository's directory, which `repository-path.txt` names.

Sources: Subversion 1.15.0 (e49d66ef): `libsvn_repos/hooks.c` (`run_hook_cmd`, `check_hook_result`, each hook's
arguments and input, `parse_hooks_env_option`), `libsvn_repos/fs-wrap.c` (the post-lock and post-unlock wrapping),
`libsvn_repos/commit.c` (`svn_repos__post_commit_error_str`), `svnserve/serve.c` (`commit`, `lock_many`,
`unlock_many`, `change_rev_prop`), `libsvn_subr/io.c` (`svn_io_start_cmd3`).
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
# How long any one step may take before the run is refused: a client's command, a relay's drain, a server's start.
STEP_SECONDS = 60
# The repository's fixed identity (`svnadmin setuuid`), so the transcripts name no random UUID.
UUID = "00000000-0000-4000-8000-00000000a623"
# The users and their password (`password-db`), the Subversion book's.
PASSWORD = "rayjandom"
USERS = ("jrandom", "jconstant")
# The nine hooks `svnserve` runs (`repos.h` `SVN_REPOS__HOOK_*`).
HOOKS = (
    "start-commit", "pre-commit", "post-commit", "pre-revprop-change", "post-revprop-change",
    "pre-lock", "post-lock", "pre-unlock", "post-unlock",
)
# The hooks-env file: a default section split in two (Subversion's configuration merges a repeated section), a hook's
# own section (which replaces the default one), and an empty one (which has no table, so its hook takes the default).
HOOKS_ENV = (
    "[default]\n"
    "LANG = en_US.UTF-8\n"
    "[pre-commit]\n"
    "SLATES_FIXTURE = pre-commit section\n"
    "[post-commit]\n"
    "[default]\n"
    "SLATES_FIXTURE = default section\n"
)
# The hook script. It writes its record, then sources the case's control file when there is one. Every program by
# its path: a hook's environment holds what hooks-env gives it alone, never a PATH.
HOOK_SCRIPT = """#!/bin/sh
name=%(name)s
case_name=$(/bin/cat %(case_file)s)
n=$(/bin/cat %(counter)s)
n=$((n + 1))
printf '%%s' "$n" > %(counter)s
stem="%(runs)s/$case_name.$n-$name"
for argument in "$@"; do printf '%%s\\0' "$argument"; done > "$stem.args"
/bin/cat > "$stem.stdin"
/usr/bin/env -0 > "$stem.env"
printf '%%s' "$(/bin/pwd)" > "$stem.cwd"
if [ -f %(control)s/$name ]; then . %(control)s/$name; fi
exit 0
"""


def log(message):
    print(message, flush=True)


def run(argv, **kwargs):
    """Runs a stock tool to its end, refusing a failure."""
    return subprocess.run(argv, check=True, timeout=STEP_SECONDS, **kwargs)


def pump_sockets(source, sink, record):
    """Copies one socket's bytes to another until the first ends, recording every chunk before forwarding it."""
    try:
        while True:
            data = source.recv(CHUNK)
            if not data:
                break
            record.extend(data)
            sink.sendall(data)
    except OSError:
        pass
    try:
        sink.shutdown(socket.SHUT_WR)
    except OSError:
        pass


class Relay:
    """Relays every client connection to `svnserve`, recording both directions of each under the case current when
    it was accepted."""

    def __init__(self, upstream_port):
        self.upstream_port = upstream_port
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.port = self.listener.getsockname()[1]
        self.case = "setup"
        self.connections = {}
        self.guard = threading.Lock()
        self.failures = []
        self.acceptor = threading.Thread(target=self.accept_all, daemon=True)
        self.acceptor.start()

    def accept_all(self):
        while True:
            try:
                client, _ = self.listener.accept()
            except OSError:
                return
            try:
                server = socket.create_connection(("127.0.0.1", self.upstream_port), timeout=STEP_SECONDS)
            except OSError as error:
                self.failures.append("%s: %r" % (self.case, error))
                client.close()
                continue
            client.settimeout(STEP_SECONDS)
            upward_bytes, downward_bytes = bytearray(), bytearray()
            upward = threading.Thread(target=pump_sockets, args=(client, server, upward_bytes), daemon=True)
            downward = threading.Thread(target=pump_sockets, args=(server, client, downward_bytes), daemon=True)
            with self.guard:
                self.connections.setdefault(self.case, []).append(
                    (upward, downward, upward_bytes, downward_bytes, client, server)
                )
            upward.start()
            downward.start()

    def drain(self, case):
        """Waits for the case's connections to end; its client and server bytes, each connection's in order."""
        with self.guard:
            connections = list(self.connections.get(case, []))
        client_bytes, server_bytes = bytearray(), bytearray()
        for upward, downward, upward_bytes, downward_bytes, client, server in connections:
            upward.join(STEP_SECONDS)
            downward.join(STEP_SECONDS)
            if upward.is_alive() or downward.is_alive():
                raise RuntimeError("relay for %s did not drain within %d s" % (case, STEP_SECONDS))
            client.close()
            server.close()
            client_bytes.extend(upward_bytes)
            server_bytes.extend(downward_bytes)
        if self.failures:
            raise RuntimeError("relay failures: %s" % self.failures)
        return bytes(client_bytes), bytes(server_bytes)

    def close(self):
        self.listener.close()


def free_port():
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    return port


def start_server(root, log_file, env):
    """`svnserve --foreground` in `/` with the null device as its input, as a daemon starts its hooks."""
    port = free_port()
    server = subprocess.Popen(
        [
            "svnserve", "-d", "--foreground", "--listen-host", "127.0.0.1", "--listen-port", str(port),
            "-r", root, "--log-file", log_file,
        ],
        env=env,
        cwd="/",
        stdin=subprocess.DEVNULL,
    )
    deadline = time.monotonic() + STEP_SECONDS
    while True:
        try:
            probe = socket.create_connection(("127.0.0.1", port), timeout=STEP_SECONDS)
            probe.close()
            return server, port
        except OSError:
            if server.poll() is not None or time.monotonic() > deadline:
                raise RuntimeError("svnserve did not listen on %d" % port)
            time.sleep(0.05)


def stop(process):
    process.terminate()
    try:
        process.wait(STEP_SECONDS)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait(STEP_SECONDS)


class Recorder:
    """The repository, its hooks, the clients and the relay, one case at a time."""

    def __init__(self, out, work):
        self.out = out
        self.work = work
        self.runs = os.path.join(out, "runs")
        self.told = os.path.join(out, "told")
        os.makedirs(self.runs)
        os.makedirs(self.told)
        self.root = os.path.join(work, "root")
        self.repository = os.path.join(self.root, "r")
        self.hooks = os.path.join(self.repository, "hooks")
        self.control = os.path.join(work, "control")
        self.case_file = os.path.join(work, "case")
        self.counter = os.path.join(work, "counter")
        self.sources = os.path.join(work, "sources")
        self.wc = os.path.join(work, "wc")
        self.config = os.path.join(work, "config")
        home = os.path.join(work, "home")
        for directory in (self.root, self.control, self.sources, self.config, home):
            os.makedirs(directory)
        base = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": home, "TZ": "UTC"}
        self.env = dict(base, LANG="en_US.UTF-8")
        self.cases = []
        self.relay = None
        self.server = None

    def write(self, path, text, mode=None):
        with open(path, "w", encoding="utf-8") as handle:
            handle.write(text)
        if mode is not None:
            os.chmod(path, mode)

    def create(self):
        run(["svnadmin", "create", self.repository], env=self.env)
        run(["svnadmin", "setuuid", self.repository, UUID], env=self.env)
        conf = os.path.join(self.repository, "conf")
        self.write(
            os.path.join(conf, "svnserve.conf"),
            "[general]\nanon-access = none\nauth-access = write\npassword-db = passwd\n",
        )
        self.write(os.path.join(conf, "passwd"), "[users]\n" + "".join("%s = %s\n" % (u, PASSWORD) for u in USERS))
        self.write(os.path.join(conf, "hooks-env"), HOOKS_ENV)
        for name in HOOKS:
            self.install_hook(name)
        self.write(os.path.join(self.config, "config"), "[auth]\npassword-stores =\n")
        self.write(os.path.join(self.config, "servers"), "[global]\nstore-passwords = no\nstore-auth-creds = no\n")
        self.write(self.case_file, "setup")
        self.write(self.counter, "0")

    def install_hook(self, name):
        path = os.path.join(self.hooks, name)
        if os.path.lexists(path):
            os.remove(path)
        script = HOOK_SCRIPT % {
            "name": name,
            "case_file": self.case_file,
            "counter": self.counter,
            "runs": self.runs,
            "control": self.control,
        }
        self.write(path, script, 0o755)

    def start(self):
        self.server, port = start_server(self.root, os.path.join(self.work, "svnserve.log"), self.env)
        self.relay = Relay(port)
        self.url = "svn://127.0.0.1:%d/r" % self.relay.port

    def client(self, tool, user, arguments):
        common = ["--config-dir", self.config, "--non-interactive", "--no-auth-cache"]
        if tool == "svnmucc":
            return ["svnmucc"] + common + ["-u", user, "-p", PASSWORD] + arguments
        return ["svn"] + common + ["--username", user, "--password", PASSWORD] + arguments

    def source(self, name, text):
        path = os.path.join(self.sources, name)
        self.write(path, text)
        return path

    def case(self, name, commands, controls=None, prepare=None, restore=None, told=False):
        """Runs one case: its control files and hook changes in place, each command in turn, its connections drained
        and written, everything put back. A case of the inputs refuses the run when a command fails; a case of what
        the client is told keeps whatever each command did, for `cases.txt` and the tests to judge."""
        log("case %s" % name)
        for hook, snippet in (controls or {}).items():
            self.write(os.path.join(self.control, hook), snippet)
        if prepare:
            prepare()
        self.write(self.case_file, name)
        self.write(self.counter, "0")
        self.relay.case = name
        results = []
        try:
            for tool, user, arguments in commands:
                argv = self.client(tool, user, arguments)
                result = subprocess.run(argv, capture_output=True, timeout=STEP_SECONDS, env=self.env, cwd=self.work)
                results.append((argv, result))
                if result.returncode != 0 and not told:
                    raise RuntimeError(
                        "case %s: %s exited %d: %s"
                        % (name, " ".join(argv), result.returncode, result.stderr.decode("utf-8", "replace"))
                    )
        finally:
            for hook in (controls or {}):
                os.remove(os.path.join(self.control, hook))
            if restore:
                restore()
        client_bytes, server_bytes = self.relay.drain(name)
        with open(os.path.join(self.told, name + ".client.bin"), "wb") as handle:
            handle.write(client_bytes)
        with open(os.path.join(self.told, name + ".server.bin"), "wb") as handle:
            handle.write(server_bytes)
        self.cases.append((name, results))

    def control_run(self):
        """The pre-commit script run by hand under `env -i`: what `/bin/sh` adds to an environment by itself."""
        self.write(self.case_file, "control")
        self.write(self.counter, "0")
        run(
            ["/usr/bin/env", "-i", os.path.join(self.hooks, "pre-commit"), "repository", "transaction"],
            cwd="/",
            stdin=subprocess.DEVNULL,
        )

    def write_cases(self):
        with open(os.path.join(self.out, "cases.txt"), "w", encoding="utf-8") as handle:
            for name, results in self.cases:
                handle.write("== %s\n" % name)
                for argv, result in results:
                    shown = [a.replace(self.work, "WORK") for a in argv if a != PASSWORD]
                    handle.write("$ %s\n" % " ".join(shown))
                    handle.write("exit %d\n" % result.returncode)
                    handle.write(result.stderr.decode("utf-8", "replace").replace(self.work, "WORK"))

    def finish(self):
        self.write(os.path.join(self.out, "repository-path.txt"), self.repository)
        versions = []
        for tool in ("svn", "svnserve", "svnmucc"):
            lines = subprocess.run(
                [tool, "--version"], capture_output=True, text=True, timeout=STEP_SECONDS, env=self.env
            ).stdout.splitlines()
            versions.append(lines[0].rstrip() if lines else tool + ": no version")
        self.write(os.path.join(self.out, "subversion-version.txt"), "\n".join(versions) + "\n")
        sums = []
        for directory, _, files in os.walk(self.out):
            for file_name in files:
                path = os.path.join(directory, file_name)
                relative = os.path.relpath(path, self.out)
                if relative == "SHA256SUMS":
                    continue
                with open(path, "rb") as handle:
                    sums.append((relative, hashlib.sha256(handle.read()).hexdigest()))
        with open(os.path.join(self.out, "SHA256SUMS"), "w", encoding="utf-8") as handle:
            for relative, digest in sorted(sums):
                handle.write("%s  %s\n" % (digest, relative))


def refuse(text, code):
    """A control snippet: `text` on standard error (printf's escapes), then exit `code`."""
    return "printf '%s' >&2\nexit %d\n" % (text, code)


def record(out):
    work = tempfile.mkdtemp(prefix="g229-a623-hooks.", dir=os.path.expanduser("~/.cache"))
    recorder = Recorder(out, work)
    try:
        recorder.create()
        recorder.control_run()
        recorder.start()
        r = recorder
        url = r.url
        mucc, svn = "svnmucc", "svn"
        a = r.source("a.txt", "a, first\n")
        b = r.source("b.txt", "b, first\n")
        cafe = r.source("cafe.txt", "café, first\n")
        a2 = r.source("a2.txt", "a, second\n")
        new = r.source("new.txt", "new\n")

        # The inputs: commits, locks and revision properties, each hook recording what it was given.
        r.case("commit-by-jrandom", [(mucc, "jrandom", [
            "-U", url, "-m", "First commit.", "mkdir", "trunk", "mkdir", "trunk/with space",
            "put", a, "trunk/a.txt", "put", b, "trunk/with space/b.txt", "put", cafe, "trunk/café.txt"])])
        r.case("commit-by-jconstant", [(mucc, "jconstant", [
            "-U", url, "-m", "Second commit.", "put", a2, "trunk/a.txt"])])
        r.case("lock", [(svn, "jrandom", [
            "lock", "-m", "First line of the comment.\nSecond line.", url + "/trunk/a.txt"])])
        r.case("lock-stolen", [(svn, "jconstant", ["lock", "--force", "-m", "Stolen.", url + "/trunk/a.txt"])])
        r.case("lock-broken", [(svn, "jrandom", ["unlock", "--force", url + "/trunk/a.txt"])])
        r.case("checkout", [(svn, "jrandom", ["checkout", url + "/trunk", r.wc])])
        spaced = os.path.join(r.wc, "with space", "b.txt")
        r.case("lock-token-from-the-hook", [(svn, "jrandom", ["lock", spaced])],
               controls={"pre-lock": "printf '%s' 'opaquelocktoken:fixture-b'\n"})
        r.write(spaced, "b, second\n")
        r.case("commit-releasing-its-lock", [(svn, "jrandom", ["commit", "-m", "Third commit.", r.wc])])
        tokens = (
            "case \"$2\" in\n"
            "  /trunk/café.txt) printf '%s' 'opaquelocktoken:fixture-c' ;;\n"
            "  /trunk/a.txt) printf '%s' 'opaquelocktoken:fixture-a' ;;\n"
            "esac\n"
        )
        wc_cafe, wc_a = os.path.join(r.wc, "café.txt"), os.path.join(r.wc, "a.txt")
        r.case("update", [(svn, "jrandom", ["update", r.wc])])
        r.case("lock-two-paths", [(svn, "jrandom", ["lock", wc_cafe]), (svn, "jrandom", ["lock", wc_a])],
               controls={"pre-lock": tokens})
        r.write(wc_cafe, "café, second\n")
        r.write(wc_a, "a, third\n")
        r.case("commit-keeping-its-locks", [(svn, "jrandom", [
            "commit", "--no-unlock", "-m", "Fourth commit.", r.wc])])
        r.case("unlock-with-tokens", [(svn, "jrandom", ["unlock", wc_cafe]), (svn, "jrandom", ["unlock", wc_a])])
        r.case("revprop-modified", [(svn, "jrandom", [
            "propset", "--revprop", "-r", "1", "svn:log", "Edited log message.", url])])
        r.case("revprop-added", [(svn, "jrandom", [
            "propset", "--revprop", "-r", "1", "slates:note", "added later", url])])
        r.case("revprop-deleted", [(svn, "jrandom", ["propdel", "--revprop", "-r", "1", "slates:note", url])])

        # What the client is told when a hook refuses or fails.
        def put(name):
            return [(mucc, "jrandom", ["-U", url, "-m", "Refused.", "put", new, "trunk/" + name])]

        r.case("start-commit-refuses", put("x1.txt"), told=True,
               controls={"start-commit": refuse("start-commit refuses this commit\\n", 1)})
        r.case("pre-commit-refuses", put("x2.txt"), told=True,
               controls={"pre-commit": refuse("Refused: caf\\303\\251\\nsecond line\\n", 2)})
        r.case("pre-commit-refuses-silently", put("x3.txt"), told=True,
               controls={"pre-commit": "exit 1\n"})
        r.case("pre-commit-killed", put("x4.txt"), told=True,
               controls={"pre-commit": "printf 'dying\\n' >&2\nkill -9 $$\n"})
        r.case("pre-commit-invalid-utf8", put("x5.txt"), told=True,
               controls={"pre-commit": refuse("bad \\377\\376 bytes\\n", 1)})
        r.case("pre-commit-nul", put("x6.txt"), told=True,
               controls={"pre-commit": refuse("before\\000after\\n", 1)})
        r.case("pre-revprop-change-refuses", [(svn, "jrandom", [
            "propset", "--revprop", "-r", "1", "svn:log", "Refused.", url])], told=True,
               controls={"pre-revprop-change": refuse("no revision property changes\\n", 1)})
        aside = os.path.join(r.work, "aside")

        def hide(name):
            return lambda: os.rename(os.path.join(r.hooks, name), aside)

        def bring_back(name):
            return lambda: os.rename(aside, os.path.join(r.hooks, name))

        r.case("pre-revprop-change-missing", [(svn, "jrandom", [
            "propset", "--revprop", "-r", "1", "svn:log", "Refused.", url])], told=True,
               prepare=hide("pre-revprop-change"), restore=bring_back("pre-revprop-change"))
        r.case("post-revprop-change-fails", [(svn, "jrandom", [
            "propset", "--revprop", "-r", "1", "svn:log", "Edited again.", url])], told=True,
               controls={"post-revprop-change": refuse("post-revprop-change failed\\n", 1)})
        r.case("pre-lock-refuses", [(svn, "jrandom", ["lock", url + "/trunk/a.txt"])], told=True,
               controls={"pre-lock": refuse("no locks on this path\\n", 1)})
        r.case("post-lock-fails", [(svn, "jrandom", ["lock", url + "/trunk/a.txt"])], told=True,
               controls={"post-lock": refuse("post-lock failed\\n", 1)})
        r.case("pre-unlock-refuses", [(svn, "jrandom", ["unlock", "--force", url + "/trunk/a.txt"])],
               told=True, controls={"pre-unlock": refuse("no unlocking\\n", 1)})
        r.case("post-unlock-fails", [(svn, "jrandom", ["unlock", "--force", url + "/trunk/a.txt"])],
               told=True, controls={"post-unlock": refuse("post-unlock failed\\n", 1)})
        r.case("post-commit-fails", [(mucc, "jrandom", ["-U", url, "-m", "Fifth commit.", "put", new,
                                                        "trunk/post-commit-fails.txt"])],
               told=True, controls={"post-commit": refuse("post-commit failed\\n", 1)})

        def mode(name, bits):
            return lambda: os.chmod(os.path.join(r.hooks, name), bits)

        r.case("start-commit-not-executable", put("x7.txt"), told=True,
               prepare=mode("start-commit", 0o644), restore=mode("start-commit", 0o755))

        def dangle():
            os.rename(os.path.join(r.hooks, "pre-commit"), aside)
            os.symlink(os.path.join(r.work, "no-such-program"), os.path.join(r.hooks, "pre-commit"))

        def undangle():
            os.remove(os.path.join(r.hooks, "pre-commit"))
            os.rename(aside, os.path.join(r.hooks, "pre-commit"))

        r.case("pre-commit-broken-symlink", put("x8.txt"), told=True, prepare=dangle, restore=undangle)
        r.case("post-commit-not-executable", [(mucc, "jrandom", ["-U", url, "-m", "Sixth commit.", "put", new,
                                                                 "trunk/post-commit-not-executable.txt"])],
               told=True, prepare=mode("post-commit", 0o644), restore=mode("post-commit", 0o755))
        r.case("pre-lock-token-with-a-newline", [(svn, "jrandom", ["lock", url + "/trunk/a.txt"])],
               told=True, controls={"pre-lock": "printf '%s\\n' 'opaquelocktoken:fixture-newline'\n"})
        r.write_cases()
        r.finish()
    finally:
        if recorder.relay is not None:
            recorder.relay.close()
        if recorder.server is not None:
            stop(recorder.server)
        shutil.rmtree(work, ignore_errors=True)


def main():
    if len(sys.argv) != 2:
        print("usage: make-fixtures.py OUTDIR", file=sys.stderr)
        return 2
    out = os.path.abspath(sys.argv[1])
    if os.path.exists(out):
        print("%s exists; the fixtures are written into a new directory" % out, file=sys.stderr)
        return 2
    os.makedirs(out)
    record(out)
    log("written %s" % out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
