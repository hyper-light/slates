# vfs: a recovered unlinked-but-open file whose holder died with the process was never reclaimed

**Date:** 2026-10-03. **Design:** §4.8 recovery images; §4.6 "Linux" (A-61). **Found by:** reading
`Volume::from_image` while designing A-61's reference handoff.

## Description

A recovery image restores the tracking of every orphan (a file unlinked while a transport held it open), so that
"a recovered orphan is reclaimed when its handle, reacquired through the anchor handoff, finally closes". No handle
was ever reacquired: no transport's references survived a restart. So an orphan's reference count after recovery
was zero, and nothing would ever take it to zero again. The file's content and its retention charge stayed in the
volume until the volume was destroyed.

## Root cause

Two pieces were owed together and neither landed:
- The volume attributed references to a process-local key (`AttachmentId::key`, "reconciling the two is owed with
  the server wiring"), so references could not be carried across a restart.
- Recovery kept the orphan tracking and had no step that released what no surviving owner holds.

## Impact

Every daemon restart leaked the content and retention of each file that was open-and-unlinked at the last
publication (a FUSE, virtio-fs or WinFsp client's open file deleted by another process). The leak was bounded by
those files, and reclaimed only by destroying the volume.

## Exact edits

- `crates/vfs/src/ids.rs` `RefOwner { Process, Attachment }`: references and opens are attributed to a typed
  owner. A process-local key can never be taken for a durable id it happens to equal.
- `crates/bridge-core/src/authority.rs` `Attachments::set_owner` and `OpContext::owner`: the volume bridge
  attributes an attachment's references to its durable record when the server names one.
- `crates/vfs/src/recover.rs` image version 10: every recorded attachment's references (`AttachmentReferences`).
  `Volume::from_image` restores them, refusing an absent inode or a zero count.
- `crates/vfs/src/volume.rs` `settle_recovered_references`: sweeps every owner whose record did not survive, then
  reclaims every orphan left with no reference.
- `crates/server/src/verbs.rs` `settle_references`: runs after each volume's attachments are reconciled, and
  counts what it reclaimed in the boot log.

## Proof

- `crates/vfs/tests/recover.rs` `a_surviving_attachments_references_are_restored_and_every_other_holders_released`:
  three orphans are held by a surviving record, a lost record and a process owner. After the settle, only the
  surviving record's orphan remains, and that record's forget reclaims it.
- Red-checked: with the restored counts zeroed, the test fails.
- `an_image_with_a_corrupt_reference_record_is_refused`: an image naming an absent inode or a zero count is
  refused.
- All 190 vfs tests pass, and the bridge crates' tests (65, 82, 46, 140). The server's lib tests pass (152), as
  do its recovery (14) and daemon (18) integration tests.

## Sibling sweep

Opens (`attachment_opens`, the base-file pins) are not carried: a base plane's recovery is refused as a whole (its
own gate), so no recovered volume has a base file to pin. Today no server path names a durable owner. The FUSE
takeover (A-61 step 3) is the first to; until it does, the images carry no references, and the settle releases
every recovered orphan.
