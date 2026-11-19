#!/usr/bin/env python3
"""Makes slates-svn's ra_svn handshake fixtures (A-599; §4.17.4 "5. The wire" and "10. Who may do what").

Stock `svn` against stock `svnserve`, each direction of each session logged byte for byte by a relay between the two,
before the relay forwards it, so a file never holds less than its peer received:
- `svn://` sessions: `svnserve -d` on an ephemeral loopback port, and this script's relay on 127.0.0.1:3690, svn://'s
  own port, so every URL is written without a port (protocol §2: "the client connects to the server on port 3690").
- svn+ssh's form: the scheme `svn+fixture://`, whose tunnel command (the client configuration's `[tunnels]`) is this
  script in its tunnel role, running `svnserve -t --tunnel-user=alice` as sshd would run `svnserve -t`.
- Hand-made clients (`hostile-*`): bytes no stock client sends, written here and sent the same ways, with what
  `svnserve` answered, for the refusals the handshake makes where `svnserve` makes them.

Usage: python3 -I make-fixtures.py OUTDIR
Writes NAME.client.bin and NAME.server.bin per session, sessions.txt (each session's configuration, the client's
command, its exit code and its standard error), subversion-version.txt and SHA256SUMS. Every file is the same on every
run but the CRAM-MD5 sessions': their challenge holds a random number, the time and the host's name (`cram.c`
`svn_ra_svn_cram_server`), and the client's response holds the digest of that challenge; and SHA256SUMS, which lists
them.

Sources: Subversion's `subversion/libsvn_ra_svn/protocol` §2, `svnserve/serve.c` (`construct_server_baton`, `auth`,
`internal_auth_request`, `send_mechs`, `find_repos`), `libsvn_ra_svn/cram.c`, `libsvn_ra_svn/client.c`
(`find_tunnel_agent`, `open_session`), `libsvn_ra_svn/internal_auth.c`.
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

# Bytes one read takes from a socket or a pipe.
CHUNK = 65536
# svn://'s port (protocol §2), on which the relay listens so the client's URLs carry none.
RELAY_PORT = 3690
# How long any one step may take before the run is refused: a session's client, a relay's drain, a server's start.
STEP_SECONDS = 60
# The fixed identity every repository is given (`svnadmin setuuid`), so the transcripts name no random UUID.
UUIDS = {
    "anon": "00000000-0000-4000-8000-00000000a599",
    "cram": "00000000-0000-4000-8000-00000001a599",
    "both": "00000000-0000-4000-8000-00000002a599",
    "none": "00000000-0000-4000-8000-00000003a599",
}
# Each repository's svnserve.conf `[general]` section: which mechanisms `send_mechs` lists follows from it.
CONFIGS = {
    # Anonymous users read; no password database, so no CRAM-MD5.
    "anon": "anon-access = read\nauth-access = write\n",
    # Authenticated users alone, by CRAM-MD5, in a realm of the repository's own.
    "cram": "anon-access = none\nauth-access = write\npassword-db = passwd\nrealm = slates fixture realm\n",
    # Both, the realm left to its default, the repository's UUID.
    "both": "anon-access = read\nauth-access = write\npassword-db = passwd\n",
    # No anonymous access and no way to authenticate: "No access allowed to this repository".
    "none": "anon-access = none\nauth-access = write\n",
}
# The password database (`password-db`): a secret shorter than HMAC-MD5's 64-byte block, one exactly 64 bytes long
# (used as is) and one longer (hashed first; RFC 2104 §2, `cram.c` `compute_digest`).
SECRET_64 = "s" * 64
SECRET_70 = "t" * 70
PASSWD = "[users]\nalice = cram-secret\ncarol = %s\ndave = %s\n" % (SECRET_64, SECRET_70)
# The user a tunnel logs in, as sshd's login would be.
TUNNEL_USER = "alice"


def log(message):
    print(message, flush=True)


def run(argv, **kwargs):
    """Runs a stock tool to its end, refusing a failure."""
    return subprocess.run(argv, check=True, timeout=STEP_SECONDS, **kwargs)


def write_all_fd(fd, data):
    view = memoryview(data)
    while view:
        written = os.write(fd, view)
        view = view[written:]


def pump_fds(read_fd, write_fd, record, close_write):
    """Copies read_fd to write_fd until its end, recording every chunk before forwarding it."""
    while True:
        data = os.read(read_fd, CHUNK)
        if not data:
            close_write()
            return
        record.write(data)
        write_all_fd(write_fd, data)


def tunnel(argv):
    """The tunnel role: `[tunnels]` runs it as `make-fixtures.py --tunnel PREFIX ROOT HOST svnserve -t`."""
    prefix, root = argv[0], argv[1]
    server = subprocess.Popen(
        ["svnserve", "-t", "--tunnel-user=" + TUNNEL_USER, "-r", root],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
    )
    with open(prefix + ".client.bin", "wb", buffering=0) as client_record, open(
        prefix + ".server.bin", "wb", buffering=0
    ) as server_record:
        upward = threading.Thread(
            target=pump_fds,
            args=(0, server.stdin.fileno(), client_record, server.stdin.close),
            daemon=True,
        )
        upward.start()
        pump_fds(server.stdout.fileno(), 1, server_record, lambda: os.close(1))
        upward.join(STEP_SECONDS)
    return server.wait(STEP_SECONDS)


def pump_sockets(source, sink, record):
    """Copies one socket's bytes to another until the first ends, recording every chunk before forwarding it."""
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


