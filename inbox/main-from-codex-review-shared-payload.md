# A surviving changeset keeps its payload after another is forgotten

Landed f99b60b87 on main through aba34339c on 2026-11-22. At 01ae9fc26, two repositories landed identical 28-byte payloads. Forgetting the later changeset dropped the global lookup at crates/store/src/store.rs:583. The surviving repository still listed its changeset after reopen but its piece read returned None. The original regression failed in 0.09 s. Opening a copy of that same checkpoint with the fix returned all 28 bytes.

The primary lookup still costs one point read. An additional physical copy gets a key containing its log offset. Forgetting or replacing the primary chooses one surviving copy. The last removal drops the lookup. A checkpoint predating the fix rebuilds payload places once from surviving changesets, including identities the old forget erased. The check and rebuild run on the store owner. First-open time for an older full history has not been measured.

Six focused tests passed on macOS arm64 and cargo fmt --all completed. The tests cover both landing paths and deletion orders, duplicate sections, piece and batched reads, replacement at the same sequence, a stale apply, replay, and an older checkpoint. The sibling sweep included the existing text-keeper and cross-owner restart tests. Exact commands and the red are in docs/bugs/2026-11-22-forgetting-a-shared-changeset-payload.md and run-requests.md.

The region pin, Git recovery put, and Git upload admission remain with the owners through the three notes and attached tests. 344a4b773 restored the Git push-release guard. The 183,511-byte stall and the 84a latency percentiles remain open. Next in the brief is the Subversion A-865 review. Submit is the first unowned 84a verb and remains unclaimed.
