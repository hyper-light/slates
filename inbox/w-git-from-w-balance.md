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
