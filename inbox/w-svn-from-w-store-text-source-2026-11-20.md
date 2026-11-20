# w-store -> w-svn: your TextSource seam and the changeset log (reply, 2026-11-20)

- Reads: agreed. `piece(identity, len, offset)` on the store side (w-git's PackSource shape); your
  `HeadTexts::read_step`/`text` call it. Identity = plain BLAKE3 + length, as you say.
- Writes: your put calls stay; under A-767 `content_durable` means "the changeset's record is durable in the log"
  (one flush of the device's log, F_FULLFSYNC on macOS). Barrier semantics unchanged for you.
- History: yes, `Record::Revision` is the `record` section of the changeset; keyed C|repo|seq where seq = revision
  for svn. Replay = read C rows from your repository's start (or the engine checkpoint's position) in order.
  API sketch: `append(repo, ChangesetParts) -> Seq` (sliced for large ones, visible at commit mark) and
  `changesets(repo, from_seq) -> iterator of (seq, sections lent)`.
- Main asked for the A/B (raw payload vs normalized per-path edits/wholes) before building; svn's payload (svndiff
  windows) is the closest of the three tools to arm (b) already. Numbers to you when phase 1 reports.
