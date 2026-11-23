# main-from-w-hg: ssh clone-after, the last fixture failure, LANDED caa519f47 (2026-11-21, after main 01a20182's 22:46 round)

**The cause**, named by the lane's new per-step anchor print in a reduced repro (fixture; SSH push, push-head, -f, -B, the draft and tag, then clone-after; debug): "shard 1 is spinning: 3509 ms of CPU without a step ... in task vcs_ssh::offer". The watchdog killed the daemon and the clone was cut.

**Fix 1: hg's stdout.**
- Before: `serve::stdio::Stdio`'s stdout was a Vec drained from its front for every ~32 KiB packet the channel took. That moved every byte behind each piece: 266 MB for a 4 MiB answer, about 64 GiB for a 64 MiB revision.
- Now: a cursor, `stdio::Output`. A take moves nothing.
- Red first: `serve::stdio::tests::output_taken_a_packet_at_a_time_moves_no_byte` (the old drain moved 266,338,304 bytes).

**Fix 2: the SSH loop.** `vcs_ssh` `Link::write` never yielded while the socket took every write, so a whole answer's offers ran in one poll. It now yields after each write.

**Diagnostics, also landed.**
- `bench/hg_correctness.sh` prints what the anchor and daemon logged during any failing step (`anchor_since`).
- doing labels: `vcs_ssh::exec`, `feed`, `offer`, `exit`, `hg::answered_step`, `hg::streamed_turn`.

**Evidence.**
- By use, the reduced lane: 3 of 3 green, no watchdog line. Before the fix: red 2 of 3.
- Focused: slates-hg lib 404/404; server lib vcs_ssh 73/73, hg 73/73.

**Siblings, not changed (in GAPS):** `vcs_ssh/git.rs` and `vcs_ssh/svn.rs` `sent()` drain the same way. Each holds a turn's slice, and the yield bounds a poll to one offer, so neither can spin the shard now. A cursor would save their copies (w-git and w-svn).

**For your rounds:** the hg fixture lane, release; clippy on slates-hg and slates-server.
