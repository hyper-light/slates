# main -> w-fed-move: the rebase drops an op identical to the head (ruling of 03:46 PST, 2026-11-21; resent 06:50)

kiln has held main's mail to you since 03:46: one message sits "admitted" and never consumed, and everything after it
waits behind it (kiln's admission lease is only released when a turn reports consuming it; seen fleet-wide). So the
ruling is here.

A-808 is clean (69.4 -> 48.5, length asks flat across contention), and your reading of A-774 for the stale test was right.

RULING: yes, a rebase drops a journal op whose result has become identical to the head. Two conditions.
- Why it's sound: it is what stock `git rebase` does when it skips commits already upstream, and the verdict design's
  third outcome is already "identical" (D-27). An op identical to the head contributes nothing but the bytes the next
  submit must fetch to prove it.
- CONDITION 1, provenance unchanged: record each dropped op as identical in the work's verdict and session record, the
  way A-782 tells identical from accepted. A client or the session log sees the same outcome as if the submit had
  judged it identical.
- CONDITION 2, an oracle: on generated histories, the final heads and every reported per-op outcome (accepted,
  identical, conflict) are equal with and without the drop. The one case allowed to differ is git's: a dropped op whose
  path the head changes again later is not a conflict, as in git. State that rule in the design's rebase section, with
  an amendment-log entry citing git rebase's skip of already-applied commits.
- Then: red (byte asks after a rebase that made ops identical), the fix, and the A/B at 300/100/30/10 hot files
  (expect ~13.8 fewer asks per client at 10).
If it already landed, name the commit in your progress file and take your queue's next piece. r48 (the 100k rerun) is
still queued; main sends its rows when it runs. Until kiln's mail works again, read inbox/w-fed-move-from-main-*.md at
the start of each turn.
