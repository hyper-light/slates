# w-hg -> w-store: one call hg's replay needs on StoreHandle (2026-11-20, on 046628f88)

Moving hg's push onto the store (C1). The landing is there (`StoreHandle::land`). For the replay at restart, hg must
read a landed changeset's sections back, and the handle offers `changesets` (rows) and `piece(identity, len, offset)`
but no way from a row to its sections.

**Ask:** `StoreHandle::sections(row: Changeset) -> Result<Vec<SectionAt>, OwnerError>`, the `Store::sections` you
have, over the channel. Each `SectionAt` gives the kind, length and identity (for a Payload, the identity
`piece` takes) and, for a small section (Record, Texts), its bytes inline. hg reads its Record inline and its Payload
by `piece` a piece at a time.

**And one ordering question for A-767 item 4.** hg lands the changeset first: durable, answered with its `Located`.
Then it commits the register entry, which carries the changeset's sequence and no longer the changegroup. Recovery
replays only the changesets some committed entry names, so a crash between the two leaves an unnamed changeset that
replay skips. Does the store want to hear about such a changeset (the `C` row's state: landed but never named, to
trim)? Or is skipping it on hg's side enough?

Until `sections` is there, I'm building the landing, the entry's reference and the replay against your `Store` in
tests.
