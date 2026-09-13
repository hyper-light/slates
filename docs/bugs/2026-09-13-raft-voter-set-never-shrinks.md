# The council's and root group's Raft voter set never shrank: a retired voter counted toward every majority for good

Date: 2026-09-13
Area: `crates/cluster/src/raft.rs` (the Raft core), `crates/cluster/src/config_group.rs` (the regional
council), `crates/cluster/src/root_group.rs` (the root group), with the drive in `crates/server/src/fleet.rs`
and the boot-time voter derivation in `crates/server/src/daemon.rs`.
Severity: fleet liveness — after a voter died and the council committed its takeover, the dead host still
counted toward every Raft majority (election, pre-vote, commit-index advance, CheckQuorum), so a council of
three that lost two voters had no quorum until one returned, a council of five with a learner never promoted
it, and the fleet's contact predicate had to exclude dead voters by belief (`484768a`) to stop probing and
linking them. Ada named this owed: "Raft membership change so a retired voter actually leaves the voter set
(today `all_voters` never shrinks — the predicate correction papers over it)."

## Description

`RegionalCouncil::is_voter` and `RootGroup::is_voter` read `RaftNode::all_voters()`, and nothing ever changed
it after boot. The Raft core already carried a complete log-integrated joint-consensus change
(`begin_membership_change` / `complete_membership_change`, `C_old,new` and `C_new` entries that take effect on
append, carried on the wire and through compaction, proven by `tests/raft.rs`), but no caller drove it: a
committed `Reconfiguration::TakeOver(host)` retired the host from the *configuration's members* and left the
*Raft voter set* untouched. The design's council is "a small elected council per region" whose voters the
daemon derived deterministically at boot (`council_voters`: the members with the lowest ids up to the
candidate floor `2f + 1`); that derivation was never re-applied when the members changed.

## Root cause

Three gaps, each closed:

1. **No driver.** Nothing proposed a voter-set change when the membership changed. Fix: the voter set is a
   pure function of the committed membership — `council_voters(members, quorum)` for the council (moved into
   the cluster crate from the daemon; the daemon now calls it), the representatives of the committed regions
   for the root group (`root_representatives(hosts, regions)`, the lowest-id live host of each region) — and
   the leader keeps the Raft voter set equal to that target through the core's joint change, one change at a
   time: `RegionalCouncil::reconcile_voters()` / `RootGroup::reconcile_voters(&representatives)`, called by the
   coordinator each leader period right after the membership reconcile (`drive_config_council`,
   `drive_root_group`). Begin the joint change when the target differs; complete it once the joint entry
   commits (`caught_up`); nothing once `C_new` commits. A dead voter thereby leaves the consensus set and the
   next member in id order — a learner until then — is promoted in its place, so the council keeps tolerating
   `f` failures; a region whose representative died is carried by its next live host (the daemon feeds the
   root leader the alive representatives each period, `alive_representatives`).
2. **The core did not enforce the change's preconditions or the removed leader's exit.** `complete` could be
   called before the joint entry committed (the doc called it "the caller's obligation"), a second change could
   begin while the previous configuration entry was uncommitted, an empty target was accepted, a leader whose
   own removal committed kept leading, and a removed node kept campaigning. Fix (Ongaro's thesis §4.1, §4.2.2,
   §4.2.3): `begin_membership_change` refuses while the latest configuration entry is uncommitted or the
   target is empty; `complete_membership_change` refuses until the joint entry has committed;
   `advance_leader_commit` steps a leader down the moment the committed configuration (sole, not joint) no
   longer names it; `on_election_timeout` returns nothing for a node that is not a voter of its effective
   configuration.
3. **A live member demoted to learner would never learn it.** The leader replicates to `all_voters()`, which
   excludes an outgoing voter the moment `C_new` is appended, so a live demoted member would keep the joint
   configuration, believe itself a voter, and campaign whenever the leader's heartbeats — which it no longer
   receives — stop. Fix: `RaftNode::replication_targets()` — the current voters plus, while the latest
   configuration entry is uncommitted, the voters of the configuration it replaces; the groups' `voters()`
   (what the drive ships to) is this, `is_voter` is the consensus set proper. Bounded: the extra targets drop
   out the moment the change commits.

