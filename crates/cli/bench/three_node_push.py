#!/usr/bin/env python3
"""A push's latency on a fleet of three `slates anchor --fleet` processes at f = 1 (A-724 piece 8c, main's order of
2026-11-20: the three-node A/B, where a version-3 push homes its names in the region at f + 1, a region round trip).

Usage: three_node_push.py SOURCE REV ROUNDS BUILD... (each BUILD a `slates` binary, run in turn each round)

Each round starts a fresh fleet for each build: three anchors over loopback UDP, each with its version-control HTTPS edge
(`--vcs-http 127.0.0.1:0`), formed and bootstrapped; then on one node (not the bootstrap's) it creates repositories and
pushes git's receive-pack request for REV of SOURCE, once, then ten at once, each to its own repository, over TLS with a
token minted there, timing each push from its connection to its whole answer, and reads the node's homing counters.
Every round is printed, never a summary alone. The anchors and their daemons are killed by pid at each round's end.
"""

import asyncio
import json
import os
import re
import shutil
import signal
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import push_load  # noqa: E402  (the receive-pack request, its pkt-lines)

# Shape: the fleet's nodes and its TLS name.
NODES = ["a", "b", "c"]
FLEET_NAME = "slates-fleet"
# Shape: how long a fleet step may take on a loaded box (the process flows' formation wait).
FLEET_WAIT = 60.0
# Format: the lines the anchor and its daemon print.
SERVING = "slates daemon: instance {} serving on "
HTTPS_EDGE = re.compile(r"the version-control HTTP edge serves HTTPS on 127\.0\.0\.1:(\d+)")
ISSUER = "slates anchor: issuer surface: export "
DAEMON_PID = re.compile(r"daemon pid (\d+)")
# Format: the counters a round reads from the pushing node.
WATCHED = re.compile(r"homes\.(homed_in_region|homed_short|published_for_ticket|homing_untaken|homing_local_ns|"
                     r"homing_region_ns)|state_log\.push_uncommitted")


def run(slates, instance, args, env=None, check=False):
    return subprocess.run([slates, "--instance", instance, *args], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                          text=True, env=env, check=check)


