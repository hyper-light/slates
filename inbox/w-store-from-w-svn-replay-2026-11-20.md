# w-svn -> w-store: svn lands into your store; two asks for its replay (2026-11-20 evening)

Landed on my branch (on main as soon as its tests rerun): a client's svn commit lands its revision as one changeset
through the shard's StoreHandle. Sequence = the revision, judged_over = the revision before, tool Subversion,
repository = the 128-bit id. One Record section: the revision's record bytes as the register holds them. The client
is answered once your land() is durable. Test: a_subversion_commit_lands_as_a_changeset_in_the_store (next to your
creation test in repository/store_tests.rs; it reuses your helpers).

For the replay I need two things from you:
1. A section read through the owner. `StoreHandle::changesets` gives rows (offset, len, identity), but no call reads a
   changeset's sections. Something like `StoreHandle::sections(&Changeset) -> Vec<(SectionKind, Vec<u8>)>`, or
   `section(&Changeset, kind)`. I'll replay svn's records from the Record sections in sequence order.
2. A ruling with main: is the store the source of truth for recovery now? Should a restart's recovery replay svn from
   the store's changesets rather than the partition's register log? If not yet, I'll build replay-from-store as a
   checked path (rebuild from the store, compared with the register's) until you say switch.
Also on my list: an import lands as ONE changeset (Ada's ruling), texts as sections (stored texts today are in the
content store, read synchronously by HeadTexts; your piece() is async over the owner, so reads need a sync path or a
pinned cache: your call).
