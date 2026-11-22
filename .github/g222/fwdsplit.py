"""w-perf 84a (never main): the forwarded verdict's hops under TB, its p99 and p999 tails named by hop and by wake.

Reads the bench's G222_DIAG file (one row a forwarded submit: eight hops in ns, then for the owner and the origin how it
met its message: 0 running, 1 parked and kicked, 2 parked and woken otherwise; the kick's lateness after the send; the
kick to running; running to the stamp).
"""
import sys

HOPS = ["park_staged", "staged_asked", "asked_sending", "sending_owner_in", "owner_judge", "owner_back",
        "back_resuming", "resuming_replied"]


def rows(path):
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return []
    return [[int(v) for v in line.split("\t")] for line in lines if line and line[0].isdigit()]


def pc(values, p):
    return values[min(len(values) - 1, int(len(values) * p))] if values else 0


def med(values):
    return sorted(values)[len(values) // 2] if values else 0


def us(ns):
    return f"{ns / 1e3:.0f}"


samples = [r for r in rows(sys.argv[1]) if len(r) >= 16]
if not samples:
    print("forwards n 0")
    sys.exit(0)
samples.sort(key=lambda r: sum(r[:8]))
totals = [sum(r[:8]) for r in samples]
n = len(samples)
print(f"forwards n {n} total p50/p99/p999/max us {us(pc(totals, .5))} {us(pc(totals, .99))} {us(pc(totals, .999))} "
      f"{us(totals[-1])}")
for hop, name in enumerate(HOPS):
    values = sorted(r[hop] for r in samples)
    print(f"  {name} p50/p99/p999 us {us(pc(values, .5))} {us(pc(values, .99))} {us(pc(values, .999))}")
for label, low in (("p99 tail", .99), ("p999 tail", .999), ("body (p50 and below)", None)):
    tail = samples[int(n * low):] if low is not None else samples[: n // 2]
    if not tail:
        continue
    dominant = {}
    for r in tail:
        name = HOPS[max(range(8), key=lambda hop: r[hop])]
        dominant[name] = dominant.get(name, 0) + 1
    print(f"  {label}: n {len(tail)}; dominant hop {dominant}")
    print("    hop medians us " + " ".join(f"{name}={us(med([r[h] for r in tail]))}" for h, name in enumerate(HOPS)))
    for side, at in (("owner", 8), ("origin", 12)):
        met = {0: 0, 1: 0, 2: 0}
        for r in tail:
            met[r[at]] = met.get(r[at], 0) + 1
        kicked = [r for r in tail if r[at] == 1]
        print(f"    {side} met: running {met[0]} kicked {met[1]} woken otherwise {met[2]}; kicked: kick after send "
              f"med {us(med([r[at + 1] for r in kicked]))} kick->run med {us(med([r[at + 2] for r in kicked]))} "
              f"max {us(max([r[at + 2] for r in kicked], default=0))} run->stamp med "
              f"{us(med([r[at + 3] for r in kicked]))}; running: send->stamp med "
              f"{us(med([r[at + 3] for r in tail if r[at] == 0]))} us")