def relay_one(listener, upstream_port, prefix, failures):
    """Accepts one client on the relay's port and relays it to `svnserve`, recording each direction; a failure is
    kept in `failures` for the caller to refuse the run with."""
    try:
        relay_session(listener, upstream_port, prefix)
    except Exception as error:  # noqa: BLE001 -- handed to the caller, which refuses the run with it
        failures.append("%s: %r" % (prefix, error))


def relay_session(listener, upstream_port, prefix):
    client, _ = listener.accept()
    server = socket.create_connection(("127.0.0.1", upstream_port), timeout=STEP_SECONDS)
    client.settimeout(STEP_SECONDS)
    with open(prefix + ".client.bin", "wb", buffering=0) as client_record, open(
        prefix + ".server.bin", "wb", buffering=0
    ) as server_record:
        upward = threading.Thread(target=pump_sockets, args=(client, server, client_record), daemon=True)
        downward = threading.Thread(target=pump_sockets, args=(server, client, server_record), daemon=True)
        upward.start()
        downward.start()
        upward.join(STEP_SECONDS)
        downward.join(STEP_SECONDS)
        if upward.is_alive() or downward.is_alive():
            raise RuntimeError("relay for %s did not drain within %d s" % (prefix, STEP_SECONDS))
    client.close()
    server.close()


def free_port():
    probe = socket.socket()
    probe.bind(("127.0.0.1", 0))
    port = probe.getsockname()[1]
    probe.close()
    return port


def start_daemon(root, log_file, env):
    """`svnserve -d` on an ephemeral loopback port, waited for by a connection that it answers and that then ends."""
    port = free_port()
    daemon = subprocess.Popen(
        [
            "svnserve", "-d", "--foreground", "--listen-host", "127.0.0.1", "--listen-port", str(port),
            "-r", root, "--log-file", log_file,
        ],
        env=env,
    )
    deadline = time.monotonic() + STEP_SECONDS
    while True:
        try:
            probe = socket.create_connection(("127.0.0.1", port), timeout=STEP_SECONDS)
            probe.close()
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
    """An ra_svn string item, `length:bytes ` (protocol §1)."""
    return b"%d:%s " % (len(data), data)


def answer(url, capabilities, tail=b"10:SVN/1.15.0 ( ) "):
    """A client's answer to the greeting, `( version ( cap ... ) url ra-client ( client ) )` (protocol §2)."""
    return b"( 2 ( " + b"".join(c + b" " for c in capabilities) + b") " + item_string(url) + tail + b") "


