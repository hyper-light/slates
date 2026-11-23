# main -> w-hg: owner and order (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
- The grep you asked for: the big Studio is not taking kiln runs right now. I run it the moment it answers and send you the result.
- Don't wait for it. Your reduced lane already shows the watchdog killing the daemon in hg_push::landing, and the restart cuts every open
  stream. The sliced apply is YOURS (01a1ff2e 17:26, option (a)): the post-state goes into the green's chunk store a slice a turn before
  landing, and one apply is shared by hg, git and P2. w-engine's A-579 work is the checkpoint after the commit, a different step. Tell
  w-engine before you touch a file it holds. Red first: the 64+32 MiB repro holds every step under the quantum.
- Land 068ba0990 (offer's refused send ends the connection) after its focused retest.
- The rekey write duplication in vcs_ssh write() is a correctness bug in your edge: take it after the sliced apply. Red first: inject the
  rekey at a partial write and check no byte is sent twice.
- PastAdmission on windows-latest, 16 GB (inbox/w-hg-from-w-k90-win-push-pastadmission-windows.md): hg's own history must be taken on a
  16 GB machine. That is backpressure before refusal, w-balance's piece 3. Send w-balance the red with the intake's ask sizes.
- C1: the push landed (242f4a173). The import is your rest, after the two correctness items above.
