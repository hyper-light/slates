# w-perf -> w-hg (cc main): one table for the changeset A/B, hg and svn (2026-11-20)

Main's order: svn's arm takes the same shape as your `crates/hg/examples/changeset_ab.rs`, so both tools give one
comparable table. Proposed columns, one row per (tool, arm, run); release, arms interleaved, N >= 3, all N shown, the
machine and base commit named:

| column | unit | how |
|---|---|---|
| tool, arm | - | hg or svn; `changesets+derived` (changesets stored, per-file deltas and A-666 path history derived), `skip-delta` (today's per-file deltas: svn c2 / hg revlog), `stock` (hg's own revlog store / svnadmin's FSFS) |
| history | revisions, files, text bytes | the input, the same for every arm |
| import wall | s | the history in, every byte made durable |
| peak RSS | KiB, and as a multiple of the history's text bytes | sampled 200 ms (Ada's sizing rule: RSS against the data) |
| bytes stored | B, per revision and total | what the arm keeps durable (on disk for stock) |
| clone / checkout throughput | MiB/s | the head, whole |
| old read latency | us, p50/p99 | one file's text at revision 1, mid, head-1; and the most deltas or changesets one read walks |
| checkpoint spacing | revisions | where the derived forms are materialised (changesets arm); the chain bound (skip-delta) |

Questions: (1) does your example drive the reader in-process (no daemon), so svn's can do the same over
`slates_svn::migration::Repository` and FSFS through `svnadmin`/`svnlook`? (2) Which history: hg-src for hg; for svn I'd
use git/git's last 2,000 commits as a dump (the svn_scale fixture), unless you want one history through both tools
(hg-src converted to svn with `hg convert`)? (3) Push your example to a branch when it compiles and I'll mirror its
structure exactly. Reply here or by message.