One more consequence handled: a learner that **adopted** a fetched configuration and is later promoted to
voter receives the whole log from the leader; folding those commands on top of the adopted configuration
would apply them a second time (a takeover's epoch bump doubled). Both groups now keep the formed `base`
configuration and start their first fold from it, so every voter folds the same log from the same point.

## Impact

- A dead voter no longer counts toward any majority: with three voters and one dead, the two survivors carry
  the council (and every later commit needs their two acknowledgements, not two of three including the
  dead); with five members at `f = 1` the fourth member is promoted when a voter dies.
- The believed-dead clause of `keeps_direct_contact_with` (`484768a`) remains correct and is unchanged; it no
  longer papers over a stale voter set.
- Admitting a member with a lower id than a current voter demotes the highest-id voter to learner (the
  design's deterministic lowest-id rule); if that voter is the leader it steps down once `C_new` commits and
  the new voters elect among themselves — a bounded, one-time leadership change per such admission.

## Exact edits

- `crates/cluster/src/raft.rs`: `is_voter`, `replication_targets`, `latest_config_index`,
  `config_before_latest`, `committed_config`, `step_down_if_removed`; the gates in `begin_membership_change`
  and `complete_membership_change`; the no-campaign guard in `on_election_timeout`; the step-down in
  `advance_leader_commit`. Tests: `a_removed_leader_steps_down_once_its_removal_commits`,
  `a_change_waits_for_the_previous_configuration_entry_to_commit`,
  `outgoing_voters_are_replicated_to_until_their_removal_commits`; the pre-existing
  `completing_a_change_adopts_the_new_configuration` now commits the joint entry before completing (the gate
  it relied on the caller for is enforced).
- `crates/cluster/tests/raft.rs`: `a_removed_voter_leaves_and_the_survivors_commit_under_the_new_majority`
  (three nodes, one dead, the survivors commit alone; Election Safety checked).
- `crates/cluster/src/config_group.rs`: `council_voters` (moved from the daemon), `base`,
  `reconcile_voters`, `voters()` → replication targets, `is_voter` → the consensus set, the first-fold reset
  in `apply_committed`. Tests: `a_retired_voter_leaves_the_council_and_the_survivors_commit_alone`,
  `a_learner_is_promoted_when_a_voter_dies` (with the base re-fold: the promoted learner's configuration
  equals the leader's exactly), over a multi-council in-process harness.
- `crates/cluster/src/root_group.rs`: `root_representatives`, `base`, `reconcile_voters(&representatives)`,
  the same `voters()`/`is_voter`/first-fold changes. Test:
  `a_retired_regions_representative_leaves_the_root_voters`.
- `crates/cluster/tests/config_group_live.rs`: `a_dead_voter_is_removed_from_the_council_across_the_transport`
  — a three-voter council over real mutually-authenticated sim-UDP sessions with the third voter dead (no
  endpoint): the takeover, the joint entry, `C_new` and a further change each commit on the one live voter's
  acknowledgement, the configuration entries riding the wire, and the dead voter is a voter on neither.
- `crates/server/src/daemon.rs`: the boot-time derivations delegate to the cluster crate's functions.
- `crates/server/src/fleet.rs`: `reconcile_voters` driven after each group's membership reconcile;
  `alive_representatives`.

## Validation (2026-09-13, worktree `agent/raft-membership`, 18-core box shared with five other agents' builds)

- Before: on `f3ced8b` the council's voter set has no path that shrinks it — `reconcile_voters` did not
  exist and `all_voters()` was the boot set; after the existing takeover test
  (`the_leader_takes_over_a_failed_member_bumping_its_epoch`) `is_voter(A)` remained true and `voters()`
  remained `[OWNER, A]`. The new tests assert the opposite and could not compile against that tree.
- `cargo test -p slates-cluster`: 121 unit tests, the 8-test conformance suite (`tests/raft.rs`, one new),
  `config_group_live` 2/2 (one new), `root_group_live` 1/1, `raft_live` 1/1, and the remaining live suites —
  all green.
- `cargo test -p slates-server --lib`: 20/20.
- The one in-process fleet test whose behaviour this changes,
  `three_daemons_form_a_fleet_and_the_survivors_retire_a_dead_node`, run singly: FAILED twice (495.23 s,
  495 s — its post-retirement head never placed) with this change alone, which exposed a pre-existing
  holder defect (`docs/bugs/2026-09-13-holder-acceptor-born-stale-never-placed.md`); with that fixed it
  passes in 10.88 s. The retirement itself (both survivors' membership, the takeover commit, the joint
  change and `C_new`) completed within one second of the death in every traced run.
- Gates: `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo xtask check`
  clean.

## Siblings swept

- **Fixed in the same change:** a holder's acceptor created by a refused first record was pinned at a
  stale generation and never raised at install, so a head provisioned in the window between the owner's
  install and the holder's never placed — the failure this change's extra council rounds surfaced, and the
  same signature under which the probe-cadence dilation was measured-and-rejected earlier the same day
  (`docs/bugs/2026-09-13-holder-acceptor-born-stale-never-placed.md`).
- The root group of a **single-region** fleet has one voter (the region's representative), so if that host
  dies no root leader exists to move the voter set — pre-existing, inherent to "one representative per
  region", and now healable only for multi-region fleets. A single-region fleet's root group is the
  degenerate whose only committed content is the sole region; reported, not changed.
- The learner-fetch path (`adopt`) never makes a node a voter — only a configuration entry replicated to it
  does — so a retired voter that later fetches a configuration stays a learner (verified by reading the
  drive: `is_voter(local)` reads the Raft configuration, never the fetched one).
- Compaction folds the latest configuration entry into the base (pre-existing); a snapshot installed on a
  promoted learner carries the configuration but not the group's `applied` counter — the daemon never compacts
  today; when it does, the group must restore its fold state from the snapshot's state bytes (owed with
  compaction itself).
