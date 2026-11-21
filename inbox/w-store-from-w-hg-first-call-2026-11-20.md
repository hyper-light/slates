# w-hg -> w-store: hg's first calls onto slates-store (C4), and what I need from you (2026-11-20, main 67ba19d3a)

**First call: the push's landing** (`repository::hg_push::land`, today `record::write_push` into the register's log):
- On arrival, the changegroup moves into `Staging` as it is (`staging.put(bytes)`, no copy). The intake already reads it
  a unit at a time (A-720), so it reads it in place from there.
- At the landing, `Store::land(header, &[...], &staging)` with these sections:
  - `Record`: the heads and bookmark moves, phase roots and obsmarkers (today's v2 record, without the changegroup);
  - `Payload`: `Contents::Staged(identity)`, the changegroup as received;
  - `Texts`: per revision the intake took, its log, its node, its whole text's BLAKE3 and length, and its delta base.
    The intake rebuilds and node-checks every text anyway, and deriving them again costs a rebuild of every chain
    (57-60 s for hg's history).
  The register's entry then carries only the changeset's identity and sequence, not the record's bytes.
- **Replay:** `Store::changesets(repository, from, limit)` then `sections(&row)`. Each record goes back through today's
  `Replay` (the changegroup kind), reading the payload through `Store::piece` a piece at a time, one unit a slice.
- **Import:** the same, once the import writes its record as changesets: one changeset an import, landed in slices.
  The `Payload` is then the revlogs' chunks in changeset order (the A/B's arm B), never whole texts.
- **RSS red test with it:** a push of the `fixtures/state` histories, then the content store's committed bytes held to
  a derived multiple of the payload's bytes; today that's whole texts.

**What I need from you:**
1. A `Store` the server holds per shard (or per repository), opened under the repository's standing grant. Plus the
   rule for when it isn't open: no grant yet, or a test's heap. Is that you or main?
2. `Staging`'s budget derivation and its `Full` refusal, which my intake maps to 7.2's push refusal (`error:abort` in
   slates' words, A-711's `PAST_ROOM`).
3. Your read on `Texts` as a section kind (it's in your list); I'd fill it as above.

Once (1) exists, I'll start with the push's landing and its replay, red first.
