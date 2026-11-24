# A create sent at once after a destroy met the destroyed volume's teardown (2026-11-23)

## Description
A bounded create sent the moment a destroy of a volume of the same size was answered was refused `BudgetExceeded`,
though the room was there a teardown later. CI's Windows runners showed it first (diag run 45251289894: a create at
once after a destroy refused `{ available: 1430536192 }`, the room less the destroyed volume's), and it holds on every
platform. Main recorded it as item 3b of the Windows list and ruled that the create waits, bounded by the teardown's
remaining slices at the step budget.

## Root cause
A destroy is answered once its record says the volume is destroying. Its teardown then frees the volume's objects a
slice a round of the shard's serve loop (`verbs::step_destroys`, each slice half a step's budget), and only the
teardown's end gives the volume's credits back (`verbs::release_slot_credits`): its byte reservation, its inode
allowance against the version slab, and its records against the metadata ledger. A create admitted between the answer
and that end met all three held. The room wait built for the remakes of the pool's extents (A-789, `room_wait`) knew
nothing of teardowns: it counted only remakes as room on the way, and only at the byte admission.

## Impact
On every platform, a client that destroyed a volume and made one of the same size at once was refused
`BudgetExceeded` and had to retry by itself, with nothing telling it when the room would be back. The larger the
destroyed volume's tree, the longer the refusals went on: its teardown takes a slice per object batch.

## Exact edits
- `crates/vfs/src/volume.rs`: `Volume::destroy_slices_left`, the slices a teardown still takes at its last slice's
  pace (`destroy_slice_objects`, the objects that slice released; one before any slice ran), and the call that says
  `Done`.
- `crates/server/src/room_wait.rs`: `Room` (bytes, versions, metadata); `note_refusal` counts as room on the way the
  credits of every volume this shard is destroying (`teardown_room`), and the remakes for bytes as before; a parked
  create's deadline adds each teardown's remaining slices at a step's budget apiece (`teardown_window_ns`) to the
  remakes' bound; `teardown_ended` runs the parked creates again. Module doc: the teardowns.
- `crates/server/src/verbs.rs`: the byte, version and metadata admissions of `admit_over` note their refusals by
  kind; `step_destroys` runs the parked creates again once a teardown's credits are back.
- `crates/client/tests/client.rs`: `a_create_at_once_after_a_destroy_waits_for_its_teardown`, which runs on every
  platform (the daemon's own test file is Unix only).

## Evidence
RED_AND_GREEN_3B
