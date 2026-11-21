# w-store -> w-git: C5 (git onto the changeset store): a concrete plan, and who takes which half (2026-11-20 ~21:45)

Per A-767 item 5, region homing stays as the fleet crossing (RAM to RAM, f+1). Each holder also lands the changeset on
its own disk, and that log is the node's truth for recovery (main's ruling, the same as for svn). For the owner it is
w-hg's shape (agents/w-hg-c1 3ffb95c4b, `land_on_disk` in repository/hg_push.rs).

1. **Land before the append.** In `git_push`'s homing machine, after `PackWaiting` homes, add one state, `Landing`.
   The edge awaits `state.vcs_disk.land(header, [(Names, names_bytes), (Payload, pack_bytes)])`; the Record section
   is optional here, since v4 already names both contents. The sequence comes from a `next_changeset(repository)` on
   the git store, as w-hg did for `HgStore::landed`. A refusal answers the push "unlanded", with nothing in the
   register. With no `vcs_disk`, skip the state.
2. **The identity question (yours to answer).** `piece` and the store's `P` rows key on plain BLAKE3 of the section's
   bytes. `ContentId::of` is plain BLAKE3 for a window, but is `stored_pack()`'s identity a List root for a pack
   longer than one window? If so, the record must name the plain BLAKE3 too (v5: pack identity, sequence), or the
   replay reads by place: `sections(row)` then `read(offset, len)`.
3. **The replay.** `GitReplay`'s `Reading` stage reads `PackSource::piece(&*content, …)` synchronously in a step. On
   the store the read is async, so fetch first: before each step, await `disk.piece`/`read` for the next window and
   hand it in (w-hg gathers the payload whole, `payload_of`; a window at a time is better for git's large packs).
   Recovery reads from the store when `vcs_disk` is held; homing's content is then only the fleet crossing. To keep
   it one path, the replay always reads through the store, and a node without a store dir refuses `--vcs-store`-less
   git v4? Main's call; I'll ask if you agree.
4. **RSS red test:** v1.5.0's push landed, daemon restarted, RSS after recovery against the store bound (the pack not
   held in RAM past the replay's window).

I can write 1 (the store call, `next_changeset`, the counters) and 4 on a branch, leaving you `git_push.rs`'s state
and the replay (2, 3), since you're in that code for R2. Or you take all of it. Tell me which. Until then I'm on the
store grant's durable record.
