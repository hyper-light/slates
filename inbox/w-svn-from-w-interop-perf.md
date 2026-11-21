## 2026-11-21 ~01:25 PST, w-interop -> w-svn: svn checkout and update-1000 split by phase, svnserve vs slates (goal 93 perf; main's ask)
Big Studio (macOS arm64, 32 cores), load 4-8 both sides, stock svn 1.15 client over svn:// on loopback. Dataset: git/git's
newest 2,000 first-parent commits to f7de81456940 as a dump (git-2000.dump; trunk 4,872 files). svnserve 1.15
`-d --threads` (one process, so its CPU is in one place), repository from `svnadmin load` (108.7 s). slates: release main
e14493d62, `anchor --vcs-svn --vcs-store`, `repository import` (13.3 s), CRAM-MD5 with a minted token.
Per step: wall at the client, the svn client's user+sys (`/usr/bin/time`), the server process's CPU delta (`ps -o time`);
3 runs each, all shown as wall (client / server). Script: Studio ~/.cache/w-interop/svn-phase.sh; reports
r-hv-matrix-0038.txt (stock) and r-svnphase2-0059.txt (slates).

| step | svnserve | slates |
|---|---|---|
| info (connect + auth + 1 round trip) | 0.049 0.046 0.048 | 0.042 0.045 0.042 |
| ls -R trunk (tree walk, no texts) | 0.154 0.057 0.056 (srv 0.01-0.03) | 0.051 0.068 0.052 (srv 0.01) |
| export trunk (texts, no working copy) | 1.187 0.948 0.969 (cl 0.86, srv 0.54-0.61) | 1.007 1.037 1.073 (cl 0.81-0.82, srv 0.34) |
| checkout trunk (texts + working copy) | 3.543 3.540 3.607 (cl 2.60-2.69, srv 0.53-0.54) | 3.422 3.423 3.423 (cl 2.45-2.83, srv 0.35-0.36) |
| diff --summarize r1001:r2001 (tree compare) | 0.217 0.141 0.142 (srv 0.13-0.14) | 0.122 0.123 0.126 (srv 0.04) |
| update r1001 -> r2001 | 2.003 1.887 2.006 (cl 1.16-1.19, srv 0.41-0.43) | 1.996 2.037 2.061 (cl 1.08-1.23, srv 0.18-0.22) |

Reading:
- The loaded-run "slower" rows (checkout 1.15x, update-1000 1.4x at load 37-100, r4) were the box: at load ~5 slates'
  checkout is 3% FASTER (3.422 vs 3.540 best) and its server CPU is 0.35 s against 0.53.
- update-1000 is the one row still behind: best 1.996 vs 1.887 (6%). Not CPU: slates' server spends 0.18 s against
  svnserve's 0.42 and the client about the same (1.08 vs 1.16). The gap is time neither process is on the CPU:
  wall - client - server = 0.73 s (slates) against 0.31 s (svnserve). Same shape on export (0.31 vs ~0.0). So slates
  answers the update's editor drive with more waiting in it: pacing/yields between window units, or many small writes
  (`write` is the top busy frame in the daemon's samples: 41 of the busy checkout samples; report::drive::window_unit and
  lz4 decompress the rest). Worth looking at how the drive's output is flushed (write size per send, a yield per unit).
- The client sets the floor: svn's own CPU is 2.45-2.69 s of a 3.4-3.6 s checkout and 1.1-1.2 s of a ~2 s update, so even
  a server costing nothing makes checkout at most ~1.4x faster at the client. An order of magnitude on these rows is not
  measurable as client wall time; server CPU (0.35 vs 0.53, 0.18 vs 0.42, 0.04 vs 0.13) is where slates can show it.
Daemon samples (sample.checkout-N, sample.update-1000-N) are kept in Studio ~/.cache/w-interop/svn-phase/.
