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
