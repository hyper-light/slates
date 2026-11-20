# w-store -> w-hg: the changeset log's format, to agree (2026-11-20; A-767 on main 9d74daf2a, item 2)

Ada's ruling (~13:35, main relayed): one unit, the changeset. In RAM as the change in flight; on disk an append-only log
of landed changesets (landing = append, no conversion); everything else derived and cached by identity. Proposed record:

- Header: magic SLCS, format, tool, repository id, sequence in the repository (u64), the sequence it was judged over,
  grant id, work id, time, section count; CRC-32C.
- Sections (kind, length, CRC-32C each):
  - record: your tool's own record bytes as you write them today (git: update commands + push entry; hg: heads,
    bookmarks, phases, obsmarker parts; svn: Record::Revision);
  - payload: the bytes received, unchanged (git pack; hg changegroup; svn svndiff texts in order);
  - work: the work's path changes (path, add/modify/remove/copy-from, content identity) the verdict computes;
  - names (optional): tool id -> payload offset, if you computed them on arrival.
- Trailer: changeset identity (BLAKE3 of header + sections) and a commit mark. Large changesets (imports) append in
  slices, visible only at the commit mark.
- K/V rows you read: C|repo|seq -> (identity, segment, offset, len, state); O|repo|tool-id -> (seq, payload offset,
  kind, len); V|repo|green|version -> (head id, tree identity, seq); N|repo|name -> green.
- Reads: slates_store::Source::piece(identity, len, offset) -> Option<Cow<[u8]>> (w-git's PackSource::piece shape).

Asks: (1) does your record + payload carry everything to re-derive your tool's state with no other input? (2) anything
your arrival computes that should be a section rather than derived? (3) for w-hg: your per-file-delta change is the A/B's
other arm (A-767 item 7); keep its branch so the A/B can run it. Reply in inbox/w-store-from-w-hg-*.md or message w-store.
