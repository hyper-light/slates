"""w-perf 84a (never main): what a round's slowest requests waited on, joined to their taking shard's steps and parks.

Reads the bench's G222_DIAG files: `<base>.ring` (a request's queued and served ns, landed and written monotonic ns,
the taking shard last), `<base>.polls` (every poll past its allowed quantum: shard, start, length, allowed, attribution
code, task label; label names in `# label` lines) and `<base>.parks` (every park: ..., shard, began). For the requests
in the p99 and p999 tails by landed -> written, it splits the queued window (landed -> taken) and the served window
(taken -> written) across what the taking shard was doing then: its long polls (by label and attribution), its parks,
and the rest (short work, or a shard thread off its CPU between steps).
"""
import bisect
import sys

ATTRIBUTION = ["task (long)", "task (blocked)", "host (preempted)", "ambiguous", "off CPU (macOS)", "no window",
               "no clock"]


def rows(path):
    try:
        return [[int(v) for v in line.rstrip("\n").split("\t")] for line in open(path) if line[:1].isdigit()]
    except OSError:
        return []


def labels(path):
    names = {}
    try:
        for line in open(path):
            if line.startswith("# label "):
                _, _, index, name = line.rstrip("\n").split(" ", 3)
                names[int(index)] = name.rsplit("/", 1)[-1]
    except OSError:
        pass
    return names


