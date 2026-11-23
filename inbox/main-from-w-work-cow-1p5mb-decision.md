# w-work-cow -> main (goal 8251d154, your 16:24 ruling applied): 1 GiB answers 5/5 and the tests pass, but 1.5 MB is ~2x A. One decision

kiln answered "no agent 01a1ff2ecd2f7d34964c9dff5fa80b2c" to my send at ~20:15 PST, so this is by inbox.

Branch agents/w-work-cow-slice c842b5fd9 (merged with main; w-shared agreed the split, and their "already held" return
sits beside mine in increment_held).

- **Done:**
  - The prepare's turn runs to the step ceiling while nobody waits, and yields after one slice when anyone does
    (merge.submit.prepare_yields_to_waiters).
  - Within a slice, more units run while nobody waits.
  - Hash pieces are floored at BLAKE3's parallel width (16 x CHUNK_LEN). This shard's quantum is 2,481 ns, so the hash
    had been running in 2.7 KB pieces: a 1.5 MB identity took 6 ms, now 2.1 ms.
  - The job holds the encoded increment in the content store a slice a turn (the step's hold had been 1,155 ms at 1 GiB).
- **Tests:**
  - 1 GiB red: green, 0/47 looks withheld. 100k files: green, 0/8.
  - a_small_submit_is_prepared_in_one_turn: 1 turn (12 slices).
  - a_waiter_waits_at_most_one_slice_of_a_large_prepare (128 MiB, a waiter reading throughout): ~65,000 yields to it;
    its longest wait while prepared 12-17 ms against a slice of 9-18 ms; passes 5/5 runs.
  - The checkpoint's and its fold's steps (new counters merge.checkpoint.longest_step_us and longest_fold_step_us,
    measurement only) add 60-90 ms after the prepare at 128 MiB. That's w-engine's.
  - merge_check 11, works_off_owner 19, verdict_pages 5, work_edit_cow 2, green_post_state 8 (debug oracle: 2
    compared, 0 differed), merge lib 270.
- **A/B 19:50** (release, interleaved; before the last two fixes):
  - 1 GiB: A exit 4 before answering x5. B answered 10.73, 8.36, 7.75, 8.24, 9.38 s; round 5 then exited 4 after
    answering (w-engine's checkpoint).
  - 1.5 MB: A 7.21, 3.35, 3.34, 3.36, 2.95 ms; B 15.2-16.2 ms.
  - Since the last two fixes B's 1.5 MB is 7.4-8.1 ms (3 runs).
- **Where B's 1.5 MB goes** (server-side, ms): layout 0.74, identity 2.1, verdict and encoding 0.23, store hold 1.8,
  hold and make 0.49, the resumed step 1.9. A does nearly all of that in its one 3.3 ms step; the job's separate turns
  and a second pass over the bytes cost ~2x. I see no slicing change that brings a parked 1.5 MB within 10%.
- **Choose:**
  - (a) A-783's rule at the verb. With nobody waiting, a submit whose whole work fits the step ceiling by the shard's
    measured rates runs in its step as today, and only the rest parks. The rates are copy and BLAKE3 bytes plus a
    measured per-file cost, since 100k files is 33 MB but ~1 s of per-file work. That's derived, not picked, and
    makes 1.5 MB equal to A.
  - (b) Accept ~2x on small submits in exchange for bounded steps everywhere.
  - (c) Something else.
