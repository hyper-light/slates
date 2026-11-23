# w-git -> main: the sweep of stored-object reads in receive and upload (what a let-go window does to each)

At main 1d7fd0ec4 and later (the guard in: nothing landed is let go, so none of this breaks main today; each B is a
reader a re-lift would meet). A read that meets a let-go window must **A** stop and be retried once the window is back,
or it **B** refuses as if the object were missing. **C**: unclear, mostly reads of content kept whole, which only a
landed pack's let-go would reach.

The census was made by a read-only search of every site; I checked the B sites below against the code myself.

## A: stops and is retried (no work owed)

- **Fetch negotiation:** every object read goes through `Repository::bytes_of` → `row_bytes` → `read_stored`, which
  notes the miss. `GraphFault::Store` becomes `Refusal::Store`; `Answer::choose` holds it (`hold_away`) while no pack
  has begun; the HTTP and SSH drivers' `read_through_held` bring the window back and make the answer again. This covers
  graph.rs `named` / `named_entries_of` / `object_bytes` / `tree_entry`, and so objects.rs, ready.rs, refs.rs,
  revision.rs and sparse.rs.
- **Fetch pack stream** (store/upload.rs): `send_whole` (in a pack), `copy_window`, `Upload::read_window`, `loose`,
  `reused` (raw entry), `base_of`, `reused_as_kept` (header, CRC), `delta_reused`.
- **Encoding:** `entry_checked`, `Reading::read_window` (in a pack), `Current::Copy`, `reused`, `reused_as_kept`.
- **Push unpack:** `IndexedObjects::object` → `Outside::away` → `resolve_turn` stops → `brought_back_waited`.
- **Push judge:** the tag peel (`Verdicts::peel_away`, 6eae9f8bb); `PushedObjects::bytes` (indexed) inside `prepare`
  (`NotYet`, `wait_for`).

## B: refuses (owed before a re-lift), most damaging first

1. **The push's quarantine reads** (`git_push/intake.rs` `Quarantined::content`: `Push::bytes`, and the A-753 resend
   path's `read_stored`). On a miss these answer None, and `Push::bytes` notes nothing. Callers, all before the verdicts:
   `checking.rs:240` fsck (`BadObjectSha1`: the pack refused), `connect.rs:443` edges (`Connected::Missing`: the ref
   refused), `unpack.rs:1044` a delta's base (`DeltaBaseMissing`: the pack refused). A thin push over a let-go base
   reaches these.
2. **The server's tag-headed green** (`git_push.rs` `ShardGreens::tree_of`: `self.indexed.object(&target)?`). The miss
   makes `newest` and `version_of` None, `prepare` reads it as ready, and `update` declines `moved`: "cannot lock ref".
   The away is set but never asked. This is an update of a ref whose green's head is an annotated tag in a let-go pack.
3. **The admission's peel and references** (`vcs_store.rs` `admission_turn` → `admitting.rs` `peel`/`references` →
   `bytes_of`, `.ok_or(unreadable)`). Refused, it is treated as ready, and the append's re-admission answers the
   refusal. There is no away check.
4. **The policy's reads** (`protect.rs` `judge_brought`, `signature_refused` `bytes_of`): `PushRefusal::Unjudged`, a
   decline. This only bites where a rule imposes linear history or signed commits.
5. **`PushedObjects::bytes` for a pushed object** (`views.rs` `At::Pushed`): `Push::bytes` → `Some(Err(_))` →
   `Unavailable::Malformed`. The miss isn't noted, so the command is declined.
6. **`PushedObjects::bytes` in a verdict's one-call `build`** (no prepared build to take): `work_refused`,
   MISSING_OBJECTS. Normally `prepare` builds first, but a verdict reached without it refuses.
7. **`push_of` → `tree_of` at judge time** (`works.rs`): the decline "an object it names cannot be read" if met there.
   `prepare`'s peel normally meets it first and waits.
8. **The encoding's tree walk** (`encoding.rs` `compare` → `Repository::tree_entries`, `.ok_or(Unreadable)`). The
   encoding is dropped and counted `vcs_store.git_encode.refused`; the turn is `Idle`, so `encode_within` /
   `encode_ahead` never bring the window back. Nothing reaches the client, but the fetch answers without that version's
   fragments: slower, not wrong.

## C: unclear (reads of content kept whole, or order only)

- Fetch stream: `send_whole` (whole content), `deflate_window` (content), the free `read_window` (whole; its in-pack
  arm looks unreachable), `read_range` via `read_header` (a kept part, with no away check, unlike `copy_window`).
- Encoding: `Reading::read_window` (whole content).
- `IndexBases::locate` (whole), `PushedObjects::bytes` (whole/stored: `LengthDiffers`), `works.rs` `first_parent` (a
  create based silently on none), `peeled_generation` (rank 0: order only), the hook views' `Unreadable` (`Turn::NotRun`).
- These are C because the let-go targets landed packs only. Kept parts and whole contents are let go only if a later
  change lets them go. If one does, every site here becomes B.

## What I'd do, behind the guard, red first

B1 (thin push over a let-go base) and B2 (update over a tag-headed green) first: a stock git push reaches both, and
each declines a ref git would take. Then B3 to B7 by the same pattern: note the window (`wait_for` / `away`), answer
not ready or turn, and let the driver's `brought_back_waited` bring it back, with the same-window-twice rule ending
typed. B8 last: its cost is time, not correctness. A re-lift waits on every B.
