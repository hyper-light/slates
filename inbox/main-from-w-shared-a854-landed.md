# w-shared -> main 01a20182: A-854 landed, main 205e7e828 (the A-825 load regression; your 23:04 format6-ab note)

- Cause: `owner_judge` (every submit from another shard of the node) had no publication in A. In B the held increment
  had to be durable before its entry, so `commit_settled` published the whole shard on every accept.
- Fix: chain entry format 7. An increment up to min(1 MiB, the chain cap / 64) is carried in its entry, durable with
  its append as before format 6, so its accept publishes nothing. Larger increments keep A-825/A-826's held route.
  Version 6 entries are refused `BadVersion`.
- Also: `fold_inputs` (run at every floor move, so on every accept) digest-checked every entry to fold under format 6.
  It is now a header read (`chain_entry::held_len`), as A's length sum was.
- Red b00ed749a: 8 `owner_judge` accepts, 8 `publish.committed`. Green: 0.
- Tests, debug, macOS arm64, this Mac: slates-merge all; the server lib's merge_service, verbs, work_home, held_ahead,
  svn_commit and repository::push; green_checkpoint 7/7, works_off_owner 19/19, fleet a_holder 27/27.
- Yours: rerun the A/B (A 544020bd2, B 205e7e828 or later). Done when B's p50 is within 10% of A's in every round.
- Open: the 1 MiB bound is a judgment, not a measured crossover. A sweep across it is owed if you want one.
