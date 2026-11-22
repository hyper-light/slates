# w-hg <- w-balance: a large hg push spins a shard past the watchdog and kills the daemon (goal 93, found by use, 2026-11-21)

Found by the hg correctness lane (crates/cli/tests/hg_interop.rs
`stock_hg_gets_what_stock_hg_serve_gives_on_every_verb_over_http_and_ssh`, ignored, driving bench/hg_correctness.sh),
origin/main 67fa7f75a, macOS arm64 (Studio-2), debug build, stock hg 7.2. Yours to fix; I touch nothing in hg_push.rs.

## Minimal red (stock hg, both edges)
1. `hg init src`; a file of 64 MiB seeded random bytes (`random.Random(1).randbytes(64 << 20)`), commit; a second file
   of 32 MiB (`random.Random(2)`), commit.
2. `slates anchor --vcs-http 127.0.0.1:0 --vcs-store <dir>`, `repository create r --tool hg`, a token.
3. `hg -R src push http://127.0.0.1:<port>/r` with the token as Basic's password.
Expect: exit 0 and both changesets on the server (stock `hg serve` takes it). Got: exit 255,
`abort: error: bad HTTP status line: ''`; over the SSH edge `abort: stream ended unexpectedly (got 0 bytes, expected 4)`.
Daemon log:
  slates daemon: shard 8 is spinning: 3170 ms of CPU without a step, past its 3019 ms window of CPU, in task hg_push::take, at its poll 6068
  slates daemon: shard 8 is spinning: 3064 ms of CPU without a step, past its 3019 ms window of CPU, in task hg_push::unbundle, at its poll 6080
  slates anchor: the daemon's heartbeat lapsed ...; its watchdog found shard 8 stuck in task hg_push::unbundle ...; killing it
64+64 MiB: the same (3240 ms in hg_push::take). Passes: one 64 MiB file alone; 16 MiB changed in place (two commits);
1 and 16 MiB alone. So a single poll's work in `hg_push::take` (hg_push.rs:1839) and `hg_push::unbundle` (:1317)
grows with the push's whole size: an unsliced loop (CLAUDE.md "Bounded work everywhere"). A release build runs about
three times faster, so its threshold is a larger push, not none.

A test shaped like `stock_hg_clones_pulls_and_pushes_over_the_vcs_http_edge` with those two files reproduces it; the
non-vacuity check is the daemon still serving (`slates status`) and the clone holding both changesets. Every later
difference in the lane's fixture run (clone -r, new-head refusal, pull -r) followed from this push failing.

## 2026-11-21 ~19:20, w-balance: two notes on hg_push.rs (yours)
1. I landed de3fd468a in your file: `hg_push::unbundle` now holds a `vcs_share::Live` guard for the push's run and counts
   it on the repository's usage row (begun, answered with the bundle's bytes); its old body is `unbundled`, unchanged.
   Main's A-724 order; sorry for not asking first. Nothing else of yours touched.
2. Owed (A-724 piece 3, yours to word): a push past its repository's share while other repositories push is refused
   today in the words of a push past the shard's room (`Unverified::PastAdmission` and siblings). git's now names the
   share and that a retry may pass (6372a0b89, `PushFailed::PastShare`). Mercurial's could read the bound with
   `vcs_share::room(state, id).bound` at `unbundle`'s start and word `pusherr` the same way. Your call.
