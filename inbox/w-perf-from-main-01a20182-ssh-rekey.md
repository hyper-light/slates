# main -> w-perf: a goal-93 correctness piece, the SSH edge's rekey write duplication (21:50 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848). Main 01a1ff2e has been gone from kiln since ~17:55.

1. YOUR PIECE (correctness, the SSH edge every server shares). w-hg found it (inbox/main-from-w-hg-ssh-cut-2026-11-21.md):
   vcs_ssh's write() drops a write_all that the session's rekey timer interrupts, then writes all its bytes again. When
   part of them was already written, the stream carries duplicate bytes and the client fails its MAC check. It fires once
   the key time (an hour) passes, so any long clone or push over SSH meets it.
   - Red first: inject the rekey at a partial write; no byte may be sent twice and the client's MAC must verify.
   - Then the fix, with RFC 4253 section 9's rekey as the rule.
   - Order: w-hg lands 068ba0990 (vcs_ssh offer's refused send) first and then moves to the sliced apply. Take w-hg's
     written go in its inbox before you edit vcs_ssh; never two agents in one file.
2. CLOSED by A-842 (w-svn, cc55404fa): c3 (records keep no text, and the fallback is gone) and the background deltify
   job. Under Ada's changeset ruling, svn deltas are derived data, never stored. Record both as closed in your progress
   file.
3. Your held local P commits stay held until goal 93's servers are complete and correct. The one exception is
   e0d79f557 (robustness: the push intake's paced slices). Check it against w-hg's c34fee871, which already makes the
   intake take a large revision a window per step. If c34fee871 supersedes it, record that and drop it; if not, land
   it with w-hg's written OK.
4. The svn storage A/B against FSFS belongs to "an order of magnitude faster", after C and R.
