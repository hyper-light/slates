# A live takeover made its volume in one owner-shard turn, and every period made it again (2026-11-18)

## Description

`live_chaos`'s nemesis (`a_seeded_nemesis_of_single_faults_keeps_every_register_invariant`) failed at seeds
17036665960917068132 and 2120743995065985386 on CI's Linux runners, x86_64 and arm64 (diag 44311638654, main
c1b33245a): a bare daemon's watchdog ended its process with exit 4, "shard 1 is spinning, 1000 ms of CPU without a step
against the 1000 ms window", after an owner fault, and the fleet then never settled at three (a bare daemon is not
restarted). g225 traced both seeds' unsettled fleets to these kills.

## Root cause, from the logs

A diagnostics build dumped every shard's pulse at a stuck verdict, with the task the shard was polling named by its
spawn (a spawned future's type, a spawn request's caller, and a cross-shard call's closure): diag-g251-spin-3 (3 of 3
runs on each Linux arch) and two runs on a Mac Studio named the stuck shard's task
`xshard::call_within<Result<(), &str>, fleet::materialize::{{closure}}::{{closure}}>`: the takeover's call to the
volume's new owner shard, inside one poll (the polled slot set, the loop's phase a step, no full-ring wait), 1.39 to
1.42 s of CPU. Timed in that closure (diag-g251-spin-6 and -7, release, Mac Studio, the nemesis's own volumes):

| | seed 17036665960917068132 | seed 2120743995065985386 |
|---|---|---|
| the live log | 16,737 to 27,430 sealed payloads, 152 to 306 MB | 17,559 to 17,872 payloads, 173 to 214 MB |
| open the payloads | 0.30 to 0.62 s | 0.16 to 0.24 s |
| apply 16,736 to 27,429 deltas | 0.15 to 0.44 s | 0.09 to 0.17 s |
| lay the archive out | 0.06 to 0.16 s | 0.05 to 0.10 s |
| restore, populate | 0.18 s, 0.14 s | 0.14 s, 0.10 s |
| publish (its live whole entry) | up to 1.06 s | 0.48 s (0.46 s) |

The whole of it ran in the one owner-shard turn the call made. And the call waited a period (`HEARTBEAT_NS`, 100 ms),
so the turn always outlasted it: the attempt counted `fleet.materialize.unanswered`, and the next period's attempt
fetched the whole log again and rebuilt it again before finding the volume made (five more rebuilds of 0.68 to 0.99 s
in one run), until one of those turns crossed the spin window.

## Impact

Every live-shipped volume taken over: its successor's owner shard served nothing else for the materialization's
length, every period, and at a few hundred megabytes of log the successor's daemon was ended by its own watchdog (a
lone daemon) or killed by its anchor, taking every volume it served with it. A volume larger still would have been
given up on: no attempt could ever be answered within a period.

## Exact edits (A-374)

- `crates/server/src/live_ship.rs`: `Rebuilding` (begin with the lineage key adopted and the keys taken; `advance`, a
  payload at least until the turn's share is spent; `archive`) in place of `archive_from_live`; `rebuild_refused`, and
  `taken_over`, counted in the turn that makes and publishes the volume, as the one-turn materialization counted it
  (a successor restarted once the count rose must find the volume published: `a_successor_restarted_before_any_write_
  keeps_the_live_volume_it_took_over`, A-343, failed when the count came two turns before the publication).
- `crates/server/src/fleet.rs`: `make_live`, a task of the owner shard that claims the volume's record, makes it in
  turns (`make_live_in_turns`: the rebuild's slices, then the archive, the restore and the making with its publication,
  each a turn) and leaves its end there; `begin_live` waits a period for it, and the task outlives the wait (waited
  for in the shard's own time, it held the coordinator through the whole materialization);
  `making_of` reads the record before an attempt fetches anything (`Joined`): one running is joined
  (`VolumeWait::Making`, its turns progress for the give-up bound), one ended is read and forgotten; a give-up forgets
  the record too.
- `crates/server/src/verbs.rs`: `materialize_taken_over` split into `made_already`, `restore_taken_over` and
  `make_restored`, with `materialize_taken_over_restored` for a restore made in an earlier turn.
- `crates/server/src/state.rs`: `ShardState::making_taken_over`, `Making`, `PendingVolume::record_turns` and
  `live_adopted`; `crates/server/src/daemon.rs`: `Daemon::inject_live_turn_pause` (test support).

## Tests

`a_live_takeover_is_rebuilt_in_turns_and_joined_while_it_runs` (`crates/server/tests/fleet.rs`): three nodes at
`f = 1`, one shard each; a live volume written 24 files on a; on b and c the seal quantum cut to one unit and a pause of
half a period between a rebuild's turns; a stopped; through the successor every file reads back, the rebuild took at
least 24 turns, one rebuild began, later attempts joined it and none began twice, and the successor's longest step
stayed under the watchdog's window. Red-checked by mutation: the whole rebuild in one turn, and the attempts not
joining.
