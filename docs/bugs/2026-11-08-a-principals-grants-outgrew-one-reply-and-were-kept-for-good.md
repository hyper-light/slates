# A principal's grants outgrew one reply, and were kept for good

**Found** 2026-11-08 by the sibling sweep of `2026-11-08-a-volume-listing-outgrew-one-reply.md`, while paging `grants`.

**Reproduced by use** (a two-shard daemon, a landing presented and approved again and again): `grants` was refused
"reply too large for the bulk area" between 25 and 50 grants. Paging it found two more defects under the reply:

1. **The table kept every grant for the daemon's life.** A consumed, expired or revoked grant stayed in the durable
   table and in the runtime one, was listed, and was copied into every partition snapshot, against banned item 8.
   Nothing needs a grant after it ends: a used-up grant presented again is refused whether its record is there or not,
   and its history is the audit's (`grant_issued`, the landing's records).
2. **An expired grant was never ended.** Nothing looked at a grant's term but a landing presenting it.

And a bound: a `land` request carries a target of about 4,046 bytes, and a grant is listed with its target, so a grant
on one page could need more room than the page holds.

**Fix (A-266).**
- `Grants { after }` is answered by one page, `Grants { grants, next }`: the caller's grants in grant-id order past the
  cursor, sized to one reply chunk less its framing, each shard sending at most a page (the gather the listing uses).
  The client walks the pages.
- The table holds live grants: at a terminal step (consumed, expired, revoked) a grant leaves the durable table in
  `apply` (`change_grant_state`) and the runtime one once that commits (`end_grants_recorded_ended`). The partition
  keeps the highest id ever issued (`grants_issued_through`, snapshotted), so a grant id is never reused and an absent
  id at or below it is refused as spent (`GrantInvalid`), never taken for a new presentation.
- The reap tick ends grants past their term (`end_expired_grants`, counted `landing.grants_expired`).
- A landing target is refused `TargetUnavailable` past the longest a grant's summary holds alone on one grants page
  (`longest_grantable_target`: the page's room less a summary with an empty target in its longest state, 3,995 bytes),
  before the daemon touches the host. Landing runs on macOS and Linux: on macOS a path past 1,023 bytes (`PATH_MAX`
  less its NUL) cannot be opened anyway, so the bound binds on Linux, where `PATH_MAX` is 4,096.

**Tests** (`crates/server/tests/grants_pages.rs`, two shards): 300 grants listed each once in id order (refused before);
a walk under churn lists every grant issued before it exactly once; a target one byte past the bound is refused naming
both lengths; 60 used-up grants are not listed, each one's `grant_issued` stays in the audit, and a used-up grant is
refused `GrantInvalid`; a grant with a one-millisecond term leaves the table at the next reap though nothing used it,
and is then refused `GrantInvalid`. `crates/db/tests/grants.rs`: 40 grants ended at each terminal step leave the table,
and a recovery from a snapshot and the log keeps the same table and the highest id. `daemon.rs`'s grant scenario: the
consumed grant is not listed, and its history is in the audit.

**Open** (GAPS): an expiry writes no audit record (the audit's kinds have no expiry), so an expired grant's history is
its `grant_issued` alone, which names no term.
