#!/usr/bin/env python3
"""Concurrent large pushes against one slates daemon (UI goal 93; Ada's requirement of 2026-11-20: one server takes
tens of thousands of concurrent large pushes). `docs/wip/BENCHMARKS.md` records each run; the command is the contract.

What it does. A slim protocol-level client, since stock git cannot run ten thousand pushes at once on one box: it makes
git's smart-HTTP receive-pack request once, in memory (a create command for `refs/heads/main` with `report-status`,
then a pack of every object SOURCE's REV reaches, from `git pack-objects --revs --stdout`), and replays it, the request
`git push` sends after its advertisement (smart HTTP is stateless: the POST alone is a push). For each step's
concurrency N it makes N fresh repositories through the CLI (untimed), then sends N pushes at once, one to each, over
its own connections, and waits for every answer. Per push it records the wall time to the whole answer, the HTTP
status and git's report (`unpack ok` and `ok refs/heads/main`, or the `ng`/`unpack` words, or the transport's failure).
Per step it records the completions, the refusals by kind, p50 and p99 of the times, and the daemon's resident set
before and at its peak during the step (`ps`, sampled through the step), from which the memory a push held.

The steps. Powers of ten from 1 up to the most the client can hold open: its open-file limit (raised to the hard
limit), less a reserve of a tenth of it for the daemon's CLI calls and the process itself (R3: from the machine, no
fixed count). Every step runs against the same daemon, in order, so a step meets what the earlier ones left.

Nothing is written outside memory: the pack is read from `git pack-objects`'s pipe, the answers are kept in memory,
and the daemon is RAM-only.

Usage: push_load.py SLATES SOURCE REV [--max N]
  SLATES  the release `slates` binary
  SOURCE  a git repository holding REV (a bare clone of the dataset)
  REV     the revision the pack reaches (e.g. v1.0.0^{commit} for git/git cut at v1.0.0)
  --max   the largest step, at most the derived one (default the derived one)
"""

import asyncio
import os
import re
import resource
import subprocess
import sys
import time

ZERO_ID = "0" * 40


def pkt_line(text: bytes) -> bytes:
    return b"%04x" % (len(text) + 4) + text


