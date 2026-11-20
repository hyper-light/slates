"""A-767 item 7, phase 1: changesets stored (a) raw as received plus a work index, against (b) normalized per-path
edits or wholes, content-addressed, one copy per identity across repositories and tools.

Run under Mercurial's own interpreter (so its revlog, mdiff and zstd are at hand, nothing installed):
  PY=$(head -1 "$(command -v hg)" | sed 's/^#!//'); "$PY" -I changeset_ab.py OUT_JSON SPEC...
  SPEC is hg:<repo path>[@<rev range>] or git:<repo path>@<rev>; every SPEC shares one identity store (arm b), so the
  dedup across repositories and tools is the difference between the per-spec sums and the shared total.

Per spec it reports:
  a_payload_*   the bytes one push of the whole history would carry (hg: changegroup v2 uncompressed and zstd; git: a
                full pack), i.e. arm (a)'s log bytes; plus the work index (path, op, identity a touched path)
  b_new_bytes   arm (b): for each touched path, its new text stored once by identity: the smaller of zstd(whole) and
                zstd(delta against p1's text), 0 when the identity is already held (by this or any earlier spec)
  b_alone_bytes arm (b) as if this spec were alone (no identities from earlier specs)
  chain_*       arm (b)'s delta-chain depth a text read needs with no checkpoints (p50, p99, max), the cost of an old
                version's read before checkpoints
  normalize_s   CPU seconds spent hashing, diffing and compressing (Python: an upper bound on the Rust landing)
"""
import hashlib, json, os, subprocess, sys, time, zlib

def compressor():
  try:
    import zstandard
    level3 = zstandard.ZstdCompressor(level=3)
    return 'zstd-3', lambda data: len(level3.compress(data))
  except ImportError:
    pass
  try:
    from mercurial.utils import compression
    engine = compression.compengines['zstd']
    if engine.available():
      comp = engine.revlogcompressor({'level': 3})
      def zstd(data):
        out = comp.compress(data)
        return len(out) if out is not None else len(data)
      return 'zstd-3', zstd
  except Exception:
    pass
  return 'zlib-6', lambda data: len(zlib.compress(data, 6))

COMP_NAME, csize = compressor()

def textdiff(old, new):
  try:
    from mercurial import mdiff
  except ImportError:
    import difflib
    return b''.join(difflib.diff_bytes(difflib.unified_diff, old.splitlines(True), new.splitlines(True), n=0))
  return mdiff.textdiff(old, new)

def ident(data):
  return hashlib.blake2b(data, digest_size=32).digest()

BOUNDS = (16, 64, 256, None)

class Store:
  """Arm (b)'s shared store, once per chain bound K (a text whose chain would pass K is stored whole: a checkpoint).
  Per bound: identity -> depth."""
  def __init__(self):
    self.depth = {k: {} for k in BOUNDS}
  def put(self, data, base, alone):
    """alone: per bound, this spec's own identity -> depth. Returns {bound: (shared, alone, depth)}."""
    key = ident(data)
    whole = csize(data)
    delta = csize(textdiff(base, data)) if base is not None else None
    base_key = ident(base) if base is not None else None
    out = {}
    for k in BOUNDS:
      shared_map, alone_map = self.depth[k], alone[k]
      base_depth = None
      if base_key is not None:
        base_depth = shared_map.get(base_key, alone_map.get(base_key))
      if base_depth is not None and delta is not None and delta < whole and (k is None or base_depth + 1 <= k):
        size, depth = delta, base_depth + 1
      else:
        size, depth = whole, 0
      shared = 0 if key in shared_map else size
      alone_size = 0 if key in alone_map else size
      shared_map.setdefault(key, depth)
      alone_map.setdefault(key, depth)
      out[k] = (shared, alone_size, shared_map[key])
    return out

def new_alone():
  return {k: {} for k in BOUNDS}

def account(stats, depths, result):
  for k, (shared, alone_size, depth) in result.items():
    name = 'inf' if k is None else str(k)
    stats['b_new_bytes_K' + name] = stats.get('b_new_bytes_K' + name, 0) + shared
    stats['b_alone_bytes_K' + name] = stats.get('b_alone_bytes_K' + name, 0) + alone_size
    depths.setdefault(name, []).append(depth)

class Meta:
  """A changeset's own record (message, author, date): per changeset compressed, and as one stream."""
  def __init__(self):
    self.each = 0
    self.stream = []
  def add(self, record):
    self.each += csize(record)
    self.stream.append(record)
  def finish(self, stats):
    stats['b_meta_each_bytes'] = self.each
    stats['b_meta_stream_bytes'] = csize(b''.join(self.stream))

def pct(values, q):
  if not values:
    return 0
  values = sorted(values)
  return values[min(len(values) - 1, int(q * len(values)))]

def finish(stats, depths, index_bytes, cpu0):
  stats['work_index_bytes'] = index_bytes
  for name, values in depths.items():
    stats['chain_p50_K' + name] = pct(values, 0.5)
    stats['chain_p99_K' + name] = pct(values, 0.99)
    stats['chain_max_K' + name] = max(values) if values else 0
  stats['normalize_s'] = round(time.process_time() - cpu0, 2)
  return stats

