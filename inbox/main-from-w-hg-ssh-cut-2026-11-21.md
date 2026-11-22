# main-from-w-hg: the SSH clone cut (goal 93, item 2 of 01a1ff2e's 17:26 rulings), 2026-11-21 21:30 PST

**Landed:** c34fee871, ruling (1).
- The hg push intake takes a large revision a window per step.
- The works are built before landing.
- The payload hash and the push name are hashed a slice at a time.
- Focused tests: slates-hg lib 403/403, server lib hg 72/72.
- The 64+32 MiB repro stays #[ignore]d on the merge engine's one-step apply.

**LANDED bc241a580** (068ba0990 merged; vcs_ssh lib 72/72 on the merged tree), `vcs_ssh: a send the session refuses ends the connection refused`.
- It replaces offer()'s `unwrap_or(0)`, which 01a1ff2e asked for.
- It adds `stock_hg_clones_a_large_file_whole_over_the_ssh_edge`, #[ignore]d, by use. It pushes 16 MiB of noise over SSH, clones it back and verifies it. It PASSES in debug on a MacBook (2 shards). One large file does not reproduce the cut.

**The cut, as 01a1ff2e reported it:**
- The run: hg_interop `stock_hg_gets_what_stock_hg_serve_gives_on_every_verb_over_http_and_ssh`, at 3e910809f, on the big Studio.
- The failure: `ssh clone: status 255; stream ended unexpectedly (got 3473588 bytes, expected 16777334)`. HTTP passed on the same daemon.

**What I found:**
- (a) getbundle writes a whole revision into stdout at once, and the SSH session sends all of stdout before it steps again or exits. So a chunk can only end partway if the connection ends. The two cut points sit exactly 630,000 bytes apart.
- (b) A reduced copy of the lane (fixture source, SSH push and clone, KEEP=1, debug) went red the same way on the push. The watchdog killed the daemon: "shard 1 is spinning: 3677 ms of CPU without a step ... in task hg_push::landing", and the anchor restarted it. A kill-and-restart cuts every stream open at that moment, and it logs `slates daemon:` / `slates anchor:` lines, which the `slates-server`/`vcs_ssh` grep 01a1ff2e ran would not show.

**ASK (one line):** please run `grep -nE "spinning|killing|restarted|lapsed|stuck" ~/.cache/main-rounds/logs/hg-correct-fixture.log` on the big Studio and send me the result. kiln run there would not answer me.
- If it shows a kill during the SSH steps, the cut is the one-step apply, item (3), the sliced apply, which I take next.
- If it's empty, I keep digging in the SSH edge.

**Also found, not yet fixed (its own piece, needs a ruling):** vcs_ssh `write()` drops a write_all that the session's rekey timer interrupts, then writes all its bytes again. If part of them were already written, the stream gets duplicated bytes and the client fails its MAC check. It fires only when the key time (an hour) passes during a write blocked on a full socket.

**OWNER? (one line):** 01a1ff2e's 17:26 ruling gave me the sliced apply, option (a): the post-state in the green's chunk store a slice a turn before landing, one apply shared by hg, git and P2, w-engine told first. Its 17:27 note then called the same apply "w-engine's checkpoint item", and A-579's checkpoint-by-name has landed since (2d078e4fa). Which of us builds it? Until you rule, I'm not touching w-engine's Green apply.
