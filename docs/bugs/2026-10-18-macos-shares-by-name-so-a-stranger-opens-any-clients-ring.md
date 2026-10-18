# macOS shares by name, so a same-uid stranger opens any client's ring and the daemon's segment

- **Date:** 2026-10-18
- **Area:** `crates/ipc/src/rendezvous.rs` (the macOS/Windows bootstrap-object rendezvous),
  `crates/server/src/daemon.rs` (`ClientRegion::create`'s name), `crates/mem/src/shared.rs`
  (`object_name`, `shm_open`)
- **Status:** **measured, not fixed** — this is the known open gap **GAP-A9-9 ("same-uid isolation
  owed")**, measured concretely for the first time. The fix is a platform decision (below) and is
  Ada's.
- **Class:** missing capability on a shared resource; predictable-name disclosure and injection
  (CWE-668, CWE-340)

## Description

Linux hands a client its ring as a **descriptor** over an abstract-namespace socket: the memfd has no
name in any namespace, so only a process the daemon passed it to can reach it. macOS has no abstract
socket namespace, so slates' rendezvous there is a **named** shared object, and every object it leads
to is opened **by name**:

| object | name | holds |
|---|---|---|
| the rendezvous bootstrap | `/slates-rv-<instance>-<uid>` | the doorbell word and the claim slots |
| a client's ring region | `/slates-cr-<instance>-<client_id>-<uid>` | that client's requests and replies |
| the anchor segment | `/slates-seg-<instance>-<uid>` | the daemon's own RAM |

Every part of those names is public: the instance name is how clients find the daemon, the uid is the
caller's own, and a client id is a small integer that can simply be counted up from 1. `shm_open` mode
is `0600`, so the only barrier is the uid — and **the uid is exactly the boundary slates needs to
hold**: §4.13 consumers are untrusted processes of the daemon's own uid, and `crates/ipc/src/delivery.rs`
says so in its own words when it chooses an inherited descriptor as "the one channel other same-uid
processes provably cannot read".

## The measurement

A real in-process daemon, a victim client that writes a 64 KiB file of the marker
`SL-VICTIM-PRIVATE-BYTES!` through its ring, and a **stranger** — not a client, never connected, holding
no capability — that derives the names (macOS arm64, 2026-10-18):

```
victim is client 1; it wrote its secret through its ring
stranger opened /slates-cr-mr78796-1-501 read-write: 278528 bytes, no capability at all
the victim's secret appears 2719 times in the ring a stranger just mapped
the stranger also wrote into it (header byte flipped and restored)
stranger opened /slates-seg-mr-79327-501 read-write: 148892368896 bytes
  mapped and scanned the first 256 MiB
stranger opened /slates-rv-mb72074-501 read-write with no capability at all
```

So a stranger can **read every request and reply** a client exchanges with the daemon — paths, file
bytes, everything that crosses the ring — and **write into them**, which is request injection and reply
forgery against a client that did nothing wrong. It can also map the daemon's whole 138.7 GiB segment
read-write.

### What was *not* shown, stated so no one repeats a vacuous scan

The victim's file content was **not** found in the first 256 MiB of the segment (0 hits). That is a
bounded negative, not an absence: the object is 138.7 GiB and scanning it all would commit it. Two
traps found on the way, both worth keeping:

- **macOS shm objects answer neither `SEEK_DATA` nor `pread`** (`SEEK_DATA answered: false; pread
  returned 0 bytes`). A first version of this probe scanned with both and reported a clean object that
  was in fact wide open. Any scan of a macOS shared object must `mmap` it.

### A measured negative: the macOS doorbell is not suppressible

The Linux finding of the same day — a client steals its shard's wakes through the shared kick eventfd
(`docs/bugs/2026-10-18-a-peer-steals-its-shards-wakes-through-the-shared-doorbell.md`) — does **not**
carry over. A stranger holding the bootstrap object can write the doorbell word, but restoring it after
every ring does not slow the victim, because `Bell::ring` follows its `fetch_add` with an explicit
`wake_one` on the address and the waiter is woken by that, not by the value:

| arm | p50 | p90 | max |
|---|---|---|---|
| A idle | 402 µs | 684 µs | 22,790 µs |
| B the identical store loop on a word of its own | 400 µs | 437 µs | 603 µs |
| C storing the doorbell word back after every ring (211,635,442 stores) | **396 µs** | 419 µs | 610 µs |

Three arms interleaved round-robin, B the CPU-matched control. No effect.

## Impact

On macOS, slates has no isolation between same-uid processes at all: ring disclosure, ring injection
and full access to the daemon's segment, all without being a client. On Linux the equivalents require
the daemon to have handed you something (and two such hand-offs were found and dealt with the same
day). Windows uses the same bootstrap-object rendezvous and is presumed to share the shape; it was not
measured here, because this host cannot build or test Windows.

The bound worth stating: this is a **same-uid** break. A different user cannot open these objects, and
the daemon's control plane is per-uid. It matters exactly where slates says it matters — a §4.13
consumer, an agent subprocess, anything running as the user but not entitled to the volume.

## Why it is not fixed here

Every obvious patch fails, and the reason is the platform, not the code:

1. **A random name per object.** The name of a client's region is published in the bootstrap object's
   claim slot, which a stranger can read — and a stranger polling that object managed 211 million reads
   per run above, so the window between the daemon writing the slot and the client opening it is not a
   defence.
2. **`shm_unlink` right after hand-over.** Closes the name afterwards but not the window, same as 1.
3. **A Unix domain socket**, as Linux uses, would pass descriptors instead of names and end the whole
   class — but macOS has no abstract namespace, so it needs a path, and R1 forbids an on-disk socket
   "if any alternative exists".

**An alternative does exist, and it is the macOS-native one: Mach.** A Mach port *is* a capability, the
bootstrap namespace is per-user and not a filesystem, and `fileport_makeport`/`fileport_makefd` pass a
file descriptor over a Mach message — which is exactly the Linux `SCM_RIGHTS` hand-off, with no path and
no name to guess. That makes it an R1-compatible answer rather than a trade-off against R1.

## Owed

- Ada's decision on the macOS rendezvous: Mach bootstrap + fileport (recommended, R1-clean) against the
  alternatives above. This is GAP-A9-9's macOS half, now with numbers.
- The same question for Windows, which shares the bootstrap-object shape and was not measured.
- A by-use regression test once the mechanism is chosen: a stranger that derives every name slates could
  use must open **nothing** — the inverse of the run above.
