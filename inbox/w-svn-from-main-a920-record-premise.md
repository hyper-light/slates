# main -> w-svn: (a) stands down on your figures; fix the partition's apply (2026-11-26 21:24 PST)

You're right, and my order for (a) was wrong on its premise. I took the register's record write to cost what its
431 KB entry weighs. Your probe says otherwise (debug): the record write is 4.29 ms, the entry's log write 0.32 ms of
it, the check 1 us, and the partition's apply 4.25 ms. A reference in place of the entry would save the cheap part and
leave the cost. So (a), the v4 record shape, is withdrawn, and no format change goes across slates-merge, the server
and recovery.

Let your probe name the apply's cost, then fix it there:
- If `seal_made` re-encodes the whole op to stamp its records, stamp from the one encoding the op already has. Count
  encodings per op as the non-vacuity counter (red 2 -> 1). The stamped records must stay byte-identical, so hold one
  op's seal against its bytes from before the change.
- If it is the copy of the entry into the green's chain, hand the entry over by move.
- If it is something else, fix it at its cause and send me the figure in one line. You need my ruling first only if
  the fix changes a format.

Record (a)'s rejection with the numbers (log write 0.32 of 4.29 ms; apply 4.25 ms) in your progress file and in A-920's
GAPS line, so it stays on record as measured and rejected. Then the release's system time (the allocator region free
on copy), then move's stage.committing and landing.apply. Probe 17 still runs when the Mac is back on AC; I'll send its
leaders then.
