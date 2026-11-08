# A green's changed-since answer outgrew one reply

**Found** 2026-11-08 by the sibling sweep of `2026-11-08-a-volume-listing-outgrew-one-reply.md`: `changed_since`
answered every path a green changed after a version in one reply.

**Reproduced by use** (the client over a one-shard daemon, a work that changed 800 files submitted to a green):
`changed_since(green, base)` was refused `BadRequest { reason: "reply too large for the bulk area" }`. A lagging work
reconciles from this answer, so a green that moved on by more than one reply's worth of paths could not be followed.

**Root cause.** `ReplyBody::ChangedSince { paths }` held the whole answer, and a reply is one bulk chunk (4,096 bytes);
at these names a chunk holds about 150 paths.

**Red** (`crates/server/tests/green_changes.rs`, `a_green_answers_every_path_it_changed_however_many`, 3,000 changed
files): `Refused(BadRequest { reason: "reply too large for the bulk area" })` on `agents/reland-a207-15`.

**Fix** (A-264), as the volume listing (A-263):
- `ChangedSince { green, version, after }` is answered by one page, `ChangedSince { paths, more }`: the paths changed
  after `version` in path order past `after`, as many as one reply chunk holds less the page's own framing. The next
  page is asked after the page's last path. `more` is a flag, not that path again, so a page holding one long path
  need not hold it twice.
- The green's index of each path's last change (`Green::changed_since_after`) is already in path order, and a path
  enters the answer once its last change passes `version` and never leaves it, since versions only rise. So a walk
  answers every path that had changed when it began exactly once, however the green moves on between pages. A path
  changed for the first time between pages, before the cursor, is the next walk's.
- `Client::changed_since` walks the pages; the asynchronous form (`changed_since_begin`, `_spin`, `_poll`, which both
  SDKs use) walks them as a chain under one word.

**Why one path always fits a page.** A path reaches a green only from a work's operations, and through the SDK each
travels in a request (an edit, a rename, a mkdir), whose framing is larger than a page's. So any such path fits one page
alone, and fits the next request as its cursor. Measured: the longest path an edit carries is 4,044 bytes, and the
green answers it (`the_longest_path_an_edit_carries_is_answered`). Merging a mounted work, whose paths no request
carries, is owed (§4.16's A-26 status); when it lands it must bound a path to what one page holds.

**Green** (`green_changes.rs`, one shard): 3,000 changed paths answered by the call and by its asynchronous form, each
once; a walk under churn, a submit of one new file before each page, answered 3,019 paths in 20 pages with every path
changed before the walk exactly once and none twice; the 4,044-byte path answered.

**And every dimension** (the supervisor's call, 2026-11-08). `changed_since` read the green's content index alone
(`last_changed`, which only content changes write), so a green that moved a symlink, a mode, an xattr, a special, a
directory or a hard link under a work told it nothing (reproduced by use: a submit that linked `/alias.txt` answered
`["later.txt", "original.txt"]`). §4.16 gives `changed_since` as how a work "learns early which of its pending paths
green has moved under", so the content-only answer was quietly wrong. It is now `Green::changed_between(v, head)`, the
rule `advance` invalidates by: walked lazily, every dimension's path-keyed history ranged from the cursor, each filtered
to its paths changed in the span and merged in path order (`Green::changed_since_after`), so a page costs the paths it
passes and a walk passes each path once. Tests: `changed_since_reports_every_dimension_as_changed_between_does`
(`crates/merge/tests/engine.rs`: equal to `changed_between`, whole and in pages, across six dimensions) and
`a_green_answers_every_dimension_it_changed` (`green_changes.rs`, by use: the link, the symlink, the directory).

**Its cost** (`crates/merge/tests/changed_since_cost.rs`, ignored; release; the kernel tree `linux-v7.4-rc1`, 97,229
paths, seeded, then 2,000 submits of 20 overwrites each, every tenth also a symlink, a mode, an xattr and a hard link;
this Mac at load 30 to 170, best of 5; the merged walk checked equal to `changed_between` at every version):

| Since | Content index, before | `changed_between` | Merged walk | Paged walk (150 a page) |
|---|---|---|---|---|
| version 0 (40,518 paths; 40,000 by content) | 1.27 ms | 8.3-16.5 ms | 2.3 ms | 4.4 ms, 271 pages, 16 µs a page |
| version 1,000 (20,279; 20,000) | 1.49 ms | 4.3-11.0 ms | 1.9 ms | 3.0 ms, 136 pages, 22 µs a page |
| version 1,990 (203; 200) | 0.37 ms | 0.9-1.4 ms | 1.0 ms | 1.1 ms, 2 pages |

Every dimension costs 1.3 to 2.7 times the content-only answer over a whole walk, with no second index. A first cut
that merged every path and then looked each one up in every dimension took 47 to 99 ms at the same sizes and was
replaced by filtering each history as it is walked.

