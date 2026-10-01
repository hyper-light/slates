# A warm restart dropped acknowledged replicas

**Date:** 2026-09-30. **Area:** `slates-cluster` (content hold), `slates-vfs` (shard recovery image),
`slates-server` (holder path, recovery). **Audit:** AUD-29-59. **Design:** §4.8 "Recovery" and persistence
before reply; §4.10 placement closure; amendment A-51.

## Description

A holder acknowledged a content put once the archive was in its process-local `ContentHold`. Nothing
published the hold into anchor-owned RAM, and every rebuilt `ShardState` began the hold empty. A warm
daemon restart, with the anchor retained, therefore discarded replicas that placement had counted. The
holder's register records survived the same restart, so it came back holding heads that named content it no
longer had.

Red test, `crates/server/tests/recovery.rs`: `an_acknowledged_replica_survives_a_warm_daemon_restart`.
The put was acknowledged and held before the restart, and not held after it (`left: Ok(false)`).

## Root cause

The hold had no place in the shard's recovery image. Owned volumes survive a restart because every
acknowledged mutation first publishes the shard's image into its double-buffered slot of the anchor content
object. The holder path acknowledged without a publish, and recovery never read a hold back. §4.8 described
this as intended ("holds nothing for others until re-replication fills it"), while records already
persisted before reply.

## Fix

- **The hold has a canonical image** (`ContentHold::to_image`). Every distinct referenced chunk appears once,
  plus each held manifest with its object and placement sequence, in a deterministic order.
  `ContentHold::from_image` rebuilds through `hold()`, so recovery re-verifies identities and re-owns
  chunks per object instead of trusting the image. A damaged image is refused typed (`HoldImageError`).
- **The shard image carries it.** `ShardImage::held`, image version 8. Every publish includes the shard's
  hold.
- **The acknowledgement stands behind the publish.** `content_holder::serve` publishes after a held put
  and answers no acknowledgement if the publish is refused (`fleet.content.unpublished`).
- **Recovery holds it again** before the shard serves (`rebuild_recovered`), and reports the count.
- **The holder path is one function** (`content_holder::serve`), shared by the fleet's record session and
  the daemon's test hook (`Daemon::serve_content_as_authorized`). The test therefore drives production
  code, with only the authority decision granted.

## Tests

- The red test above now passes.
- The recovery suite passes 10/10.
- The ownership oracle recovers every generated hold from its image, checks the same rule against the
  recovered hold, and requires a byte-identical re-image.
- `a_damaged_hold_image_is_refused_and_a_whole_one_recovers`: truncation, a trailing byte, a flipped
  payload byte and an unknown encoding are each refused typed.

## Owed, reported

Publishing re-images the whole shard on every acknowledged mutation, and now on every held put too. The
incremental publish recorded in `docs/wip/recovery.md` is owed for both.
