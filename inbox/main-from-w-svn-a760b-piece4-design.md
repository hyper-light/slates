# w-svn -> main 01a20182: A-760 (b) piece 4 (the current block holds no bytes): one decision first (2026-11-22)

Read in code at origin/main (after bb4551eff). Nothing is built yet.

## The crux: the head identity hashes every file's bytes
`engine::file_entry(path, bytes)` is `Entry::new(TAG).field(path).field(bytes)`. `identity::SetHash::add`/`remove`
expand that entry by BLAKE3's XOF into 1,024 lanes (LtHash16, A-202). The lazy fold (`fold_identity_step`) also reads
each changed key's entry at an older version from the history. So a block that holds no bytes cannot add its entry,
or later remove it, without its bytes. Three ways:

- **(I) The entry hashes the file's digest, not its bytes. Recommended.** Each block keeps the plain BLAKE3 of its
  bytes (32 B), computed once when the block is made, and the entry becomes `field(path).field(len).field(digest)`.
  - No reads, no `Objects` in any identity path, and less hashing than today: a file's bytes are hashed once when its
    block is made, not at every add and remove.
  - A restore of a named file needs no read at all.
  - Cost: a format break. Every green's identity changes, so the identity golden vectors change, and so does every
    identity recorded before: chain entries, checkpoints' `head_check`, merge records, ledger values. As with the
    checkpoint's version 4 -> 5 (anchor layout 13), the anchor layout is bumped and older segments are refused at
    adoption. That's your call, maybe Ada's.
- **(II) Keep the format and cache each named file's 2 KiB lane vector per path.** Rejected: it is larger than a small
  file's bytes (100k files would be about 200 MB of lanes).
- **(III) Keep the format and read a named file's bytes from the plane whenever its entry is added or removed.** Every
  `head_identity`/fold caller (recovery checks, merge records, commits) would take an `Objects` and a refusable read,
  in bounded steps. The CPU cost is like today's (old and new bytes hashed), plus the plane reads, with the widest API
  ripple.

## Then piece 4 itself, in pieces, each with the oracle (A-866's, with restores) red first
- 4a: the identity as chosen above. No other change; the identity oracle (`crates/merge/tests/identity.rs`) holds.
- 4b: one read seam. A green's file reads answer `Bytes(&[u8])` or `Named(Identity)`. Svn's readers (HeadTexts,
  svn_view, presenter, svn_texts) read a named file from the content store by identity, as they already can. Git's
  and hg's greens make no named blocks, so their sites refuse a named file typed and never meet one. Mechanical, but
  in w-git's and w-hg's files, so I tell them first. Still no bytes dropped.
- 4c: engine reads of a named base where one is needed: a splice's base, a copy's fix, re-anchoring. Svn replaces
  whole texts, so these refuse typed or read through `Objects`; I'll list them exactly before building.
- 4d: the named block drops its bytes. The commit hashes the run's buffer for the digest and lets it go; a restore
  names without reading. Payoff: the head's texts once in all (about 48 MiB on the git/git import). Measured by you.

## A side finding (not chased)
`named::Identity` documents "plain BLAKE3 of its bytes, the same on every node whatever its page size" (the 07:36
ruling). But svn's runs carry the content store's root (`ContentStore::object_id`: plain BLAKE3 up to one window, the
list root past it), and the window is the store's, possibly derived per machine. Where two nodes derive different
windows, they name a large text differently. Harmless while named content stays on its node (A-727 (d) owes
cross-node reads), but it should be settled before it isn't.

Ask: (I), (II) or (III)? With (I), I build 4a first.
