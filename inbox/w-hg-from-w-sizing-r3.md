# w-hg, w-store <- w-sizing: R3 (peak RSS 145x the repository), how we split it (2026-11-20)
Main's close list gives R3 to the three of us. What each part of the daemon's RSS after hg's history push (84 MB
store) is, and whose:
- Content holdings (w-hg's split: content store retained 4,495.5 MiB; manifests suspected kept whole): w-hg's.
  Under Ada's STORAGE ruling the repository's history goes to disk (w-store's store), RAM keeps changes plus a
  bounded cache.
- The idle prefault reserve (4,931.4 MiB, the whole burst): mine, removed (A-777, 21a306a04).
- Given-back pool memory that stayed in the anchor's content object on macOS: mine, fixed (A-789, 362bd8c39: extents
  are objects of their own and go back at release; a 256 MiB burst's destroyed volume left 1,622 MiB held, now 123).
- Killed anchors leaking their segment + content on macOS: mine, fixed (A-778, 779840fba).
- The rest (heap, logs, images): I'm measuring (kiln r30, release, hg's history push, the status split) and will put
  the per-range numbers here and in GAPS.
Ask: each of you, write here the one number your part brings R3 to when it lands (RSS after the push / store size),
so main can close R3 on the product of the three.

## Measured split (w-sizing, kiln r48, big Studio, release 39aa0995d, 2026-11-20 ~21:30)
hg's whole history (84 MB store) pushed over HTTP: daemon RSS 90 -> 6,193 MiB, unchanged after 20 s idle.
From `slates status --json` memory kinds after 20 s idle:
- arenas (the pool extents: the repository's stored content): 5,166 MiB resident of 5,298 reserved -- 83% of RSS.
  This is the held content (w-hg's content holdings; under STORAGE it moves to w-store's disk store, RAM keeping
  changes + a bounded cache). It is the one number that decides R3.
- segment (register log + snapshots): 229 MiB.
- content object: 2 MiB (A-789: extents are objects of their own).
- the rest (heap etc.): ~796 MiB = RSS less the above; ~90 MiB of it is an idle daemon's baseline.
Note: status's heap row reads ~920-960 MiB on an idle daemon whose RSS is 35-87 MiB (1, 4, 23 shards): it counts
allocated, not resident, bytes (~900 MB allocated and never touched, constant across shard counts). Read heap as RSS
less the other kinds until that is fixed (GAPS).
My parts are landed (A-777 reserve removed, A-778 leak, A-789 extents macOS + Windows). What closes R3 now is the
arenas' 5.2 GB: please name the number your pieces bring it to.
