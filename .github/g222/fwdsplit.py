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
        "origin_run_home", "home_written", "placement", "resolve_met", "resolve_kick_after", "resolve_kick_run",
        "resolve_run_at", "mask", "deliver_file", "deliver_line"]
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


FORWARD = {
    "owner wake (send -> task start)": owner_wait,
    "owner work (start -> reply sent)": lambda r: r[AT["owner_work"]],
    "origin wake (reply sent -> reply task)": origin_wait,
    "origin write (reply task -> written)": lambda r: r[AT["home_written"]],
}
ACCEPTED = {
    "owner wake (send -> task start)": owner_wait,
    "owner work (start -> acceptance wait)": lambda r: r[AT["owner_work"]],
    "placement (wait -> resolved)": lambda r: r[AT["placement"]],
    "origin wake (resolved -> reply task)": origin_wait,
    "origin write (reply task -> written)": lambda r: r[AT["home_written"]],
}
SIDES = {"owner": ("owner_met", "owner_kick_after", "owner_kick_run", "owner_run_start"),
         "origin": ("origin_met", "origin_kick_after", "origin_kick_run", "origin_run_home"),
         "owner at resolution": ("resolve_met", "resolve_kick_after", "resolve_kick_run", "resolve_run_at")}

ring = [r for r in rows(sys.argv[1] + ".ring") if len(r) >= len(COLS)]
deferred = [r for r in ring if r[AT["deferred"]] == 1]
print(f"deferred n {len(deferred)}: forwards {sum(1 for r in deferred if r[AT['complete']] == 1)}, accepted submits "
      f"{sum(1 for r in deferred if r[AT['complete']] == 2)}, unstamped {sum(1 for r in deferred if r[AT['complete']] == 0)}")
files = {}
try:
    for line in open(sys.argv[1] + ".ring"):
        if line.startswith("# deliver file "):
            _, _, _, hashed, name = line.split(maxsplit=4)
            files[int(hashed)] = name.strip().rsplit("/", 1)[-1]
except OSError:
    pass
groups = {}
for r in deferred:
    key = (r[AT["complete"]], r[AT["mask"]], files.get(r[AT["deliver_file"]], "-"), r[AT["deliver_line"]])
    groups.setdefault(key, []).append(r[AT["served"]])
print(" by kind, stamps made (sent 1 started 2 replied 4 home 8 waiting 16 resolved 32 delivered 64) and deliver site:")
for key, served in sorted(groups.items(), key=lambda item: -len(item[1])):
    served.sort()
    print(f"   complete {key[0]} mask {key[1]:3} deliver {key[2]}:{key[3]}: n {len(served)} served p50/p99/p999/max us "
          f"{us(pc(served, .5))} {us(pc(served, .99))} {us(pc(served, .999))} {us(served[-1])}")
for kind, flag, parts in (("forwards", 1, FORWARD), ("accepted submits", 2, ACCEPTED)):
    whole = sorted((r for r in deferred if r[AT["complete"]] == flag), key=lambda r: r[AT["served"]])
    if not whole:
        continue
    served = [r[AT["served"]] for r in whole]
    print(f" {kind}: n {len(whole)} served p50/p99/p999/max us {us(pc(served, .5))} {us(pc(served, .99))} "
          f"{us(pc(served, .999))} {us(served[-1])}")
    for name, part in parts.items():
        values = sorted(part(r) for r in whole)
        print(f"   {name} p50/p99/p999 us {us(pc(values, .5))} {us(pc(values, .99))} {us(pc(values, .999))}")
    n = len(whole)
    for label, tail in (("p99 tail", whole[int(n * .99):]), ("p999 tail", whole[int(n * .999):]),
                        ("body (p50 and below)", whole[: n // 2])):
        if not tail:
            continue
        dominant = {}
        for r in tail:
            name = max(parts, key=lambda part: parts[part](r))
            dominant[name] = dominant.get(name, 0) + 1
        print(f"   {label}: n {len(tail)}, served med {us(med([r[AT['served']] for r in tail]))} us; dominant {dominant}")
        print("     medians us " + "; ".join(f"{name} {us(med([part(r) for r in tail]))}" for name, part in parts.items()))
        for side, (met_at, late_at, run_at, after_at) in SIDES.items():
            if side == "owner at resolution" and flag != 2:
                continue
            met = {0: 0, 1: 0, 2: 0}
            for r in tail:
                met[r[AT[met_at]]] = met.get(r[AT[met_at]], 0) + 1
            kicked = [r for r in tail if r[AT[met_at]] == 1]
            print(f"     {side} met: running {met[0]} kicked {met[1]} woken otherwise {met[2]}; kicked: kick lateness med "
                  f"{us(med([r[AT[late_at]] for r in kicked]))} kick->run med {us(med([r[AT[run_at]] for r in kicked]))} "
                  f"max {us(max([r[AT[run_at]] for r in kicked], default=0))} run->stamp med "
                  f"{us(med([r[AT[after_at]] for r in kicked]))} us")
