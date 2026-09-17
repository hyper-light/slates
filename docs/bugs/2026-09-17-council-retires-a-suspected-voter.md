# The council retires a live voter on a transient failure belief, stranding a later loss

Date: 2026-09-17. Contracts: §4.8 Membership and the configuration master (D-14), AC-8.1, AUD-07.
Design: SWIM's suspicion window and Lifeguard ("peer confirmation before suspicion; SUSPECT before
DEAD"); Raft membership-change safety (§6).

## Symptom

`a_whole_ram_replacement_joins_as_a_fresh_voter_and_commits_after_another_loss` failed
intermittently on Linux — about 1 run in 8–10 in a bounded container (`--memory=2g --cpus=4`),
37 s (the 30 s `audit_wait` spent) versus 9–15 s on a pass; reliably green on macOS. It fails at the
final assertion: after the whole-RAM replacement has joined the three-voter regional council and a
second voter (the leader) is lost, the surviving pair never commits the retirement the fresh voter's
acknowledgement requires.

## Root cause (from the committed council log)

Targeted capture was added first (`council_snapshot` and a council Raft dump, `council_debug`, in the
test). At the stall both survivors show the committed configuration-change log, and it is decisive:

```
role=PreCandidate term=3 voters=[5983, 7814(dead victim), 10882(fresh)] joint=true
  commit=12 members=[5983, 7814] v5
log=[ … t2 Admit{10882 fresh} … t2 cfg→voters{5983,7814,10882}   (fresh is a full voter)
      t3 noop | t3 TakeOver{dead: 10882 (fresh)} ]                (a NEW leader retires the LIVE fresh voter)
```

Sequence: the replacement joins (`old` retired, **fresh admitted**, voter set `{5983, 7814, 10882}`
— matching the passing mid-test assertions). The test then stops the leader `7814`. The survivors
`{5983, 10882}` re-elect (term 3), and the new leader `5983` commits **`TakeOver{fresh}`** — retiring
the *live* fresh voter — while the actually-dead victim `7814` is **not** retired. The applied config
becomes `{5983, 7814}`: a voter set holding a dead member and excluding a live one. No majority is
reachable (`5983` alone), so no leader is elected (both `PreCandidate`), the voter change never
completes, and nothing more commits. Deadlock.

Why did the new leader retire the live fresh voter? The council leader reconciles membership each
period from its SWIM view, and it retired the member it believed failed. During the turmoil of the
leader loss and re-election, the fresh voter's just-formed sessions churned (its record link
re-dialed, its discovery exchange invalidated — the `fleet.discovery.*` and `fleet.accept.replaced`
counters), so `5983` transiently held it `Dead` — a false positive SWIM later refuted (the survivors
see it alive at capture) — while the genuinely-dead victim had not yet aged to `Dead`. The council
took an **irreversible consensus retirement** on a **revocable failure belief**, and picked the wrong
member.

Two defects compounded it:

1. `reconcile_alive` retired every configuration member **absent from the SWIM alive set**
   (`membership().alive()` is `Alive`-only), i.e. every member that was `Suspect` **or** `Dead` — so
   even a single missed probe (a member in the transient `Suspect` state) could retire a live voter,
   bypassing the suspicion window entirely.
2. Even keyed strictly off confirmed `Dead`, a re-election can drive a live member all the way to
   `Dead` for a period or two before its refutation arrives; retiring it there is the same
   irreversible action on a transient belief.

## Fix

Retire a council member only when its death is **confirmed and stable**:

- `RegionalCouncil::reconcile_alive(alive, dead)` (cluster) now takes an explicit confirmed-dead set:
  it admits every `alive` host not yet a member and takes over every `dead` host still a member; a
  member that is merely `Suspect` is in neither set and is left alone (defect 1).
- The server retires a member only after its death has held for a **confirmation window** — the
  council's own election-timeout base (`ElectionTiming::floor().base_periods`, the longest transient
  membership disruption a leader loss and its re-election cause). A per-member death watch
  (`ShardState::council_death_watch`) is advanced **every period on every node** (leader, voter and
  learner), so the count is monotonic for a genuinely dead member and a fresh leader inherits the
  fleet-wide death history instead of restarting the window; a member seen alive again resets it. Only
  members past the window are passed to `reconcile_alive`'s `dead` set (defect 2). A live voter
  transiently declared dead is refuted and reset before it can be retired; the genuinely dead victim
  crosses the window and is taken over.

No timeout was raised; no consensus rule or Raft safety property changed. This restores "confirm
before an irreversible action" to the council's membership reconcile.

## Failing test first, and regression

- Failing evidence: the Linux repro (bounded container). Baseline: 1 failure in 8. Targeted capture
  named the stage, and the committed-log dump named the exact bad command (`TakeOver{fresh}` at the
  re-election term).
- Deterministic regression:
  `crates/cluster/src/config_group.rs::a_suspected_voter_is_not_retired_until_its_death_is_confirmed`
  — a member absent from `alive` with an empty `dead` set is not retired; once confirmed dead it is.
- The confirmation window itself is validated by the Linux repro (below); it is timing-dependent and
  has no deterministic unit form.

## Validation (bounded Linux container, `--memory=2g --cpus=4`, 2026-09-17)

- Baseline (before the fix): 1 failure in 8.
- Suspicion fix alone: 2–3 in 30 — a real improvement to the `Suspect` case, but the re-election drove
  the fresh voter to full `Dead`, so it did not close the flake.
- A leader-only confirmation window: it fixed the second loss but a leadership flap reset the window
  and stranded the *first* loss's retirement (`old` un-retired, 4 members) — rejected.
- Final fix (all-node watch + election-timeout window): **30/30**, then a further confirmation run;
  the whole-RAM history passes reliably. macOS was already green throughout.

## Siblings reviewed

`sync_membership` already folds a death only on confirmed `Dead` (a suspect is untouched) — correct,
not a sibling. `RootGroup::reconcile_voters` reconciles region **representatives**, not per-member
liveness in the same way, and the whole-RAM history keeps the root's sole voter alive; left for the
separate TBD_FIXES §3 check. No other caller of `reconcile_alive` exists.
