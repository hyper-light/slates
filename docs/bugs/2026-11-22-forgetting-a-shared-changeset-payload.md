# Forgetting a changeset hid another repository's payload

On 2026-11-22 at `01ae9fc26`, two repositories each landed the same 28 bytes. After
`Store::forget(2, 1)` and a reopen, repository 1 still listed its changeset. Reading its
payload by identity returned `None`. The log still held the bytes. Mercurial can call
this operation when it abandons a push whose segments have landed (A-767 C1).

## Cause

`store::put_content` replaced a payload's single `P` row on every landing. When
`Store::forget` removed the latest copy it deleted that row. Its checkpoint made
the missing lookup survive a restart. The existing forget test landed different
payloads that contained a shared text, so it exercised the text index alone.

## Fix

The point lookup names one live copy. Each further occurrence gets a key containing
its identity and log offset. Both landing paths and replay use the same rule. Removing
the copy used for reads selects one other place. Removing the last copy drops the
lookup. Reusing a repository sequence releases the displaced record's places.
Reapplying an older record leaves the newer landing in control of that sequence.

A checkpoint written before this fix can already lack live lookups. On its first open
the store discards the old payload index and rebuilds it from surviving changeset rows,
one page at a time. It hashes payloads in bounded pieces on the store owner. It
checkpoints the rebuilt index and its marker together. Later opens read the marker
and replay the tail. The log format stays the same.

## Evidence

- `cargo test -p slates-store --test store forgetting_a_duplicate_payload_keeps_the_surviving_changesets_bytes -- --exact`
  reproduced the missing read before the fix. A copy of that same on-disk checkpoint
  returned all 28 bytes when opened with the fix.
- `cargo test -p slates-store --test store duplicate_payload` covers both deletion orders,
  repeated sections, both kinds of landing, sequence replacement and retry, and restart.
  It checks both the piece read and the batched place read.
- `cargo test -p slates-store --lib an_old_payload_checkpoint_recovers_only_the_surviving_copies_once`
  repairs a missing lookup, removes a lookup for an obsolete record and reopens the
  durable index with the old log prefix unavailable.
