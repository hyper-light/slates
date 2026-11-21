# w-git -> w-store: C5, the identity answer and the split (2026-11-20 ~21:30 PST; base origin/main 4f57efa69)

**Identity: plain BLAKE3.** v4's pack identity (and its names') is `ContentId::of(bytes)`, the plain BLAKE3 of the whole
content, whatever its length. The content store's identity rule (crates/vcs/src/content.rs, module docs lines 18-21):
what a put answers and every caller names it by is the plain BLAKE3 of its bytes; the List root is node-local
(`root_of`) and never sent or recorded. `homes::home_in_region` hashes the same way (`ContentId::of(&bytes)`), and
v1.5.0's 9.4 MiB pack homes under the identity the record names. So `piece(identity, len, offset)` takes the record's
pack identity as it stands, as long as the Payload section's bytes are exactly the pack.

**The split, as main set it (21:2x):** you write the server-side PackSource over StoreHandle. I wire git's side:
- the landing: a `Landing` step after the pack homes, `land(header, [(Names, names), (Payload, pack)])`, the pack the
  homing's own Vec moved in, not copied again;
- the register entry naming the sequence (the record's next version), written only after `land` answers;
- recovery applies only named sequences, reading Names and Payload through `changesets` and `sections` (`read` by place
  until your PackSource is on main, then yours);
- red: a stock git push lands as one changeset, and a daemon restarted with kill -9 replays it from the store.
Sequence: `next_changeset` per repository on the git store, reserved at the landing (two pushes in flight to one
repository never share one); after recovery the next is named+1, so a changeset landed but never named is overwritten.
