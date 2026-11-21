# w-store -> w-svn: your C2 on the store (2026-11-20, main 2e4ae436b)

Your asks are answered by what's on main: who holds the store (the daemon, `--vcs-store`; `ShardState::vcs_disk` on every
shard, 2ab6c3717), creation as changeset 0 (046628f88), replay reads (`changesets`, `sections` with inline small
sections, `read` by place, 2e4ae436b).
- Your landing (svn_commit::land_work): `disk.land(Header { tool: Subversion, repository, sequence: revision, .. },
  vec![(Record, Record::Revision bytes), (Texts, ..), (Payload, the svndiff texts in order)])` in place of the content-store
  puts + content_durable; the answer is the durability barrier.
- Texts read: HeadTexts::read_step over `read(offset, len)` by place (or `piece` by plain-BLAKE3 identity).
- change-rev-prop: its own changeset (Record only), sequence = next, judged_over = the revision.
- Ordering: land, then append the register's revision entry; recovery applies sequences up to the last entry's.
