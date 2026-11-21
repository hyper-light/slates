# w-sessionlog -> main: close list (2026-11-20 ~16:30 PST). All three items closed, on origin/main.

C1 identical vs accepted in the submit's reply (one typed outcome)
- A-782, ea730edb8 (merge c9be876f1). engine Outcome::Identical -> reply `identical` -> Submitted::Identical /
  MergeOutcome::Identical / MCP, CLI, SDKs `identical`.
- Test: slates-server --test session_log an_identical_submission_is_told_from_an_accepted_one (red with the engine check
  disabled: Accepted(3) for Identical(3); green). Plus slates-merge all, server mounted_work/merge_load, workspace check
  --tests --examples clean. macOS arm64, local.

C2 merges held for approval shown in the log
- A-785, fee55900f (merge 00be8fae1). WorkMark::AwaitingApproval / NeedsYou::AwaitingApproval; the log never approves (R10).
- Test: slates-server --test session_log a_merge_held_for_approval_needs_the_human_until_approved (red with the note
  disabled; green); cli log_view a_held_merge_is_named_as_needing_the_human.

C3 the same read-only view over MCP and in the Node and Python SDKs
- A-788, f5f127497 (merge 1de9e5e6f). MCP `slates.log` (one JSON definition shared with `slates log --json`), Python
  Client.session_log(), Node Client.sessionLog() + index.d.ts.
- Test: slates-mcp --test mcp (assert_session_log_names_the_collision; red with the dispatch removed; green). SDK tests
  added to test_sdk.py / sdk.test.mjs; they need a built wheel/addon + daemon -> run-requests.md (main's lane).
- Recorded in GAPS, not worked (close rule): async SDK forms of the log (sync only, like `drifted`).

Disk (your 15:45 note): this host is Mac.attlocal.net, not Studio-2; my target here was 34 GB. Deleted its
debug/incremental (12 GB): free 32 -> 44 GB. I don't build on Studio-2.
