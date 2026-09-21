//! The OCI attachment form (§4.6 A-9 "virtio-fs and OCI attachment contract": "A host OCI runtime
//! passes the established host attachment into the container mount namespace ... Requesting an
//! unsupported form returns `AttachmentUnsupported{transport, reason}`. A metadata record is
//! insufficient evidence of a usable container path or guest device. No disk socket, image
//! construction, target mkdir or privilege escalation is implicit in attaching a VFS volume"; RQ-20:
//! "Host processes, OCI containers and Linux microVM guests can consume the same VFS"; Appendix C:
//! "OCI namespace handoff ... must report [its] own tested semantics").
//!
//! What the form is. A container consumes a volume through a bind mount, performed by the host's OCI
//! runtime, of a host attachment that already exists — the kernel mount `slates mount` establishes
//! (the NFS loopback mount on macOS, §4.6 "macOS fallback"; the FUSE mount on Linux, §4.6 "Linux").
//! slates does the namespace work of neither the runtime nor the launcher: it verifies that the host
//! path the consumer names is that mount, records the authorized binding, and hands back the
//! runtime-specification `mounts` entry (`destination`, `type: "bind"`, `source`, `options`) for
//! the harness to pass to `runc`, `crun`, `docker` or whatever it drives. The read/write policy is the
//! attachment's: a read attachment yields a read-only bind, which the runtime enforces (`ro`: the
//! container's write gets `EROFS`).
//!
//! What is verified, and how. The kernel's mount table is read as a **query that never touches the
//! mount**: `getfsstat(MNT_NOWAIT)` on macOS ("without requesting an update from each file system",
//! `getfsstat(2)`) and `/proc/self/mountinfo` on Linux (`proc(5)`). A `statfs`/`stat` of the path
//! itself would issue an NFS or FUSE request to the very daemon asking — on a single-shard daemon the
//! shard that must answer is the one blocked in the syscall (measured hazard: the `soft,intr` mount
//! would time the call out, seconds later), so the table is the only sound source. The table says
//! whether the path is exactly a mount point, its filesystem type and its source: the loopback mount's
//! source is `localhost:/<volume name>@<attachment>.<token>` (§4.13), so on macOS the evidence
//! names the volume after checking the capability's format and removing its secret. The FUSE mount's
//! source is `slates` for every volume (`crates/bridge-fuse/src/mount.rs`), so there the evidence is
//! the filesystem type alone, and the report says so (`names_volume`). The container's view is then
//! exactly the host mount's; that it *works* is proven by use, never by the record — T-4.13 runs the
//! same workload on the host path and inside a real container over the bind.
//!
//! What is refused, typed, before any effect: a relative path (`NotAbsolute`, nothing consulted), a
//! path that is not a mount point (`NotAMountPoint` — a directory inside a mount is not the
//! attachment), another filesystem (`ForeignFilesystem{fstype}`), another volume's export
//! (`NotThisVolume{source}`), a destination that is not absolute, and a table that cannot be read
//! (`MountTableUnavailable`). Bounds: the table is the kernel's own, bounded by its mount count
//! (`getfsstat`'s count; Linux `fs.mount-max`), and the Linux read is capped at that count times a
//! per-line bound derived from `PATH_MAX`.
//!
//! The crate holds no `std::fs`, no `std::net`, no socket and no directory creation; its one `unsafe`
//! block is the macOS `getfsstat` pair (no safe wrapper exists). The pure parts — the table parser,
//! the verification and the entry — are tested on every host with simulated tables (`tests/verify.rs`).
//! Record: `docs/wip/oci-handoff.md`.

pub mod binding;
pub mod mount_table;
pub mod verify;
