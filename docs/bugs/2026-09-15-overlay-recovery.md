# Overlay recovery images could not satisfy their creation barrier

Date: 2026-09-15. Contracts: §4.5, §4.8, D-25, AUD-05, AC-2.12, T-2.14.

## Reproduction and cause

The daemon lifecycle test refused at `create overlay` after **4.06 s**, logging an omitted
recovery image. Recovery images refused base bodies and whiteouts and could not describe the
unvisited source. The stricter AUD-05 barrier correctly prevented an unrecoverable success.
This also broke the merge service's base-seeded green fixture.

```sh
env RUSTUP_TOOLCHAIN=1.98.0 CARGO_BUILD_JOBS=2 cargo test --offline -p slates-server --test daemon -- --test-threads=1
```

## Change

Image version 2 includes the base plane, witnesses and original witness homes, private file
ranges and zero ranges, drift, whiteouts, redirects, and each directory's merge/opaque state.
Directory source paths are independent of overlay renames. Capturing an overlay requires its
host seam; missing host state remains a typed refusal.

The daemon reopens the catalog base with the existing read-only, no-follow host implementation.
Recovery opens source components without following links and compares every retained directory
fingerprint. Missing, replaced or changed source directories refuse. Cached listings, digest
caches, watcher tokens and process-local file descriptors are rebuilt rather than serialized.
Unpinned file reads still validate their saved witness; they cannot silently use changed bytes.
Refused reconstruction releases its opened directories and partial VFS allocations.

This is validated source reacquisition, not descriptor handoff. An external source rename or
identity change across restart can make an overlay unavailable. The daemon reports that refusal
instead of serving a substitute source. Anchor handoff of live host descriptors remains separate.

## Verification and sibling findings

- Both new VFS recovery histories passed, **0.02 s**: an unvisited base and a renamed nested
  directory with a whiteout, metadata-only witness, private large-file range and snapshot.
  The source drift check refuses unpinned bytes while preserving the private range.
- The complete daemon suite passed: **8 tests, 22.09 s**, including both reported failures.
- Linux checking also exposed two stale calls in `bridge-fuse/tests/oci_container.rs`: the
  mount's required deadline and the serving loop's mutable attachment table. Both are corrected.
- An existing overlay-clone ownership issue remains separate: `BasePlane::for_clone` retains
  only a root listing, and the server clone slot has no host. This change does not claim that
  overlay clones now have a complete independently owned host-handle lifecycle.

Final Linux results and commands are recorded with the consensus recovery validation.

## Linux verification, 2026-09-15

Using the bounded RAM-only build container in the consensus recovery record:

```sh
cargo test --offline -p slates-vfs --test recover --test base --test retained_bytes --test chunk_ownership --test entitlement -- --test-threads=1
cargo test --offline -p slates-server --test daemon --test recovery -- --test-threads=1
```

**69 VFS tests passed:** base 20 (16.02 s), recovery 31 (0.09 s), ownership 6 (<0.01 s),
entitlement 3 (0.01 s), retained bytes 9 (0.01 s). **All 8 daemon tests passed (33.87 s)**,
including both previously failing overlays; **all 3 recovery histories passed (40.88 s)**.
Linux also exposed a BSD-only `mktemp` fixture prefix; the test now supplies the GNU-required
`XXXXXX` suffix and reports stderr on refusal. The source image does not substitute a different
base when validated reacquisition fails.
