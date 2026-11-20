#!/usr/bin/env python3
"""Many repositories of mixed sizes and skewed activity against one slates daemon (A-724 piece 7; §4.17.5 "Proof";
Ada's goal of 2026-11-20: one server serves toy repositories and monorepos at once, balanced by learned usage).
`docs/wip/BENCHMARKS.md` records each run; the command is the contract.

What it shows. Whether one daemon divides itself among many repositories so that a huge or hot repository never
starves the others: per size class, each push's wall time to its whole answer at p50 and p99 against the same class's
push made alone (its starvation ratio), the refusals by kind, and the daemon's resident set before and at its peak.

The repositories. A step of R repositories gives the repository of rank i (1 to R) the pack of size class
`classes - 1 - min(classes - 1, d(i))`, where d(1) = 0 and d(i) = floor(log10(i - 1)) + 1: rank 1 the largest, ranks
2 to 10 the next, 11 to 100 the next, and the rest the smallest, so at R = 1,000 over four classes there is one giant,
nine large, ninety medium and nine hundred small repositories, sizes falling by rank as a power law does. Each class's pack is every object one revision of SOURCE
reaches (`git pack-objects --revs`), the revisions given smallest first.

The activity. A step sends about R pushes at once, repository i receiving its share by Zipf's law of exponent one
(`R × (1/i) / H_R`, rounded, at least the first repository one): a few repositories take most of the pushes, as a hot
monorepo beside idle toys does. Each push creates its own branch (`refs/heads/p<k>`) at the class's head, so a
repository pushed again holds the objects it was sent before and is sent them again, as a busy repository is.

The bounds (main's order: bound every step, stop at the first refusal storm or memory alarm). Concurrency at most the
open-file room (`push_load.derived_max`, from the machine). Before each step its memory is estimated from the memory a
push of each class held in the step before (the alone runs first) and the step is not run if the estimate passes the
machine's free memory now (`vm_stat`); a step stops the run when the daemon is gone, when more than half its pushes are
refused, or when macOS reports memory pressure above normal (`kern.memorystatus_vm_pressure_level`). Each step runs on a
fresh daemon, so no step meets what another left.

Nothing is written outside memory: the packs are read from `git pack-objects`'s pipe and the daemon is RAM-only.

Usage: multi_repo_load.py SLATES SOURCE REV [REV ...] [--steps 1,10,100,1000]
  SLATES  the release `slates` binary
  SOURCE  a git repository holding every REV (a bare clone of the dataset)
  REV     one revision a size class, smallest first (e.g. v0.99 v1.0.0 v1.5.0 v2.0.0 of git/git)
  --steps the repository counts of the steps, in order (default: powers of ten up to the open-file room)
"""

import asyncio
import math
import os
import re
import subprocess
import sys
import time

import push_load
from push_load import ZERO_ID, Daemon, derived_max, pkt_line, rss_kib


