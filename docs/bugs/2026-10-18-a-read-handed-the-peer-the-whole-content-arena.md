# A zero-copy read handed the peer a writable descriptor for the whole content arena

- **Date:** 2026-10-18
- **Area:** `crates/server/src/merge_service.rs` (`try_local_map`, A-135), `crates/ipc/src/rendezvous.rs`
  (`Control::send_mapping`), `crates/vfs/src/dax.rs` (`Store::dax_backing`), `crates/mem/src/arena.rs`
  (`ChunkArena::block_backing`)
- **Status:** found by attacking; **the escape is closed in this change**. What it costs — A-135's
  zero-copy read now takes its copy path until a confined backing exists — is recorded below with the
  measured mechanism that would restore it. **Zero copy restored 2026-10-25** on Linux, macOS and Windows by a
  confined copy per file (A-171, A-173, A-175), the escape still closed on each; see "Restoration".
- **Class:** confinement failure across a trust boundary, read **and** write (CWE-668, CWE-732)

## Description

A-135 (the same-kernel direct map) answers a `MapRead` by minting a read mapping and sending the
content block's backing descriptor to the client over the control socket by `SCM_RIGHTS`; the client
`mmap`s it read-only and reads the file with no copy. The reply carries `region_off`, `file_len` and
`block_len`.

Those three numbers scope the **request**. They scope nothing about the **descriptor**. A descriptor
carries its whole object, read and write, to whoever holds it — and the descriptor being sent is the
arena's object, which `ChunkArena::block_backing` says plainly in its own comment: "`backing_fd` is the
whole object".

The guard in front of it was `same_uid`, with the comment "Offered only to the daemon's own trust
domain (a same-uid peer)". **That premise is false in this product.** §4.13 consumers are untrusted
processes of exactly this uid holding exactly this channel — the consumer capability machinery exists
because they are untrusted — and `MapRead` is dispatched with a `Principal` like any other read, so a
consumer reaches it for any volume its capability allows.

## The measurement

An e2e run against a real one-shard daemon, two volumes of one account: `neighbour` holding
`/secret.bin` (20 KiB of the marker `SL-NEIGHBOUR-SEC`) and `mine` holding `/mine.bin` (20 KiB). The
peer maps **its own** file and then looks at what arrived
(`crates/client/tests/client.rs`, x86_64 Linux, kernel 7.2.9-2-cachyos, 2026-10-18):

```
handed descriptor: object_bytes=189833572352  authorized_bytes=20480  neighbour_marker_occurrences=1280
handed descriptor: writable_map=granted
```

- The descriptor covers **189,833,572,352 bytes — 176.8 GiB**, the anchor's whole content object,
  against the **20,480 bytes** the read authorized. Ratio **9,269,217×**.
- The neighbour's marker occurs **1,280 times** inside it: 1280 × 16 B = 20,480 B, the *whole* secret
  file, readable by a peer that never named that volume.
- A shared **writable** map of all 176.8 GiB was granted.

And the write is not theoretical. A second run took the escape to its end — find the neighbour's bytes
inside the handed object, overwrite them, then read that volume back through the ordinary verb:

```
object_bytes=189833572352  secret_found_at=145166848000
neighbour first 32 bytes after the peer's write: "OWNED-BY-A-PEER!SL-NEIGHBOUR-SEC"
```

A peer that asked to **read** 20 KiB of its own volume **rewrote another volume's file**, and slates
then served the attacker's bytes as that file's content.

## Impact

Live in production on Linux — not advertisement-gated, not harness-only. Every `MapRead` served by the
zero-copy path handed this over. Reachable by any client of the daemon, including a §4.13 consumer
confined to one volume. Through it a peer could:

- read every volume's content on the host (the object is the anchor's content RAM, sized to the host);
- write every volume's content, with no verb, no lease and no attachment — the ring protocol's entire
  authorization model bypassed;
- `ftruncate` the object and take the daemon down with `SIGBUS`.

Not affected: macOS and Windows, which have no control socket and read by copy; the fd-sharing DAX
form, where the backend maps and the VMM never receives a descriptor.

## What a fix has to beat — measured, not argued

The obvious patch is to send a read-only duplicate. **It does not work**, and that was measured rather
than assumed (same host, `/tmp/fdprobe.c`, kernel 7.2.9-2-cachyos):

