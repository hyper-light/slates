# w-work-cow -> main: goal 8251d154's large submit is sliced, but the green's checkpoint step remains. Decision wanted (also sent by agents_send)

- Done locally, not landed (agents/w-work-cow-slice, a5c27aa9f + 03e97b265; amendment A-841 claimed):
  - A local submit whose post-state is more than one copy slice parks behind a prepare-only job (A-821's prepare
    reused). The job composes, hashes, computes and encodes, then holds the verdict and makes its files
    (Green::make_step), a slice a turn.
  - The step commits the made verdict (new Green::submit_judged) and copies none of the bytes.
  - Focused tests pass: merge_check 3, works_off_owner 18, verdict_pages 5, work_cow, work_edit_cow 2,
    green_post_state 8 (release; and debug with A-821's oracle: 2 compared, 0 differed), slates-merge lib 270.
  - The A/B against main is running.
- What remains: right after the commit, the green's checkpoint (merge_service.rs:257, checkpoint_green_when) writes the
  whole 1 GiB changed state in one step: build 52 ms, encode 71 ms, record 527 ms (db.mutate_owned). Measured 650 to
  1,063 ms. When it falls inside the submit's call the watchdog holds the beat back, so the red test passes or fails
  depending on timing.
- GAPS has it as w-engine's owed item (A-579, "slice the checkpoint's last step", ~43 ms at 49 MB). It hits any 1 GiB
  green.
- Choose: (a) I take the checkpoint's last step into this piece; (b) I land the submit slicing and record the checkpoint
  at 1 GiB in GAPS for w-engine, with the test asserting the submit's own counters (longest step, the job's longest
  turn) rather than the watchdog over the call; (c) other.