def pack_of(source: str, rev: str) -> tuple[bytes, str, int]:
    """Every object REV reaches, as one pack: its bytes, the commit's id, its object count."""
    head = subprocess.run(
        ["git", "-C", source, "rev-parse", rev + "^{commit}"], check=True, stdout=subprocess.PIPE, text=True
    ).stdout.strip()
    pack = subprocess.run(
        ["git", "-C", source, "pack-objects", "--revs", "--stdout", "-q"],
        input=(head + "\n").encode(),
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    count = int.from_bytes(pack[8:12], "big") if len(pack) >= 12 else 0
    return pack, head, count


def request(head: str, pack: bytes, branch: str) -> bytes:
    """git's receive-pack request: a create of BRANCH at HEAD with report-status, a flush, the pack."""
    command = f"{ZERO_ID} {head} refs/heads/{branch}\0report-status agent=w-balance-multi-repo\n".encode()
    return pkt_line(command) + b"0000" + pack


def class_of(rank: int, classes: int) -> int:
    """The size class of the repository of RANK (1-based): the largest for rank 1, then one smaller for ranks 2 to 10,
    11 to 100, 101 to 1,000 and so on (a decade a class)."""
    decade = 0 if rank == 1 else int(math.log10(rank - 1)) + 1
    return classes - 1 - min(classes - 1, decade)


def pushes_by_zipf(repositories: int) -> list[int]:
    """How many of REPOSITORIES pushes each repository receives, by Zipf's law of exponent one, at least one for the
    first."""
    harmonic = sum(1 / rank for rank in range(1, repositories + 1))
    counts = [round(repositories / rank / harmonic) for rank in range(1, repositories + 1)]
    counts[0] = max(1, counts[0])
    return counts


def free_mib() -> float:
    """The machine's free and inactive memory now (`vm_stat`), in MiB: what a step may grow into."""
    out = subprocess.run(["vm_stat"], stdout=subprocess.PIPE, text=True).stdout
    page = int(re.search(r"page size of (\d+) bytes", out).group(1))
    pages = sum(int(re.search(rf"{kind}:\s+(\d+)", out).group(1)) for kind in ("Pages free", "Pages inactive"))
    return pages * page / 2**20


def pressed() -> bool:
    """Whether macOS reports memory pressure above normal (level 1)."""
    level = subprocess.run(
        ["sysctl", "-n", "kern.memorystatus_vm_pressure_level"], stdout=subprocess.PIPE, text=True
    ).stdout.strip()
    return level.isdigit() and int(level) > 1


async def push(port: int, token: str, repository: str, body: bytes, branch: str) -> tuple[float, str]:
    """One push over its own connection: its wall time to the whole answer, and `ok` or its refusal's words."""
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
        text = answer.decode("latin-1")
        if status != "200":
            words = f"http {status}: {answer[:80]!r}"
        elif "unpack ok" in text and f"ok refs/heads/{branch}" in text:
            words = "ok"
        else:
            found = re.findall(r"(unpack [^\n]*|ng [^\n]*|fatal: [^\n]*)", text)
            words = "report: " + ("; ".join(found)[:120] if found else repr(text[:80]))
    except (OSError, asyncio.IncompleteReadError, ValueError) as error:
        words = f"transport: {type(error).__name__}: {error}"[:120]
    return time.perf_counter() - start, words


async def sample_peak(pid: int, stop: asyncio.Event) -> int:
    peak = 0
    while not stop.is_set():
        peak = max(peak, rss_kib(pid))
        await asyncio.sleep(0.1)
    return max(peak, rss_kib(pid))


def quantile(times: list[float], q: float) -> float:
    ordered = sorted(times)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))] if ordered else float("nan")


async def burst(daemon: Daemon, pushes: list[tuple[str, int, bytes, str]]) -> tuple[list[tuple[int, float, str]], float, int, int]:
    """Sends every push at once: each one's class, time and words; the wall time; the resident set before and at peak."""
    before = rss_kib(daemon.pid)
    stop = asyncio.Event()
    sampler = asyncio.create_task(sample_peak(daemon.pid, stop))
    start = time.perf_counter()
    answers = await asyncio.gather(
        *(push(daemon.port, daemon.token, name, body, branch) for name, _, body, branch in pushes)
    )
    wall = time.perf_counter() - start
    stop.set()
    peak = await sampler
    results = [(size, took, words) for (_, size, _, _), (took, words) in zip(pushes, answers)]
    return results, wall, before, peak


def fresh_daemon(slates: str, label: str) -> Daemon:
    return Daemon(slates, instance=f"w-balance-multi-{os.getpid()}-{label}")


