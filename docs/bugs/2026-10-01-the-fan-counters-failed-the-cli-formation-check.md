# The configuration fan's counters failed the CLI formation check

**Date:** 2026-10-01. **Area:** `slates-cli` tests (`tests/cli.rs`), `slates-server` (`fleet.rs`, the fan's
counters). **Audit:** a regression from AUD-29-29's change (5530a47). **Design:** §4.8 "Lookup", §4.14.

## Description

From 5530a47 on, two real-process CLI tests failed on both CI platforms (runs 36844774220 and 36844975495):
`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` and
`a_terminated_council_leader_process_hands_off_before_it_exits`. Every process formed the fleet; the check
that "only superseded link work may end during formation" failed on these lines of the counter map:

```
shard 0 refused fleet.fan.sent: 1; shard 0 refused fleet.fan.unchanged: 2
```

## Root cause

The shard's counter map (`ShardState::refusals`, shown by `slates status` as "refused") carries events as
well as refusals. The CLI test allowlists the events formation is expected to produce. AUD-29-29 added two
normal events — a fan delivered (`fleet.fan.sent`) and a period that owed a shard nothing
(`fleet.fan.unchanged`) — and I did not sweep the tests that judge the map. The CLI suite runs only under
`SLATES_TEST_CLI=1`, which my pre-commit runs did not set.

## Fix

The formation check admits the two events. A fan a shard's channel refused (`fleet.fan.refused`) is a lost
delivery, so it is still not admitted and still fails the test.

## Tests

- Reproduced locally before the fix (`SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli --
  three_daemon_processes…`: the same message). After it, the whole CLI suite passes: 13 of 13, 2026-10-01.
- The sibling sweep found no other allowlist over the map: the server fleet suite reads one counter by name.

## Siblings reported

- **The map's name says "refused", but it holds events as well.** At least `fleet.fan.sent`,
  `fleet.fan.unchanged` and `fleet.probe.indirect.relayed` are events, so `slates status` prints each
  delivered fan as "shard 0 refused fleet.fan.sent". Splitting it into refusals and events would make an
  allowlist unnecessary. That change touches the observability surface (§4.14) and is reported, not made
  here.
- **My pre-commit runs never set `SLATES_TEST_CLI=1`,** so a change to fleet counters must be followed by the
  CLI suite with the gate set.
