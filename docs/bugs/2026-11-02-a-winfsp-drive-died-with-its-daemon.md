# A WinFsp drive died with its daemon

Observed 2026-11-02, A-198 and Step 9.3 of the VFS and DAX plan.

## Description and evidence

The public mount oracle keeps an ordinary Windows file handle and mapped view open, kills the serving process,
and checks that the drive still names the original kernel volume. Diagnostic branch `diag-s9-mount-baseline-1`,
commit `6514b4cc81fedf074ac7a5f4d5c0d664301acbcb`, run
[41404129748](https://github.com/hyper-light/slates/actions/runs/41404129748), loses that target at the first kill
on both `windows-latest` and `windows-11-arm`. Each log prints `drive held false` before the assertion, and each test
reports zero passed and one failed. The diagnostic job succeeds only when that particular runtime assertion fails.
Command: `cargo test -p slates-cli --test winfsp a_winfsp_drive_handle_and_view_survive_twenty_killed_daemons -- --exact --nocapture --test-threads=1`.

## Root cause and impact

Before the change, the daemon alone owned WinFsp's volume control handle and the temporary DOS drive-link handle. Process termination
closed both. A-198 records the WinFsp v2.1 source evidence: cleanup of the last control handle deletes the volume,
and the last link handle's close removes the drive letter. The generic anchor hold from Step 9.2 was not wired to
these objects. Recovery also explicitly removed the WinFsp attachment record.

Each per-open context was a leaked `Box<OpenFile>` pointer. Keeping the volume alone would leave those
kernel-held pointers naming a dead process's heap. They must become generation-checked records in anchor-held RAM,
resolved by the owning shard rather than dereferenced on foreign dispatcher threads.

## Implementation and verification

The failing test was written and run before changing the mount implementation. The change:

- `bridge-winfsp::held` owns the temporary drive link and validates the fixed held-volume record. The mount offers
  duplicate control and link handles, and an unnamed paging-file section for its bounded open ledger. The server waits
  for `Held` before committing the attachment. Normal detach waits for `Released` before deleting the file system.
- `bridge-winfsp::opens` replaces each heap pointer with a generation/slot token. A CRC32C protects each fixed frame;
  a torn frame is quarantined. Recovery suppresses every predecessor bridge-handle value, so a reused local handle
  cannot redirect an old kernel context. The shard resolves file identity, including the purge-open mark, from RAM.
- `server::winfsp` retains a matching attachment, moves its inherited objects to the durable owner partition, and
  starts fresh DLL dispatcher state against the held control handle and volume name. The first coherence round covers
  the ledger's live names. Failed or uncommitted adoptions retain their ids until their anchor holds are released.
- The public oracle keeps a mapped view and a sector-aligned, unbuffered handle across all twenty kills. It checks the
  old handle's first read after adoption, then changes the file through a new SDK attachment, waits for the view to
  follow, and requires both an uncached read of the new bytes and movement of `winfsp.restored_reads`.

The first diagnostic, [41415062574](https://github.com/hyper-light/slates/actions/runs/41415062574), lost zero drives
of twenty on each architecture (5.53 s x86_64, 5.99 s arm64). The stronger oracle adds the uncached-read check above.
Diagnostics [41422186336](https://github.com/hyper-light/slates/actions/runs/41422186336) lost zero drives of twenty on both architectures; every iteration required a restored-context read, and the last daemon counted four. All five live WinFsp tests, 36 bridge tests, 32 active anchor tests, native clippy and Windows docs passed on each runner. With only the control-handle hold omitted, the unchanged oracle failed its first post-kill read with Windows error 995 on both runners ([41422382101](https://github.com/hyper-light/slates/actions/runs/41422382101)); the still-held link alone left a dangling drive letter.

The final positive tree is `26c79eeeffed287c929b97107f5bd8f6be77dbda` (`diag-s9-mount-fix-4`); its full live-suite
times were 31.06 s on x86_64 and 14.97 s on arm64. The red tree is
`24e51aa2a781a0f219ce1fc605d27b812b521cde` (`diag-s9-mount-red-3`): the anchor duplicates the ledger in place of the
control handle, retaining three valid inherited handles and leaving the link and ledger held. The same public test
fails after kill one, in 1.66 s and 1.80 s respectively. No diagnostic mutation is in the implementation.

A draft recovery incremented the shared header in place. A kill during that copy could have invalidated every open.
`recovering_opens_never_rewrites_the_shared_header` was added first: on jupiter,
`cargo +1.98.1 test -p slates-bridge-winfsp recovering_opens_never_rewrites_the_shared_header -- --nocapture` failed
with `Err(Bound)` when recovery tried to write the read-only model. The header is now immutable: recovery derives an
epoch beyond every valid live frame without changing shared memory. The full bridge suite then passed 35 of 35.
The anchor suite passed 15 unit, 14 integration and 3 supervised-handle tests; native bridge/server/CLI clippy and the
Windows documentation gate passed (jupiter, 2026-11-02).

A second review test caught the draft's zero sentinel. Zero is the first live handle in a fresh `VolumeBridge`
store, so a recovered close could settle a successor's own open of the same inode instead of its restored reference.
`a_recovered_handle_cannot_name_the_successors_first_open_of_the_same_file` drives the real bounded handle store:
on jupiter its first run failed, `the old open must not address the new one`. Recovered contexts now use the packed
word whose slot index is `u32::MAX`, beyond `MAX_OPEN_HANDLES`; it can never name an admitted local handle.
The restored-read counter uses that reserved word too. The corrected collision test and all 36 bridge tests passed on both Windows runners and macOS; the final positive and red measurements above include this correction.

The final positive diagnostics run all five live tests with
`cargo test -p slates-cli --test winfsp -- --nocapture --test-threads=1`. On macOS the final chain passed:
`cargo +1.98.1 fmt --check && cargo +1.98.1 clippy --workspace --all-targets -- -D warnings && cargo +1.98.1 test -p slates-bridge-winfsp --lib && cargo +1.98.1 xtask check && RUSTDOCFLAGS="-D warnings -A rustdoc::private_intra_doc_links" cargo +1.98.1 doc --workspace --no-deps --document-private-items`
(2026-11-02). The unsafe count is six lower overall: WinFsp 112 → 104, IPC 109 → 110, server 6 → 7.

## Remaining scope and sibling findings

Step 9.4 still owns the requests already taken by the dead dispatcher, hint reuse, close/cleanup settlement and
refusal of a foreign or second daemon. Its coherence proof must also cover restored references and replay-written
inodes beyond the live ledger's home names, including multiple links. A close returning a restored open must publish
that changed reference count before another kill; normal shutdown's cancellation path must release anchor holdings.
These are not established by a static two-open, twenty-kill history. The pre-existing delete-on-cleanup path also
resolves the current name instead of checking the old open's identity after a replacement; that sibling needs its own
failing test. Purging a removed name remains A-190's existing limit.
