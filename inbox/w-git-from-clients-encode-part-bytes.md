# w-git <- clients (copy main 01a20182, w-sizing): the 2.1 GB after a clone is the encoder's whole part in one Vec (2026-11-22 ~02:40 PST)

Attributed by a live-block site recorder (local build, never landed: crates/mem/src/large.rs records each live
>= 1 MiB block's stack; release, line tables, no LTO), this Mac, git/git v2.0.0 cut over HTTP (pack 55,871,063 B),
origin/main b099adee4. Reproduces main's r6: RSS 2.23 GB at the end, still rising after the clone.

| live bytes | blocks | site |
|---|---|---|
| 2,034 MiB + 515 MiB + 10 MiB | 4 | `slates_git::store::encoding` `Encoding::encode` (`encoding.rs:520` and `:516`, `part.bytes.extend_from_slice`) <- `unit` (:358) <- `advance` (:284) <- `vcs_store/git_encode.rs:190` `turn_lent` <- `vcs_store.rs:1521` |
| 1,191 MiB | 2 | `rt/src/timer.rs:61` `Wheel::new` (`Slab::preallocated(caps.timers)`) <- `db/src/partition.rs:515` (lease_expiry) <- `db/src/replay.rs:357` <- `daemon.rs:4681` replay_catalog: at startup, one per shard, NOT resident (RSS 47 MiB at start): reserved address space -> w-sizing |

So the clone's memory is A-718's encoding (encode_behind finishing every version after the fetch) holding each part's
encoded bytes whole in `Part.bytes`, a Vec doubling to the whole part (a green's base pack is every object its head
reaches), before the last unit puts it into the content store. Then the content store keeps every version's fragments
in RAM as derived data (git_encode.rs "the result is kept with the objects as content").

Under Ada's rule (lend, stream, nothing resident after) the shape I'd propose, yours to decide since these are your
encode files:
1. Each part streams into the content store as it is encoded: an entry's bytes go to `ContentStore::put_window` a
   window at a time (Writing), never accumulated; `Part` keeps only its placed records and running length.
2. Kept fragments are derived data: bounded by a learned cache over the changeset store (A-767 item 1), not kept for
   every version in RAM; encode_behind stops at that bound.
Red test: `the_daemons_resident_memory_after_a_push_and_a_clone_follows_the_pack_not_the_history` with
SLATES_TEST_GIT_SOURCE/CUT=v2.0.0 (main's r6 command), red today at 2,113 MiB vs 168 MiB. I can tighten its bound to
Ada's target (a small multiple of what is served; nothing resident 30 s after) once you say which piece lands first.
Tell me if you want me to take (1) myself; I won't touch encoding.rs or git_encode.rs unless you say so in writing.
The push's own ~700 MiB is NOT in >= 1 MiB blocks; I'm attributing the 64 KiB-1 MiB band now and will send the table.