def run_hg(path, revs, store):
  from mercurial import hg, ui as uimod
  ui = uimod.ui.load()
  repo = hg.repository(ui, path.encode())
  stats = {'spec': 'hg:' + path, 'changesets': 0, 'touched': 0}
  for kind, flag in (('none', 'none-v2'), ('zstd', 'zstd-v2')):
    out = os.path.join(os.environ['AB_SCRATCH'], 'hg-%s.hg' % kind)
    subprocess.run(['hg', '-R', path, 'bundle', '-q', '-a', '-t', flag, out], check=True)
    stats['a_payload_' + kind] = os.path.getsize(out)
    os.unlink(out)
  stats['stock_store_bytes'] = dir_bytes(os.path.join(path, '.hg', 'store'))
  alone, depths, index_bytes, records = new_alone(), {}, 0, Meta()
  cpu0 = time.process_time()
  for rev in (repo.revs(revs.encode()) if revs else range(len(repo))):
    ctx = repo[rev]
    p1 = ctx.p1()
    stats['changesets'] += 1
    records.add(ctx.user() + b'\0' + repr(ctx.date()).encode() + b'\0' + ctx.description() + b'\0' + repr(sorted(ctx.extra().items())).encode())
    for f in ctx.files():
      index_bytes += len(f) + 1 + 32
      stats['touched'] += 1
      if f not in ctx:
        continue
      data = ctx[f].data()
      base = p1[f].data() if f in p1 else None
      account(stats, depths, store.put(data, base, alone))
  records.finish(stats)
  return finish(stats, depths, index_bytes, cpu0)

class Cat:
  def __init__(self, path):
    self.proc = subprocess.Popen(['git', '-C', path, 'cat-file', '--batch'], stdin=subprocess.PIPE,
                                 stdout=subprocess.PIPE)
  def blob(self, oid):
    self.proc.stdin.write(oid.encode() + b'\n')
    self.proc.stdin.flush()
    header = self.proc.stdout.readline().split()
    size = int(header[2])
    data = self.proc.stdout.read(size)
    self.proc.stdout.read(1)
    return data

def run_git(path, rev, store):
  stats = {'spec': 'git:%s@%s' % (path, rev), 'changesets': 0, 'touched': 0}
  pack = subprocess.run(['git', '-C', path, 'pack-objects', '--revs', '--stdout', '-q'], input=(rev + '\n').encode(),
                        stdout=subprocess.PIPE, check=True).stdout
  stats['a_payload_none'] = len(pack)
  stats['a_payload_zstd'] = csize(pack)
  stats['stock_store_bytes'] = len(pack)
  cat = Cat(path)
  alone, depths, index_bytes, records = new_alone(), {}, 0, Meta()
  cpu0 = time.process_time()
  lines = subprocess.run(['git', '-C', path, 'rev-list', '--reverse', '--topo-order', '--parents', rev],
                         stdout=subprocess.PIPE, check=True).stdout.decode().splitlines()
  for line in lines:
    ids = line.split()
    stats['changesets'] += 1
    commit = cat.blob(ids[0])
    records.add(b'\n'.join(l for l in commit.split(b'\n') if not l.startswith((b'tree ', b'parent '))))
    against = [ids[1], ids[0]] if len(ids) > 1 else ['--root', ids[0]]
    raw = subprocess.run(['git', '-C', path, 'diff-tree', '-r', '--no-commit-id', '-z'] + against,
                         stdout=subprocess.PIPE, check=True).stdout.split(b'\0')
    i = 0
    while i + 1 < len(raw):
      meta, name = raw[i], raw[i + 1]
      i += 2
      parts = meta.split()
      if len(parts) < 5:
        continue
      old_mode, new_mode, old, new, status = parts[0], parts[1], parts[2].decode(), parts[3].decode(), parts[4]
      index_bytes += len(name) + 1 + 32
      stats['touched'] += 1
      if status.startswith(b'D') or new_mode.endswith(b'160000') or set(new) == {'0'}:
        continue
      data = cat.blob(new)
      base = cat.blob(old) if set(old) != {'0'} and not old_mode.endswith(b'160000') else None
      account(stats, depths, store.put(data, base, alone))
  records.finish(stats)
  return finish(stats, depths, index_bytes, cpu0)

def dir_bytes(path):
  total = 0
  for root, _, files in os.walk(path):
    for name in files:
      total += os.path.getsize(os.path.join(root, name))
  return total

def main():
  out, specs = sys.argv[1], sys.argv[2:]
  store = Store()
  results = {'compressor': COMP_NAME, 'specs': []}
  for spec in specs:
    kind, rest = spec.split(':', 1)
    path, _, rev = rest.partition('@')
    t0 = time.time()
    stats = run_hg(path, rev, store) if kind == 'hg' else run_git(path, rev or 'HEAD', store)
    stats['wall_s'] = round(time.time() - t0, 1)
    results['specs'].append(stats)
    print(json.dumps(stats), flush=True)
  for k in BOUNDS:
    name = 'inf' if k is None else str(k)
    results['b_shared_total_K' + name] = sum(s['b_new_bytes_K' + name] for s in results['specs'])
    results['b_alone_total_K' + name] = sum(s['b_alone_bytes_K' + name] for s in results['specs'])
  results['b_meta_each_total'] = sum(s['b_meta_each_bytes'] for s in results['specs'])
  results['b_meta_stream_total'] = sum(s['b_meta_stream_bytes'] for s in results['specs'])
  results['a_total_none'] = sum(s['a_payload_none'] for s in results['specs'])
  results['a_total_zstd'] = sum(s['a_payload_zstd'] for s in results['specs'])
  results['work_index_total'] = sum(s['work_index_bytes'] for s in results['specs'])
  with open(out, 'w') as handle:
    json.dump(results, handle, indent=1)
  print(json.dumps({k: v for k, v in results.items() if k != 'specs'}))

main()
