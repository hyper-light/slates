#!/usr/bin/env python3
"""A-783's A/B: paced work that yields only when someone waits, against today's yield at every quantum (main's
condition (d), 2026-11-20 15:35). `docs/wip/BENCHMARKS.md` records each run; the command is the contract.

What it measures, per side and round, against a fresh daemon each time:
- pushes: K concurrent pushes of SOURCE's REV (push_load.py's request) into K fresh repositories; the wall time of the
  whole step and each push's time to its whole answer (p50, p99);
- newcomers: while the pushes run, a ref advertisement (`GET /idle/info/refs?service=git-upload-pack`) of an idle
  repository, one after another with a pause of a probe's own length between them, each timed from the request's send
  to the answer's first byte: the cost a paced step puts on someone arriving at its shard;
- the runtime's own counters over the step, read from `slates status` before and after (the rows A-783's A/B adds to
  both sides): steps, polls, polls past their allowance, and the polls' length by powers of two, from which p99 and
  p999 (each the bucket's upper bound); the longest poll is the shard's since its start (a maximum has no difference),
  so it is the boot's on both sides unless a push passed it;
- the push build's own turns (`push.build.turns`, both sides) and, on B, the quanta its turns took past their first
  and the turns that yielded to someone waiting;
- provisioning: `provision_bench`'s spinning single-client p99 (R9), once a side a round.

The rounds alternate the sides' order (A B, B A, ...), every round shown. "load" runs the same rounds with as many CPU
burners as the machine has cores (`yes > /dev/null`, owned and killed here).

Usage: pacing_ab.py A_SLATES A_BENCH B_SLATES B_BENCH SOURCE REV [--rounds N] [--pushes K] [--load idle|cores|both]
"""

import asyncio
import os
import re
import subprocess
import sys
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from push_load import Daemon, push, receive_pack_request  # noqa: E402


async def first_byte(port: int, token: str, repository: str) -> float:
    """One ref advertisement: seconds from its send to the answer's first byte, the rest read and dropped."""
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    request = (
        f"GET /{repository}/info/refs?service=git-upload-pack HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
        f"Authorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    ).encode()
    start = time.perf_counter()
    writer.write(request)
    await writer.drain()
    await reader.read(1)
    took = time.perf_counter() - start
    await reader.read()
    writer.close()
    return took


async def newcomers(port: int, token: str, done: asyncio.Event) -> list[float]:
    times: list[float] = []
    while not done.is_set():
        took = await first_byte(port, token, "idle")
        times.append(took)
        await asyncio.sleep(took)
    return times


ROW = re.compile(r"^shard (\d+) refused ((?:rt|push\.build)\.[\w.]+): (\d+)$")


def counters(daemon: Daemon) -> dict[str, int]:
    """The rt and push.build rows over every shard: summed, the longest the most."""
    out = daemon.cli("status").stdout
    total: dict[str, int] = {}
    for line in out.splitlines():
        if m := ROW.match(line):
            kind, count = m.group(2), int(m.group(3))
            if kind == "rt.longest_step_ns":
                total[kind] = max(total.get(kind, 0), count)
            else:
                total[kind] = total.get(kind, 0) + count
    return total


def quantile_ns(buckets: dict[int, int], q: float) -> int:
    """The upper bound of the bucket the q-th poll falls in."""
    total = sum(buckets.values())
    if total == 0:
        return 0
    seen = 0
    for bucket in sorted(buckets):
        seen += buckets[bucket]
        if seen >= q * total:
            return 1 << bucket
    return 1 << max(buckets)


def pick(values: list[float], q: float) -> float:
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(q * len(ordered)))] if ordered else float("nan")


async def push_step(daemon: Daemon, names: list[str], body: bytes) -> tuple[float, list, list[float]]:
    done = asyncio.Event()
    prober = asyncio.create_task(newcomers(daemon.port, daemon.token, done))
    start = time.perf_counter()
    results = await asyncio.gather(*(push(daemon.port, daemon.token, name, body) for name in names))
    wall = time.perf_counter() - start
    done.set()
    return wall, results, await prober


