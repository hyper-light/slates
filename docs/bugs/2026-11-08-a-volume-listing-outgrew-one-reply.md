# A volume listing outgrew one reply

**Found** by liveness in its restart harness: `slates volume list` refused "reply too large for the bulk area" past a
few hundred volumes, so an operator could not list what a server holds. Taken over from fs-server 2026-11-08.

**Reproduced by use** (main `fb435eac`, this Mac, a two-shard anchor started `slates anchor --quick --shards 2`, 2,000
volumes of 1 MiB made with `slates volume create`): `slates volume list` and `slates volume list --json` exited 1 with
`refused: BadRequest { reason: "reply too large for the bulk area" }` at every count tried, 100, 200, 300, 400, 500,
750, 1,000, 1,500 and 2,000. All 2,000 creates succeeded.

**Root cause.** `List` was answered by one `Listed` reply holding every volume the caller may read, gathered from
every shard. A reply is one bulk chunk (4,096 bytes), and a page holds about 80 summaries at these names (the walk below took 37
pages for 2,991), so a listing past about 80 volumes could not be sent. The daemon then answered the `BadRequest` its send path keeps for a reply
that does not fit. The gather also held every shard's whole listing on the origin shard, so its memory grew with
the server.

**Red** (`crates/server/tests/volume_list.rs`, `a_listing_of_thousands_of_volumes_holds_each_once_in_name_order`,
3,000 volumes on two shards): `Refused(BadRequest { reason: "reply too large for the bulk area" })` on main.

**Fix**, the way `listing.rs` pages a directory:
- `List { after }` is answered by one page, `Listed { volumes, next }`. A page holds the volumes in volume-id order
  past `after`, as many as one reply chunk holds less the page's own framing. `next` is the last volume of the page
  when more remain. A cursor by id lists every volume that exists for the whole walk exactly once, whatever is
  created or destroyed between pages.
- Each shard answers only the first page's worth of its own volumes past the cursor, and whether it holds more. No
  page takes more than a page from any one shard, so a gather holds at most a page a shard.
- `Client::list` walks the pages and returns the listing in name order, as before. The asynchronous list
  (`list_begin`, `list_spin`, `list_poll`, which both SDKs' async lists use) walks them as a chain under one word,
  as a large edit does, so the SDKs see one operation. A page that does not move the cursor forward is refused, so a
  walk always ends.
- `List` was replaced, not joined by a second verb (CLAUDE.md, banned item 7).

**A name no page could hold.** A `create` request carried names up to 4,062 bytes, and the summary of such a volume
does not fit any page. A daemon that admits a volume it cannot list has the same bug in a smaller form. A new volume's
name is now refused `InvalidName` past `longest_listable_name`: the page's room less a summary with an empty name,
4,025 bytes. That holds for every verb that names a new volume (`Create`, `CreateGreen`, `CreateWork`, `Clone`,
`ImageBuild`, `ImageMerge`), checked before the verb runs. A volume recovered under a longer name is not refused.
`the_longest_name_a_create_carries_is_listed` failed with the paging alone (`refused: BadRequest { reason: "reply too
large for the bulk area" }` for a 4,062-byte name) and passes with the bound.

**Green.**
- `volume_list.rs`, 3 tests, on two shards:
  - 3,000 volumes listed by the sync and the async list, each once, in name order; then exactly the survivors after
    every tenth was destroyed.
  - A walk a page at a time, destroying one unlisted volume and creating one before each page: 2,991 volumes in 37
    pages, each listed once, none listed after its destroy.
  - The longest admitted name, 4,025 bytes, listed.
- `crates/cli/tests/cli.rs`, `volume_list_answers_a_listing_longer_than_one_reply` (`SLATES_TEST_CLI=1`): 400 volumes
  made and listed through the CLI, plain and JSON, each once.
- The reproduction again on the fixed binary: `volume list` exited 0 at every count from 100 to 2,000 with one line a
  volume, and `--json` held 2,000 distinct ids.

**Siblings** (reported, not changed here). The same one-reply shape answers:
- `changed_since` (a green's changed paths): refused "reply too large for the bulk area" at 800 changed files,
  reproduced through the client on the fixed branch.
- `audit` (every record since a sequence) and `grants`: unbounded by their code; not reproduced, since a probe without
  landings held no records.
- A volume status's `drifted` list, and the conflict lists of `Submitted`, `Rebased` and `GrantRequired`, and
  `Advanced`'s `invalidated`: unbounded by their code; not reproduced.
- There is no snapshot listing: a volume's status counts its snapshots.
