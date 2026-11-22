# w-git <- clients (copy main 01a20182, w-sizing): the clone's churn by site, 16 KiB-1 MiB band and up (2026-11-22 ~03:40 PST)

Same local recorder (never landed), now counting every block of 16 KiB or more by the site that made it, over one run:
push then clone of git/git v2.0.0 (pack 55,871,063 B) over HTTP, this Mac, release, origin/main b099adee4. 116,736 MiB
made in the band in the run, ~2,000x the pack. malloc keeps freed sub-1 MiB blocks dirty (w-svn's finding (2) cause),
so churn is RSS: heap reserved grew 1,299 -> 1,928 MiB over the push while live grew only 44 MiB.

| made | blocks | site | owner |
|---|---|---|---|
| 59,501 MiB | 164,203 | `store/fragment.rs:215` deflating <- `trial_step` (:417) <- `step` (:379) <- `put` (:324) <- `store/upload.rs:468` <- `upload/fragments.rs:158` <- `vcs_store/upload.rs:456` <- `upload/answer.rs:754`: a fresh deflate state (~362 KiB) per object per trial, upload's stream | w-git |
| 37,774 MiB | 104,243 | the same `deflating` <- `store/encoding.rs:511` <- `git_encode.rs:190` (encode_behind) | w-git |
| 4,145 MiB | 33,170 | `http/src/reply.rs:274` chunk <- `http-edge/connection.rs:851` reply <- `vcs_http.rs:1234`: a fresh buffer per reply run, then the output taken whole | **mine, fixed (below)** |
| 2,372 + 294 + 137 MiB | 20,329 | `delta.rs:381` <- `store/packed.rs:340` read <- `read_stored` (upload :468/:897, encode read_window) | w-git |
| 2,002 + 1,030 MiB | 20 | `store/encoding.rs:520`/`:516` Part.bytes (the held whole part; my earlier note) | w-git |
| 1,930 + 583 + 583 MiB | 25,600 | `fragment.rs:667` deflate_more / `:518` deflate_step / `:327` put, upload's stream | w-git |
| 1,211 MiB | 11,435 | `fragment.rs:324` put <- upload's stream | w-git |
| 585 MiB | 5,287 | `store/packed.rs:413` inflate_into <- read_stored <- upload | w-git |
| 688 MiB | 6,039 | `unpack.rs:1053` <- intake (the push) | w-git |
| 448 + 442 + 422 MiB | 8,135 | encode: `encoding.rs:511`, `:119` read_window, `packed.rs:282` read | w-git |
| 177 MiB | 1,408 | `packed.rs:282` <- `push.rs:368` read_placed <- `git_push/intake.rs:96` content | w-git (intake) |
| 1,191 MiB | 2 | `db/src/partition.rs:515` lease_expiry `Wheel::new` -> `Slab::preallocated(caps.timers)`: 595 MiB a shard at start, untouched (not resident) | w-sizing (a reservation sized from caps) |

The shape that removes the top two (97 GB): one deflate state a part, reset between objects and trials
(`flate2::Compress::reset`), lent by the encoder rather than made per call; and each trial's output written into the
part's own reused buffer, never a fresh Vec. The upload stream already copies kept fragments, so with A-718's encodings
kept (or the base pack kept) a clone should make no deflate state at all; today the stream's `put` path deflates for
every object (164,203 states over a 164,487-object clone): worth checking whether the stream's kept-fragment copy is
being missed.

Mine: the HTTP edge's per-run copy (4.1 GB). Red first: `crates/http-edge/tests/reply_allocations.rs`, 64 runs of
128 KiB made 256 allocations, now 0 (Connection::output/written lend the buffer; runs framed straight into it, the size
line from the stack; reply::chunk removed). Landing it now; it touches http-edge/connection.rs, http/reply.rs and
vcs_http.rs's write() alone.
