# main from w-rewrite-ops: A-812's three robustness gaps fixed (A-813, slates main 8f4011a2e); one limit open (2026-11-21 ~08:45 PST)

## 1. Holders at f > 0 (R8)
The owed version's merge record is enqueued as any version's.
- Its inputs are the push's record, and its identity is the owing green's, which is its base's.
- A holder applies inputs through one function, `merge_service::apply_inputs`, which both the record's recomputation and
  the ledger rebuild use. A push's record makes the replica owe the same push.
- Replica versions are read through `Green::reached()` (the owed version and its tail), so a takeover adopts at the
  version the replica stands for. The successor's door then makes the green.

Test: `a_holders_replica_owes_the_same_push_and_a_takeover_makes_it_the_eager_green`.
- The shipped inputs are the push's record.
- The replica owes version 1 at the record's identity.
- A later increment is refused while it owes.
- Made, the adopted replica reads as the eager green: head, files, identity.

It runs at the record level, in the N=1-differential spirit: the owner's shipped record goes through the holder's own
apply path. The end-to-end f = 1 kill test needs a public owed create, i.e. git push through a daemon, which is w-git's
half. I've asked for it once their create defers.

Before A-813, an owed version shipped no inputs at all (`inputs_of` couldn't read a GDEF entry), so no holder could ever
take it.

**OPEN, the limit, and your call.** A version committed after an owed one, at f > 0, meets a holder replica that still
owes it. The replica can't judge an increment without content, so the record is refused (`merge.replica.owes`, counted),
and that version waits for placement until some holder's replica is made. The fix: holders make their replica through
the same door over their tool's objects. That needs the repository's git store on holders. Is it replicated today? If
not, it's w-git and w-store's. Strictly better than before, where version 1 itself never placed, but it blocks later
versions of an owed green at f > 0 until a holder can make it. It's in GAPS A-812 (b).

## 2. Retention, secured before the commit
Once the build is done and before `materialize` commits, the work's history and change index are secured through the
submit's own `verbs::secure_retention`. Past the room, the parked requests get `BudgetExceeded { available }` and the
green keeps owing (`merge.green.materialize_over_room`).

Test: `a_materialization_past_the_greens_retention_room_is_refused_before_it_commits`.
- With the green's charge held at its cap, the build is refused, the green still owes, and nothing is recorded.
- With room, the next read makes it.
- Without the fix it committed past the room.

## 3. Tail signatures (A-321)
A verdict recorded for a version an owing green stands for (up to `reached`) is kept and recorded once materialization
applies the version.

Test: `a_signature_recorded_on_a_version_in_the_owed_tail_is_kept`. It was red: the verdict was refused while the green
owed.

## Tests
Focused, after merging main (which has your round 17, 62eb2ed10):
- slates-merge lib 259, engine 50, checkpoint 5, chain_replay 6.
- server: push 96, git 79, hg 71, svn 104, merge 40, green 43, vcs_store 109, replica 12, holder 46, adopt 20,
  origin 21, takeover 31.
- fmt clean.

For your queue: clippy on both crates, and the fleet suites (holders, adoption).

## Next
svn stage C, my half, against the proposed split.
