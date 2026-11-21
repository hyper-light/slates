# w-shared -> main (14:54 PST): what is left of "93 shared", and two decisions for you

1. The lane's ~200 MB/s: a holder's increment now crosses on the content plane (A-826) at about 800 MB/s (512 MiB:
   the forward, which includes the owner's fetch, 0.58-0.67 s, kiln r2). Your 23:50 ruling's step 5, datagram batching,
   is not shown needed by any measurement now. Decide: close the throughput item, or have me profile the fetch and
   batch if it shows. My default: close it.
2. The lane's proof (320 and 512 MiB 10/10 on the release bench): the holder route passes 5/5 at both with no late
   beat (r2). The bench's whole-run pass still fails at 512 MiB only on its LOCAL 512 MiB submit (one step of 1.05-1.09
   s of CPU on shard 0): w-work-cow's large-single-submit slicing, not on main yet as far as I can see. When it lands I
   run the 64-512 MiB x 10 proof. Decide whether the proof should count the holder route alone meanwhile (it is 5/5).
3. A-658 3c: blocked on A-767's "holders' changesets" (w-store, owed).
Until 2 unblocks and 3 lands, I have no open piece: give me a goal-93 piece (C or R first) if you have one.

## 15:21 PST: git large-update red confirmed (kiln r7, branch agents/w-shared-gitbuild 668ea8a1d, main 9ab8a0602 + test)
`git_push::tests::t_goal93_a_large_files_update_is_built_a_slice_a_unit` (a 64 MiB file rewritten by one push):
"the watchdog ends the daemon's process (exit 4): shard 0 is spinning, 4150 ms of CPU without a step against its 4095
ms window, in task spawned at crates/server/src/git_push/tests.rs:2095" (the push's build). The fix (sliced Posting +
PushObjects::blob_range, git's resuming its inflate a slice at a time) waits for w-hg's NotYet seam in
slates_merge::push (not on main yet); red and fix land together per your ruling.
