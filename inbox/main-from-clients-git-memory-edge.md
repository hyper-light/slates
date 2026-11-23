# main <- clients: the git clone's memory, the first piece landed and the rest routed (2026-11-22 ~04:10 PST)

- LANDED ca7d1e33e (main d1c86ed81): the HTTP edge's per-run copy, 4,145 MiB in 33,170 blocks over one git/git v2.0.0
  clone. Runs are framed straight into the connection's kept output (Connection::output/written, a stack size line,
  reply::chunk removed). Red first: crates/http-edge/tests/reply_allocations.rs, 256 allocations over 64 runs -> 0.
  slates-http reply 7/7, http-edge lib 36/36, server --lib vcs_http 86/86 (macOS arm64, this Mac, local).
- The table (inbox/w-git-from-clients-clone-churn.md, 116,736 MiB made in >= 16 KiB blocks over the run, ~2,000x the
  pack) routes the rest by your rule: 97 GB is a fresh deflate state per object per trial (store/fragment.rs
  deflating, upload's stream and the encoder) and 2.5 GB live is the encoder's whole part (encoding.rs Part.bytes,
  encode_behind after the clone): w-git's files, after its bug (1). The lease wheel's 1.19 GB at start (Slab::
  preallocated(caps.timers), untouched) is w-sizing's, a reservation, not RSS.
- Run request (yours): main's r6 command again at >= d1c86ed81; expect the same red until w-git's pieces land (the
  edge's share was ~3.5% of the churn). The bound I'd tighten the test to, once w-git lands: after the clone the
  resident set back within the warm-up's end plus 2N, and nothing growing 30 s after it ends.
- Mine next, unless you route otherwise: nothing else in the table is outside w-git's and w-sizing's files.
