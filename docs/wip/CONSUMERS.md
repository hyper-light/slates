# What actually consumes a slates volume, and how far each is proven

> **Status, 2026-10-19.** Built from the tree, not from memory: every row cites the code or the lane that
> backs it. §4.6 A-9 and RQ-20 state the contract ("Host processes, OCI containers and Linux microVM guests
> can consume the same VFS"); this is the ledger of how far each consumer is actually carried.

The columns are deliberately separate, because the gap between them is where the surprises live:

- **Built** — the code path exists and is unit-tested.
- **Connected** — a real consumer process has spoken the protocol to a real daemon.
- **Mounted + read** — a consumer has mounted a volume and read bytes a writer put there.
- **In CI** — a lane runs it on every push or on the push cadence.

| Consumer | Transport | Built | Connected | Mounted + read | In CI |
|---|---|---|---|---|---|
| **Kubernetes pods** | NFS PV over the daemon's RPC-with-TLS export, through the node's `tlshd` | yes | yes | **yes** | **yes** |
| **Host processes** | kernel mount — FUSE on Linux, NFS loopback on macOS, WinFsp on Windows | yes | yes | yes | yes |
| **crosvm / QEMU guests** | vhost-user virtio-fs, message-based `SHMEM_MAP` (DAX) | yes | yes | yes | no |
| **OCI containers** | bind mount of a host attachment the runtime performs | yes | — | **no** | unit only |
| **shards microVMs** | virtio-fs over shards' own frame socket (`shards_fs`), mediated — no DAX window | yes | yes | **no** | no |
| **kiln agents** | nothing | **no** | — | — | — |

## Row by row, with what backs it

**Kubernetes pods — the best covered, and the only one with an end-to-end CI lane.** `cargo xtask kind export`
(`xtask/src/kind.rs`) "mounts a published volume as an `nfs` PersistentVolume over the daemon's RPC-with-TLS
export, through the node's `tlshd`, and a second mount reads what the first wrote", and CI runs it as
`cargo xtask kind export --require-kernel-tls` in the `kind` lane. It skips loudly on a kernel without TLS.
The same lane covers the fleet itself: formation, SIGKILL takeover, rejoin, scale, and WAN profiles under
`tc netem`.

**OCI containers — the contract is built and the consumption is not tested.** `crates/bridge-oci` verifies that
the host path a consumer names really is the volume's mount (`getfsstat`/`mountinfo`, read as a query that
never touches the mount) and hands back the runtime-specification `mounts` entry for the harness to pass to
`runc`, `crun` or `docker`. Its 15 tests (`tests/verify.rs`, `tests/runtime.rs`) all assert on that
verification and that entry. **None of them starts a container.** The crate's own doc is honest that slates
"does the namespace work of neither the runtime nor the launcher" — but nothing proves the entry it emits is
one a runtime accepts, or that the container can read the file. That is the nearest real gap, and Docker is
available here to close it.

**shards microVMs — connected, not booted.** `crates/bridge-fuse/src/shards_fs.rs` is the slates backend for
shards' virtio-fs frame socket, dispatching through the one FUSE seam, and `tests/shards_fs.rs` drives the
frame protocol in memory and over a real `UnixStream::pair`, hostile frames included. A live connection to a
real HVF microVM has been made. **A guest boot that mounts and reads is owed**, and was previously held back
as workload interference on a shared machine. This path is deliberately **non-DAX**: shards' fs device has no
guest-physical cache window, so the guest reaches slates through the mediated FUSE path.

**kiln agents — nothing exists.** There is no kiln code in this tree and no slates code in kiln's. What the
integration would be is clear from both sides: a kiln agent today receives its files by `reach`, an rsync of
the repository to `~/.kiln/reach/<host>/<repo>`; a slates volume would replace that copy with a provisioned,
copy-on-write view of the same content that merges back — which is the product's stated purpose, "coding
agents provision in under 50 µs, see as a normal path, merge through". It is cross-repository work and has not
been scoped.

## What this ledger is for

The failure it exists to prevent is the one that cost a day on 2026-10-19: believing a thing is covered because
a crate exists for it. A crate that emits a mount entry is not a container that read a file, and a protocol
test over a socket pair is not a guest that booted.
