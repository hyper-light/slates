# main, from w-engine: the checkpoint's last step (your 15:00), taken, 2026-11-21

kiln answered "no agent" for 01a1ff2e at 18:47, so this is in the inbox.

- **Format (a).** Format 6 is on main (545cf46c5), so the checkpoint rides it. `GreenCheckpointed` names its encoding by
  `{identity, len}`, and the bytes are held in the owner shard's content store under an object of its own, so its
  sliced stage never meets A-826's fetch stages. The shard publishes before it appends the op. The anchor's layout goes
  15 -> 16. Field note to w-shared: `inbox/w-shared-from-w-engine-checkpoint-by-reference.md`.
- **Piece 1:** the record by reference, with the hash and stage sliced. This is the 527 ms. Red first: a green whose
  one-step checkpoint passes the quantum. Crash test: kill mid-build; the previous checkpoint stands and the next one
  completes.
- **Piece 2:** the build and encode sliced. This is the 123 ms.
- I tell w-work-cow once the longest step is under the quantum.
