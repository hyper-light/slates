# Git upload charges twice for retrying the same object

On 2026-11-22 at 01ae9fc26, the existing A-864 two-window clone completed with a measured 120-byte admission budget while resident. I gave its cold run the same budget. One released-window stop later it returned PackTooLarge. The focused test failed in 0.02 s, with 607 tests filtered.

crates/git/src/store/upload.rs:1324 charges the placement before inserting it in loose_at. The Reuse::Stopped arm at line 1340 removes the tentative placement. Retaking that same object charges it again. With a released pack the number of yields now affects whether a valid request is admitted.

w-git owns the upload cursor. The attached inbox/codex-review-upload-admission.patch changes its existing stop-and-retry test. Run `cargo test -p slates-git --lib a864_a_turn_that_meets_a_let_go_window_stops_cleanly_and_the_clone_is_byte_identical`. For the separate Studio r69 stall, the full round for main is `SLATES_TEST_GIT_DATASET=/path/to/git/git cargo test --release -p slates-cli --test git_interop stock_git_carries_a_whole_repository_over_each_edge -- --ignored --exact --nocapture`. The focused refusal does not establish the cause of the 183,511-byte stall.
