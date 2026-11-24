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
  `Done`. A volume whose destroy has not begun yet counts its live inodes, each at least one object.
- `crates/server/src/room_wait.rs`: `Room` (bytes, versions, metadata); `note_refusal` counts as room on the way the
  credits of every volume this shard is destroying (`teardown_room`), and the remakes for bytes as before. A parked
  create's deadline adds the teardowns' remaining slices at a step's budget apiece (`teardown_window_ns`) to the
  remakes' bound, and moves on from now while the shard's teardown slices release objects (`teardown_sliced`,
  `teardown_released`, `Resuming`), never sooner than it was. `teardown_ended` runs the parked creates again. Module
  doc: the teardowns.
- `crates/server/src/verbs.rs`: the byte, version and metadata admissions of `admit_over` note their refusals by
  kind; `step_destroys` counts the objects each slice releases, runs the parked creates again once a teardown's
  credits are back, and runs no slice while a test holds the teardowns.
- `crates/server/src/daemon.rs` and `state.rs`: `Daemon::inject_teardown_hold` and `teardowns_held`, test support on
  the pattern of the daemon's other injections.
- `crates/client/tests/client.rs`, which runs on every platform (the daemon's own test file is Unix only):
  `a_create_at_once_after_a_destroy_waits_for_its_teardown`, and
  `a_create_waiting_for_a_teardown_that_does_not_go_on_is_refused_at_its_bound` (main's ruling: the test shows the
  bound too).

## Measured and rejected
- The first cut counted a teardown not begun yet as one slice. The destroy verb only records the volume as
  destroying, and the queue is built at the next destroy round, so a create sent at once parked a step from its
  deadline, ran again before the teardown ended, and was refused at once (diag run 45320326826: macOS and ubuntu,
  "Err(Refused(BudgetExceeded { available: 1250033664 }))" for 3,750,100,992 bytes of 5,000,134,656). Both Windows
  runners passed 5 of 5 rounds, the remakes' bound giving the window its slack.
- The second cut judged progress by the slices left. Building a teardown's queue counts objects its live inodes did
  not (directory blocks among them), so the estimate rose as the teardown began, read as no progress, and the create
  was refused while its teardown was being served (diag run 45322799003, macOS: the same
  `BudgetExceeded { available: 1250033664 }`). Progress is now the objects the slices released.

## Evidence
RED_AND_GREEN_3B
