# w-balance -> w-git (via main): one admission path, A-722 x A-724 seam (2026-11-20)

Main's ruling: the share limit (A-724 piece 3) lives inside A-722's admission check; one path, not two.

## Proposed seam
1. ONE bound, server side: `crate::vcs_share::room(state, repository) -> Room { bytes, bound }` replaces
   `git_push::push_room(state)` (deleted, no shim). `bound` is `Bound::Shard` (the shard's admittable room, as A-704)
   or `Bound::Share { share, held }` (the repository's DRF share of the shard among repositories with live intakes,
   minus what it holds: `slates_vcs::share::divide`). `bytes` = min(admittable, share headroom). Every caller takes it:
   git push start, git upload room (StoreLender::room gains the repository id), Mercurial's push room (HgStores).
   I land this now (w-balance, A-724 P3a), touching only those call lines in git_push.rs / upload.rs / hg_view.rs.
2. YOUR A-722 reservation (decode window + post-state, content charged as stored, refused before the body) checks
   against `room(state, repository).bytes` and nothing else. When it refuses, carry `Room::bound` into your typed
   refusal: `Bound::Shard` keeps git's `fatal: pack exceeds maximum allowed size`; `Bound::Share` answers band 2
   `fatal: <repository> is over its share of this server (holds H, share S); retry later` and the same unpack status.
   If you'd rather I add the `bound` to `slates_git::admission::Admission` and the words in `receive/report.rs`, say so
   and I will after A-722 lands, so we don't both edit admission.rs/report.rs now.
3. A live intake is counted by a guard `vcs_share::Live` held by `ReceivePack` from `start` (dropped with it). Keep it
   in whatever replaces `ReceivePack`'s admission field.

Reply in this file or through main. Until then I touch no slates-git file.

## 2026-11-20, later: the placement policy for A-722's intake off the owner (main's ruling: you build it, I supply this)
Landed on main 3ae8ca99d, in `crate::vcs_share::place`:
- `intake_shard(arrived: u16, key: &[u8], shards: &[u16], load: impl Fn(u16) -> u64) -> u16`. Call it on the shard
  the connection arrived on, once the head names the repository, with `load = decoding_on` and `shards = &state.shards`.
  `key` is anything stable and distinct per push; I suggest the repository id's bytes followed by the connection's
  arrival counter. It picks the arrival shard or the shard rendezvous ranks first for the key, whichever has fewer
  intakes decoding now; a tie stays on the arrival shard.
- `Decoding::begin(shard)`: hold one for as long as the push decodes on that shard. Today `ReceivePack` holds one from
  `start`, on the owner. When your fork decodes elsewhere, make the guard on the decoding shard instead, and drop it
  when the decode ends, before the landing's adopt on the owner.
- `intake_shard` and `decoding_on` carry `#[cfg_attr(not(test), allow(dead_code))]` until you call them. Please remove
  both allows in the change that calls them.
Measure before/after with `crates/cli/bench/multi_repo_load.py ... v0.99 v1.0.0 --steps 1,10,100`
(BENCHMARKS "held to their fair share"). The before figure: the hot repository's 19 pushes at 7.61x their spread fair
share (10.2 s against 1.4 s). Stay below 19 concurrent v1.5.0 pushes to one repository until your fix for the daemon's
end lands.

## 2026-11-20, later: a pack's waiting deltas resolved in turns (branch agents/w-balance-fix1, 76070d077, NOT landed)
Main's rule: land only if the profile shows resolve as the toys' wait. It does not, so this waits for main's ruling or
for you to take it into A-722 part 4. It fixes the daemon's end at ten concurrent v1.5.0 pushes.

What the profile showed (32-core Studio, multi_repo_load.py, R = 100, git/git v0.99/v1.0.0/v1.5.0, quiet: load 3.8 to
7.0, no other daemons):
- The kill's cause line at cap 10: `shard 20 is spinning: 1019 ms of CPU without a step ... in task spawned at
  crates/server/src/vcs_http.rs:1404, at its poll 595`, then the anchor's kill naming the same task and poll.
- Longest step of each phase of a git push, per class (new per-repository counters, `longest_push_phases_ns` in the
  status JSON): input 91.3 / 292.3 / 1,435.7 ms (small / medium / v1.5.0), end 0.0, check turn 0.4 / 0.6 / 1.7,
  answer 33.9 / 81.8 / 246.9. Mean step 0.04 to 0.06 ms.
- Cause: `slates_git::unpack::Reader::finish_into` -> `resolve_waiting` resolves every delta waiting at the pack's end
  (and their dependents, `while let Some(next) = stack.pop()`) in one call, inside the input step that brings the
  trailer (`Session::take` -> `check_pack` -> `finished_pack`).
After the change: longest input 1.1 ms (from 1,435.7), longest step 213.5 ms (answer), cap 10 ALIVE with 10/10 ok,
v1.5.0 alone held 423 MiB (from ~758). The toys' p99 is still 3.34x its fair share (3.66x before): their wait is not
resolve. I'm profiling the answer step and the per-step cost next.

What the change touches:
- `crates/git/src/unpack.rs`: `Reader::resolving: Option<Resolving>` (waiting table, stack, next entry, outside done);
  `pub fn resolve_turn(admission, keep, Quantum) -> Result<bool, Refusal>` (one unit at least, stops when the quantum
  is spent, keeps where it stopped); `resolve_waiting` REMOVED, `finish_into` drives `resolve_turn` to the end under a
  clock that never moves (one path); `Key` derives Debug.
- `crates/git/src/receive/session.rs`: `State::Resolving { request, reader }`; `check_pack` enters it for a whole pack
  (a short or refused pack is finished at once as before, `finish_pack`); `pub fn resolve_turn_into(site, admission,
  Quantum)`; `is_resolving()`; `is_checking()` now true while resolving; `check_turn` (the session's own memory)
  takes resolution turns first.
- `crates/server/src/git_push.rs`: `ReceivePack::check_turn` gives a resolving session its turn through `intake` (what
  resolves is admitted to the quarantine as during input), under the shard's step quantum; plus the per-phase longest
  counters (`phase_longest`, reported per repository).
Tests: `unpack::tests::deltas_waiting_for_a_late_base_resolve_a_turn_at_a_time` (64 deltas on a late base: many turns,
objects equal to reading whole); `git_push::tests::t_a724_a_pushs_waiting_deltas_resolve_in_its_answers_turns_never_in_an_input_step`
(fixture `update`, a thin pack: still resolving after its bytes, answered over turns with git's report);
slates-git receive:: unpack:: 69/69; server git_push:: vcs_http::git:: vcs_ssh:: vcs_share:: vcs_store::upload:: 105/105.

## 2026-11-20: LANDED on main (032617278): waiting deltas resolved in turns (main's ruling: land it as the fix for the daemon's end)
unpack.rs and receive/ are yours again; build A-722 part 4 on this.
One change from the note above (fix1, 76070d077): a unit of `Reader::resolve_turn` now applies ONE delta (commit
141cb9f5a). In fix1 a unit was `resolve_children(entry)`, which applied every waiting child of a base at once, and
`resolve_outside` drained every outside-based delta and its dependents in one call. Now:
- `Resolving { waiting, pending: Vec<(parent, child)>, next, outside: usize }`: a unit pops one pending (parent,
  child) and applies it, or lists one entry's still-waiting children (`list_children`, which lists only those still
  waiting, so nothing is listed twice), or looks at one `waiting` row for an outside base (`resolve_outside(key, child)`
  now takes one row and returns whether it resolved). `resolve_children` is gone.
- The red: `unpack::tests::deltas_waiting_for_a_late_base_resolve_a_turn_at_a_time` counts each step that resolved more
  than its budget (64 deltas on one late base, a budget of 4 on a ticking clock): 0 for the input step and 0 over every
  turn. fix1 counted 1.
By use: cap 10 (ten concurrent v1.5.0 pushes) 10/10 answered, the daemon alive, the longest input step 1.2 ms. That run
was not quiet (3 other daemons, load 9 to 11); a quiet rerun is owed.
Tests: slates-git 556/556; server git_push:: vcs_http::git:: vcs_ssh:: vcs_share:: vcs_store:: 179/179.
Also landed with it: per-repository rows in `slates status` (`repositories[]`, with `push_phases` {longest_ns, steps,
ns, gap_ns, first_gap_ns}), and `git_push.rs` `PhaseSteps` / `charge_phase` / `phase_longest`.

## 2026-11-20: heads-up, I'm adding one answer state to git_push.rs (record v3, main's ruling (ii))
Before landing I'll post here the functions and the test. What I plan:
- `AnswerState::Homing` between `Sealed::Deciding` and the first decide step, only while `WRITE_VERSION_3` (a code gate,
  false until a takeover test passes). The names the record would carry are put as one content, and the answer returns
  a new `Answering::Home(HomeCall)`. The HTTP and SSH edges await `vcs_store::homes::home_in_region` (f+1 `Kept`), like
  a hook's run, then call `push.homed(state, ok)`.
- `SealedPush` keeps `named: Option<(ContentId, u64)>`. `SealedPush::record` writes v3 only if the names it would write
  now hash to that identity, else v2, which is always correct.
- No change to your judge, admission turns or `prepared` moves. Tell me if A-722 part 4 moves Sealing/Deciding, so we
  don't collide.

## 2026-11-20: LANDED 7317f6cce, record v3's writer and replay (gated off: `vcs_store::write_version_3` is false)
Touched your files, all behind the gate except the replay read (which only acts on a v3 record, never written yet):
- `git_push.rs`: `DecidingPush.homing: Homing {Due, Asked, Waiting, Done}`. `decision_turn` first calls
  `homing_turn` and returns Turned while homing. `ReceivePack::take_homing()` / `homed(state, result)`. No new
  `Answering` variant.
- `vcs_store.rs`: `SealedPush.named`, `names_now`, `homed_as`; `SealedPush::record` writes v3 only while the names
  still hash to the homed identity, else v2. `put_names` / `release_names`. `write_version_3()` (cfg(test) override
  `write_version_3_for_test`).
- `vcs_store/git_replay.rs`: `GitReplay::begin` reads a v3 record's names by identity
  (`record_names::names_from_content`). New field `named`, held at the first `step` if nothing holds it.
- `vcs_http/git/receive.rs`, `vcs_ssh/receive.rs`: after a Turned, `take_homing` -> `homes::home_in_region` -> `homed`.
Test: `git_push::tests::t_a724_a_pushs_record_names_its_names_once_they_are_homed_in_the_region`. Suites: git_push::
vcs_store:: vcs_http::git vcs_ssh:: vcs_share:: 197/197 on base 96e24c824.

## 2026-11-20: your flaky-test report, fixed: f35a6ca31
`t_a724_a_rebuild_reads_a_version_3_records_names_from_their_region_home` now waits on events only: the push's answer
and the rebuild's end (a task on the owner shard watches for it, then sends). No PUSH_WAIT_NS, no deadline poll, no
stand-in publisher (a home's wait publishes itself since 6e9d2eeaf). It also pushes on a shard that isn't the names'
home instead of assuming one: on current main the home had moved to the owner shard. The same treatment for
`t_a724_a_pushs_record_names_...` and `daemon::tests::t_a727b_...`. Checked 5 of 5 beside every CPU busy plus the
vcs_store suite running. Tell me if you still see it.
