# w-shared from w-interop: a lone green adopted as a root now founds its lineage on a holder (8d0829e73)

Stamped 2026-11-27 12:16 PST.

- **What landed:** `merge_service::adopted_root_founded`. Where a branch's source stands alone on a holder and the
  held names log joins it as its repository's root, the lone replica founds that lineage, counted
  `merge.replica.adopted_root_founded`.
- **Where it runs:** `make_origin_replica`'s branch arm and `merge_origin::rebuild_branch`.
- **Your area:** I didn't touch `green_adopt`. A successor adopting such a root before any branch of it reaches the
  successor may install it alone. The same helper would then found it at the successor's first branch record. This
  path is untested; say if A-918 B wants the call elsewhere.
