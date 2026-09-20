# FIFO/socket snapshots refused by the merge service

Date: 2026-09-20. Scope: A-26, §4.16, AC-6.8 and AC-6.13.

## Reproduction and cause

`cargo test --offline -p slates-server --lib ipc_snapshot_seeds_a_replayable_green_origin`
failed in 0.00 s with `SpecialFileOperation`. The fixture creates a FIFO and socket,
hard-links each, freezes the namespace, and calls the service's real snapshot walker.
Log: `/private/tmp/slates-ipc-origin-red.log`. An earlier daemon-backed fixture hit the
sandbox's `shm_open` refusal before reaching the bug; the final fixture needs only RAM.

The walker explicitly rejected both kinds. The separate merge origin, base and engine had
no IPC metadata dimension. Treating them as empty regular files would lose their kinds,
make incorrect conflicts possible, and allow content operations on non-content inodes.

## Change

- Origin format 2 records FIFO/socket kinds, owners and four timestamps; modes remain the
  independent mode dimension. Hard-link aliases point to the captured primary inode.
- `WorkOp::Mknod` carries explicitly declared IPC metadata. `VolumeOp::Mknod` composes it;
  operation tag 15 references a fixed metadata payload, never stream contents.
- The engine hashes, conflicts, versions, replays, rebases, invalidates and folds IPC metadata.
  Unknown kinds and malformed metadata payloads refuse. File-content operations cannot compose
  against IPC nodes. A create/unlink cancels. A deletion clears live mode/xattrs and preserves
  the historical metadata; aliases observe and invalidate with their inode.
- Appending new paths to an ops document now remaps its link/rename and xattr-name references
  as well as the operation's path. Otherwise sorting IPC names can redirect an existing link.
- Retention admission reserves the old values being superseded. Incoming payload size was
  not a bound: a one-byte edit can retain a 2 KiB file, and unlink carries zero bytes but
  retains the old IPC metadata value. The same correction covers regular-file unlinks.

The old path-table canonicalizer was restored temporarily for a controlled regression run:
`cargo test --offline -p slates-merge --test special ipc_names_preserve_other_namespace_references_when_paths_are_sorted`
failed in 0.00 s with symlink target `Some("a")` instead of `Some("target")` after an IPC
unlink inserted `a` into the sorted table. The remapping fix was then restored.
Log: `/private/tmp/slates-ipc-reference-remap-red.log`.

A delayed chmod also went red after the FIFO was unlinked and replaced by a regular file
whose mode already equalled the requested mode: the old metadata comparison accepted it.
`cargo test --offline -p slates-merge --test special an_ipc_metadata_change_conflicts_after_the_path_changes_kind`
failed in 0.00 s (`/private/tmp/slates-ipc-metadata-kind-red.log`). Metadata verdicts now
compare the historical IPC kind before considering equal modes or xattr values; the sibling
xattr set/removal cases are exercised as well. Invalid MKNOD path-table references refuse
before replay can create an unnamed inode.

The unlink regression first reported mode `Some(384)` for an absent inode
(`/private/tmp/slates-ipc-unlink-red.log`). The alias regression first reported no mode
instead of `Some(416)` (`/private/tmp/slates-ipc-alias-red.log`). Both are now gated by use.

The initial IPC retention oracle used the 41-byte wire encoding as its expectation. Replacing
that expectation with the real Rust value size made the Linux wire regression fail in 0.12 s:
charged 41 bytes, expected 48 (`/private/tmp/slates-ipc-ram-charge-red.log`). Retention, reservation,
recount and folding now use `size_of::<SpecialNode>()`, so alignment padding is charged on every
target. The wire encoding remains 41 bytes. This corrects the oracle as well as the ledger.

## Boundaries and sibling findings

This does not import a host FIFO, activate a device, transfer queued bytes or preserve a
socket listener. Origin format 1 is refused after the format change; there is no legacy decoder.

The merge service still uses a path-based work journal rather than mounted VFS work volumes.
Its existing symlink/hard-link rename and cross-kind composition gaps remain. IPC rename,
metadata declarations through an alias, and unlink of a primary IPC name while aliases exist
remain typed refusals until the inode-aware namespace journal is implemented. Snapshot aliases
and metadata reads are preserved now. The Rust wire/client declaration is available; dedicated
Python/Node/CLI/MCP creation convenience methods and mounted green/work volumes remain owed.
These are explicit limits, not evidence that the full A-26 merge namespace is complete.

Existing non-IPC namespace removals also retain stale mode/xattr entries, and the existing
origin decoder does not comprehensively validate cross-table namespace collisions. These
sibling issues require their own regressions; this change does not claim to close them.

## Verification

- Original snapshot-walker regression: green in 0.00 s;
  `/private/tmp/slates-ipc-origin-green.log`.
- Linux 6.12 `io_uring`, ordinary user in the approved disposable container:
  `cargo test --offline -p slates-server --test daemon declared_ipc_metadata_merges_over_the_wire_and_retains_its_old_version -- --exact`
  passed in 0.12 s. It crosses the real ring, proves differing owners conflict, and proves
  unlink charges the retained inode. `/private/tmp/slates-merge-ipc-linux.log`.
- Full merge tests, formatting, workspace Clippy and `git diff --check` pass on the host.
- Final Linux validation on 2026-09-20: `cargo clippy --offline --workspace --all-targets --
  -D warnings`, `cargo xtask check`, and `cargo test --offline --workspace` passed in the
  approved four-CPU, 4 GiB disposable container, using `io_uring` as an ordinary user.
  The workspace reported **1,540 passed, 0 failed, 14 ignored**; all 49 fleet tests passed
  in **271.81 s**. This includes the corrected RAM-size oracle and mounted FUSE regressions.
  Log: `/private/tmp/slates-merge-ipc-workspace-ram-verified.log`.
  Pjdfstest is a separate gate: its 1,800 recorded failures remain open.
