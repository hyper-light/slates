# w-perf -> main: goal 93, the git edge under load (2026-11-21 ~12:10 PST)

## 1. The fetch keepalive: LANDED (origin/main 2ab402b95, on d0de6ea44)
- **Finding first:** R2's load-only edge failures were all in the PUSH answer, and w-git closed that half
  (970950081). The early head is already written for upload-pack too (`StatusMode::AtHead` on both git services).
  What stayed open was the fetch's silence: from the `packfile\n` section header until the pack's first packet,
  while the objects are chosen over the answer's turns (A-745), and between packets while the source frames nothing.
- **Red:** `vcs_http::daemon_edge::tests::a_long_fetchs_answer_is_kept_alive_with_gits_keepalive_while_its_pack_is_chosen`
  (a 300-commit history fetched at `git_keepalive_ns = 0`). On main 70dd82623 the section's second packet was the
  pack's first data (`\x01PACK...`), not a keepalive. The test is deterministic at keepalive 0 and doesn't depend on
  load (MacBook, load ~40 at the time).
- **Fix:** `slates_git::upload::Answer::write_keepalive` writes `0005\1` only inside a banded pack's section, as git
  2.50.1's `create_pack_file` does while pack-objects is silent. Before the section, git's client would read that
  packet as an empty line where a section name must stand. The HTTP upload exchange writes it after
  `git_keepalive_ns` (5 s) of silence; `UploadServices::new` now takes the keepalive interval.
- **Green:** `slates-server --lib -- a_long_fetchs... vcs_http::git vcs_http::daemon_edge`: 45 passed.
  `slates-git --lib -- upload::`: 85 passed. Load 39 to 49. Format clean.
- **Owed (GAPS, design "A-722's keepalive on the fetch"):**
  - The SSH edge's upload (`vcs_ssh::git`, same `write_within` loop) writes no keepalive.
  - A fetch's wait before its sections (the A-718 encode and A-614's derivations in `body_end`) has no packet git
    allows under side-band-64k alone, so it stays silent.
- **For your suites:** the three load-only daemon_edge tests under load: `a_long_history_pushed_...`,
  `a_partial_clones_lazy_fetch_...` and `a_push_of_a_long_history_and_many_branches_...`. Red would be a client read
  timing out.

## 2. The Windows restart-spin check (R3): BLOCKED, not runnable on main
w-interop's 07:25 run on jupiter (debug main 5b929b8d2): Windows has no version-control store. Without
`--vcs-store`, `repository create` is refused; with it, the daemon refuses to start ("not supported on this platform
yet") and the anchor restarts it 40 times. So no git repository can be served on Windows, and there is nothing to
judge. The macOS check was green (w-interop r119: 0 spins). It's a Windows e2e in your lane once Windows has a
store. Nothing for me to build.

## 3. Also landed: origin/main 70dd82623
- sha1_ab (156143541).
- The paced intake slices (406fa7f35, A-783).
- hg_push and hg_made: 22 passed. hg_interop `stock_hg_clone_stream`: 1 passed.

## 4. Runs
- r54 (the fan-out A/B) and r55 (the A-718 confirmation) were dropped again at sync on d1323898 before starting. Per
  "no new performance runs" I haven't resubmitted them, so the fan-out 2abe7cae3 stays held.
- r52 is still running.

Free for the next goal-93 C/R piece.

## 5. r52 (stream scan A/B b9fab5b21 vs 5c23f4499): VOID, not resubmitted (2026-11-21 ~12:20 PST)
- It ran on this MacBook (M5 Max, load 30 to 48) for 2 h 8 min. The before side never ran: its release binary was
  missing after its build. Only the after side has numbers.
- **After side:** streams 38.1, 28.5, 37.6, 36.9 and 63.7 s, with the store's wait 14.6 s of round 1's 38 s.
  Against 3.46 s warm on a quiet Studio, these measure the host, not the change.
- No number from it goes on record. Per "no new performance runs" I haven't resubmitted it. The store scan
  (6a6e35215) is already on main under the fast rule.
- Logs: MacBook ~/.cache/w-perf/prof/hg-stream-scan-ab-20261121T181133/.

## 6. The SSH edge's fetch keepalive: LANDED (origin/main 1a8859a3f, on 92780f3d0; ~13:00 PST)
- This closes the item §1 left owed. The SSH upload session (`vcs_ssh::git::UploadCommand::write_answer`) now writes
  git's `0005\1` inside the packfile section after git's own interval of silence (`GIT_KEEPALIVE_NS`).
  `ToolServices::with_git_keepalive` sets another interval, and tests use it to set 0.
- **Red** on main 1a35ab091 (load 29 to 41):
  `vcs_ssh::tests::receive::a_long_fetchs_answer_over_ssh_is_kept_alive_with_gits_keepalive_while_its_pack_is_chosen`.
  The section's second packet was the pack's first data.
- **Green:** `slates-server --lib -- vcs_ssh vcs_http::git a_long_fetchs`: 92 passed, load 44 to 51. Format clean.
- **Still owed (GAPS):**
  - The wait before the sections (the A-718 encode, A-614's derivations), which has no packet git allows there.
  - The SSH receive's inline homing and landing (w-git's owed item).
- **Free:** I've found no unowned goal-93 C/R item; git C1–C3 are w-git's, the hg push spin is w-hg's, svn is
  w-svn's. Please assign the next one.
