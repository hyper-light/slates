# A region read waited out a dead first home, and a push's barrier gave up first

- **Date:** found 2026-11-27 (a914, main's 16 runs at 526859d1e, runs 1 and 16), diagnosed and fixed the same day
  (main's ruling of 12:37).
- **Status:** fixed (the read). Its siblings are reported in GAPS.
- **Subsystem:** the region's homes (`crates/server/src/vcs_store/homes.rs`: `read_in_region`, `read_remote`,
  `read_in_parts`), as a holder's replica store replays (`vcs_store::rebuild_sliced_from`).
- **Severity:** a push during a member's death was refused `unable to commit` at its record barrier, 2 of 16 runs. No
  durability loss: every object the replay needed was served by a live home.

## Symptom

The second death's push in a914 was refused at its barrier: version 2 of the green was placed on its owner alone
(placed `Some(1)`), and the other survivor's replica was `building {C: 0}`.

## Root cause

The diagnosis comes from a914 rerun locally with every node's counters dumped. In run 18
(`~/.local/state/w-interop-logs/a914-run-18.log`):

- The survivor C held the green's records, but its replica owed the green's push (P2). It began its replica's build
  when version 2's record arrived (`replica_build::wanted`).
- That build first replays the repository's replica store, reading each name the node lacks from the region
  (`homes::read_in_region`).
- `read_remote` asked a name's homes one at a time, in rendezvous order over the configuration's members, which still
  include a member that has died but is not yet taken over.
- Each home's `exchange` waited until that home answered or the whole live budget (about 2 s) was spent. A name whose
  first home had just died therefore cost 2 s before its live home was asked.

C's counters: `merge.replica_build.made_ns` 2,010,340,750, and `names_fetched` 3 with `remote_served` 19, every name
served by a live home. The log line just before the barrier's refusal: "a region exchange ended with [B] unanswered
after 1998031208 ns". The barrier gave up first.

## Exact edits

- `read_in_region` orders the homes live first by this node's own view (`live_first`: alive, suspect, dead). It reads
  the view once per read and keeps the rendezvous order within each rank, so one view always orders them the same way.
- `read_remote` gives each home its share of what is left of the read's budget (`share_of`): the rest divided over the
  homes still to ask, the last given all of it, so the shares never sum past the budget.
- `read_in_parts` holds every part of one home's read to that home's one share. A home cut off at its share is
  abandoned by its exchange (`slates_cluster::broadcast`'s deadline), and its session goes back on its link.
- Test support: `Daemon::home_bytes_in_region`, `read_from_region` and `homes_of_identity`.

## Verification (MacBook, debug, 2026-11-27)

- Reds before the fix: a dead first home, and a live one slower than its share. Each read came back only after
  1,998,960,833 ns against a 1,998,000,000 ns budget (`~/.local/state/w-interop-logs/a914-read-red.log`).
- After the fix, both pass:
  - the dead first home, read in 1,000,226,917 ns;
  - the slow one, read in 1,000,508,666 ns, its session back in its link (`SessionHold::Took`, waited 86.6 ms).
  Each is one home's share of the 1,998,000,000 ns budget plus the second home's answer
  (`~/.local/state/w-interop-logs/a914-read-green.log`).
- Owed (main): a914 16 times, with A-914 (C).
