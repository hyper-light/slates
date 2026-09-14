# The OCI namespace handoff and the attachment capability report (§4.6 A-9, RQ-20, AC-4.11 / T-4.13): design and measurement record

> Status (2026-09-14, branch `agent/oci-handoff`, four commits over `0ac7aad`). `attach` and
> `status` now report, per host, every transport with the six facts of §4.6 A-9 (supported
> transport, target-path constraints, read/write policy, sharing/cache semantics, residency boundary,
> conformance evidence), each fact read from the machine or stated from what the tree holds, and a
> request for a form the host cannot offer refuses `AttachmentUnsupported{transport, reason}` before
> any effect. The OCI form is built: a container bind of the established host mount, verified
> against the kernel's mount table (never by touching the mount), recorded as `AttachForm::Oci`, and
> handed back as the runtime-specification `mounts` entry; a read attachment is a read-only bind.
> T-4.13's container leg runs by use here (macOS, Docker Desktop over the NFS-loopback mount) and
> has a CI Linux variant over a real FUSE mount; the virtio-fs device's own report is carried on the
> same fields and a guest form over the ring is refused typed. Measured and reported typed: over
> the container bind on macOS, Docker Desktop's share holds files a container touched open beyond
> the container's lifetime, so an in-container delete leaves an NFS silly-renamed `.nfs.*` entry that
> blocks `rmdir` and the plain unmount. This document is the assistant-owned record; the numbered
> requirements live in `SLATES_DESIGN.md` §4.4/§4.6 and the ledger in `GAPS.md` (the sentences for
> both are in §9 below, for the integrator).

## 1. The requirement and where each sentence is enforced

| Contract sentence (§4.6 A-9 unless noted) | Enforced by |
|---|---|
| "Capabilities differ by host, kernel, runtime and VMM and must be reported by `attach` and `status`: supported transport, target-path constraints, read/write policy, sharing/cache semantics, residency boundary and conformance evidence." | `StatusReport.transports: TransportReport {os, kernel, oci_runtime, capabilities}` and `Attached.capability: AttachmentCapability` (`crates/ipc/src/protocol.rs`); the table `crates/server/src/transports.rs`, pure over one `Platform` seam; `os`/`kernel` from `uname`, the listener from `NFS_PORT`, the runtime from a `PATH` probe. |
| "Requesting an unsupported form returns `AttachmentUnsupported{transport, reason}`." | `Refusal::AttachmentUnsupported{transport: AttachTransport, reason: UnsupportedReason}`; raised in `verbs::establish_form` before the lease or the record (`crates/server/src/verbs.rs`). |
| §4.4 "attach(volume\|snapshot, consumer, transport, chosen_path?)"; "Attach with a chosen path that cannot be honoured: Refused (`ChosenPathUnavailable{reason}`)." | `RequestBody::Attach.form: AttachRequest {Root, Oci{source, destination}, Guest{transport}}`; `Refusal::ChosenPathUnavailable{reason: HostPathReason}`. |
| "A host OCI runtime passes the established host attachment into the container mount namespace." | `crates/server/src/oci.rs` + `crates/bridge-oci`: slates verifies, records and reports; the runtime binds (`docker run -v`/`runc` `mounts[]`); slates enters no namespace. |
| "A metadata record is insufficient evidence of a usable container path or guest device." | The bind is verified against the kernel's mount table (`HostMountEvidence`), and its usability is proven by use, never by the record: T-4.13 runs the workload inside the container (`crates/cli/tests/cli.rs`, `crates/bridge-fuse/tests/oci_container.rs`). |
| "No disk socket, image construction, target mkdir or privilege escalation is implicit in attaching a VFS volume." | `bridge-oci` holds no `std::fs`/`std::net`, creates nothing; the destination is created by the runtime inside the container's rootfs; R10 by construction. |
| "the read/write policy" | A read attachment yields `options: [rbind, ro]`; the runtime enforces it (`Read-only file system` measured). |
| Appendix C "OCI namespace handoff ... must report [its] own tested semantics"; "macOS NFS fallback: `.nfs` temp files on delete-while-open" | `SharingSemantics.delete_while_open: DeleteWhileOpen {NoKernelClient, Unlinked, SillyRenamed}` — the NFS loopback mount and the bind on macOS say `SillyRenamed`, measured (§4 below). |
| AC-9.7 "A skipped lane or pure simulation cannot close its transport guarantee." | `Conformance {None, VerbLifecycleTest, LiveKernelMountTest, ContainerWorkloadTest, SimulatedGuestDriver}` names the evidence class the tree holds for the transport on this platform; a refused transport claims `None`. |
| The guest form (GAP-A9-5's owed "transport report on the `attach`/`status` wire") | `transports::translate_guest` carries `slates_bridge_virtiofs::capability::host_capability` fact for fact; `AttachRequest::Guest` over the ring refuses `SeamNotOnWire` (or the device's own `BindingNotBuilt`). |

