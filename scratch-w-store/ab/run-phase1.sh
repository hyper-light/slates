set -e
command -v uv >/dev/null || { echo NO_UV; exit 3; }
D=~/.cache/w-store-ab; mkdir -p $D/scripts; cd $D
cat > $D/scripts/changeset_ab.py <<'EOF_AB'
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

class Store:
  """Arm (b)'s shared store: identity -> chain depth (0 for a whole)."""
  def __init__(self):
    self.depth = {}
  def put(self, data, base, alone):
    """Returns (bytes stored shared, bytes stored alone, depth)."""
    key = ident(data)
    whole = csize(data)
    delta = None
    base_depth = None
    if base is not None:
      base_key = ident(base)
      base_depth = self.depth.get(base_key, alone.get(base_key))
      if base_depth is not None:
        delta = csize(textdiff(base, data))
    if delta is not None and delta < whole:
      size, depth = delta, base_depth + 1
    else:
      size, depth = whole, 0
    shared = 0 if key in self.depth else size
    alone_size = 0 if key in alone else size
    self.depth.setdefault(key, depth)
    alone.setdefault(key, depth)
    return shared, alone_size, self.depth[key]

def pct(values, q):
  if not values:
    return 0
  values = sorted(values)
  return values[min(len(values) - 1, int(q * len(values)))]

def finish(stats, depths, index_bytes, cpu0):
  stats['work_index_bytes'] = index_bytes
  stats['chain_p50'] = pct(depths, 0.5)
  stats['chain_p99'] = pct(depths, 0.99)
  stats['chain_max'] = max(depths) if depths else 0
  stats['normalize_s'] = round(time.process_time() - cpu0, 2)
  return stats

def run_hg(path, revs, store):
  from mercurial import hg, ui as uimod
  ui = uimod.ui.load()
  repo = hg.repository(ui, path.encode())
  stats = {'spec': 'hg:' + path, 'changesets': 0, 'touched': 0, 'b_new_bytes': 0, 'b_alone_bytes': 0}
  for kind, flag in (('none', 'none-v2'), ('zstd', 'zstd-v2')):
    out = os.path.join(os.environ['AB_SCRATCH'], 'hg-%s.hg' % kind)
    subprocess.run(['hg', '-R', path, 'bundle', '-q', '-a', '-t', flag, out], check=True)
    stats['a_payload_' + kind] = os.path.getsize(out)
    os.unlink(out)
  stats['stock_store_bytes'] = dir_bytes(os.path.join(path, '.hg', 'store'))
  alone, depths, index_bytes = {}, [], 0
  cpu0 = time.process_time()
  for rev in repo.revs(revs.encode() if revs else b'all()'):
    ctx = repo[rev]
    p1 = ctx.p1()
    stats['changesets'] += 1
    for f in ctx.files():
      index_bytes += len(f) + 1 + 32
      stats['touched'] += 1
      if f not in ctx:
        continue
      data = ctx[f].data()
      base = p1[f].data() if f in p1 else None
      shared, alone_size, depth = store.put(data, base, alone)
      stats['b_new_bytes'] += shared
      stats['b_alone_bytes'] += alone_size
      depths.append(depth)
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
  stats = {'spec': 'git:%s@%s' % (path, rev), 'changesets': 0, 'touched': 0, 'b_new_bytes': 0, 'b_alone_bytes': 0}
  pack = subprocess.run(['git', '-C', path, 'pack-objects', '--revs', '--stdout', '-q'], input=(rev + '\n').encode(),
                        stdout=subprocess.PIPE, check=True).stdout
  stats['a_payload_none'] = len(pack)
  stats['a_payload_zstd'] = csize(pack)
  stats['stock_store_bytes'] = len(pack)
  cat = Cat(path)
  alone, depths, index_bytes = {}, [], 0
  cpu0 = time.process_time()
  lines = subprocess.run(['git', '-C', path, 'rev-list', '--reverse', '--topo-order', '--parents', rev],
                         stdout=subprocess.PIPE, check=True).stdout.decode().splitlines()
  for line in lines:
    ids = line.split()
    stats['changesets'] += 1
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
      shared, alone_size, depth = store.put(data, base, alone)
      stats['b_new_bytes'] += shared
      stats['b_alone_bytes'] += alone_size
      depths.append(depth)
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
  results['b_shared_total'] = sum(s['b_new_bytes'] for s in results['specs'])
  results['b_alone_total'] = sum(s['b_alone_bytes'] for s in results['specs'])
  results['a_total_none'] = sum(s['a_payload_none'] for s in results['specs'])
  results['a_total_zstd'] = sum(s['a_payload_zstd'] for s in results['specs'])
  results['work_index_total'] = sum(s['work_index_bytes'] for s in results['specs'])
  with open(out, 'w') as handle:
    json.dump(results, handle, indent=1)
  print(json.dumps({k: v for k, v in results.items() if k != 'specs'}))

main()

EOF_AB
export HGPLAIN=1 HGRCPATH= AB_SCRATCH=$D
HG="uvx --quiet --python 3.12 --from mercurial==7.2 hg"
if [ ! -d $D/hg-as-git.git ]; then
  git init -q --bare $D/hg-as-git.git
  $HG -R ~/.cache/w-interop/hg-src fastexport | git -C $D/hg-as-git.git fast-import --quiet
fi
REF=$(for r in $(git -C $D/hg-as-git.git for-each-ref --format='%(refname)' refs/heads); do echo "$(git -C $D/hg-as-git.git rev-list --count $r) $r"; done | sort -rn | head -1 | cut -d' ' -f2)
echo "hg-as-git ref $REF, commits $(git -C $D/hg-as-git.git rev-list --count $REF)"
mkdir -p $D/bin; printf '#!/bin/sh\nexec uvx --quiet --python 3.12 --from mercurial==7.2 hg "$@"\n' > $D/bin/hg; chmod +x $D/bin/hg
export PATH=$D/bin:$PATH
/usr/bin/time -l uv run --quiet --python 3.12 --with mercurial==7.2 python -I $D/scripts/changeset_ab.py $D/ab-phase1.json \
  hg:$HOME/.cache/w-interop/hg-src "git:$D/hg-as-git.git@$REF" git:$HOME/.cache/w-interop/git-src@v2.0.0 git:$HOME/.cache/w-interop/git-src@v2.1.0 2>&1 | tail -30