def med(values):
    values = sorted(values)
    return values[len(values) // 2] if values else 0


def us(ns):
    return f"{ns / 1e3:.0f}"


base = sys.argv[1]
ring = [r for r in rows(base + ".ring") if len(r) >= 6]
names = labels(base + ".polls")
by_shard_polls = {}
for shard, start, length, allowed, attribution, label in rows(base + ".polls"):
    by_shard_polls.setdefault(shard, []).append((start, start + length, length, allowed, attribution, label))
by_shard_parks = {}
for row in rows(base + ".parks"):
    if len(row) >= 7:
        waited, ended, wake_after, shard, began = row[1], row[2], row[4], row[5], row[6]
        by_shard_parks.setdefault(shard, []).append((began, began + waited, ended, wake_after))
by_shard_serves = {}
try:
    for line in open(base + ".polls.serves"):
        if line[:1].isdigit():
            shard, start, length, verb = line.rstrip("\n").split("\t", 3)
            by_shard_serves.setdefault(int(shard), []).append((int(start), int(start) + int(length), int(length), verb))
except OSError:
    pass
for table in (by_shard_polls, by_shard_parks, by_shard_serves):
    for shard in table:
        table[shard].sort()


def overlap(intervals, lo, hi):
    """The intervals of a shard's sorted list that overlap [lo, hi), each with its overlap ns."""
    starts = [interval[0] for interval in intervals]
    at = max(0, bisect.bisect_left(starts, lo) - 64)
    found = []
    for interval in intervals[at:]:
        if interval[0] >= hi:
            break
        part = min(hi, interval[1]) - max(lo, interval[0])
        if part > 0:
            found.append((part, interval))
    return found


def split(r, window):
    queued, served, landed, written, shard = r[0], r[1], r[2], r[3], r[-1]
    taken = landed + queued
    lo, hi = (landed, taken) if window == "queued" else (taken, written)
    polls = overlap(by_shard_polls.get(shard, []), lo, hi)
    parks = overlap(by_shard_parks.get(shard, []), lo, hi)
    in_polls = sum(part for part, _ in polls)
    in_parks = sum(part for part, _ in parks)
    dominant = max(polls, key=lambda item: item[0], default=None)
    rest = max(0, (hi - lo) - in_polls - in_parks)
    return hi - lo, in_polls, in_parks, rest, dominant, parks


timed = set()
try:
    for line in open(base + ".timed"):
        if line[:1].isdigit():
            client, edit, submit = (int(v) for v in line.split("\t")[:3])
            timed.add((client, edit))
            timed.add((client, submit))
except OSError:
    pass
# Only the bench's timed requests (its edits and submits), when the bench says which: its untimed housekeeping
# between runs (destroys, creates, span drains) is not what its percentiles measure.
ranked = [r for r in ring if (r[-3], r[-2]) in timed] if timed else ring
print(f"timed requests {len(ranked)} of {len(ring)} rows")
total = sorted(ranked, key=lambda r: r[0] + r[1])
n = len(total)
all_serves = [serve for serves in by_shard_serves.values() for serve in serves]
serve_verbs = {}
for serve in all_serves:
    seen, spent = serve_verbs.get(serve[3], (0, 0))
    serve_verbs[serve[3]] = (seen + 1, spent + serve[2])
print("long serves (>= 200 us) by verb: " + "; ".join(f"{verb} n {count} total {us(total)} us max "
      f"{us(max(s[2] for s in all_serves if s[3] == verb))} us" for verb, (count, total) in
      sorted(serve_verbs.items(), key=lambda item: -item[1][1])[:8]))
print(f"tailjoin: requests n {n}; long polls {sum(len(v) for v in by_shard_polls.values())}; "
      f"parks {sum(len(v) for v in by_shard_parks.values())}")
for label, low in (("p99 tail", .99), ("p999 tail", .999)):
    tail = total[int(n * low):]
    if not tail:
        continue
    print(f" {label}: n {len(tail)}, landed -> written med {us(med([r[0] + r[1] for r in tail]))} us")
    for window in ("queued", "served"):
        parts = [split(r, window) for r in tail]
        causes = {}
        for whole, in_polls, in_parks, rest, dominant, parks in parts:
            if whole == 0:
                continue
            biggest = max((in_polls, "long poll"), (in_parks, "park"), (rest, "short work"))[1]
            if biggest == "long poll" and dominant:
                _, (_, _, length, allowed, attribution, task) = dominant
                biggest = (f"long poll: {names.get(task, task)} ({ATTRIBUTION[attribution] if attribution < 7 else attribution}, "
                           f"{us(length)} us of {us(allowed)} allowed)")
            elif biggest == "park":
                late = [park for _, park in parks if park[2] == 1]
                biggest = f"park ({'kicked' if late else 'timer or other'})"
            causes[biggest] = causes.get(biggest, 0) + 1
        print(f"   {window}: med {us(med([p[0] for p in parts]))} us, in long polls {us(med([p[1] for p in parts]))}, "
              f"in parks {us(med([p[2] for p in parts]))}, short work {us(med([p[3] for p in parts]))}")
        for cause, count in sorted(causes.items(), key=lambda item: -item[1])[:6]:
            print(f"     {count:4} {cause}")
        if window == "queued":
            # A request that landed while its shard was parked: how that park ended, and how long after the landing.
            met = {"kicked": [], "timer or other": [], "untimed": [], "found pending": [], "not parked": []}
            for r in tail:
                landed, shard = r[2], r[-1]
                holding = [park for _, park in overlap(by_shard_parks.get(shard, []), landed, landed + 1)]
                if not holding:
                    met["not parked"].append(0)
                    continue
                park = holding[0]
                name = {1: "kicked", 0: "timer or other", 2: "untimed", 3: "found pending"}.get(park[2], "timer or other")
                met[name].append(park[1] - landed)
            print("     shard at landing: " + "; ".join(f"{name} {len(waits)} (then ran after med {us(med(waits))} us)"
                                                   for name, waits in met.items() if waits))
        verbs = {}
        for r in tail:
            queued, served, landed, written, shard = r[0], r[1], r[2], r[3], r[-1]
            lo, hi = (landed, landed + queued) if window == "queued" else (landed + queued, written)
            for part, serve in overlap(by_shard_serves.get(shard, []), lo, hi):
                verbs[serve[3]] = verbs.get(serve[3], 0) + part
        if verbs:
            print("     long serves overlapping, by time: " + "; ".join(
                f"{verb} {us(ns)} us" for verb, ns in sorted(verbs.items(), key=lambda item: -item[1])[:6]))
