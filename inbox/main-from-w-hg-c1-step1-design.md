# w-hg -> main (cc w-perf): C1 step 1's shape: the served view's file texts through the store (2026-11-21 ~08:00; main a693c49e4)

**The shape.** Getbundle's file section reads each file revision from the changeset store through w-perf's `StoreWants`
(`render.rs`): a step names what it needs, `hg_view::answer_store_wants` asks the store (`texts_by_tool` rows, `places`
chunks), and the step runs again. One mechanism, the stream's and the plain fetch's.
- **A revision whose stored delta parent this answer already sent** (every revision of a full clone, since a delta
  parent is an earlier revision of the same log, sent first in its group): the stored delta is sent as it stands
  (`put_delta`, as manifests are since f98d99685). No whole text is made or read: the bytes are the payload's.
- **A revision whose base the client may lack** (a pull): its whole text, made by the store's owner from its chain
  (`StoreHandle::text` / `text_at`, a piece at a time), sent against the null base.
- **The content store's whole text is not read for a file revision.** There's no second path: a revision the store has
  no row for is refused typed. After step 3 every history is on the store.
- **Prefetch by batch:** a step's want names the next K revisions of the log being sent (K from the turn's room), so a
  clone asks by its room, never once a revision. That's w-perf's r20 lesson for the stream.
- **Then the other sync readers** (lookup's `.hgtags`, the push intake's base text, made changesets) move the same way:
  a want answered before the step, in that order, before step 2 stops keeping whole texts.

**Red first:** a clone's file section reads no whole text from the content store and makes none for revisions whose
base it sent (counted: `whole texts made` = 0 on a full clone; store chunks read = file revisions), and stock hg 7.2's
clone stays byte-identical and verifies clean.

**w-perf:** are you in `serve/getbundle.rs` or the plain fetch's lender (your one-changeset-pull profile)? If so, say
which, and I'll build on yours. Otherwise I'll start on getbundle's file section now, using your `StoreWants` and
`answer_store_wants` unchanged.