def mint(directory, node):
    """A self-signed P-256 end-entity certificate (no CA bit, as rcgen makes them: a verifier refuses a CA certificate
    as a peer's identity) and its PKCS#8 key, DER, named for the fleet."""
    pem_key, pem_cert = os.path.join(directory, f"{node}.key.pem"), os.path.join(directory, f"{node}.crt.pem")
    subprocess.run(["openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256", "-nodes",
                    "-keyout", pem_key, "-out", pem_cert, "-subj", f"/CN={FLEET_NAME}",
                    "-addext", f"subjectAltName=DNS:{FLEET_NAME}", "-addext", "basicConstraints=critical,CA:FALSE",
                    "-addext", "keyUsage=critical,digitalSignature", "-days", "30"],
                   check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    subprocess.run(["openssl", "x509", "-in", pem_cert, "-outform", "DER", "-out", os.path.join(directory, f"{node}.crt.der")],
                   check=True)
    subprocess.run(["openssl", "pkcs8", "-topk8", "-nocrypt", "-in", pem_key, "-outform", "DER",
                    "-out", os.path.join(directory, f"{node}.key.der")], check=True)


def port_blocks():
    """Two consecutive free loopback UDP ports a node, held open: its probe and record ports."""
    blocks, base = [], 20000 + (os.getpid() * 7) % 30000
    while len(blocks) < len(NODES):
        try:
            first = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            first.bind(("127.0.0.1", base))
            second = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            second.bind(("127.0.0.1", base + 1))
            blocks.append((base, first, second))
        except OSError:
            pass
        base += 2
    return blocks


class Fleet:
    """Three anchors from one manifest, formed and bootstrapped."""

    def __init__(self, slates, tag):
        self.slates = slates
        self.scratch = tempfile.mkdtemp(prefix=f"three-node-{tag}-", dir=os.path.expanduser("~/.cache"))
        for node in NODES:
            mint(self.scratch, node)
        blocks = port_blocks()
        manifest = {"name": FLEET_NAME, "f": 1, "nodes": [
            {"node": node, "address": f"127.0.0.1:{base}", "certificate": f"{node}.crt.der", "key": f"{node}.key.der"}
            for node, (base, _, _) in zip(NODES, blocks)]}
        path = os.path.join(self.scratch, "fleet.json")
        with open(path, "w") as out:
            json.dump(manifest, out)
        self.nodes = []
        for node, (_, probe, record) in zip(NODES, blocks):
            instance = f"three-node-{tag}-{node}-{os.getpid()}"
            fds = (probe.fileno(), record.fileno())
            for fd in fds:
                os.set_inheritable(fd, True)
            env = dict(os.environ, SLATES_ANCHOR_FLEET_SERVE=f"{fds[0]},{fds[1]}")
            anchor = subprocess.Popen(
                [slates, "--instance", instance, "anchor", "--quick", "--shards", "2", "--fleet", path, "--node", node,
                 "--vcs-http", "127.0.0.1:0"],
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True, env=env, pass_fds=fds)
            self.nodes.append(self.started(instance, anchor))
            probe.close()
            record.close()
        self.formed()

    def started(self, instance, anchor):
        port = issuer = pid = None
        served = False
        deadline = time.time() + FLEET_WAIT
        while not (served and port and issuer is not None):
            line = anchor.stderr.readline()
            if not line or time.time() > deadline:
                raise SystemExit(f"{instance} did not serve")
            served |= line.startswith(SERVING.format(instance))
            if m := HTTPS_EDGE.search(line):
                port = int(m.group(1))
            if line.startswith(ISSUER):
                issuer = dict(pair.split("=", 1) for pair in line[len(ISSUER):].split())
            if m := DAEMON_PID.search(line):
                pid = int(m.group(1))
        threading.Thread(target=lambda: [None for _ in anchor.stderr], daemon=True).start()
        return {"instance": instance, "anchor": anchor, "port": port, "issuer": issuer, "pid": pid}

    def formed(self):
        deadline = time.time() + FLEET_WAIT
        while True:
            views = []
            for node in self.nodes:
                out = run(self.slates, node["instance"], ["status"]).stdout
                fields = dict(line.split(": ", 1) for line in out.splitlines() if ": " in line)
                views.append((fields.get("fleet_host"), len(fields.get("fleet_members", "").split()),
                              fields.get("fleet_peers_probed")))
            if all(members == 3 and probed == "2" for _, members, probed in views):
                hosts = [int(host) for host, _, _ in views]
                break
            if time.time() > deadline:
                raise SystemExit(f"the fleet did not form: {views}")
            time.sleep(0.05)
        self.bootstrap = min(range(3), key=lambda at: hosts[at])
        self.ready(self.nodes[self.bootstrap], ["bootstrap", "root"])
        self.owner = self.nodes[(self.bootstrap + 1) % 3]

    def ready(self, node, args):
        env = dict(os.environ, **node["issuer"])
        deadline = time.time() + FLEET_WAIT
        while True:
            done = run(self.slates, node["instance"], args, env=env)
            if done.returncode == 0:
                return done.stdout
            if time.time() > deadline:
                raise SystemExit(f"{args} on {node['instance']}: {done.stdout}{done.stderr}")
            time.sleep(0.05)

    def counters(self, node):
        out = run(self.slates, node["instance"], ["status"]).stdout
        found = {}
        for line in out.splitlines():
            if m := re.search(r"refused (\S+): (\d+)", line):
                if WATCHED.search(m.group(1)):
                    found[m.group(1)] = found.get(m.group(1), 0) + int(m.group(2))
        return found

    def stop(self):
        for node in self.nodes:
            node["anchor"].kill()
            node["anchor"].wait()
            if node["pid"]:
                try:
                    os.kill(node["pid"], signal.SIGKILL)
                except ProcessLookupError:
                    pass
        shutil.rmtree(self.scratch, ignore_errors=True)


async def push(port, token, repository, body):
    """One push over TLS (the node's self-signed certificate not verified): its time and its answer's words."""
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    start = time.perf_counter()
    reader, writer = await asyncio.open_connection("127.0.0.1", port, ssl=context)
    writer.write((f"POST /{repository}/git-receive-pack HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
                  f"Authorization: Bearer {token}\r\nContent-Type: application/x-git-receive-pack-request\r\n"
                  f"Accept: application/x-git-receive-pack-result\r\nContent-Length: {len(body)}\r\n"
                  f"Connection: close\r\n\r\n").encode() + body)
    await writer.drain()
    answer = await reader.read()
    writer.close()
    took = time.perf_counter() - start
    ok = b" 200 " in answer.split(b"\r\n", 1)[0] and b"unpack ok" in answer
    return took, "ok" if ok else answer.split(b"\r\n", 1)[0].decode("latin-1", "replace")


async def burst(fleet, token, names, body):
    return await asyncio.gather(*(push(fleet.owner["port"], token, name, body) for name in names))


def main():
    source, rev, rounds, builds = sys.argv[1], sys.argv[2], int(sys.argv[3]), sys.argv[4:]
    body, head, objects = push_load.receive_pack_request(source, rev)
    print(f"request: {len(body):,} bytes, {objects:,} objects at {head[:12]}; {os.cpu_count()} cores, "
          f"load {os.getloadavg()}", flush=True)
    for round_ in range(1, rounds + 1):
        for build in builds:
            label = os.path.basename(build)
            fleet = Fleet(build, f"{label}-{round_}")
            try:
                env = dict(os.environ, **fleet.owner["issuer"])
                names = ["one"] + [f"many{n}" for n in range(10)]
                for name in names:
                    fleet.ready(fleet.owner, ["repository", "create", name, "--tool", "git"])
                token = re.search(r"^token: (\S+)", fleet.ready(fleet.owner, ["credential", "token"]), re.M).group(1)
                del env
                one = asyncio.run(burst(fleet, token, ["one"], body))
                many = asyncio.run(burst(fleet, token, names[1:], body))
                times = sorted(took for took, _ in many)
                kinds = sorted({word for _, word in one + many})
                print(f"-- round {round_} {label}: 1 push {one[0][0]:.3f} s; 10 at once p50 {times[4]:.3f} s, "
                      f"p99 {times[-1]:.3f} s; outcomes {kinds}; counted {fleet.counters(fleet.owner)}", flush=True)
            finally:
                fleet.stop()
    print(f"load at the end {os.getloadavg()}", flush=True)


if __name__ == "__main__":
    main()