# What a stock 1.15 client announces (`client.c` `open_session`), for the hand-made answers.
STOCK_CAPABILITIES = [
    b"edit-pipeline", b"svndiff1", b"accepts-svndiff2", b"absent-entries", b"depth", b"mergeinfo", b"log-revprops",
]


def main():
    if len(sys.argv) >= 2 and sys.argv[1] == "--tunnel":
        return tunnel(sys.argv[2:])
    out = os.path.abspath(sys.argv[1])
    os.makedirs(out, exist_ok=True)
    work = tempfile.mkdtemp(prefix="g234-a599-svnfix.", dir=os.path.expanduser("~/.cache"))
    daemon = None
    try:
        root = os.path.join(work, "root")
        os.makedirs(root)
        home = os.path.join(work, "home")
        os.makedirs(home)
        config = os.path.join(work, "config")
        os.makedirs(config)
        env = dict(os.environ, HOME=home, LC_ALL="C", LANG="C", TZ="UTC")
        for name, general in CONFIGS.items():
            repository = os.path.join(root, name)
            run(["svnadmin", "create", repository], env=env)
            run(["svnadmin", "setuuid", repository, UUIDS[name]], env=env)
            with open(os.path.join(repository, "conf", "svnserve.conf"), "w") as conf:
                conf.write("[general]\n" + general)
            with open(os.path.join(repository, "conf", "passwd"), "w") as passwd:
                passwd.write(PASSWD)
        script = os.path.abspath(__file__)
        with open(os.path.join(config, "config"), "w") as client_config:
            client_config.write(
                '[auth]\npassword-stores =\n[tunnels]\nfixture = "%s" -I "%s" --tunnel "%s" "%s"\n'
                % (sys.executable, script, os.path.join(out, "external"), root)
            )
        with open(os.path.join(config, "servers"), "w") as servers:
            servers.write("[global]\nstore-passwords = no\nstore-auth-creds = no\n")
        versions = [
            subprocess.run([tool, "--version"], capture_output=True, text=True, timeout=STEP_SECONDS, env=env)
            .stdout.splitlines()[:2]
            for tool in ("svn", "svnserve")
        ]
        with open(os.path.join(out, "subversion-version.txt"), "w") as version_file:
            for lines in versions:
                version_file.write("\n".join(line.rstrip() for line in lines) + "\n")
        log_file = os.path.join(work, "svnserve.log")
        daemon, port = start_daemon(root, log_file, env)
        listener = socket.socket()
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", RELAY_PORT))
        listener.listen(1)
        listener.settimeout(STEP_SECONDS)
        sessions = []
        svn = ["svn", "--config-dir", config, "--non-interactive", "--no-auth-cache"]
        stock = [
            # (name, repository path, client arguments, what the configuration lists)
            ("anonymous", "anon", [], "ANONYMOUS"),
            ("cram-md5", "cram", ["--username", "alice", "--password", "cram-secret"], "CRAM-MD5"),
            ("cram-md5-secret-64", "cram", ["--username", "carol", "--password", SECRET_64], "CRAM-MD5"),
            ("cram-md5-secret-70", "cram", ["--username", "dave", "--password", SECRET_70], "CRAM-MD5"),
            ("cram-md5-wrong-password", "cram", ["--username", "alice", "--password", "not-it"], "CRAM-MD5"),
            ("anonymous-or-cram-md5", "both", [], "ANONYMOUS CRAM-MD5"),
            ("no-repository", "missing", [], "none: no repository"),
            ("no-access", "none", [], "none: no mechanism"),
        ]
        for name, path, arguments, mechanisms in stock:
            prefix = os.path.join(out, name)
            failures = []
            relay = threading.Thread(target=relay_one, args=(listener, port, prefix, failures))
            relay.start()
            url = "svn://127.0.0.1/%s" % path
            result = subprocess.run(
                svn + arguments + ["info", url], capture_output=True, text=True, timeout=STEP_SECONDS, env=env
            )
            relay.join(STEP_SECONDS)
            if relay.is_alive() or failures:
                raise RuntimeError("relay for %s did not end cleanly: %s" % (name, failures))
            sessions.append((name, "svn:// via relay", path, mechanisms, "svn info " + url, result))
        # svn+ssh's form: the tunnel runs svnserve -t, which lists EXTERNAL for the tunnel's user.
        url = "svn+fixture://127.0.0.1/both"
        result = subprocess.run(svn + ["info", url], capture_output=True, text=True, timeout=STEP_SECONDS, env=env)
        sessions.append(
            ("external", "tunnel, svnserve -t", "both", "ANONYMOUS EXTERNAL CRAM-MD5", "svn info " + url, result)
        )
        # Hand-made clients over svn://: each sends its whole session at once and half-closes; svnserve answers what
        # it reads in order and ends at the end of the bytes or at its first refusal.
        anon_url = b"svn://127.0.0.1/anon"
        hostile = [
            ("hostile-version-3", answer(anon_url, STOCK_CAPABILITIES).replace(b"( 2 ", b"( 3 ", 1)),
            ("hostile-no-edit-pipeline", answer(anon_url, STOCK_CAPABILITIES[1:])),
            ("hostile-capability-not-a-word", answer(anon_url, STOCK_CAPABILITIES + [b"7"])),
            ("hostile-ra-client-without-client-list", answer(anon_url, STOCK_CAPABILITIES, b"10:SVN/1.15.0 ")),
            (
                "hostile-unlisted-mechanism-then-anonymous",
                answer(anon_url, STOCK_CAPABILITIES) + b"( PLAIN ( 0: ) ) ( CRAM-MD5 ( ) ) ( ANONYMOUS ( 0: ) ) ",
            ),
            ("hostile-answer-only-version", b"( 2 ) "),
        ]
        for name, client_bytes in hostile:
            prefix = os.path.join(out, name)
            with socket.create_connection(("127.0.0.1", port), timeout=STEP_SECONDS) as connection:
                with open(prefix + ".client.bin", "wb", buffering=0) as client_record:
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
            sessions.append((name, "svn://, hand-made client", "anon", "ANONYMOUS", "(written by this script)", None))
        # A hand-made client through the tunnel: EXTERNAL naming another user, then the tunnel's own.
        both_url = b"svn+fixture://127.0.0.1/both"
        name = "hostile-external-other-user-then-own"
        client_bytes = answer(both_url, STOCK_CAPABILITIES) + b"( EXTERNAL ( 3:bob ) ) ( EXTERNAL ( 5:alice ) ) "
        prefix = os.path.join(out, name)
        with open(prefix + ".client.bin", "wb") as client_record:
            client_record.write(client_bytes)
        result = subprocess.run(
            ["svnserve", "-t", "--tunnel-user=" + TUNNEL_USER, "-r", root],
            input=client_bytes,
            capture_output=True,
            timeout=STEP_SECONDS,
            env=env,
        )
        with open(prefix + ".server.bin", "wb") as server_record:
            server_record.write(result.stdout)
        sessions.append((name, "svnserve -t, hand-made client", "both", "ANONYMOUS EXTERNAL CRAM-MD5", "(written)", None))
        listener.close()
        with open(os.path.join(out, "sessions.txt"), "w") as summary:
            for name, transport, path, mechanisms, command, result in sessions:
                summary.write("%s: %s; repository %s; lists %s; client %s\n" % (name, transport, path, mechanisms, command))
                if result is not None:
                    summary.write("  exit %d\n" % result.returncode)
                    for line in result.stderr.splitlines():
                        summary.write("  stderr: %s\n" % line)
        names = sorted(f for f in os.listdir(out) if f != "SHA256SUMS")
        with open(os.path.join(out, "SHA256SUMS"), "w") as sums:
            for file_name in names:
                with open(os.path.join(out, file_name), "rb") as fixture:
                    sums.write("%s  %s\n" % (hashlib.sha256(fixture.read()).hexdigest(), file_name))
        log("svnserve log:")
        with open(log_file) as server_log:
            log(server_log.read())
    finally:
        if daemon is not None:
            stop(daemon)
        shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
