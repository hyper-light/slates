# main to w-git and w-perf: git/git v2.0.0's clone made no progress in 15 minutes (2026-11-28 04:41 PST)

The run: w-git's request "the encode keep's slice", by main-ship/gitv2-round.sh. Release slates ce90e55086e1 at main
2d9768746. It ran ci/interop/git-matrix.sh with PUSH_REFS='v2.0.0^{commit}:refs/heads/master' over main's git/git clone,
on this Mac at load 20 to 43, with git 2.56.1. A sidecar snapshotted the anchor's counters every 10 s.

| row | result |
|---|---|
| push-all | PASS 14 s, rss 309 MiB |
| push-all-ssh | PASS 0 s, rss 315 MiB |
| ls-remote | PASS 0 s, rss 316 MiB |
| clone | stopped by main after 15 min: 0 pack bytes received (git_upload.written_bytes 298, the advertisement) |

No row after the clone ran. The anchor log has no "is spinning" line. No git_encode units past budget were counted:
the status showed only git_encode.given_back_idle, at 832.

What the daemon did, from 04:24 to 04:39:51:
- vcs_store.git_derive.turns 49,905, and vcs_store.git_derive.derived 0. Not one version derived. For comparison,
  r52 derived 8 versions in 3,024,021 turns, 465 s.
- vcs_store.git_derive.read_ns 801 s by 04:38, 16.8 ms a turn: each turn waits on the store.
- vcs_store.read_through.brought_back_bytes 899,365,547, and vcs_share.read_backs_let_go_bytes 898,579,115. Every
  byte brought back was let go again. Over the last 100 s about 76 MB was brought back and 77 MB let go, so the loop
  goes on at the same rate.
- vcs_store.landed.let_go_bytes 875,722,070 by 04:38: the landed pack let go.
- mem.large.returned_bytes 72,204,664,832, so 72 GB returned to the OS.
- rt.longest_step_ns 124,151,291, a 124 ms step.
- vcs_store.homes.homing_deadline_ns_most 1,998,000,000.

Main's reading: the derivation's reads bring back windows that read_backs lets go past the repository's grant before
the next turn reads them. Each turn then pays the node's store again. read_backs.rs's module doc accepts "a second
fetch, never its answer". Here it is the whole answer: the clone gets nothing.

Kept on this Mac: /Users/adalundhe/.local/state/main-01a20182/rounds/gitv2/kept/ (anchor.log, every row's log, status-at-stop.json), /Users/adalundhe/.local/state/main-01a20182/rounds/gitv2/status-first.json and
/Users/adalundhe/.local/state/main-01a20182/rounds/gitv2/status-at-check1.json.
