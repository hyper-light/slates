# A clone of an older snapshot was judged by the head's witnesses (2026-09-30, A-48)

Contracts: §4.5 (the base plane), §4.15 (the landing verdict), D-26. Found while preparing AUD-29-02's
exact snapshot landing, which needs each snapshot's own witnesses.

## Description

A landing's verdict for an entry is a pure function of three inputs: the witnessed base (what the
agent's edit was based on), the disk now, and the overlay. The base plane kept its witnesses, their disk
homes, and the fingerprints behind whiteouts and redirects in tables keyed by inode, and those tables
were the head's only. Snapshots froze the directory tree and the inode table, but not these.

`Volume::clone_of(snapshot)` gave the clone the snapshot's tree and a copy of the origin's *head*
tables. Suppose the head rewitnessed an entry after the snapshot: an outsider changed the disk, and the
agent took the change in. A clone of the older snapshot then held the agent's old edit, based on the old
base, next to a witness of the outsider's bytes. Its landing saw "disk unchanged since the witness,
overlay changed", answered `Apply`, and replaced the outsider's file.

## Root cause

The witness tables were not part of copy-on-write. Every other input to a snapshot's verdict was
versioned by epoch; these were overwritten in place.

## Impact

- **A lost update without a conflict.** An outsider's change was silently overwritten by a clone's
  landing, the failure the landing verdict exists to prevent (D-26).
- **No exact landing of an older snapshot.** Nothing kept the witnesses an older snapshot was based
  on (AUD-29-02).

## Exact edits

- **`crates/vfs/src/base_versions.rs` (new).** `Versioned<K, V>`: per key, its values by the epoch
  each holds from.
  - A snapshot frozen at epoch `e` reads the last version at or before `e`.
  - A write overwrites the key's last version when no live snapshot reads it, and appends otherwise.
  - A snapshot's destroy keeps only the versions some remaining snapshot or the head reads, so a key
    holds at most one more version than the live snapshots.
  - `view_at` starts a clone; `versions` and `restore` serve the recovery image, which refuses a
    history out of order.
- **`crates/vfs/src/base.rs`.**
  - The four tables (witnesses, their homes, whiteouts, redirects) are `Versioned`, and every write
    goes through a plane method stamped with the volume's `witness_clock` (the head's epoch and the
    newest live snapshot's).
  - `for_clone` takes the snapshot's view.
  - New reads: `witness_at`, `Volume::witness_in`, `whiteout_witness_in`, `redirect_witness_in`, and
    `diverged_in`, which shares its walk with `diverged`.
  - `prune_versions` and `witness_versions`.
- **`crates/vfs/src/volume.rs`.** `clone_of` passes the snapshot's epoch, and `destroy_snapshot` prunes
  after its deadlist hand-down, which is now its own function.
- **`crates/vfs/src/base_recovery.rs`, `recover.rs`.** The image carries every version: layout 7. The
  head's witnesses still each need a home in a reacquired directory.

## Evidence

- **Red.** `crates/land/tests/source.rs`
  `a_clone_of_a_snapshot_from_before_a_rewitness_conflicts_rather_than_overwrite_the_outsider`, run on
  `c5b47cb`: the clone's landing ended `Done` with verdict `Apply`, and the disk's "outsider" became the
  clone's edit.
- **Green.** The same landing is refused `Conflict(ModifyModify)`, and the outsider's bytes survive.
- **Unit and VFS tests.**
  - The versioned table against a model over generated histories (2,000 cases each): every live
    snapshot reads what it froze, the bound holds after every step, and a clone starts from its
    snapshot's values.
  - A restored history must run forward.
  - `crates/vfs/tests/base.rs`: a snapshot keeps its witnesses and whiteouts through a later
    rewitness and a later whiteout, and its diverged set is the one it froze; a clone starts from its
    snapshot's witnesses; a snapshot's destroy leaves the head's three versions; a recovered snapshot
    keeps its witnesses.
- **Suites.** vfs whole (base 25, recover 33, model 10), land whole, server recovery 8 and daemon 17,
  client 5. Clippy clean over the workspace; xtask check ok.

## Open

- The base plane's tables are heap, outside the shard's metadata ledger (§4.2), and versioning
  multiplies them by the live snapshots. The bound is structural (live snapshots + 1 per key), not an
  admitted charge.
- A file created over a whiteout keeps the whiteout's record at the head (observed while writing the
  tests; existing behaviour, unchanged here). How the planner then presents that file — a create where
  the disk has the name, or a replacement — is unverified and owed a test.
