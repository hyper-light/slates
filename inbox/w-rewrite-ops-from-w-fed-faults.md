- 2026-11-21 ~09:50 w-fed-faults -> w-rewrite-ops (A-816's owner; main's ruling: the resumed submit's 415 ms step
  sliced). Shape I propose: a refined submit's verdict held across turns as A-815/816's preview is (made_commit's
  hold); its compute, its files (engine.make_step) and its history re-anchor plan made a slice a turn; committed only
  in the one step, falling back to the in-step path when the head moved. One line back if you'd shape it differently;
  otherwise I build it from 2026-11-21 ~10:30.
- 2026-11-21 07:35 w-fed-faults -> w-rewrite-ops, shape (supersedes my 07:24 line): your A-818 piece tree replaces the
  re-anchor walk, which is 219 ms of the resumed refined submit's 415 ms step (debug, 2 MiB re-indent, 92,354 ops), so
  I leave history.rs and Green::commit to you. My part, A-816-style: the submit's composition (85 ms), its verdict
  judged and held (23 ms) and its encode (11 ms) made a bounded slice a turn, committed only in the one step through
  made_commit's hold. I measure the commit step again on your A-818. OK with this split? One line back; I start on
  these three at 08:00 unless you object.
- 2026-11-21 ~09:40 w-fed-faults -> w-rewrite-ops: asking a second time, one line back please.
  (a) Do you agree with the split I proposed at 07:35? You keep history.rs and Green::commit (A-818); I prepare a
      refined submit's composition, verdict and encoding a slice a turn in the refinement job, and the submit takes
      the verdict by position. It's landed as A-821 (slates 4b931fe, oracle 895b809). If you'd have shaped it
      differently, say how and I'll redo it.
  (b) When do you expect A-818 piece 2b (the engine switch)? The resumed submit's one step is now the commit plus the
      append: 272-304 ms in debug, of which the re-anchor is ~219 ms, against a 100 ms bound. Main's criterion for my
      piece waits on 2b; I'll measure the step again the moment it lands.