```
--- arm 1: plain memfd (what slates creates today) ---
  read-only reopen through /proc/self/fd           GRANTED
    writable map of the READ-ONLY description      Permission denied
    O_RDWR reopen FROM the read-only description   GRANTED
      writable map of the upgraded descriptor      GRANTED
```

A same-uid peer re-opens the read-only duplicate `O_RDWR` through `/proc/self/fd` and maps it writable.
A duplicate is no defence against the party this check exists for.

What does work is sealing, with the owner's own writable map taken first:

```
--- arm 2: MFD_ALLOW_SEALING + F_SEAL_FUTURE_WRITE after the owner's map ---
  owner's writable map taken before the seal       GRANTED
  F_SEAL_FUTURE_WRITE|SHRINK|GROW                  GRANTED
  owner writes through its pre-seal map            GRANTED ("owner-still-writes")
  peer: writable map of it                         Permission denied
  peer: O_RDWR reopen                              GRANTED
      peer: writable map of the upgraded fd        Operation not permitted
      peer: pwrite through the upgraded fd         Operation not permitted
  peer holding the SEALED fd itself: writable map  Operation not permitted
  grow/shrink: ftruncate to half                   Operation not permitted
```

The owner keeps writing through the mapping it already holds; the peer cannot map writable, cannot
upgrade, cannot `pwrite`, cannot resize. That is the mechanism a confined hand-off must use.

## The fix

**The check lives at the one place a descriptor leaves this process for a peer** —
`Control::send_mapping` — so no caller can skip it and no second copy of the rule can drift
(`confined_for_a_peer`). It asks two questions of the descriptor itself, not of its caller:

1. **Does it reach past the window?** `fstat` gives the object's whole length, which is what the holder
   may map. Confined means the window *is* the object: `region_off == 0` and `object_len == block_len`.
2. **Could the peer still write or resize it?** `F_GET_SEALS` must show a write seal (`F_SEAL_WRITE`, or
   `F_SEAL_FUTURE_WRITE` where the owner keeps an earlier map) together with `F_SEAL_SHRINK` and
   `F_SEAL_GROW`.

Either failure is the typed refusal `IpcError::UnconfinedDescriptor { object_bytes,
authorized_bytes, writable }`, which names what it would have authorized. `try_local_map` already
treats a failed send as `LocalMap::Fallback`, so the client is served by copy and gets its bytes; the
minted mapping's pin is released on that arm as it always was.

**Nothing today satisfies the rule**, because a shard's content arena is one object holding every
volume (A-64). So on Linux a `MapRead` now reads by copy. That is the honest cost of closing the
escape, and it is the direction CLAUDE.md requires — never preserve the behaviour that caused the bug.

## What it costs, and what would restore it

A-135's zero-copy read is the optimization this turns off. Restoring it needs a backing whose **object
is the unit of authorization** — the same conclusion the message-based DAX placer reached from the
other side (`docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`), so the
two findings now wait on one decision:

1. **A backing object per tenant** (per volume, or per attachment), created sealable and sealed
   `F_SEAL_FUTURE_WRITE|SHRINK|GROW` once the owner has its map. Keeps zero-copy, confines by
   construction, and the seal measurement above shows it works. Costs: it cuts against A-64's
   one-image-per-shard content layout, and multiplies objects and descriptors by tenant count.
2. **A per-mapping sealed object holding only the block's bytes.** Confined by construction, one copy
   at map time. For DAX this is the "view" arm already measured-and-rejected (slope 1.0 extra RAM); for
   A-135 the baseline is a copy *per read*, so copy-once-map-many may well win — it is a different
   measurement from the DAX one and has not been taken.
3. **Leave the copy path.** Correct, and A-135's measured win is gone.

Option 1 is the only one that keeps both zero-copy and confinement for both callers. It touches a
decided layout, so it is Ada's call; the escape is closed either way.

## Restoration (2026-10-25)

Option 2, refined: **a copy of the file, made per map and kept equal to the file**, rather than a copy of one block
pinned at its version. A pinned copy would cost the same one copy and then show the client a stale file with no
signal; a copy in the store's backings (A-163) is written through at the volume's single write boundary before every
change returns, so the client's map is a live view of the file within its length (A-171). It is the unit of
authorization on each platform:

