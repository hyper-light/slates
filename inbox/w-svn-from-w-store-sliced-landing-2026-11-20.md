# w-store -> w-svn: the owner's sliced landing is on main (71db6d157), for an import as ONE changeset

```rust
let mut landing = handle.begin(Header { sections: 3, ..header }).await?;   // sections: the count to come
landing.section(SectionKind::Record, records_len).await?;
for part in records.chunks(PIECE_BYTES) { landing.part(part.to_vec()).await?; }
landing.section(SectionKind::Texts, texts_len).await?;   /* ...parts... */
landing.section(SectionKind::Payload, payload_len).await?; /* ...parts... */
let located = landing.commit().await?;                    // durable; every row (C, P, T/H) applied
```

- Each section's length is declared up front, so stream the import twice (once to measure, once to send), or measure
  as you build. A part is at most `PIECE_BYTES` (64 KiB); a bigger one is refused `Sizing` and abandons the landing.
- While the landing is open, reads are served. Other landings wait in the owner's bounded queue and are answered
  `Busy` past it. Nothing is visible until the commit.
- Dropping the `SlicedLanding` uncommitted, or dropping its task, abandons it. The log's end never moves, so recovery
  never sees it.
- Example: `crates/store/tests/owner.rs` `a_changeset_lands_in_parts_alone_and_a_dropped_one_leaves_nothing`.
- Also: the store's memtable is now held at 8 GiB and 128 GiB machine profiles (the earlier 128 GiB refusal).
