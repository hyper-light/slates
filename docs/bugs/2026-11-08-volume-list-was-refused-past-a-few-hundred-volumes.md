# `volume list` was refused past a few hundred volumes

**Found** by liveness's restart harness (2026-11-08): `slates volume list` answered `refused: BadRequest { reason: "reply
too large for the bulk area" }` once a daemon held a few hundred volumes.

**Reproduced** through the CLI on adacachy (Linux 7.2.9 x86_64, 2026-11-08, a capped unit at 8 CPUs):
`volume_list_answers_every_one_of_thousands_of_volumes` in `crates/cli/tests/cli.rs` creates 2,000 volumes with `volume
create` and lists them; `volume list` exited 1 with that refusal.

**Root cause.** `List` was a scatter-gather over the daemon's shards whose merged answer was one `Listed` reply holding
every volume. One reply is one bulk chunk of the client's region (`config::bulk_chunk_bytes`, 4,096 bytes), and a
volume's summary is its 16-byte id, its name and 18 bytes more, so a few hundred volumes outgrow it; `reply_to` then
answers the refusal. The design's other large answers are paged by the chunk (`ReadRange`, `ReadDir` A-85, the daemon
status by §4.14's status paging of 2026-09-20); this one was not.

**Fix** (A-pending, its number the supervisor's to allocate). `ListPage { cursor }` answers one page: the readable volumes of the shard
the cursor names after its last listed one, in id order, as many as one chunk holds (at least one), with the cursor that
continues it; the shards are listed in turn. A page on another shard is sent there as a scatter's part is, waiting for
room in a full control channel within half the liveness budget before it is answered `Overloaded`. `List` is retired:
kept for its wire index and refused `BadRequest`. The client's `list` walks every page (`ListWalk`, which refuses a page
whose cursor does not advance) and sorts by name as before; the SDKs' async `list` walks the same pages under one
awaitable, cancellable at whichever page is waiting.

**Evidence** (adacachy, 2026-11-08): the CLI test red before (the refusal), green after (2,000 volumes, each listed once
by `volume list` and `volume list --json`, in 6.8 s); the Python suites 6 of 6 and the Node suites 6 of 6 over a live
daemon, the async listing's cancel and restart cases among them; the listing's full-channel refusal by the other shard
(`a_listing_whose_part_cannot_be_sent_is_refused_overloaded`).

**Siblings, not fixed here.** Other replies carry a whole list in one reply and meet the same bound:
`changed-since` (reproduced: a green whose submit changed 400 files of about 40-byte paths answered `changed-since
GREEN 0` with the same refusal), `audit`, `grants`, `advance`'s invalidated paths and `submit`/`rebase`'s conflict
windows. Greens and works are volumes, which `volume list` now pages; no verb lists snapshots.