## 2. Inventory before this work (file:line at `0ac7aad`)

- `crates/ipc/src/protocol.rs:218` `RequestBody::Attach { volume, snapshot, intent }` — no transport
  or form; `:1193` `ReplyBody::Attached { attachment, lease_epoch, path: Option<String> }` ("none until
  a bridge exists"); `:888` `StatusReport` — an attachment count and `nfs_port`, no capability facts;
  `:994` `Refusal` — no `AttachmentUnsupported` (only `Unsupported{feature}`).
- `crates/server/src/verbs.rs:3285` `attach` — always `AttachForm::Root`, `Consumer::Sdk`, `path: None`;
  `:3655` `status`.
- `crates/db/src/catalog.rs:309–348` `Consumer {Sdk, Bridge, Launcher}`, `AttachForm {Root,
  ChosenPath{path}}`, `AttachmentRecord`.
- `crates/bridge-virtiofs/src/capability.rs` `TransportCapability` (the six facts, in-process only);
  `crates/bridge-virtiofs/src/admission.rs:184` `AdmissionError::AttachmentUnsupported {transport:
  GuestTransport, reason: UnsupportedReason}` — the only raise site; `crates/server/src/virtiofs.rs:67`
  `guest_transport_capabilities()` — "the guest form is not yet on the `attach`/`status` wire".
- The mounted host attachment: macOS `slates mount ID PATH` (`crates/cli/src/mount.rs`, `mount_nfs
  ... localhost:/<name>` at a user-owned directory; the `classify` probe for `mount_nfs`), served by
  `crates/server/src/nfs.rs`; no attachment record is made for a mount. Linux: `crates/bridge-fuse`
  has the codec, dispatch and `fusermount3` launcher (`src/mount.rs`, `fsname=slates,subtype=slates`)
  but the daemon does not serve `/dev/fuse` (`crates/server/src/lib.rs` has `nfs` and `virtiofs`
  only; `crates/cli/src/exec.rs` calls the root mount "owed").
- CLI `attach ID [--read|--write] [--snapshot N]` (`crates/cli/src/args.rs:737`), `emit_attach`
  (`verbs.rs:397`); MCP `slates.attach.attach` (`crates/mcp/src/lib.rs:281`, `attachment_json:764`,
  `status_json:716`).

## 3. What is built

```
crates/ipc/src/protocol.rs      AttachTransport, TargetPathConstraint, ReadWritePolicy, KernelCache,
                                DeleteWhileOpen, SharingSemantics, Residency, Conformance,
                                UnsupportedReason, AttachmentCapability, OciRuntime, TransportReport,
                                AttachRequest, Established, HostMountEvidence, OciBinding,
                                HostPathReason; Refusal::{AttachmentUnsupported, ChosenPathUnavailable}
                                (appended); StatusReport.transports (Box), Attach.form,
                                Attached.{established, capability} (appended fields)
crates/wire/src/codec.rs        impl Wire for Box<T>: transparent (same bytes, same schema hash)
crates/server/src/transports.rs the report: Platform seam, Situation, the per-transport table,
                                host facts (uname), the OCI runtime probe, translate_guest
crates/server/src/oci.rs        the container bind: paths → mount table → verify → the entry
crates/server/src/verbs.rs      attach: rights → form (establish_form) → lease (take_write_lease)
                                → record; status.transports; refusal names
crates/bridge-oci/              mount_table.rs (getfsstat(MNT_NOWAIT) / /proc/self/mountinfo),
                                verify.rs (pure), binding.rs (pure); tests/verify.rs (7)
crates/db/src/catalog.rs        AttachForm::Oci{source, destination, read_only} (appended)
crates/client/src/client.rs     Attachment.{established, capability}; attach_with(form)
crates/mcp/src/lib.rs           one vocabulary: capability_json, transport_report_json,
                                established_json, oci_binding_json, oci_mount_json, *_name
crates/cli/src/{args,verbs}.rs  attach ... [--oci-source HOST_PATH --oci-destination CONTAINER_PATH]
                                (the source resolved to the real path); status prints the report
crates/server/tests/attach_forms.rs   4 by-use tests over the ring (report, forms, refusals, guest)
crates/cli/tests/cli.rs               T-4.13 on macOS over Docker Desktop (SLATES_TEST_CLI=1)
crates/bridge-fuse/tests/oci_container.rs   T-4.13's CI Linux variant over a real FUSE mount
```

Commits, in order: `71cd63f` the report and the typed refusal; `416b273` the OCI form;
`f5b1c94` T-4.13 by use and the measured share semantics; `33fb52f` the guest transports' report.

### 3.1 The wire, and what the integrator must know

Appended variants go last in their enums: `Refusal::{AttachmentUnsupported, ChosenPathUnavailable}`,
`AttachForm::Oci`, and every new variant of the new enums. Three existing shapes gained trailing
fields — `RequestBody::Attach.form`, `ReplyBody::Attached.{established, capability}`,
`StatusReport.transports` — so the schema hashes of `RequestBody`, `ReplyBody`, `Refusal`,
`StatusReport` and the db `LogEntry` change; every construction site in the tree was updated (three
in `crates/server/tests/daemon.rs`; the client fills `AttachRequest::Root`). `Box<T>` is
transparent on the wire (golden test `a_boxed_value_is_transparent_on_the_wire`), which keeps every
`ReplyBody` move small: clippy found `Served` at 272 bytes when the report was inline.

### 3.2 What the daemon verifies for the OCI form, and why through the mount table

The kernel's mount table is read as a query that never touches a mount: macOS
`getfsstat(MNT_NOWAIT)` — "without requesting an update from each file system", `getfsstat(2)`;
Linux `/proc/self/mountinfo` (`proc(5)`), through a read-only descriptor, bounded by
`fs.mount-max × 4·PATH_MAX`. A `statfs`/`stat` of the path itself would issue an NFS (or FUSE)
request to the very daemon asking; on a single-shard daemon the shard that must answer is the one
blocked in the syscall, and the `soft,intr` mount would time the call out seconds later. Verified,
in the order that consults the least: the source absolute → the destination absolute → the table
read → exactly a mount point (the last mount at a path wins) → the bridge's filesystem type → where
the bridge's source names the volume (`nfs` + `localhost:/<name>` on macOS), this volume;
`fuse.slates`'s source is `slates` for every volume, so on Linux the evidence is the type alone and
`names_volume: false` says so. Every refusal is typed (`HostPathReason`): `NotAbsolute`,
`DestinationNotAbsolute`, `NotAMountPoint`, `ForeignFilesystem{fstype}`, `NotThisVolume{source}`,
`MountTableUnavailable{errno}`.

The record carries the authorized binding (`AttachForm::Oci`); the reply carries the entry the
harness hands its runtime — `{destination, type: "bind", source, options: [rbind, ro|rw]}` — and
the evidence. The OCI runtime specification's field names and option words (`config.md` "Mounts",
the Linux bind form) are **quoted from memory** of the specification; the entry drove Docker's
`-v source:destination:ro|rw` equivalently in T-4.13; verifying the `type`/`options` words against
the published specification before they are cited outside this tree is owed.

