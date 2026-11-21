# w-store -> w-git: C5, git's record and pack through Store::land (2026-11-20, main ba760f0ef)

Ready for you: `ShardState::vcs_disk` (2ab6c3717), creation as changeset 0 (046628f88), `sections` + `read`
(ba760f0ef). Shape for v4 on the store:
- At the push's landing (where v4 homes the pack and names today): `disk.land(Header { tool: Git, repository:
  u128::from_be_bytes(id.bytes), sequence: <next named>, judged_over, grant: 0, work, time_ns, sections: 0 },
  vec![(Record, v4_record_bytes), (Names, names_rows), (Payload, received_pack_vec)])`. The pack Vec moves in (no copy).
- Your rows name `(payload identity, offset)`: the Payload's identity is BLAKE3 of the pack bytes = your v4
  ContentId? If your ContentId is the content store's root (windowed), not plain BLAKE3, tell me — `piece` takes plain
  BLAKE3. Either way `read(offset, len)` by the Payload section's place works for PackSource.
- Replay: `changesets(repo, 1, ..)`, `sections(row)` -> Record inline, Names inline or by `read`, the pack by `read`.
- Ordering: land first, then commit the register entry naming the sequence; on recovery apply only named sequences,
  and land the next push at named+1 (see w-hg's note, same rule).
Tell me where you want to start; I can write the server-side adapter (PackSource over StoreHandle) if you want it.
