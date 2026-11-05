# A survivor's other shard never held its live peer

Observed in main CI [41774055498](https://github.com/hyper-light/slates/actions/runs/41774055498) on `76b450a1`
(ubuntu workspace job, `crates/cli/tests/cli.rs:3359`) in
`three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death` (§4.8 "Membership"; D-7).
After the owner's death, one survivor's `slates status` listed only itself (`members: [12079765767769331346]`,
`peers_probed: 1`) while the other survivor listed both. A rerun on the same commit passed (41817402309).

## Reproduced

Measured on adacachy (AMD Ryzen 9 9950X3D2, 32 threads, Linux 7.2.9-2-cachyos), 2026-11-04. Each run was a capped
user unit pinned to four CPUs, with four busy loops running inside it (`AllowedCPUs=10-13`; CI's runner has four
vCPUs) and `SLATES_TEST_CLI=1`. The case failed 1 of 50 runs on `a578f1df`. With a temporary diagnostic it failed
4 of 80 runs (runs 44, 56, 57, 75); the diagnostic logged every member a node folded dead and every finding of its
detector. Every failure has the CI failure's shape. One survivor's view holds only itself, with one peer probed and
nothing dropped. The other survivor's view holds both. The fleet wait expired at 40 s. Logs:
`~/.cache/slates-agents-scratch/fixtures/fleet-runs/cli4-17.log` and `clidiag-{44,56,57,75}.log`.

## Cause, from the logs

The detector never suspected the live peer, and no node ever folded it dead. What differs is where it was folded
alive. A daemon folds its membership on the control shard and hands each change to its other shards
(`fold_membership`, `crates/server/src/member_task.rs`). In all four failing runs, the lone survivor folded its live
peer alive once, on the control shard by learn-on-contact (`crates/server/src/fleet.rs:587`). It never folded the
peer on its other shard (`member_task.rs:604`):

| run | lone survivor | live peer | learn-on-contact folds | other-shard folds |
|---|---|---|---|---|
| 44 | 6109728529969656516 | 9516261948737526664 | 1 | 0 |
| 56 | 9486693269250739099 | 591915278738097488 | 1 | 0 |
| 57 | 9101086766596129600 | 17827398684277308607 | 1 | 0 |
| 75 | 13503408749056159911 | 3719434265023535372 | 1 | 0 |

The dead owner, by contrast, was folded at both sites on every node. `slates status` reports the membership of the
shard that serves it, and each CLI invocation is a new client on its own shard. After the owner's death the other
shard held the survivor alone.

Why the peer never reached the other shard:

1. `changed_members` reports each detector belief not yet folded and records it as folded at once.
2. `admit_beliefs` then refuses an alive belief about an id that has not yet been learned on contact (unknown to the
   control shard's `FleetNode`, or not authenticated), so a stranger's gossip enrolls nothing.
3. When the detector heard the peer was alive before learn-on-contact had run, that belief was refused and still
   recorded as folded.
4. Learn-on-contact then made the id alive on the control shard alone. The detector's belief did not change again,
   so it was never offered again and never reached the other shards.

The race is between the detector's first acknowledgement and the announcement exchange. Under load it went the
detector's way in 1 of 50 runs.

The unit test `a_peer_the_detector_knew_before_learn_on_contact_reaches_every_shard` (`member_task.rs`) drives
this order on a real member plane: the detector holds a peer alive, the turn folds, learn-on-contact runs, and the
turn folds again. Before the fix the second turn handed nothing to the other shards, and the test failed at its
second assertion (2026-11-04, `cargo +1.98.1 test -p slates-server --lib
a_peer_the_detector_knew_before_learn_on_contact_reaches_every_shard`).

## Impact

A node with more than one shard could hold a live peer on its control shard only. Its other shards' membership
then omitted that peer for as long as the detector's belief stayed unchanged: every view, placement and status
those shards answered. It needed the detector to hear of a peer before learn-on-contact completed, which load makes
likely. In CI it failed a fleet deployment test after its owner died.

## Exact edits

- `crates/server/src/member_task.rs`: the control shard's turn of the fold is now its own function
  (`fold_on_control`; the same steps). A belief `changed_members` reported but `admit_beliefs` did not take is no
  longer recorded as folded, so the next turn offers it again. Once learn-on-contact has authenticated the id, the
  detector's unchanged belief is admitted and reaches every shard. This is the same re-offer the fold already makes
  for a fan-out a shard's channel refused (`FOLD_REFUSED`). A stranger's gossip is still never enrolled.
- The new unit test above.
- `crates/cluster` is unchanged.

## Verification

Interleaved A/B on adacachy, 2026-11-04: `a578f1df` (before) against this change (after), each built in its own
target directory so each test binary runs its own `slates`. Command:
`EXTRA="-p AllowedCPUs=<cpus> --setenv=SLATES_TEST_CLI=1" capped2.sh 7200 "ab.sh 50 4 <before cli test> <after cli test>
three_daemon_processes_deploy_a_fleet_from_one_manifest_and_survive_the_owners_death --exact"`.

| load | before | after |
|---|---|---|
| two CPUs, four busy loops (`cli-ab2.log`) | 1 of 50 failed (round 4, `cli.rs:3359`, "two members after the death") | 0 of 50 |
| four CPUs, four busy loops (`cli-ab.log`) | 0 of 50 | 0 of 50 |

With the reproduction runs above, the unchanged code failed 6 times in 230 runs under load, each with this record's
shape, and the change failed 0 times in 100. The unit test fails on the old fold and passes on the new one, with
the other four tests of the module. The whole fleet target (`crates/server/tests/fleet.rs`) passes 87 of 87 on the
change.