### 3.3 The report, transport by transport (this build, 2026-09-14)

| Transport | macOS | Linux | Windows |
|---|---|---|---|
| `Root` (the record form) | offered; `RootMount`; `NoKernelClient`; `DaemonRam`; `VerbLifecycleTest` | same | same |
| `NfsLoopback` | offered iff the listener bound; `UserOwnedExistingDirectory`; `ClientTimeouts`; `SillyRenamed`; `DaemonRamAndKernelCache`; `LiveKernelMountTest` | refused `MountNeedsPrivilege` (the kernel refuses `nfs` in an unprivileged user namespace; R10) | refused `HostPlatform` |
| `Fuse` | `HostPlatform` | `BridgeNotWired` (the daemon does not serve `/dev/fuse` yet) | `HostPlatform` |
| `Fskit` | `BridgeNotWired` | `HostPlatform` | `HostPlatform` |
| `WinFsp` | `HostPlatform` | `HostPlatform` | `BridgeNotWired`; `DriveLetter` |
| `Oci` | offered iff a host mount is (the listener bound); `ContainerDestination`; `InheritedFromHostMount`; `SillyRenamed`; `DaemonRamKernelCacheAndRuntimeVm`; `ContainerWorkloadTest` | refused `HostMountRequired` | refused `HostPlatform` |
| `VirtioFsInProcess` | offered (the device's report); `GuestTag`; `Unlinked`; `DaemonRamAndGuestPageCache{dax_mapped: false}`; `SimulatedGuestDriver` | same | refused `HostPlatform` |
| `VirtioFsInheritedDescriptor` | refused `BindingNotBuilt` (the device's own reason) | same | refused `HostPlatform` |

The read/write policy is the caller's ceiling on `status` and the intent's on `attach`. The host
facts: `os`/`kernel` from `uname` (`Darwin 25.4.0` here), `oci_runtime` the first of `runc, crun,
youki, docker, podman, nerdctl` on the daemon's `PATH` (`docker` here) or a typed absence
(`NoneOnPath`; `NotProbed` on Windows, where the `PATHEXT` probe is not built).

## 4. Measured (macOS 26.4 / Darwin 25.4.0, Apple Silicon, rustc 1.98.0, Docker Desktop 29.3.1 with the `linux/amd64` `alpine:3.20` image under emulation, runtime `runc`; the box at the memory wall throughout — swap 16.0 GB of 17.4 GB used, fseventsd 32 GB RSS — so timings are not benchmarks)

- **Docker Desktop's file sharing carries the NFS-loopback mount point into its VM.** Inside the
  container the bind is `fakeowner` over `/run/host_mark/private` (the container's own
  `/proc/self/mountinfo`; the share is of `/private`, so a `mktemp -d` mount point under
  `/private/var/folders` is shared without configuration). A host file read back `hello from host`
  (15 bytes); a container write read back on the host `hello from container` (20 bytes) owned by
  the mounting user (`adalundhe staff` — the share's requests carry the host user's `AUTH_SYS`
  uid), one copy; `-v ...:ro` gave `Read-only file system`. (`probe.sh`, 2026-09-14, 3 min 14 s
  including one 90 s `timeout` on the first emulated run; the two later runs returned in seconds.)
- **Ownership as the container sees it** is the `fakeowner` layer's: root:root for the default
  user, `501 dialout` with `--user 501:20`; on the host every entry is the mounting user's. The
  volume root itself lists as `root wheel` on the host (`drwxr-xr-x 2 root wheel`) — a sibling
  observation, §7.
- **Delete-while-open through the share** (`sidecar.sh`, `release.sh`, 2026-09-14): inside the
  container, `rm /work/d/g.txt` after the container wrote and renamed it leaves
  `.nfs.2005102c.1812` (5 bytes) in `/work/d`, `rmdir /work/d` answers `Directory not empty`, and a
  top-level `rm /work/top.txt` leaves `.nfs.2005102d.1812`; the container exited after 1 s; the
  entries were **not released within 120 s** after the exit; the host's `rmdir` failed the same way
  and `slates unmount` answered `Resource busy` for a further 30 s until `umount -f`. The same
  steps by the host alone (no container) remove and `rmdir` cleanly. Mechanism: the macOS NFS
  client silly-renames a file unlinked while any process on the host holds it open (Appendix C);
  Docker Desktop's host-side share holds every file a container touched open for the share's
  lifetime (the VM mounts `/private` once, not per container). Consequence for users: an
  in-container delete leaves a `.nfs.*` entry in the volume until the share lets go, its directory
  cannot be removed meanwhile, and a forced unmount leaves the entry in the volume for good. slates
  serves the client's RENAME and REMOVE faithfully; the report now states the rule
  (`delete_while_open: SillyRenamed` on the NFS loopback mount and the bind), and T-4.13 asserts it.
- **AppleDouble sidecars**: a file the host shell writes carries `com.apple.provenance` (`ls -la@`),
  which the NFS client stores as a `._<name>` sidecar (4096 bytes) the share refuses to list
  (`ls: /work/._host.txt: Operation not permitted`); the sidecar is renamed and removed with its
  file. Files written from the container carry no xattr. The listings compare without `._*`.
- **T-4.13 live** (`SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli -- --exact
  an_oci_container_consumes_the_host_attachment_through_the_runtime_bind --nocapture`): the first
  full attempt failed on the container's `rmdir` (the finding above; the workload's expectation was
  wrong, not slates); then `ok` in 6.45 s and, after the awk fix for the view probe, `ok` in 5.27 s —
  one anchor+daemon, one real `mount_nfs`, two containers, the plain unmount busy afterwards and
  forced by the guard. During the 6.45 s run the anchor restarted the daemon once at startup (its
  documented heartbeat-lapse recovery under load).
- **Ring-level tests** (`cargo test -p slates-server --test attach_forms`): 4 passed in 1.03–1.17 s
  across the pieces; red first at each piece (10, then 5 unresolved names).
- `crates/bridge-oci/tests/verify.rs`: 7 passed (the real table lists `/`, `apfs` here).
- Gates: `cargo fmt --all --check` clean; `cargo clippy --workspace --all-targets -- -D warnings`
  clean (10.9 s); `cargo xtask check` structural ok (28 shipped crates), literals ok, unsafe ok
  (`slates-bridge-oci` 1/1). Linux cross-lint: `CC_x86_64_unknown_linux_gnu=clang
  CFLAGS_x86_64_unknown_linux_gnu="--target=x86_64-unknown-linux-gnu -nostdlibinc -isystem $(xcrun
  --show-sdk-path)/usr/include" cargo clippy -p slates-bridge-oci -p slates-bridge-fuse -p
  slates-server --all-targets --target x86_64-unknown-linux-gnu -- -D warnings` → `Finished`, no
  diagnostics (10.5 s; zstd-sys and ring built with the clang stand-in). The Windows cross-lint's
  result is in the final report (run after this document was written).

## 5. Tests and evidence, with the commands

| Command | Result (2026-09-14) |
|---|---|
| `cargo test -p slates-server --test attach_forms` | 4 passed (status: host facts = `uname`, every entry supported xor reasoned, NFS loopback iff the listener, FUSE `HostPlatform`, WinFsp `HostPlatform`, the bind's constraints; attach: read-only/read-write by intent, `/` → `ForeignFilesystem{apfs}`, `/private` → `NotAMountPoint`, relative → `NotAbsolute`, snapshot → `SnapshotNotPresentedByHostMount`, nothing recorded; the Oci entry iff NfsLoopback; the guest entries and the three ring refusals) |
| `cargo test -p slates-server --lib transports` | 8 passed (the pure table on four platforms × listener bound/unbound; the runtime probe with an injected predicate; the guest translation over the device's own `host_capability`) |
| `cargo test -p slates-bridge-oci` | 7 passed |
| `cargo test -p slates-server --test daemon -- --exact the_daemon_serves_the_lifecycle_verbs_exactly_once_with_leases_and_typed_refusals` | 1 passed (13.45–13.49 s) |
| `cargo test -p slates-server --test virtiofs` | 2 passed |
| `cargo test -p slates-wire --test golden` | 5 passed (the Box transparency) |
| `cargo test -p slates-ipc --lib` | 7 passed, 1 ignored |
| `cargo test -p slates-db` | 34 + 16 + 6 + 10 + 2 + 7 passed |
| `cargo test -p slates-cli --bin slates` | 21 passed (the OCI flags together, or a usage refusal naming the missing one) |
| `cargo test -p slates-mcp --test mcp` | 1 passed |
| `SLATES_TEST_CLI=1 cargo test -p slates-cli --test cli -- --exact an_oci_container_consumes_the_host_attachment_through_the_runtime_bind --nocapture` | 1 passed (6.45 s; 5.27 s) |
| `crates/bridge-fuse/tests/oci_container.rs` | Linux only: cross-linted here; runs in the CI Linux lane's `cargo test --workspace`, skipping loudly without `fusermount3`, `docker`, or `allow_other` (`/etc/fuse.conf` `user_allow_other`) — **not run here** |

Not run, by the charter: the fleet suite, the `SLATES_TEST_CLI=1` suite as a whole, load runs,
benchmarks.

## 6. Decisions

- **One `attach` verb with a form**, as §4.4 writes it, rather than a second attach verb per form:
  `AttachRequest {Root, Oci, Guest}` on the request; `Established {Record, OciBind}` on the reply.
- **The form is checked before the lease and the record**, after the rights (§4.13: rights before
  any lookup or effect), so "Refusal rolls back owned resources" holds by having taken none.
- **The bind is offered exactly where a host mount is** (`transports::host_mount_offered`): macOS
  with the listener bound today; Linux says `HostMountRequired` until the FUSE serve lands, never
  `BridgeNotWired` for a form that is built. Windows says `HostPlatform`.
- **The conformance fact names the evidence class the tree holds**, never a run: a build-time
  statement per platform, cross-checked by the tests that are that evidence (the T-4.13 test asserts
  the reply's `conformance`/`delete_while_open` against what it then observes).
- **The runtime probe is a fact, not a requirement**: the daemon reports the first OCI runtime (or
  CLI) on its `PATH` or a typed absence; the harness may hold its own.
- **A cold report lives boxed** (`Box<TransportReport>`, `Box<OciBinding>`) rather than boxing every
  `ReplyBody`: the provisioning reply stays allocation-free (R9).
- **The workload asserts the measured delete-while-open rule** rather than avoiding it: the
  contract requires the semantics to be reported and tested; a workload that skirted the delete
  would certify less.

## 7. Siblings found (reported, not changed)

- The mounted volume's root directory lists as `root wheel` (uid/gid 0) on the host and in the
  container (`drwxr-xr-x 2 root wheel 0`), while every file created through the mount is the
  mounting user's; the container user (`--user 501:20`) can still write into it because the NFS
  server's `ACCESS` grants everything (slates is not a sandbox). Whether the root inode should carry
  the creator's uid is `crates/server/src/nfs.rs`'s/the base-fuse agent's call.
- `crates/cli/tests/cli.rs`'s `MountPoint` guard swallowed a busy `umount` and could leave a dead
  mount behind a failed assertion; it now forces the unmount when the plain one is busy (changed).
- `crates/server/src/virtiofs.rs`'s module doc said the guest form's report "is the next leg"; now it
  points to `crate::transports` (changed).

## 8. Owed, with what each needs

1. **The daemon-served FUSE mount on Linux** (the base-fuse agent's GAP-A9-4 work): when it lands,
   `transports::host_mount_offered` gains the Linux arm, the Oci entry is offered there, and
   `bridge-oci`'s `expected_mount(Fuse, ..)` should gain a source that names the volume (today
   `fsname=slates` for every volume — `fsname=slates:<name>` would let the table name the volume, as
   the NFS export does).
2. **Verification of the runtime-specification words** (`type: "bind"`, `rbind`, `ro`/`rw`) against
   the published OCI runtime specification (`config.md` "Mounts"), quoted from memory.
3. **The Windows `PATH` probe** (`PATHEXT`) for `oci_runtime`; reported `NotProbed` until built.
4. **A subtree bind** (a directory inside the mount as the source) is refused `NotAMountPoint`
   today; a narrower authorized view would need the subpath in the record.
5. **The container leg on the CI Linux lane** needs `fusermount3` and `user_allow_other` on the
   runner; the test skips loudly without them, which cannot close the transport guarantee (AC-9.7).
6. **The share's open-handle lifetime** with Docker Desktop is the runtime's; a user who needs
   clean deletes from containers over the NFS-loopback mount needs a share that closes with the
   container (or a host mount without silly rename — the FSKit primary, when wired).

## 9. For the integrator (docs this branch does not touch)

`docs/wip/GAPS.md`, Bridges (4.6) row — append to the status cell:

> **The attachment capability report and the OCI form are built (2026-09-14, `71cd63f`…`33fb52f`; GAP-A9-5's report and container halves): `attach` and `status` report every transport with the six facts of §4.6 A-9 — `TransportReport {os, kernel, oci_runtime, capabilities}` on `StatusReport` and the form's `AttachmentCapability` on `Attached` — each fact read from the machine (`uname`, the bound listener, the `PATH` probe) or stated from what the tree holds, a refused transport carrying its typed `UnsupportedReason` and no evidence; a request for a form the host cannot offer refuses `AttachmentUnsupported{transport, reason}` before the lease or the record. The OCI form (`AttachRequest::Oci{source, destination}`, `crates/bridge-oci`, `crates/server/src/oci.rs`) verifies the host path against the kernel's mount table without touching the mount (`getfsstat(MNT_NOWAIT)` / `/proc/self/mountinfo`; a `statfs` of the path would deadlock a single-shard daemon), records `AttachForm::Oci`, and returns the runtime-specification `mounts` entry (`type: bind`, `rbind` + `ro`/`rw` by the attachment's policy) with `HostMountEvidence`; an unbound path is refused `ChosenPathUnavailable{NotAbsolute | NotAMountPoint | ForeignFilesystem | NotThisVolume | ...}`. T-4.13 runs by use on macOS over Docker Desktop (`crates/cli/tests/cli.rs`: the same workload on the `slates mount` path and in `docker run -v` over the daemon's entry; byte and name/size agreement; the container's edit is the host's and the host's delete the container's; the read-only bind refuses a write; `/private` refused typed) with a CI Linux variant over a real FUSE mount (`crates/bridge-fuse/tests/oci_container.rs`). Measured and now reported typed (`SharingSemantics.delete_while_open`): Docker Desktop's share of the host path holds files a container touched open past the container's life, so an in-container delete over the NFS-loopback mount is silly-renamed to `.nfs.*` (not released within 150 s), blocking `rmdir` and the plain unmount. The guest transports carry the virtio-fs device's own report fact for fact (`VirtioFsInProcess` offered with `GuestTag`, DAX not mapped, `SimulatedGuestDriver`; `VirtioFsInheritedDescriptor` refused `BindingNotBuilt`), and a guest form over the ring refuses `SeamNotOnWire`. Owed: the daemon-served FUSE mount (then the bind on Linux), the runtime-specification words verified against the published text, a live guest for AC-9.7. Record: `docs/wip/oci-handoff.md`.**

`docs/wip/GAPS.md`, GAP-A9-5 row — replace the "Gap and source finding" cell's text with:

> Device half **built** (2026-09-13): the owned FUSE-over-virtio device with device admission, per-attachment credits, refusal-before-access on malformed chains (T-4.14) and the owning-shard loop, wired into the daemon; DAX not advertised. **Report and container halves built (2026-09-14):** `attach`/`status` carry the six §4.6 A-9 facts for every transport, the guest transports from the device's own report; `AttachmentUnsupported{transport, reason}` and `ChosenPathUnavailable{reason}` are `Refusal`s raised before any effect; the OCI form is a verified bind of the host mount, recorded (`AttachForm::Oci`) and handed back as the runtime's `mounts` entry, proven by T-4.13 on macOS over Docker Desktop with a CI Linux variant over FUSE (`docs/wip/oci-handoff.md`). Still open: a real VMM binding (libkrun in-process, vhost-user inherited descriptor — the seam is built, the bindings are not), the guest form's durable `AttachmentRecord`, a live guest for AC-9.7, and the bind on Linux once the daemon serves the FUSE mount.

and its "Closure gate" cell: `AC-4.11–4.12/T-4.13–4.14 (device leg proven with the simulated
driver; the OCI leg proven by use on macOS and cross-linted for the Linux lane; live guest open);
Phase 4.`

`docs/wip/SLATES_DESIGN.md` §4.6, a status blockquote to add after the virtio-fs status blockquote
(before "Writeback and snapshot barrier"):

> **Status (OCI handoff and the capability report, 2026-09-14).** `attach` and `status` report every transport with the six facts above (`crates/server/src/transports.rs`, pure over one platform seam; `crates/ipc/src/protocol.rs` `TransportReport`/`AttachmentCapability`): the record form, the NFS loopback mount (offered on macOS exactly when the listener bound; refused `MountNeedsPrivilege` on Linux, R10), the FUSE/FSKit/WinFsp bridges (`BridgeNotWired` on their platform until the daemon serves them), the container bind, and the virtio-fs guest transports from the device's own report; each fact is read from the machine or stated from what the tree holds, a refused transport carries its typed reason and claims no evidence, and a request for a form the host cannot offer refuses `AttachmentUnsupported{transport, reason}` before the lease or the record. The OCI form is built: the daemon verifies that the named host path is the mount point of this volume's export through the kernel's mount table — never by touching the mount (`crates/bridge-oci`: `getfsstat(MNT_NOWAIT)`, `/proc/self/mountinfo`) — records the authorized binding (`AttachForm::Oci`) and returns the runtime-specification `mounts` entry (`type: bind`, `rbind` + `ro`/`rw` by the attachment's policy) with the table's evidence; the runtime binds; an unbound path is refused `ChosenPathUnavailable{reason}`. T-4.13 is proven by use on macOS over Docker Desktop's share of the NFS-loopback mount (`crates/cli/tests/cli.rs`) with a CI Linux variant over a real FUSE mount (`crates/bridge-fuse/tests/oci_container.rs`): the same workload on the host path and in the container agrees byte for byte and in names and sizes, an edit on either side is the other's view, the read-only bind refuses a write. Measured and reported typed (`SharingSemantics.delete_while_open`): the runtime's share holds every file a container touched open beyond the container's lifetime, so an in-container delete over the NFS mount is silly-renamed to `.nfs.*` by the macOS NFS client (Appendix C), blocking `rmdir` and the plain unmount until the share lets go. A guest form requested over the ring refuses `SeamNotOnWire` (the harness hands the VMM seam in-process) or the device's own `BindingNotBuilt`. Owed: the bind on Linux once the daemon serves the FUSE mount; a live guest for AC-9.7. Record: `docs/wip/oci-handoff.md`.
