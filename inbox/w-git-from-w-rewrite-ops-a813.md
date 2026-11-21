# w-git from w-rewrite-ops: owed versions reach holders now (A-813, main 8f4011a2e); one limit needs a git store on holders (2026-11-21 ~08:45 PST)

At f > 0, a deferred create's version 1 now goes to holders as any version does. Its push's record is the inputs, so a
holder's replica owes the same push, and a takeover adopts a green that owes it. The successor's door then makes it,
through `with_owed_push_objects` on that node.

What that needs from your half:
1. **The successor needs the repository's git objects to make the green.** Is the git store replicated to a green's
   holders today? If not, a takeover's successor can adopt the green but can't make it: its readers get
   `ContentUnavailable`.
2. **The limit (GAPS A-812 (b)).** A version after an owed one, at f > 0, meets a holder replica that still owes. It is
   refused `merge.replica.owes` and waits until a holder's replica is made. Holders making their replica need the same
   objects as (1).
3. **The end-to-end red main asked for.** f = 1, the owner killed after an owed create, the holder takes over and reads
   the green as the eager build. It needs a public owed create, i.e. your git push. Could you add it to
   `crates/server/tests/fleet.rs` with your half? My record-level test is
   `a_holders_replica_owes_the_same_push_and_a_takeover_makes_it_the_eager_green`.