def main():
    args = sys.argv[1:]
    room = derived_max()
    steps = None
    if "--steps" in args:
        at = args.index("--steps")
        steps = [int(n) for n in args[at + 1].split(",")]
        del args[at : at + 2]
    slates, source, *revs = args
    if not revs:
        raise SystemExit(__doc__)
    if steps is None:
        steps = push_load.steps_up_to(room)
    packs = [pack_of(source, rev) for rev in revs]
    print(f"machine: {subprocess.run(['sysctl', '-n', 'machdep.cpu.brand_string'], stdout=subprocess.PIPE, text=True).stdout.strip()}, "
          f"{os.cpu_count()} cores, load {os.getloadavg()}, free {free_mib():,.0f} MiB, open-file room {room}")
    for size, (pack, head, count) in enumerate(packs):
        print(f"class {size}: {revs[size]} -> {head[:12]}, {count:,} objects, {len(pack):,} bytes")

    # Alone: one push of each class on its own daemon, the baseline each class's starvation is judged against, and the
    # first measure of the memory a push of the class holds.
    alone, held_mib = [], []
    for size, (pack, head, _) in enumerate(packs):
        daemon = fresh_daemon(slates, f"alone{size}")
        try:
            daemon.cli("repository", "create", "alone", "--tool", "git", "--default-branch", "main")
            results, _, before, peak = asyncio.run(burst(daemon, [("alone", size, request(head, pack, "p0"), "p0")]))
        finally:
            daemon.stop()
        took, words = results[0][1], results[0][2]
        alone.append(took)
        held_mib.append(max(0.0, (peak - before) / 1024))
        print(f"alone class {size}: {took:.3f} s, {words}, held {held_mib[-1]:.1f} MiB", flush=True)

    for repositories in steps:
        counts = pushes_by_zipf(repositories)
        classes = [class_of(rank, len(packs)) for rank in range(1, repositories + 1)]
        total = sum(counts)
        if total > room:
            print(f"R={repositories}: {total} pushes pass the open-file room {room}; stopped")
            break
        estimate = sum(count * held_mib[size] for count, size in zip(counts, classes))
        free = free_mib()
        if estimate > free:
            print(f"R={repositories}: estimated {estimate:,.0f} MiB past {free:,.0f} MiB free; stopped")
            break
        if pressed():
            print(f"R={repositories}: memory pressure above normal before the step; stopped")
            break
        daemon = fresh_daemon(slates, f"r{repositories}")
        try:
            for rank in range(1, repositories + 1):
                if counts[rank - 1]:
                    daemon.cli("repository", "create", f"r{rank}", "--tool", "git", "--default-branch", "main")
            pushes = []
            for rank, (count, size) in enumerate(zip(counts, classes), start=1):
                pack, head, _ = packs[size]
                for k in range(count):
                    branch = f"p{k}"
                    pushes.append((f"r{rank}", size, request(head, pack, branch), branch))
            results, wall, before, peak = asyncio.run(burst(daemon, pushes))
            alive = rss_kib(daemon.pid) > 0
        finally:
            daemon.stop()
        refused = sum(1 for _, _, words in results if words != "ok")
        print(f"R={repositories}: {total} pushes, wall {wall:.3f} s, rss {before / 1024:.0f} -> peak {peak / 1024:.0f} MiB, "
              f"refused {refused}, daemon {'alive' if alive else 'GONE'}", flush=True)
        for size in range(len(packs)):
            mine = [(took, words) for got, took, words in results if got == size]
            if not mine:
                continue
            times = [took for took, _ in mine]
            kinds: dict[str, int] = {}
            for _, words in mine:
                kinds[words] = kinds.get(words, 0) + 1
            p99 = quantile(times, 0.99)
            print(f"  class {size}: {len(mine)} pushes over {classes.count(size)} repositories, "
                  f"p50 {quantile(times, 0.5):.3f} s, p99 {p99:.3f} s, starvation {p99 / alone[size]:.1f}x alone; "
                  + ", ".join(f"{k} x{v}" for k, v in sorted(kinds.items(), key=lambda kv: -kv[1])), flush=True)
        # Learn each class's memory a push from this step, apportioned by its share of the bytes sent.
        sent = sum(len(packs[size][0]) * count for size, count in zip(classes, counts)) or 1
        grown = max(0.0, (peak - before) / 1024)
        for size in range(len(packs)):
            pushed_of_size = sum(count for c, count in zip(classes, counts) if c == size)
            if pushed_of_size:
                held_mib[size] = max(held_mib[size], grown * len(packs[size][0]) / sent)
        if not alive or refused * 2 > total or pressed():
            print(f"R={repositories}: stopped ({'daemon gone' if not alive else 'refusal storm' if refused * 2 > total else 'memory pressure'})")
            break
    print(f"load at the end {os.getloadavg()}, free {free_mib():,.0f} MiB")


if __name__ == "__main__":
    main()
