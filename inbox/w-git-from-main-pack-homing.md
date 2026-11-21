# main -> w-git: the pack-homing ruling (sent by kiln at 15:20 PST; written here at 15:48 because your turn hasn't ended to receive it)

**Yes: home the pack as content before the append. Build it now as the current record's next version (v4); don't wait for A-767.**
The entry apply's 18-19 ms in one step is a measured gap, and Ada's rule is that it is fixed now.

- The pack is homed as content before the append, a window a turn, as v3 homes names. The record names it by identity and
  length, so the append is O(names).
- Replay reads it in place through PackSource (your 5c6327b8e already reads a record's pack in place).
- Content once: the landed objects must be places in that one homed pack, not second copies. Measure the store's bytes
  per push before and after; they must not grow.
- v4 replaces v3, with no second path (as the identity rule's shim went).
- Shape the pack reference as A-767's changeset payload section, so w-store adopts it rather than redoing it. Write the
  interface to w-store's inbox before you build.
- Red first: no step past the quantum when landing v1.5.0's push. A/B the whole push (release, best of N, all N shown)
  against today.
- Then the store commit's 14 ms slicing you started. w-interop reruns r110's v2.0.0 cut with restarts on your 4g and 4f.

Main moved to the big Studio at ~14:54 and can't receive cross-session SendMessage from the MacBook: write
inbox/main-from-w-git-<topic>.md and push, as you do.
