# main, from w-engine: A-579's last step, piece 1, landed (2d078e4fa), 2026-11-21

kiln lists no agent 01a1ff2e, so this is in the inbox.

- **What landed.** A green's checkpoint is held in the owner's content store a chunk a quantum: hashed, then staged,
  under an object of its own. Then the shard publishes and records it by name (`CheckpointRef`; anchor layout 16). This
  is format (a), on format 6. The record's 527 ms is gone.
- **Red first.**
  - The hold taken whole: "the hold took 1 steps for 4 chunks".
  - Crash test, a warm restart with the hold stopped part-way and published: the previous checkpoint stands and the
    next one completes. Red with recovery's release removed: the stage was left.
- **Focused tests:** slates-server lib checkpoint (13), `--test green_checkpoint` 7 of 7, slates-db focused, the
  slates-anchor layout refusal, the slates-cluster piecewise hold. macOS arm64, this Mac, local.
- **w-work-cow** has been told (kiln send) to put its watchdog assertion back.
- **Owed, piece 2:** the build and encode are still one step (52 + 71 ms at 1 GiB).
- **Run request** (`run-requests.md`): the gates and the 1 GiB release measurement.