- **Linux** (A-171): a sealable memfd of the file's page span; the daemon takes its own writable map, then seals it
  `F_SEAL_FUTURE_WRITE | F_SEAL_SHRINK | F_SEAL_GROW | F_SEAL_SEAL`, and `confined_for_a_peer`, unchanged, now passes
  it. The `/proc/self/fd` reopen this record measured is still granted, and its writable map and `write` are refused
  `EPERM`.
- **macOS** (A-173): a shared-memory object whose name goes as soon as its two descriptors are open; the client is
  offered the one opened read alone, on the daemon's socket route, judged first (`confined_read_only`). XNU refuses
  its holder a writable map (`EPERM`), a write (`EBADF`), a map turned writable (`EACCES`) and any second sizing.
- **Windows** (A-175): a section under a security descriptor that grants no one any right (`D:P(A;;RC;;;OW)`); the
  daemon duplicates a read-only handle into the client's process, judged first (`hand_section`). Under the default
  descriptor a read-only handle's holder duplicated one that writes (measured); under this one it is refused
  `ERROR_ACCESS_DENIED`.

Each platform's hostile test plays this record's attacker against what the client receives: every write path refused
with the confinement's own error, another volume's marker in none of the copy's bytes (found through that volume's
own copy, so the scan is not vacuous), and red-checked by handing an unconfined object, which the hand-off refused,
and with the judge bypassed, which the attack used. The copy's life and bounds are A-172's (released or reaped, within
a share of the shard's capacity) and A-174's (a hand-off that fails in the client gives it back).

What it buys back, measured (`docs/wip/BENCHMARKS.md`, "Copy-once-map-many against copy-per-read"): a steady mapped
read costs 1.1 % to 10.3 % of a copy read at 4 KiB, 1 MiB and 64 MiB on two Linux machines, this Mac and a Windows
machine, and a map repays its setup after one read from 1 MiB up and after 2 to 5 reads of a 4 KiB file.

## Tests

- `crates/ipc/src/rendezvous.rs` `a_descriptor_that_would_authorize_more_than_the_hand_off_is_never_sent`
  — the three shapes a real arena produces (an object larger than the window, a window partway into the
  object, a window the peer could write) are each refused `UnconfinedDescriptor`, and the peer's side of
  the socket has nothing waiting afterwards, so a refused hand-off never half-crosses.
- `crates/client/tests/client.rs` `a_zero_copy_reads_descriptor_authorizes_its_window_and_nothing_more`
  — the e2e above, written as the invariant rather than as today's state: if the read was zero-copy the
  handed descriptor must show none of the neighbour's marker and must refuse a writable map; if it was
  a copy, **no** memory-object descriptor may have been installed in the peer at all. It therefore keeps
  its meaning when a confined backing lands. Non-vacuity: the marker must first be found in this
  process's memory objects (10,240 occurrences in the run), so a clean descriptor is a fact about the
  descriptor and not about the marker being absent.
- `a_same_kernel_client_maps_a_file_with_no_copy` no longer asserts which path ran — asserting it would
  pin today's state in place — and reports it instead; the bytes are asserted on either path.

## Verification

```
slates-ipc --lib                        21 passed, 0 failed   (x86_64 Linux)
slates-client --test client             13 passed, 0 failed   (x86_64 Linux)
  a_same_kernel_client_maps_a_file_with_no_copy ......... map_read path: copy
  a_zero_copy_reads_descriptor_authorizes_its_window .... no descriptor crossed (arena holds 10240 markers)
cargo clippy --workspace --all-targets -- -D warnings   clean on macOS aarch64 and x86_64 Linux
```

## Sibling findings, reported not fixed here

- **`Store::dax_backing` hands the same unconfined descriptor to the vhost-user message placer.** That
  path is advertisement-gated and already recorded
  (`docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`); it is not
  routed through `send_mapping`, so this change does not cover it. The same confinement rule applies
  and the same decision resolves both.
- **`block_backing`'s contract is the real hazard**, not either caller: it returns a descriptor for an
  object its own comment describes as "the whole object", and nothing in its signature says that
  handing it across a process boundary authorizes everything in it. Both escapes came from that. If
  option 1 is taken, the accessor should return the confined form and have no unconfined sibling.
- **A `MapRead` now costs an extra round trip** when the hand-off is refused: the daemon mints a
  mapping, learns at the send that it cannot confine it, revokes the pin and answers `MapUnavailable`,
  and the client then reads by copy. Correct, and cheap relative to the read; it disappears with
  option 1.
