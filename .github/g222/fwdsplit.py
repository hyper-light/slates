"""w-perf 84a (never main): the forwarded requests' served hop under TB, its p99 and p999 tails named by where they waited.

Reads the bench's G222_DIAG ring file (`<base>.ring`): one row a request a shard took, its queued and served ns, and for
a deferred reply whose forward was stamped whole (`complete` 1): for the owner how it met the forward (0 running: it did
not park between the send and the task's start; 1 parked and kicked; 2 parked and woken otherwise), the kick's lateness
after the send, kick -> run, run -> the task's start, then the owner's work (start -> reply sent home), the same four for
the origin and the reply's task, and the origin's reply task to the reply written (the server task's wake and write).
"""
import sys

COLS = ["queued", "served", "landed", "written", "deferred", "complete", "owner_met", "owner_kick_after",
        "owner_kick_run", "owner_run_start", "owner_work", "origin_met", "origin_kick_after", "origin_kick_run",
        "origin_run_home", "home_written"]
AT = {name: at for at, name in enumerate(COLS)}


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


def owner_wait(r):
    return r[AT["owner_kick_after"]] + r[AT["owner_kick_run"]] + r[AT["owner_run_start"]]


def origin_wait(r):
    return r[AT["origin_kick_after"]] + r[AT["origin_kick_run"]] + r[AT["origin_run_home"]]


PARTS = {
    "owner wake (send -> task start)": owner_wait,
    "owner work (start -> reply sent)": lambda r: r[AT["owner_work"]],
    "origin wake (reply sent -> reply task)": origin_wait,
    "origin write (reply task -> written)": lambda r: r[AT["home_written"]],
}

ring = [r for r in rows(sys.argv[1] + ".ring") if len(r) >= len(COLS)]
deferred = sorted((r for r in ring if r[AT["deferred"]] == 1), key=lambda r: r[AT["served"]])
whole = [r for r in deferred if r[AT["complete"]] == 1]
print(f"forwards: deferred n {len(deferred)}, stamped whole {len(whole)}")
if not whole:
    sys.exit(0)
served = [r[AT["served"]] for r in whole]
print(f"  served p50/p99/p999/max us {us(pc(served, .5))} {us(pc(served, .99))} {us(pc(served, .999))} {us(served[-1])}")
for name, part in PARTS.items():
    values = sorted(part(r) for r in whole)
    print(f"  {name} p50/p99/p999 us {us(pc(values, .5))} {us(pc(values, .99))} {us(pc(values, .999))}")
n = len(whole)
for label, tail in (("p99 tail", whole[int(n * .99):]), ("p999 tail", whole[int(n * .999):]),
                    ("body (p50 and below)", whole[: n // 2])):
    if not tail:
        continue
    dominant = {}
    for r in tail:
        name = max(PARTS, key=lambda part: PARTS[part](r))
        dominant[name] = dominant.get(name, 0) + 1
    print(f"  {label}: n {len(tail)}, served med {us(med([r[AT['served']] for r in tail]))} us; dominant {dominant}")
    print("    medians us " + "; ".join(f"{name} {us(med([part(r) for r in tail]))}" for name, part in PARTS.items()))
    for side in ("owner", "origin"):
        met = {0: 0, 1: 0, 2: 0}
        for r in tail:
            met[r[AT[side + "_met"]]] = met.get(r[AT[side + "_met"]], 0) + 1
        kicked = [r for r in tail if r[AT[side + "_met"]] == 1]
        print(f"    {side} met: running {met[0]} kicked {met[1]} woken otherwise {met[2]}; kicked: kick lateness med "
              f"{us(med([r[AT[side + '_kick_after']] for r in kicked]))} kick->run med "
              f"{us(med([r[AT[side + '_kick_run']] for r in kicked]))} max "
              f"{us(max([r[AT[side + '_kick_run']] for r in kicked], default=0))} run->stamp med "
              f"{us(med([r[AT[side + '_run_start' if side == 'owner' else side + '_run_home']] for r in kicked]))} us")