def one_side(slates: str, body: bytes, idle_body: bytes, pushes: int, label: str) -> dict:
    daemon = Daemon(slates, f"pacing-ab-{os.getpid()}-{label}")
    try:
        daemon.cli("repository", "create", "idle", "--tool", "git", "--default-branch", "main")
        asyncio.run(push(daemon.port, daemon.token, "idle", idle_body))
        names = [f"push-{i}" for i in range(pushes)]
        for name in names:
            daemon.cli("repository", "create", name, "--tool", "git", "--default-branch", "main")
        before = counters(daemon)
        wall, results, probes = asyncio.run(push_step(daemon, names, body))
        after = counters(daemon)
    finally:
        daemon.stop()
    delta = {kind: after.get(kind, 0) - before.get(kind, 0) for kind in after}
    delta["rt.longest_step_ns"] = after.get("rt.longest_step_ns", 0)
    buckets = {
        int(kind.rsplit(".", 1)[1]): count
        for kind, count in delta.items()
        if kind.startswith("rt.poll_log2.") and count > 0
    }
    times = [took for took, _ in results]
    return {
        "ok": sum(1 for _, words in results if words == "ok"),
        "wall": wall,
        "push_p50": pick(times, 0.5),
        "push_p99": pick(times, 0.99),
        "probes": len(probes),
        "new_p50": pick(probes, 0.5),
        "new_p99": pick(probes, 0.99),
        "new_max": max(probes) if probes else float("nan"),
        "steps": delta.get("rt.steps", 0),
        "polls": delta.get("rt.polls", 0),
        "long": delta.get("rt.long_steps", 0),
        "preempted": delta.get("rt.preempted_steps", 0),
        "unattributed": delta.get("rt.unattributed_steps", 0),
        "longest_us": delta["rt.longest_step_ns"] / 1000,
        "poll_p99_us": quantile_ns(buckets, 0.99) / 1000,
        "poll_p999_us": quantile_ns(buckets, 0.999) / 1000,
        "turns": delta.get("push.build.turns", 0),
        "quanta": delta.get("push.build.more_quanta", 0),
        "yielded": delta.get("push.build.yielded", 0),
    }


# The longest the provisioning bench may take to print its single-client spinning rows before it is called hung: its
# whole first form took seconds on this Studio, and a hung client waits on its reply forever.
PROVISION_BOUND_S = 600


def provision_p99_us(bench: str) -> str:
    """R9's gated row, the single-client spinning p99 (median of the bench's runs), read as it prints. The bench is
    stopped once the row is read: on main at 779840fba both sides' later forms hung (a client's round trip unanswered,
    every shard parked; 2026-11-20, reported to main), and the row R9 gates comes first."""
    bench_process = subprocess.Popen([bench], stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    found = "?(no row: hung or ended)"
    bound = threading.Timer(PROVISION_BOUND_S, bench_process.kill)
    bound.start()
    try:
        for line in bench_process.stdout:
            fields = line.rstrip("\n").split("\t")
            if len(fields) == 5 and fields[1] == "provision.spinning_1_p99":
                found = f"{int(fields[3]) / 1000:.1f}"
                break
    finally:
        bound.cancel()
        bench_process.kill()
        bench_process.wait()
    return found


def show(label: str, side: str, r: dict, provision: str):
    print(
        f"{label} {side}: ok {r['ok']}, wall {r['wall']:.3f} s, push p50 {r['push_p50']:.3f} p99 {r['push_p99']:.3f} s; "
        f"newcomer n={r['probes']} p50 {r['new_p50'] * 1e3:.2f} p99 {r['new_p99'] * 1e3:.2f} max {r['new_max'] * 1e3:.2f} ms; "
        f"steps {r['steps']:,} polls {r['polls']:,} long {r['long']} preempted {r['preempted']} unattributed "
        f"{r['unattributed']}; longest {r['longest_us']:.0f} us, poll p99 <={r['poll_p99_us']:.0f} p999 "
        f"<={r['poll_p999_us']:.0f} us; build turns {r['turns']:,}, more quanta {r['quanta']:,} yielded {r['yielded']:,}; provision p99 {provision} us",
        flush=True,
    )


def main():
    args = sys.argv[1:]
    options = {"--rounds": "3", "--pushes": "8", "--load": "both"}
    for name in list(options):
        if name in args:
            at = args.index(name)
            options[name] = args[at + 1]
            del args[at : at + 2]
    a_slates, a_bench, b_slates, b_bench, source, rev = args
    body, head, objects = receive_pack_request(source, rev)
    idle_body, _, _ = receive_pack_request(source, "v0.99^{commit}")
    cores = os.cpu_count() or 1
    print(f"request: {len(body):,} bytes, {objects:,} objects ({rev}); {options['--pushes']} pushes a round; "
          f"{cores} cores; load {os.getloadavg()}")
    sides = {"A": (a_slates, a_bench), "B": (b_slates, b_bench)}
    loads = ["idle", "cores"] if options["--load"] == "both" else [options["--load"]]
    for load in loads:
        burners = []
        if load == "cores":
            burners = [subprocess.Popen(["yes"], stdout=subprocess.DEVNULL) for _ in range(cores)]
        try:
            for number in range(int(options["--rounds"])):
                order = ["A", "B"] if number % 2 == 0 else ["B", "A"]
                for side in order:
                    slates, bench = sides[side]
                    result = one_side(slates, body, idle_body, int(options["--pushes"]), f"{load}{number}{side}")
                    show(f"{load} round {number + 1}", side, result, provision_p99_us(bench))
        finally:
            for burner in burners:
                burner.kill()
                burner.wait()
    print(f"load at the end {os.getloadavg()}")


if __name__ == "__main__":
    main()
