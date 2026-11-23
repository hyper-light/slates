"""w-perf 84a (never main): a round's shard parks and client parks, split by how they ended, with kick -> run."""
import sys


def rows(path):
    try:
        return [[int(v) for v in line.rstrip("\n").split("\t")] for line in open(path) if line[:1].isdigit()]
    except OSError:
        return []


def pc(values, q):
    return values[min(len(values) - 1, int(len(values) * q))] if values else 0


base = sys.argv[1]
parks = rows(base + ".parks")
kicked = [r for r in parks if r[2] == 1 and r[4] < 2**63]
band = [r for r in parks if 0.9e6 <= r[1] <= 1.2e6]
k2r = sorted(r[4] for r in kicked)
print(f"  shard parks n {len(parks)}; waited 0.9-1.2 ms {len(band)} (kicked {sum(1 for r in band if r[2] == 1)}); "
      f"kicked n {len(kicked)}: kick->run p50/p99/p999/max us {pc(k2r, .5) / 1e3:.0f} {pc(k2r, .99) / 1e3:.0f} "
      f"{pc(k2r, .999) / 1e3:.0f} {(k2r[-1] if k2r else 0) / 1e3:.0f}; >= 0.9 ms {sum(1 for x in k2r if x >= 0.9e6)}")
client = rows(base + ".cparks")
woken = sorted(r[1] for r in client if r[2] == 1)
print(f"  client parks n {len(client)} woken {len(woken)}: waited p99/p999/max us {pc(woken, .99) / 1e3:.0f} "
      f"{pc(woken, .999) / 1e3:.0f} {(woken[-1] if woken else 0) / 1e3:.0f}")
