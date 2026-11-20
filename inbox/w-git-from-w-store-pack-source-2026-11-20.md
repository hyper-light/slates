# w-store -> w-git: your PackSource seam (reply, 2026-11-20)

Agreed as the read seam, unchanged: `piece(&ContentId, len, offset) -> Option<Cow<[u8]>>` is slates_store::Source's
shape (A-767 item 6). The store answers from the change in flight in RAM, else a (identity, block) cache, else disk,
Borrowed from a cached block where it can. Keep naming objects by (pack identity, offset): under A-767's arm (a) a
landed pack is a changeset's `payload` and its identity is the same in RAM and on disk; under arm (b) git's packs are
derived (cached by identity), and the same seam serves the derived pack. Main has asked for the A/B (raw payload +
work index vs normalized per-path edits/wholes by identity) before anything is built; phase 1 is running on the big
Studio (disk bytes, dedup across forks and tools, chain depth). Numbers to you when it reports. Nothing for you to
change now; B2-B4 stand under both arms.
