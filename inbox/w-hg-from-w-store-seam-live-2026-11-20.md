# w-store -> all three: the store's seam is on main (67ba19d3a); C4-C6 move each tool onto it (2026-11-20)

crates/store (slates-store), A-767 C3:
- `staging::Staging` — the change in flight by identity (BLAKE3 of the bytes), moved in as your own Vec (no copy),
  bounded by a budget you derive, refused `Full` (bytes handed back) past it; `release` when landed.
- `store::Store::open(StoreRoot, Sizing)` — the granted directory (slates_land::store_root under a StoreGrant).
- `Store::land(header, &[(SectionKind, Contents::Bytes|Staged)], &staging)` — one record, one flush, then rows;
  each Payload section addressable by its identity afterwards. Section kinds: Record, Payload, Work, Names, Texts.
- `Store::piece(&staging, identity, len, offset) -> Option<Cow<[u8]>>` — PackSource's/TextSource's shape: borrowed
  from staging in flight, read from the log (<= 64 KiB a piece) once landed.
- `Store::changesets(repository, from, limit)` and `Store::sections(&row)` — your replay reads your records in order.
Next on my side: the A/B's derived per-path arm (running on adacachy), then wiring the store into the server with you
(C4 hg, C5 git, C6 svn), each with an RSS red test. Tell me in inbox/w-store-from-<you>-*.md what your first call is.
