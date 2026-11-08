# A green's and a work's listings left out their hard links

**Found** 2026-11-08 while checking `Green::base_at`'s callers (the supervisor's question on the conflict windows).

**Reproduced by use** (one-shard daemon): a work writes `/original.txt`, declares `WorkOp::Link { path: "/alias.txt",
target: "/original.txt" }` (accepted), and is submitted; a later work adds `/later.txt`. `ReadDir` of the green then
listed `["later.txt", "original.txt"]` at the head and `["original.txt"]` at the link's version: `alias.txt` was in no
listing, though the green holds it. The work's own listing left it out too.

**Root cause.** `listing.rs` built a green's entries from its base's files, directories, symlinks and specials, and
never from `base.hardlinks` (`green_entries`); and a work's from its files and the directories and symlinks its journal
declared, never from a `Link` it declared or the hard links its green's version held (`work_paths`). `Green::base_at`
reconstructs hard links at every version; the listings did not read them. (`base_at`'s doc said hard links, symlinks
and xattrs were reconstructed empty; the code reconstructs them, and the doc is corrected with A-267.)

**Red** (`crates/server/tests/hardlink_listing.rs`,
`a_hard_linked_path_is_listed_in_a_green_and_a_work_at_every_version`): `the work: alias.txt in [DirEntry { name:
"original.txt", kind: File, size: 27 }]` on main `92dcd16c`.

**Fix.**
- `green_entries` lists each of the base's hard links as a file of its target's size.
- `work_paths` starts from the hard links of the work's green at its base (`Green::hardlinks_at`, read from the
  hard-link history alone, so a listing does not rebuild the whole base), then follows the journal: a `Link` adds one,
  an `Unlink` removes it, a `Rename` moves it. Each is listed as a file of its target's size in the work.

**Green.** The test lists `alias.txt` as a file of 27 bytes, the original's size, in the work, at the green's head, at
the link's version, at a later head, and in a fresh work over the green.

**Siblings checked.** An overlay's drift lists host paths, where a host's hard link is a file, so it is not affected.
The volume listing's counts are byte totals, not a walk of paths. `changed_since` reports file content only (it reads the
green's content index, `last_changed`) and so leaves out a hard link, and every other non-content change; whether it
should report them is the supervisor's to decide, and it is unchanged here.
