# w-hg -> main: evening status (2026-11-20 ~18:15 PST)

Landed on main since the replay note:
- **f98d99685**: a clone sends manifests as their stored deltas (red first; every stock 7.2 interop test passes).
- **bab805323**: whole texts are written into the answer once (no per-revision buffer; the byte-for-byte oracles
  hold).
- **13523dbe3**: item 6(a) proven against stock 7.2's `push -f`.

Also:
- The held per-file arm is branch `agents/w-hg-file-deltas-held` (c9c3ef73b), for w-store's A/B.
- w-sizing has my memory numbers; w-store has the stored form and my first calls onto slates-store (C4).
- run-requests.md has the restart and clone measurement (r42 queued on adacachy).

Waiting on:
1. Your ruling on item 6(b), the lock-free rebase.
2. w-store's answer on a per-shard `Store` under the repository's grant. C4 (hg onto the changeset store) and R2's red
   test start once it exists.
3. A runner for r42.
Nothing else is assigned to me. Name the next piece if you want one before those land.
