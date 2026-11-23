# The svnsync use row compared two random mirror UUIDs, so it could never pass (2026-11-23)

## Description
Main's round (B), `a_dump_reads_and_writes_alike_through_slates_and_stock_svnserve` with `SLATES_SVN_USE_VERBS=svnsync`
over `http` (the big Studio, after 414925522 served `replay-report`), failed with one difference: the two mirrors'
`svnadmin dump`s differed in the `UUID:` line alone (stock `3a47c9d9-…`, slates `b4f84b7f-…`).

## Root cause
The harness. `mirror_dump_of` (`crates/cli/tests/svn_interop.rs`) made each leg's mirror with `svnadmin create`, which
gives a repository a random UUID, and stock `svnsync` never sets a mirror's UUID: Subversion 1.15.1
`subversion/svnsync/svnsync.c` `do_initialize` records the source's UUID only in the `svn:sync-from-uuid` revision
property, and says "the RA layer doesn't have a way to set a UUID". So the two mirrors' dumps differ on every run,
whatever the server. `ci/interop/svn-matrix.sh` already gives its mirror a fixed `MIRROR_UUID` with `svnadmin setuuid`;
the Rust row did not.

Ruled out first, on the MacBook (a debug slates at main 65cdf08b0, `crates/svn/fixtures/dump/full.dump` imported):
the import keeps the dump's UUID, and the HTTP edge reports it (the OPTIONS `SVN-Repository-UUID` header and
PROPFIND's `lp2:repository-uuid` both `7bf7a5ef-cabf-0310-b7d4-93df341afa7e`, the dump's).

## Impact
The svnsync use row failed on every route. The HTTP edge and the import were not wrong. Nothing in slates changed.

## Edits
- `crates/cli/tests/svn_interop.rs`: `MIRROR_UUID`; `mirror_dump_of` gives each mirror that UUID before its `init`,
  as `svn-matrix.sh` does, so the `UUID:` line is still compared and is the same on both legs.

## Evidence
- Before: main's (B), one difference, the `UUID:` line.
- After: owed to main's rerun of (B) (no stock Subversion on the MacBook). The test builds and skips loudly here.

## Second cause, found by the rerun (main's (B) at 112e694e7, r12, the big Studio, 751.9 s)
The UUID line then matched (`00000000-0000-4000-8000-00000000a703` on both). One difference was left: revision 0's
`Prop-content-length: 221` (stock) against `220` (slates), and its `Content-length` too. `mirror_dump_of` dropped the
lines naming `svn:sync-*` properties but kept the length headers that count them, and revision 0's
`svn:sync-from-url` is each mirror's own source URL (the control's tunnel, the HTTP edge on its own port). So its length
differed by construction, whatever the servers answered. `ci/interop/svn-matrix.sh` has no such filter: it sets
`svn:sync-from-url` to the fixture's URL and compares whole bytes with `cmp`.

Edits: `MIRROR_SYNC_PROPERTIES` (Subversion 1.15.1 `svn_props.h`); `mirror_dump_of` deletes them from the mirror's
revision 0 with `svnadmin delrevprop` before the dump, and compares the dump whole, with no line filter. Red first,
3bb5e53b1: `a_mirrors_dump_is_the_same_whatever_url_it_was_synced_from` (one stock repository mirrored over svnserve's
tunnel and over `file://`; red with the line filter, green with the fix). Both runs are owed to main: no stock
Subversion on the MacBook.