def receive_pack_request(source: str, rev: str) -> tuple[bytes, str, int]:
    """The request body: a create of refs/heads/main at REV with report-status, a flush, then the pack."""
    head = subprocess.run(
        ["git", "-C", source, "rev-parse", rev], check=True, stdout=subprocess.PIPE, text=True
    ).stdout.strip()
    pack = subprocess.run(
        ["git", "-C", source, "pack-objects", "--revs", "--stdout", "-q"],
        input=(head + "\n").encode(),
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    command = f"{ZERO_ID} {head} refs/heads/main\0report-status agent=w-perf-push-load\n".encode()
    count = int.from_bytes(pack[8:12], "big") if len(pack) >= 12 else 0
    return pkt_line(command) + b"0000" + pack, head, count


def derived_max() -> int:
    """The largest step: the open-file limit, raised to the hard limit, less a tenth for everything else."""
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    target = hard
    if hard == resource.RLIM_INFINITY:
        # macOS reports no hard limit; the kernel's per-process ceiling is the real one.
        ceiling = subprocess.run(["sysctl", "-n", "kern.maxfilesperproc"], stdout=subprocess.PIPE, text=True).stdout
        target = int(ceiling.strip()) if ceiling.strip().isdigit() else soft
    try:
        resource.setrlimit(resource.RLIMIT_NOFILE, (target, hard))
        soft = target
    except (ValueError, OSError):
        pass
    return max(1, soft - soft // 10)


def steps_up_to(most: int) -> list[int]:
    steps, n = [], 1
    while n <= most:
        steps.append(n)
        n *= 10
    return steps


def rss_kib(pid: int) -> int:
    out = subprocess.run(["ps", "-o", "rss=", "-p", str(pid)], stdout=subprocess.PIPE, text=True).stdout.strip()
    return int(out) if out else 0


class Daemon:
    """An anchor and its daemon, with the HTTP edge on loopback and a token."""

    def __init__(self, slates: str):
        self.slates = slates
        self.instance = f"w-perf-push-load-{os.getpid()}"
        self.anchor = subprocess.Popen(
            [slates, "--instance", self.instance, "anchor", "--vcs-http", "127.0.0.1:0"],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            text=True,
        )
        self.lines: list[str] = []
        self.port = self.pid = None
        exports = None
        deadline = time.time() + 120
        while time.time() < deadline and (self.port is None or exports is None or not self.serving()):
            line = self.anchor.stderr.readline()
            if not line:
                raise SystemExit("the anchor ended:\n" + "".join(self.lines[-20:]))
            self.lines.append(line)
            if m := re.search(r"HTTP edge serves cleartext on 127\.0\.0\.1:(\d+)", line):
                self.port = int(m.group(1))
            if m := re.search(r"up; daemon pid (\d+);", line):
                self.pid = int(m.group(1))
            if m := re.match(r"slates anchor: issuer surface: export (.*)", line):
                exports = m.group(1).split()
        streak = 0
        while streak < 10:
            ok = self.cli("volume", "list", check=False).returncode == 0
            streak = streak + 1 if ok else 0
            time.sleep(0.1)
        self.cli("bootstrap", "root")
        env = dict(os.environ)
        env.update(dict(pair.split("=", 1) for pair in exports))
        minted = self.cli("credential", "token", env=env).stdout
        self.token = re.search(r"^token: (\S+)", minted, re.M).group(1)

    def serving(self) -> bool:
        return any(f"instance {self.instance} serving on" in line for line in self.lines)

    def cli(self, *words, check=True, env=None):
        return subprocess.run(
            [self.slates, "--instance", self.instance, *words],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            check=check,
            env=env,
        )

    def stop(self):
        self.anchor.kill()
        self.anchor.wait()
        if self.pid:
            try:
                os.kill(self.pid, 9)
            except ProcessLookupError:
                pass


async def push(port: int, token: str, repository: str, body: bytes) -> tuple[float, str]:
    """One push: the POST, then the whole answer read. Its time and its outcome's words."""
    start = time.perf_counter()
    try:
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        head = (
            f"POST /{repository}/git-receive-pack HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
            f"Authorization: Bearer {token}\r\nContent-Type: application/x-git-receive-pack-request\r\n"
            f"Accept: application/x-git-receive-pack-result\r\nContent-Length: {len(body)}\r\nConnection: close\r\n\r\n"
        ).encode()
        writer.write(head)
        writer.write(body)
        await writer.drain()
        status_line = await reader.readline()
        headers = {}
        while (line := await reader.readline()) not in (b"\r\n", b"\n", b""):
            name, _, value = line.decode("latin-1").partition(":")
            headers[name.strip().lower()] = value.strip()
        if headers.get("transfer-encoding", "").lower() == "chunked":
            answer = b""
            while True:
                size = int((await reader.readline()).strip() or b"0", 16)
                if size == 0:
                    break
                answer += await reader.readexactly(size)
                await reader.readline()
        elif "content-length" in headers:
            answer = await reader.readexactly(int(headers["content-length"]))
        else:
            answer = await reader.read()
        writer.close()
        status = status_line.split(b" ")[1].decode() if b" " in status_line else "?"
        words = outcome(status, answer)
    except (OSError, asyncio.IncompleteReadError, ValueError) as error:
        words = f"transport: {type(error).__name__}: {error}"[:120]
    return time.perf_counter() - start, words


def outcome(status: str, answer: bytes) -> str:
    """`ok` for `unpack ok` and `ok refs/heads/main`; else the status and git's first words."""
    if status != "200":
        return f"http {status}: {answer[:80]!r}"
    text = answer.decode("latin-1")
    if "unpack ok" in text and "ok refs/heads/main" in text:
        return "ok"
    found = re.findall(r"(unpack [^\n]*|ng [^\n]*)", text)
    return "report: " + "; ".join(found)[:120] if found else f"report: {text[:80]!r}"


async def sample_peak(pid: int, stop: asyncio.Event) -> int:
    peak = 0
    while not stop.is_set():
        peak = max(peak, rss_kib(pid))
        await asyncio.sleep(0.1)
    return max(peak, rss_kib(pid))


async def step(daemon: Daemon, n: int, number: int, body: bytes) -> dict:
    names = [f"load-{number}-{i}" for i in range(n)]
    for name in names:
        daemon.cli("repository", "create", name, "--tool", "git", "--default-branch", "main")
    before = rss_kib(daemon.pid)
    stop = asyncio.Event()
    sampler = asyncio.create_task(sample_peak(daemon.pid, stop))
    start = time.perf_counter()
    results = await asyncio.gather(*(push(daemon.port, daemon.token, name, body) for name in names))
    wall = time.perf_counter() - start
    stop.set()
    peak = await sampler
    times = sorted(t for t, _ in results)
    kinds: dict[str, int] = {}
    for _, words in results:
        kinds[words] = kinds.get(words, 0) + 1
    pick = lambda q: times[min(len(times) - 1, int(q * len(times)))]
    return {
        "n": n,
        "ok": kinds.get("ok", 0),
        "wall": wall,
        "p50": pick(0.50),
        "p99": pick(0.99),
        "rss_before_mib": before / 1024,
        "rss_peak_mib": peak / 1024,
        "per_push_mib": (peak - before) / 1024 / n,
        "alive": rss_kib(daemon.pid) > 0,
        "kinds": kinds,
    }


def main():
    args = sys.argv[1:]
    most = derived_max()
    if "--max" in args:
        at = args.index("--max")
        most = min(most, int(args[at + 1]))
        del args[at : at + 2]
    slates, source, rev = args
    body, head, objects = receive_pack_request(source, rev)
    print(f"request: {len(body):,} bytes, a pack of {objects:,} objects reaching {head} ({rev} of {source})")
    print(f"machine: {subprocess.run(['sysctl', '-n', 'machdep.cpu.brand_string'], stdout=subprocess.PIPE, text=True).stdout.strip()}, "
          f"{os.cpu_count()} cores; load {os.getloadavg()}; open-file room {most}")
    daemon = Daemon(slates)
    print(f"daemon pid {daemon.pid}, edge 127.0.0.1:{daemon.port}")
    try:
        for number, n in enumerate(steps_up_to(most)):
            result = asyncio.run(step(daemon, n, number, body))
            kinds = ", ".join(f"{k} x{v}" for k, v in sorted(result["kinds"].items(), key=lambda kv: -kv[1]))
            print(
                f"N={n}: ok {result['ok']}/{n}, wall {result['wall']:.3f} s, p50 {result['p50']:.3f} s, "
                f"p99 {result['p99']:.3f} s, rss {result['rss_before_mib']:.0f} -> peak {result['rss_peak_mib']:.0f} MiB "
                f"({result['per_push_mib']:.2f} MiB a push), daemon {'alive' if result['alive'] else 'GONE'}; {kinds}",
                flush=True,
            )
            if not result["alive"]:
                break
    finally:
        daemon.stop()
        print(f"load at the end {os.getloadavg()}")


if __name__ == "__main__":
    main()
