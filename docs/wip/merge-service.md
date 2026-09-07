# §4.16 merge Green/Work service — wiring status

> Status: the `slates-merge` engine (verdict, deriver, splice, chain, position map, ops document) is
> built and tested at the crate level; this note tracks wiring it into the server, client and CLI
> (Phase 6 tasks 1, 6, 7, 8). **Landed:** green volumes and the chain read side (`CreateGreen`,
> `Versions`, `ChangedSince`); the submit flow (`CreateWork`, `Edit`, `Submit`) for a fresh *and* a
> non-empty green (derived against the current base), accept and conflict, through the real verbs.
> **Owed:** `rebase`/`advance`, xattr/symlink post-state, the base at an intervening version, cross-shard submit, chain persistence, and the non-Rust SDKs (the CLI verbs are wired).

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
  it rather than looking like a create. What is owed is the base at an *intervening* version
  (`0 < base < head`, a work that lagged behind another's submit): the content history supports it per
  file, but the other dimensions keep only the current value, so that submit is refused for now rather
  than composed against the wrong base.
- **Post-state for non-content dimensions.** `assemble_post_state` handles content ops only; a
  `SetXattr`/`Symlink` increment needs its value bytes laid into the post-state at the op's `src`.
- **`rebase`, `advance`** (Phase 6 task 7): the corrective rebase mapping a work's pending operations
  to a newer version, and the attachment re-pin with targeted invalidations. (`changed_since` — the
  per-path last-changed index read — is done: `Green::changed_since` and the verb.)
- **Surfaces (task 8):** the non-Rust SDKs (MCP, Python, TypeScript); the Rust client and the CLI verbs (green, versions, changed-since, work, edit, submit) are wired.
- **Cross-shard submit and chain persistence:** a work whose green is on another shard forwards the
  increment to the green's owner; and the chain (`VersionRecord`s and `seen`) is recovered from the
  partition log after a restart (task 6's crash recovery). Single-node, same-shard only for now.
