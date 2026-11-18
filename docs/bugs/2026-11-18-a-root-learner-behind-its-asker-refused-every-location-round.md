# A root learner behind its asker's root refused every location round

`a_location_round_asks_a_peer_whose_session_was_out_once_it_returns` (`crates/server/tests/fleet.rs`; §4.8 Lookup)
failed 2 of 3 on main's quiet Mac Studio (adas-mac-studio-3, kiln runner 2768d0d5, 2026-11-18): once at main 3022c6894
(`~/.cache/slates-gate/fleet-q1.log` there) and once at 2a5bf7264 (`fleet-base.log`); the rerun at 3022c6894 passed
(`fleet-rerun.log`). The foreign client's `Status` was refused `OwnerUnreachable { region: 0 }`.

| counter | foreign node, run at 3022c6894 | at 2a5bf7264 |
|---|---|---|
| `fleet.owner_location.round` | 58 | 68 |
| `fleet.owner_location.retried` | 56 | 66 |
| `fleet.owner_location.refused_by_peer` | 116 | 136 |
| `fleet.owner_location.not_owner` | 58 | 68 |
| `fleet.owner_location.unavailable` | 58 | 68 |
| `fleet.owner_location.session_out`, `budget_spent`, `gave_up` | 1 each | 1 each |

The successor counted `fleet.owner_location.root_view_differs` 58 and 68 (and `owned_unmaterialized` 1, its own client's
first call before it had made the volume). Every home-region daemon held regional configuration 13 (12 in the second
run), installed and council alike. The foreign node held the dead owner dead and its session to the successor was back.

## Cause

Each round asked the three survivors. Two refused with an empty reply and one answered "not mine". The successor's
counter says why it refused: the query was bound to a root version other than its own (`owner_location::serve`'s
`RootViewDiffers`), once a round, for the lookup's whole budget (`verbs::locate_within_horizon`, the membership
horizon).

- The foreign node is region 1's only member, so its region's representative and a root voter: it holds the newest
  committed root. Region 0's representative is its lowest host id, also a voter, and it answered "not mine" under the
  same version. The successor and the third survivor are root learners.
- A root learner fetches the root only when its alive regions or the root voters it knows differ from the root it holds
  (`fleet::root_diverges`). A seat change moves the root's version (A-406, in both failing commits) and neither signal.
  So a learner whose last fetch at formation came before the last seat record keeps its older root for good.
- The owner in this history is the highest host id among the four, never a representative, so its death changed no
  voter and gave the learners nothing to fetch for.
- Nothing else tells a learner that the root moved on, so it refused every round. Whether a run fails depends on when
  each learner last fetched during formation: the failures are intermittent, and the test passed on its rerun.

## Impact

A lookup from a node on the newest root to an owner that is a root learner behind it is refused for its whole budget,
`OwnerUnreachable`, though the owner is alive and serves the volume. Its client's verbs fail with it. Any member that
missed a root seat change at formation stays behind until a region or a voter changes.

## Fix (A-489)

- `owner_location::serve` takes the asker. A query bound to a newer root version than the one held is still refused
  `RootViewDiffers`, since a reply binds the view it was asked under. It also records the newest such version and its
  asker (`note_newer_root` into `ShardState::root_newer_seen`), counted `fleet.owner_location.root_behind`.
- `fleet::root_learner_fetch` (extracted from `drive_root_group`'s learner branch) fetches when that record names a
  newer version than the one held, beside the regions and voters signals, and takes the record.
  `fleet::with_shown_peer` adds the asker to the hosts the fetch asks.
- The asker looks again every poll interval, and the look after the learner adopts the newer root is answered.

## Tests

- `a_location_query_bound_to_a_newer_root_sends_the_learner_to_fetch_from_its_asker` (`owner_location`'s unit tests, on
  an audit shard whose root moved past one version). Queries are bound to the older view, its own view, the next and
  the one after; the older and newer are refused and its own is served; three newer views are counted; the learner's
  next period fetches from the asker that showed the newest root, and the period after does not. Red with
  `note_newer_root` left out: no fetch from a shown peer.
- `a_root_learners_fetch_asks_the_peer_that_showed_it_a_newer_root` (`fleet`'s unit tests): the target list.
- The fleet test itself is filed for main (run-requests.md).

## Owed

The converse: an asker that is a root learner behind its peers is refused with an empty reply that names nothing, so
its own learner is not sent to fetch. A refusal naming the peer's root version is a reply format change (SLATES_DESIGN
A-489).
