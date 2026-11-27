# Four Windows reds of diag CI 2

- **Date:** found 2026-11-27 (diag CI 2, run 45986066740 at 89f01a607; windows-latest x86_64 job 140461887964,
  windows-11-arm job 140461888146); diagnosed and fixed 2026-11-27, main 01a20182's brief
  (`inbox/w-interop-from-main-windows-reds.md`).
- **Status:** reds 1, 2 and 3 fixed here; red 4 diagnosed against diag CI 6's Windows lanes, below as it settles.

## 3. An SSH session's steps read no CPU on Windows (product counter; arm64)

- **Symptom.** `vcs_ssh::tests::hg::a_clone_over_ssh_under_burners_holds_its_shard_a_step_a_poll`: "the serving steps
  counted"; `Polls { most_ns: 0, total_ns: 0, count: 13 }`, 6 serving steps, the longest 0 ns.
- **Cause.** `vcs_ssh::hg::thread_cpu_ns`, which times both the product's step counter
  (`vcs_store.hg_ssh_step_most_cpu_ns`) and the test's polls, read `GetThreadTimes` on Windows. Windows charges that
  time a scheduler tick at a time, about 15.6 ms, and the whole clone runs under one tick: on the MacBook the same 13
  polls total 2.31 ms of CPU, the longest step 0.44 ms. So every poll and step read 0 unless a tick fell inside it. The
  daemon's own step timer met the same in diag run 45238179939 and reads the thread's cycles there
  (`daemon::shard_thread_cpu_ns`, A-871).
- **Fix.** `thread_cpu_ns` on Windows is `slates_machine::own_cpu::own_cpu_ns` (`QueryThreadCycleTime` at the rate
  measured once a process), a paired `cfg` function. The counter and the test read real CPU there.

## 2. The import restart test read its image before the shard's last let-go was published (test; x86_64)

- **Symptom.** `repository::store_tests::a_cleanly_restarted_import_brings_back_only_its_checkpoints_named_texts`: the
  image read before the stop held (whole 1, Away 2); the restart found 0 whole and brought 3 back.
- **Cause.** The same run's shard lacked all 3 named texts just before the image read (`before the stop … Some((3, …))`):
  its last let-go was made but not yet published, so the committed journal the test read (`verbs::committed_held`, the
  image plus the write log) still held that text whole. A publication then committed it before the stop, and the
  successor restored it Away. The test's premise, that the committed journal read before a stop is what the successor
  restores, holds only once the shard's changes are published.
- **Fix.** `image_split` publishes the shard (`verbs::publish_shard`) in the same step as its read. On the MacBook the
  test reads (whole 0, Away 3) against the shard's 3 lacking, and passes.
- **Corrected (w-interop, 2026-11-27, after diag CI 9).** The cause given above was wrong.
  - **The real cause.** It is a test counting error, deterministic on every host with a 4 KiB page:
    - `image_split` matched the image's Away windows to each named text by the text's own identity, which names only
      a text of one window;
    - the content window is 16 pages (`slates_vfs::content::chunk_bytes`), so 64 KiB at a 4 KiB page;
    - `full.dump`'s `trunk/big.txt` is 106,766 bytes, so it spans two windows there and was read as whole.
  - **Why only some lanes failed.** A 16 KiB-page Mac makes the window 256 KiB, one window for that text, so the Mac
    never failed. Windows (diag CI 2) and the TSan lane (diag CI 9, run 46018002586, at 56b609053 with the fix above
    in) did fail.
  - **The red.** The test at a 4 KiB page on the MacBook gave (1, 2) against (0, 3) on its first run
    (`~/.local/state/w-interop-logs/red2-page4k.log`).
  - **The fix.**
    - A text is counted Away when any of its windows is, each found by the text's identity and length
      (`ContentStore::window_holding`), as `ContentStore::holds_whole` judges it.
    - The test runs at the host's page and again at a 4 KiB page; the second run requires at least one named text of
      more than one window, so it can't pass vacuously.
    - The publish stays. Before it, `image_split` now drives the encoder's cut to its end, so the publication carries
      every change the shard has made. (A-930 part 3's cadence, 7acfd685c, would have needed an ask here as well; main
      reverted it in 68e6fe9e3.)
  - **Green on the MacBook (debug):** (0, 3) at the host's page, and (0, 3) with one text of two windows at 4 KiB.

## 1. The signed-work restart test read the checkpoint's count at once (test; both lanes)

- **Symptom.** `a_merges_verdict_survives_a_daemon_restart`: "the floor rose past both versions, and a checkpoint folded
  them", `merge.checkpoint.taken` 0 right after the lagging work's destroy.
- **Cause.** Since A-579 a floor move's checkpoint takes its hold inline only within the step's quantum and the rest in
  a task of its own, so it may land after the destroy answers: the design (main's ruling of 2026-11-27). The test read
  the count at once.
- **Fix.** `checkpoint_taken_past` waits for the count to rise as a user reads it, through the status counters, within
  the client's reply deadline (the anchor's liveness budget), failing at it, so a checkpoint that never lands is still
  caught. No hook into the product. The same suite's `expect_the_verdicts_were_the_verifiers` read it at once too and
  waits the same way. On the MacBook `--test signed_work` passes, 13 of 13.

