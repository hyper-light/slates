# §4.16 merge Green/Work service — wiring status

> Status: the `slates-merge` engine (verdict, deriver, splice, chain, position map, ops document) is
> built and tested at the crate level; this note tracks wiring it into the server, client and CLI
> (Phase 6 tasks 1, 6, 7, 8). **Landed:** green volumes and the chain read side (`CreateGreen`,
> `Versions`, `ChangedSince`); the submit flow (`CreateWork`, `Edit`, `Submit`) for a fresh *and* a
> non-empty green (derived against the current base or a reconstructed intervening base), accept and
> conflict; `Rebase`, the corrective path (map a work's pending operations onto the head without
> committing, or return the windows); and `Declare`, the namespace and metadata operations beyond
> content (unlink, rename, mkdir, rmdir, mode, symlink, hard link, xattr) — all through the real verbs.
> **Owed:** `advance` (the attachment re-pin), cross-shard submit, chain persistence, and the non-Rust SDKs (the Rust CLI verbs are wired).

## What is wired (server + ipc + client)

- **Green volumes.** `CreateGreen { name, require_evidence }` records the `Green` role in the catalog
  and creates the in-memory merge engine in `ShardState.greens`, keyed by the green's id. A green is
  not a store-backed VFS tree — its merged content lives in the engine — so it takes no byte or
  version reservation. `Versions { green }` reports the engine's head version, routed to the green's
  owner shard.
- **Work volumes.** `CreateWork { green, name }` records the `Work` role over the green (based on the
  green's current head) and a `WorkState` in `ShardState.works` holding the declared operation journal
  and the work's content per path.
- **Edit.** `Edit { work, path, at, delete_len, bytes }` is a declared splice: it maintains the work's
  content and appends the `VolumeOp`s (a new path is `Create`d first; a splice is a `Delete` and/or an
  `Insert`/`Extend`).
- **Declare.** `Declare { work, op }` records a namespace or metadata operation — the counterpart to
  `Edit`'s content splice — for every dimension the deriver composes: `Unlink`, `Rename`, `Mkdir`,
  `Rmdir`, `SetMode`, `Symlink`, `Link`, `SetXattr`, `RemoveXattr` (a mounted work would journal these
  from its filesystem operations; without a mount, `Declare` records them directly). An unlink or
  rename also keeps the work's content map consistent. A symlink's target rides in the ops document's
  path table and an xattr's value in the journal, so neither needs a work-side store.
- **Submit.** `Submit { work }` composes the journal into the canonical ops document
  (`compose_volume`), seals the post-state from the work's content (each content op names a slice of
  its file's final content, placed at the op's `src`), hashes the increment identity (BLAKE3 of the
  encoded document and the post-state), and runs the green's `Green::submit` verdict — replying with
  the accepted version or the conflict windows.

Gated in `crates/server/tests/daemon.rs` (folded into the one serial lifecycle test so its daemon does
not contend): two works over a fresh green, both based on version 0, declare the same file; the first
merges on the fast path (the chain advances to version 1), the second conflicts on that file rather
than clobbering it. Non-vacuous — a broken verdict would accept both.

## What is owed

- **The base at an older version.** `submit` derives against the green's base: `Base::default()` at
  version 0, and the green's current state (`Green::current_base`) when the work's base is the head —
  and a new work is seeded with the green's content (`Green::files`) so an edit to a base file splices
  it rather than looking like a create. The base at an *intervening* version (`0 < base < head`, a lagging
  work) is now reconstructed by `Green::base_at`: files exactly from the content history, directories
  and modes replayed from the deltas; symlinks, hard links and xattrs at an older version are still
  owed (reconstructed empty, exact for file-and-directory workflows).
- **Post-state for non-content dimensions** is **landed**: `assemble_post_state` lays each
  extended-attribute value into the post-state region the `SetXattr` op names (drawn from the journal's
  composed value), so the value round-trips into the green and the verdict compares real bytes — a
  differing concurrent set conflicts, gated in `merge_declare_scenario`. A symlink's target needs no
  post-state (it rides in the ops document's path table), so `Symlink` was never a post-state gap.
- **`rebase`** (Phase 6 task 7) is **landed**: `Rebase { work }` runs the same verdict `Submit` would
  (`Green::rebase`), commits nothing to the green, and — when every operation maps cleanly — moves the
  work onto the head, restating its base, its full content and its journal in head coordinates (the
  journal stays fine-grained, so a later disjoint head move still merges rather than conflicting). A
  conflict returns the windows and changes nothing. Wired through ipc, server, client and the CLI
  (`slates rebase WORK`); gated in the engine tests (byte-exact base, content and journal; the
  no-commit property) and the server umbrella (`merge_rebase_scenario`: a clean rebase leaves the head
  unmoved then submits, and a conflicting one leaves it unmoved). **`advance`** — the green *attachment*
  re-pin with targeted invalidations — is still owed (it belongs with the mount/attachment path).
  (`changed_since` — the per-path last-changed index read — is done: `Green::changed_since` and the verb.)
- **Surfaces (task 8):** the non-Rust SDKs (MCP, Python, TypeScript); the Rust client and the CLI verbs (green, versions, changed-since, work, edit, submit) are wired.
- **Cross-shard submit and chain persistence:** a work whose green is on another shard forwards the
  increment to the green's owner; and the chain (`VersionRecord`s and `seen`) is recovered from the
  partition log after a restart (task 6's crash recovery). Single-node, same-shard only for now.
