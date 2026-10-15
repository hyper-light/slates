# A reclaimed base file leaked its descriptor; refused re-checks and undos were dropped

**Found:** 2026-10-07, in the sweep of discarded results begun by
`2026-10-07-a-seal-refused-partway-could-leave-ciphertext-marked-plaintext.md`.

## Description

1. **Descriptor leak.** A base file whose descriptor the overlay holds is one that was read, a large-class copy, or
   one read through. Unlinked through the overlay, it is reclaimed by `Volume::reclaim_inode`, which has no host.
   It called `base_forget`, which takes the descriptor out of the plane's table and returns it, and discarded the
   return. The comment said the host's owner would close it at the next `process_hints`, but nothing could, because
   the table no longer named it. On a real disk, one file descriptor leaked per unlinked base file until the daemon
   hit its descriptor limit. Reproduced with `SimHost`: three handles open after the unlink, where two (the root and
   `src`) should be.
2. **A refused drift re-check read as a pass.** `status` and the watcher's drain discarded `check_drift`'s refusal.
   A host I/O error while re-checking a witnessed entry made the entry look as if it matched its witness. Reads,
   copy-ups and landings check for themselves, so nothing landed wrongly; but a person deciding to land was shown a
   clean status for an entry nobody had checked.
3. **Dropped undos.** Every rollback after a refusal discarded the undo's own refusal:
   - an inode or a directory node just installed and taken back;
   - a rename's restore;
   - a directory tree built and discarded;
   - an open extent released after a refused spill;
   - recovery's give-backs of claimed blocks.

   A refused undo leaves something the verb made in the store, and the caller was told only the first refusal.
   Retired-object releases (`release_dead`) likewise dropped their refusals.

## Fix

1. A reclaim with no host queues the descriptor (`BasePlane::closing`, bounded by the descriptor table each one
   left), and an `Overlay` borrow closes the queue when it ends (`impl Drop for Overlay`). A verb run through an
   overlay closes what it reclaimed, and a reclaim without a host closes at the next borrow.
2. `BaseStatus::unverified` lists each entry whose re-check the host refused, with the refusal, and the daemon's
   status reports it as `PATH (unverified: …)`. The watcher's drain re-queues such an entry's directory, so it is
   checked again.
3. `VfsError::after_undo`: a refused verb returns its own refusal, unless its undo was refused too; then it returns
   the undo's, which names the worse state. This is `abandon_attribute`'s rule, now applied everywhere.
   `uninstall`, `DirTree::discard` and recovery's `give_back` try every step and return the first refusal.
   `release_dead` stays idempotent but counts its refusals (`Store::release_refusals`, status
   `store.release_refused`).

## Tests

- `crates/vfs/tests/base.rs` `an_unlinked_base_files_descriptor_is_closed`: failed before the fix with 3 handles
  open, not 2.
- `crates/vfs/tests/base.rs` `a_drift_recheck_the_host_refuses_is_reported_unverified`, using the new
  `SimVerb::OpenFile` fault.
- vfs and base: 37/37 suites. Server lib 160, daemon 20, recovery 27.

## Note for overlay users

An `Overlay` now closes descriptors when dropped, so it holds its borrows until its scope ends. Three test and bench
call sites that used the volume or host again in the same scope now borrow the overlay in a block.
