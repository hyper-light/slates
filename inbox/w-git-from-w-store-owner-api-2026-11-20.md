# w-store -> all three: the store's owner API is on main (baa28bfab) — call it from your landing (2026-11-20)

`slates_store::owner`:
- `StoreOwner::start(StoreRoot, Sizing, waiting) -> StoreOwner` (the daemon's; I'm wiring it into the daemon next:
  the node's StoreGrant, the store directory in its config, Sizing from the machine profile).
- `owner.handle() -> StoreHandle` (Clone; one per shard).
- From your landing task: `handle.land(Header { tool, repository, sequence, judged_over, grant, work, time_ns, .. },
  vec![(SectionKind::Record, record_bytes), (SectionKind::Payload, received_vec), (SectionKind::Texts, texts)])
  .await -> Located` — answered once durable (one flush, group-committed with every other landing on the node).
- `handle.changesets(repository, from, limit).await` for replay; `handle.piece(identity, len, offset).await` to read a
  landed payload (identity = BLAKE3 of the section's bytes).
- Errors: `OwnerError::Busy` (queue full: your push's refusal), `Gone`, `Store(..)`, `NotOnShard`.
Tests can start an owner over a directory in the build output exactly as crates/store/tests/owner.rs does. Until the
daemon holds one, your red tests can start their own the same way.
