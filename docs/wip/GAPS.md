# The gap ledger (authoritative, kept current in the same change as any acceptance or tripwire)

Rubric per item: research on file? spec section exists? test matrix? acceptance criteria? laptop
degenerate stated? open decisions named? Classification: `undesigned | designed-unspecced |
specced-untested | decision-open | drift (owed-and-forgotten)`. A stale ledger is itself a gap.

## 0. The one global fact

A-9, 2026-09-05: the system has substantial component source and historical tests, but it
cannot yet offer the complete product contract. This status is based on read-only review of
Slates `a1059ed` and Hecate `103c078`, not a new test run. Fourteen source findings and their
triggers are in [the audit](../bugs/2026-09-05-system-contract-audit.md); §8i is the current
closure ledger. Design corrections are implemented in docs only. They do not fix the code.

Sections §8a–§8h preserve dated implementation records. An earlier "gated" claim applies only
to the named test and its original scope; it does not close the A-9 integration or correctness
gaps. The TLA runs in §10 are historical, bounded model evidence, not a proof of current Rust.

## 1. Component inventory

| Subsystem (design §) | Current source status, 2026-09-05 | Open contract and acceptance |
|---|---|---|
| Machine/memory/runtime (4.1–4.3) | Foundation crates and historical measurements exist. Admission/residency integration incomplete. | GAP-A9-1, GAP-A9-11; AC-0.10–0.11, AC-2.11 |
| Volume/namespace/base (4.4–4.5, 4.15) | Core CoW, host seam and witnesses exist; mounted base routing and snapshot coverage incomplete. | GAP-A9-2, GAP-A9-3, GAP-A9-13; AC-1.16–1.17 |
| Bridges (4.6) | Linux FUSE codec/dispatch/transport/launcher; the NFSv3 loopback bridge (`bridge-nfs`, the macOS fallback + oracle) — now **serving a live mount**: `serve_connection` (`src/server.rs`) reads ONC RPC records off a stream and dispatches portmap `GETPORT`, MOUNT `MNT` and the NFSv3 procedures onto a `VolumeBridge`, the socket-and-mount half the codec was built to sit behind. A hand-rolled client mounts and reads a seeded file back byte-for-byte over a real socket in CI (`tests/loopback.rs`), and the `nfs_loopback` example serves a real `mount_nfs` client — no signing, no kext, no privilege beyond the mount itself (R10). This is the macOS live-mount path that needs no Apple entitlement, verified here. The **production async server** now runs on slates's own runtime, closing §4.6's "the production server multiplexes it on slates's runtime": `serve_connection_async` (`src/server.rs`) serves a connection over the runtime's async `TcpStream`, reads and writes awaiting the shard's driver (`write_all` awaits write-readiness, so a stalled client yields the shard rather than blocking it), sharing the RPC engine (`dispatch`) and record codec with the blocking form — one engine, two transport adapters. It rests on new async TCP in the rt (§4.3): `Driver::register_writable` (the `EVFILT_WRITE`/`EPOLLOUT` sibling of `register_readable`; kqueue+epoll implement it, the completion-native and sim drivers refuse it as owed) and `tcp::{TcpListener, TcpStream}` over one shared readiness future with `udp::UdpSocket` (rt unsafe budget 18→20, the two write-filter registrations). Because a volume is `!Send` (a `Box<dyn Clock>`), the serve loop reaches its shard by the daemon's own idiom (a `Send` boot task via `spawn_on`, then `futures::spawn` for the non-`Send` loop). Proven by use with no privilege: `tests/async_loopback.rs` mounts and reads a seeded file back byte-for-byte from the async server on the runtime (the same client as the blocking test), `crates/rt/tests/tcp.rs` the accept→read→write round trip on the runtime's own sockets; the `nfs_async` example serves a real `mount_nfs`. And the server now serves **many volumes**, not one: `MultiExport` (`src/multi.rs`) routes each request to the volume its file handle names — every served NFS procedure begins with a file handle, and the handle already encodes `(volume, inode, gen)`, so the router reads the leading handle's volume id and hands the untouched request to that volume's `Export` (a handle for a volume the server does not hold is `NFS3ERR_STALE`); `NfsService` is the seam the serve loop and `dispatch` now work over (a single `Export` or a `MultiExport`), so one server serves one volume or many with no transport change. And the **single root mount** the design calls for (§4.6 line 128, "the single kernel mount point per host under which volumes appear as directories") now works end to end: `MultiExport` serves a synthetic read-only root directory whose entries are the volumes — `MNT /` returns its handle, `GETATTR`/`ACCESS`/`READDIR`/`READDIRPLUS`/`FSINFO`/`FSSTAT` describe and list it, `LOOKUP` a volume's name returns that volume's own root handle (the same a direct mount gives), and every mutation is `NFS3ERR_ROFS` (a volume appears by a metadata operation, never a client `mkdir`, design line 129). And the serving core is now built to the **daemon's shape**, not the test's: a `VolumeSet` seam (`src/multi.rs`) supplies the volumes, and because a shard holds many volumes sharing *one* store, a volume is served through a *transient* `VolumeBridge` built per request (the design's "marshal each operation into the bridge queue of the owning shard", §4.6 line 1341; the shape `bridge-fskit`'s `MountSession` already takes). `MultiExport<V: VolumeSet>` does the routing and the synthetic root above the seam; the daemon implements `VolumeSet` over its `ShardState` (one store, a volume slab), a test over `OwnedVolumeSet` (one store, several volumes — the same shared-store shape). Proven by `tests/multi.rs` over a real socket against two volumes in one shared store (distinct inode prefixes, as a shard assigns). **And the daemon now serves it, end to end:** `crates/server/src/nfs.rs` binds a loopback listener at boot (its port on `Daemon::nfs_port`), serves it on the control shard, and `ShardVolumeSet` implements `VolumeSet` over the shard's `ShardState` — resolving a volume through `state::with_state` and a transient `VolumeBridge::attached` per request (the daemon's serve path; it lends the volume slot's base host and a fresh handle slab). Each connection is a detached task, so connections are concurrent (a request borrows the shard state only for the synchronous serve; the awaits are on the socket). Proven with no privilege by `crates/server/tests/nfs_mount.rs`: a single-shard daemon is started, a client provisions a volume through the real rendezvous, and then over the daemon's NFS port a client mounts that volume, **creates a file, writes bytes, and reads them back** — the bytes travel client → NFS → `ShardVolumeSet` → the shard's real volume and back. A real `mount_nfs localhost:PORT` would do the same over the kernel. And it serves volumes on **any shard**: the **cross-shard bridge queue** (§4.3, D-7 "bridge queues pinned to the owner") is built. A request naming a volume this shard does not own is routed (`route`, by the volume's owner *partition* from `verbs::owner_of` mapped to a shard) to run the same `serve_call` on the *owner* shard — spawned there exactly as the client path forwards a verb (`Control::Spawn`, §4.3) — and the owner spawns a task back on the accepting shard that hands the reply to the awaiting connection task through a per-shard, thread-local pending map (no new runtime primitive, no lock). Proven by `tests/nfs_mount.rs`: a single-shard daemon mounts a provisioned volume and writes then reads a file back byte-for-byte, and a **two-shard** daemon does the same for a volume on a shard *other* than the NFS listener's, over the bridge queue. Reaching a volume from the single host root works across shards too: a root `LOOKUP` routes by the looked-up name (which is a volume's id in hex), so `mount /` then `cd <id>` reaches a volume on any shard (`tests/nfs_mount.rs` `a_client_mounts_the_host_root_and_reaches_a_remote_volume_by_id`). And the host root's *listing* gathers every shard's volumes over the bridge queue: a root `READDIR`/`READDIRPLUS` scatters an entry-gather to each other shard (the same spawn/back-spawn the bridge calls use) and lists them all, so `mount /` then `ls /` shows every volume on the host (`tests/nfs_mount.rs` `the_host_root_listing_gathers_volumes_from_every_shard`); a remote volume's per-entry attributes are absent in READDIRPLUS (a client fills them with a `LOOKUP`, which routes across shards), the rest is complete. **So the whole browse — `mount /`, `ls /`, `cd <id>`, read/write — spans shards.** Each request runs as the **mounting user**, not always root: the daemon reads the uid from the call's `AUTH_SYS` credential (`nfs::subject_of` over bridge-nfs's new `auth_sys_uid`; `AUTH_NONE` falls back to root, §4.13), and the subject rides to the owner shard on a cross-shard call, so a request runs as the same user there. Tested by `crates/bridge-nfs/tests/auth.rs` (an AUTH_SYS credential's uid is read; AUTH_NONE names no user). And an object it creates is now **owned** by that user, not root: `VolumeBridge`'s three creating verbs (`create`/`mkdir`/`symlink`) stamp the new inode's uid from the request subject and its gid from the parent directory (the BSD/macOS create rule), at the shared seam so every transport inherits it — the fix for the *root:wheel mount bug* (`docs/bugs/2026-09-09-root-wheel-mount.md`; before it a created inode kept the volume core's born default uid 0, so a file an ordinary user made through the mount listed as `root`). And the group is the mounting user's own too (Ada's call, 2026-09-09): the request's `AUTH_SYS` **gid** is threaded beside the subject as one `Requester` (subject + group) through the daemon's NFS path — including across shards — and overlaid onto the created object via `OpContext::owner_gid`, so a file lists as e.g. `adalundhe staff`, exactly as a native NFS server stamps it (a mount with no credential group, `AUTH_NONE`, falls back to the parent's group — the BSD rule). The group is deliberately *not* in the uid-only §4.13 `Principal`; it rides as file ownership, carried on the `Export` (`set_owner_gid`, set per request) rather than the authenticated attachment, so `attach` and the 40 `Export::new` call sites stayed untouched. `auth_sys_creds` reads both uid and gid from one credential (`auth_sys_uid`/`auth_sys_gid` project it). Proven by `crates/bridge-core/tests/volume_bridge.rs` (`a_created_object_is_owned_by_the_mounting_user_and_its_parent_group`, failing-first: uid 0 where the subject's is required; and `a_created_object_takes_the_request_group_when_the_credential_names_one` for the credential-group half) and live on a real kernel mount (the `slates_mount` example now lists `hello.txt` as `adalundhe staff`, was `root wheel`). Minor remaining: the volume *root* directory (`.` at the mountpoint) is still `root wheel`, created 0/0 at provisioning and shared across mounts — the mount root's own ownership, not a file created through the mount (the reported, fixed defect); stamping it needs the provisioning user's group carried into `Volume::create`, a separate change. The served NFSv3 procedure set now includes **COMMIT** (`fsync`) and **LINK** (a hard link), which had fallen through to `PROC_UNAVAIL`, breaking real workloads over the mount. COMMIT (RFC 1813 §3.3.21): every slates write already lands `FILE_SYNC` (synchronously durable before its reply), so it is a no-op that reports the file stable and returns the same `writeverf3` a WRITE does — a client's `fsync`, which the kernel issues as COMMIT, now succeeds instead of failing, so the git/sqlite/editor workloads (§6 test set) that fsync work. LINK (§3.3.15): the volume core supports hard links (`Bridge::link`), so `ln a b` makes a second name for a file — both names resolve to the same object and its link count rises — instead of failing. Proven by use in `tests/procedures.rs`: `a_commit_over_the_export_reports_the_write_stable` (with `a_commit_of_the_host_root_is_a_no_op`, so `fsync` on the `mount /` root is fine) and `a_link_over_the_export_makes_a_second_name` (both names name one object, nlink 2; a LINK on the read-only synthetic root is `NFS3ERR_ROFS`). A routing-level stale/bad-handle COMMIT and LINK are framed in their own failure shapes (`status_only_or_wcc`). So every NFSv3 procedure a real program depends on is served (create/lookup/read/write/remove/rename/mkdir/rmdir/symlink/readlink/link/setattr/access/readdir(plus)/getattr/fsstat/fsinfo/commit). **MKNOD** (device/FIFO/socket nodes) is refused with the typed `NFS3ERR_NOTSUPP` — a RAM CoW filesystem does not create special nodes — not `PROC_UNAVAIL`, honoring the project's typed-refusal rule (an uncategorized refusal is a bug); a client's `mknod` gets a proper NFS error framed as the directory's `wcc_data`, and on the read-only synthetic root it is `ROFS` like the root's other mutations (`tests/procedures.rs` `a_mknod_over_the_export_is_notsupp`). And **PATHCONF** is served: it reports the volume's POSIX limits from the volume's own policy — the name maximum and case behaviour from `statfs` (the neutral `FsStat` gained a `case_sensitive` field, filled from the volume's `NameEquivalence`, the additive completion of the info surface that already carried `namelen`, and the *only* `Bridge` impl is `VolumeBridge`), the link maximum as the `u32` counter's range, names refused-not-truncated (`no_trunc`), and chown unrestricted (`chown_restricted` false — `Volume::chown` imposes no privilege check). **So every RFC 1813 NFSv3 procedure is now handled**: a client's `pathconf` gets real answers, an exact-name volume reporting case-sensitive and a case-folding one case-insensitive (`tests/procedures.rs` `pathconf_reports_the_volume_limits`, `pathconf_reflects_a_case_folding_volume`), and the synthetic root reports uniform limits. Only MKNOD (unsupported) is a typed refusal; nothing falls through to `PROC_UNAVAIL`. And a volume now appears under its **friendly provisioned name** (§4.6 "Chosen path"), not its id in hex: the shard's `VolumeSlot` carries the name (set at create/clone/recovery), `ShardVolumeSet::entries` lists it, and a root `LOOKUP`/`MNT` of a name routes across shards by `owner_of_name` — the same partition the create routed to and the id encodes, so a name reaches its volume with no global index (D-14) — where the owning shard resolves it against its own slots (`MultiExport` matches the name in `entries`, already name-agnostic, so bridge-nfs needed no change). Proven end to end by `tests/nfs_mount.rs`: `mount /vol`, `mount /vol-N` on another shard over the bridge queue, `cd rv-N` from the host root across shards, and `ls /` listing every volume by name. The self-describing hex scheme is replaced, not layered (no dual path). Owed here (minor, situational refinements): And the volume now **mounts**: `slates mount <id> <path>` reads the volume's name and the daemon's NFS port (`StatusReport::nfs_port`, a process-global word the daemon sets at boot) and runs `mount_nfs -o vers=3,tcp,port=P,mountport=P,noresvport,soft,intr,locallocks,nosuid,rdirplus,actimeo=1 localhost:/<name> <path>` (`crates/cli/src/mount.rs`) — no privilege (`noresvport`, R10), no kernel extension, no Apple entitlement, the same signing-free mechanism sylk mounts over FUSE-T (itself NFS-backed). The attribute-cache `actimeo` is a documented short value (loopback GETATTR is sub-millisecond so revalidation is cheap; sylk uses the same trade-off for its 100 ms FUSE timeout). **Proven by a real mount on macOS in-sandbox**: after `slates mount`, `mount` shows `localhost:/<name> on <path> (nfs, ..., mounted by <user>)`, a file written through the mount reads back byte-for-byte, and the arg construction is unit-tested (`mount.rs` `the_mount_arguments_target_the_daemon_port_and_the_named_export`). The mount **lifecycle and capability detection are adapted from sylk's cgofuse mount** (`core/purevfs`): a pure, predicate-injected `classify` (mount_nfs present → the NFS loopback backend, testable on any host with no live mount, the way sylk factors `classifyDarwinFUSEBackend`), a probe that refuses with a message naming what is missing rather than a raw `mount_nfs` failure, and `slates unmount <path>` (`umount`, the counterpart of sylk's cgofuse `Close`; no daemon needed, so a stale mount unmounts even after its daemon has gone). **Proven live in-sandbox**, now by a repeatable by-use test (`crates/cli/tests/cli.rs`, gated `SLATES_TEST_CLI=1`, loud-skip without `mount_nfs`) driving the real binary — `slates mount` → a file written through the mount reads back byte-for-byte → `slates unmount` → the mount table clean — and by a runnable example (`cargo run -p slates-cli --example slates_mount`) that drives an in-process daemon to the same live kernel mount. The **anchor-held listener** (§4.6 line 509, restart survival) is now built: a supervising anchor binds the NFS loopback listener and hands its descriptor to every daemon it spawns (`slates-cli`'s `hold_nfs_listener` + `Supervisor::hold_nfs_listener`, inheritable across the spawn), and the daemon adopts it (`daemon.rs` `nfs_listener` over `slates_rt::tcp::TcpListener::{from_fd,into_fd}`) rather than binding a fresh ephemeral one, so the loopback port is stable across a restart — a live mount survives it; a standalone daemon (tests) still binds its own (Unix only: NFS is the macOS/Linux bridge). Proven by use: `crates/rt/tests/tcp.rs` (`a_listener_handed_over_by_descriptor_serves_on_the_same_port` — a listener reduced to a descriptor and re-adopted serves on the same port) and `crates/anchor/tests/anchor.rs` (`the_supervised_child_inherits_the_held_nfs_listener` — a real spawned child finds the held listener at its bound port on the inherited descriptor, the first proof here that the `Command` descriptor hand-off works, since macOS hands the segment over by name); rt unsafe 20/20 unchanged (both new constructors are safe), slates-server 4→5 for the daemon's `from_raw_fd` on the inherited number. The root-listing gather now fans out to the shards in parallel. The §4.6 differential oracle (line 1368) is **not** an owed refinement here but gated with item (1): it mounts the same volume via FSKit *and* via NFS and compares the abstract states — two real kernel mounts — so it needs the FSKit mount and hence the Apple Developer entitlement this sandbox cannot hold; a synthetic FUSE-dispatch-vs-NFS-dispatch stand-in would be vacuous (both legs dispatch onto one `VolumeBridge`, R5). The **macOS FSKit bridge** now has its Rust half **complete** (`crates/bridge-fskit`, A-1/D-O9): the shim wire codec for the whole `Bridge` operation set (read/write/handles/dir-enumeration/namespace/links/refs/metadata/root — 21 operations) dispatched onto the one seam, a golden vector pinning the wire, 10 by-use tests against a real `VolumeBridge` (round-trip, hostile-input, a directory lifecycle, symlink→readlink, create→rename→lookup, a chmod+truncate setattr, the real root object, a NotFound reply). Codec-first, exactly as the NFS codec. The Swift half now **compiles against the real framework** (macOS 15.4+ SDK): `ShimWire.swift` is the codec (a pure library, cross-checked byte-for-byte against the Rust golden vector), and `SlatesVolume.swift` is the `FSVolume` + `FSUnaryFileSystem` handler — it conforms to FSKit's real `FSVolume.Operations`/`ReadWriteOperations`/`OpenCloseOperations` and `FSUnaryFileSystemOperations`, translating every operation to a shim request; `swiftc -emit-library` builds a dylib exporting `SlatesVolume`/`SlatesItem`/`SlatesFileSystem` with `@objc` conformance thunks over the real FSKit signatures (built on macOS 26 here; the CI step skips loud on a pre-15.4 runner). `HandlerTest.swift` drives the real handler by use against FSKit with no mount (constructing `SlatesVolume` and a mock `ShimChannel`), asserting the shim requests it emits and the FSKit objects it builds — including that two opens then two closes of one item release both daemon handles (the per-inode handle stack fixed a leaked open-reference the earlier single-handle map had), and that a NotFound lookup surfaces an error, not a crash). Owed (the Phase 4 mount spike, now just the transport and the live run): the app-group ring behind the `ShimChannel` seam, the `Slates.app` bundle + FSKit entitlement + `UnaryFileSystemExtension` `@main`, and the live mount that exercises the `FSItem` lifecycle. The earlier `SPIKE:` guesses are resolved and by-use tested: the root object id (`OP_ROOT` learns the real `compose(prefix, 1)`), the time unit (Unix nanoseconds) and the open/close handle accounting (a per-inode handle stack that releases every handle, not the single-handle map that leaked one); and `probeResource` now recognizes a `slates://` URL resource by its scheme and refuses others, rather than accepting unconditionally. `HandlerTest.swift` drives all of these against the real handler with no mount (20 checks). And the transport is no longer only a stub: the `test-harness` feature builds the crate as a cdylib exposing a C ABI over `serve` (`src/ffi.rs`), and `InProcessTest.swift` links it to drive the real handler through `serve` over a real `VolumeBridge` on a real scratch volume — the whole handler↔codec↔bridge stack end to end in one process (create→lookup→write→getattr→remove, the root proven to be `compose(prefix, 1)`). What remains is the production transport reaching the daemon's real volumes, and the live mount — and the daemon-side serve is now **built**: `VolumeBridge::attached` lends the bridge an *external* open-handle map instead of owning one (a non-breaking addition — FUSE and NFS keep `new` and its owned slab), and `bridge-fskit`'s `MountSession` owns that map per mounted volume, building a transient bridge per request over the shard's store and volume. That is the Rust shape of a garbage-collected mount handler (the per-mount state lives in the session; the volume access is threaded per call — the borrow checker's price for no GC, and a small one). It is proven by use: `a_mount_session_persists_open_handles_across_requests` drives create→open→unlink→read→release→read across *separate* requests and shows the content reclaimed only after the last handle is released — which requires the map to have persisted (§4.8 unlink-while-open). The serve handles both scratch and overlay volumes: `attached` also takes an optional *borrowed* host (a `HostRef::Borrowed`, the overlay analogue of the borrowed handle map — `OsHost` is not `Clone` and its reads take `&mut`, so the transient bridge borrows it as it borrows the store), so `MountSession` lends the shard's `OsHost` per request and a base entry the empty overlay does not hold is served from it (`a_mount_session_serves_an_overlay_base_through_the_borrowed_host`, over the repo's crates tree read-only, no RAM disk). What remains is genuinely external: the app-group ring that delivers requests (needs the signed bundle) and the mount-session/attachment lifecycle (§4.13). The in-process form is also proven end to end against a scratch volume (`InProcessTest.swift`); the production transport form is the spike's choice (§4.6). The 20th op, `OP_SETATTR`, is wired end to end: `setAttributes` carries the fields FSKit marks valid (chmod/chown/truncate/utimes) to the bridge's `setattr` and returns the new attributes (`serve_sets_attributes_through_the_bridge`). The 21st, `OP_ROOT`, fixes a real correctness bug the handler carried: `activate` learns the volume's true root object (`compose(prefix, 1)`, per-volume prefixed) from the daemon instead of assuming inode 1, which was wrong for any prefixed volume (`serve_returns_the_real_root_object` proves it against a prefix-1 volume whose root is provably not 1). Two former `SPIKE:` guesses are now verified against the daemon's code: the object generation is a stable 0 (D-4) and the shim times are Unix nanoseconds. Both ops round-trip on the Swift side. The **WinFsp bridge is now built end to end** (`crates/bridge-winfsp`, 2026-09-09) — the Windows mount, the last owed OS-integration path. Two parts: the refusal taxonomy `ntstatus(&VfsError) -> Ntstatus` (the analogue of the FUSE errno and NFS `nfsstat3` edges), host-buildable and tested on any host; and the **mount host** (`host.rs`, Windows-only) — the `FSP_FILE_SYSTEM_INTERFACE` vtable and the `FspFileSystem*` FFI **hand-transcribed from winfsp's `winfsp.h`/`fsctl.h`** (`ffi.rs`), the exact discipline the rt AFD reactor uses over the WDK, so it is a faithful model, not a guess (the earlier "winfsp's header-defined protocol needs the headers on Windows" framing is overturned: reading the real headers and transcribing the `#[repr(C)]` structs — which carry the header's own `static_assert` sizes — is the same move that built `afd.rs`). The 16 implemented callbacks (`GetVolumeInfo`/`GetSecurityByName`/`Create`/`Open`/`Overwrite`/`Cleanup`/`Close`/`Read`/`Write`/`Flush`/`GetFileInfo`/`SetBasicInfo`/`SetFileSize`/`CanDelete`/`Rename`/`ReadDirectory`) dispatch onto the shared `Bridge` over a **single owner thread** that holds the `!Send` volume — WinFsp's dispatcher threads send each op over a bounded channel and block for the reply (D-7's "sharing is a move over a bounded channel"; no `Mutex`, no `Arc`, R2), serialized on top by WinFsp's COARSE operation guard; a reference "allow Everyone" security descriptor (one SDDL string) satisfies the FSD's access checks. The whole crate cross-lints clean for `x86_64-pc-windows-msvc` from macOS (`cargo check`/`clippy -D` — no linking needed to type-check the FFI) and is CI-wired on the `windows-latest` runner: `choco install winfsp`, clippy the crate, and a **live mount test** (`tests/mount.rs`, gated `WINFSP_TEST_MOUNT=1`) that mounts a slates volume at a free drive letter through the real kernel FSD and creates/writes/reads/lists/deletes a file through the Windows filesystem, then unmounts — the Windows analogue of the macOS FSKit and `mount_nfs` live mounts. unsafe budget 0→86 (all Win32/WinFsp FFI with `// SAFETY:` lines, no `unsafe impl Send`/`Sync`). Owed: the daemon transport that reaches a *provisioned* volume's shard (the WinFsp callback forwarding to the owning shard, the analogue of the NFS `ShardVolumeSet` — the mount serves a directly-owned volume today), the reparse-point (symlink) and security callbacks left `None`, and the live-mount *runtime* proof depends on the Windows runner (the CI job runs it). | GAP-A9-3–5; AC-3.10–3.12, AC-4.11–4.12 |
| IPC/local database (4.7–4.8) | Rendezvous, metadata replay and completion transactions exist; content recovery incomplete. | GAP-A9-6, GAP-A9-9; AC-2.12–2.13 |
| Registers/configuration (4.8) | Pure register, ledger, mirror and reconfiguration simulations; BUG-12 fixed in `d9cb6e5`, broader BUG-13/protocol evidence open. Cluster plane now built in `slates-cluster` (A-10): SWIM/Lifeguard detector (live over sim UDP), the hecate Raft dialect complete in its mechanism set (election/replication/PreVote/CheckQuorum/ReadIndex/joint consensus with log-integrated transition/snapshot compaction + install-snapshot, sans-io, plus a multi-node conformance suite), the config group folding its committed log into the `Configuration` (reconcile + takeover), register-commit live over authenticated sim UDP, Vivaldi coordinates (live over the wire), progress extension. The Raft dialect also rides the transport (a `RaftMessage` codec and a live election+replication proof over sim UDP). Phase-one promotion now exists in the transport-driven register too (`register.rs`: `Prepare`/`Promise`, `install_authority`, `prepare`, `promote_over_holders`) and rides the transport (`serve_promotion`/`promote_record`/`promote_under_configuration`, a live `f=1` takeover adopting the committed head in `promote.rs`), oracle-tested for Continuity and StaleNeverCommits — single-value register takeover. The **ledger's committed-prefix adoption now rides the transport too**, the multi-entry generalization of that single-value takeover (`ledger::{LedgerAcceptor, LedgerPromise, adopt}` + `cluster::{serve_ledger_promotion, promote_ledger_record}`): a new owner ships a phase-one prepare, each holder raises its fence and reports its **whole log**, and the owner adopts per position the identity under the highest epoch across the quorum. Proven live over sim UDP in `tests/ledger_promote.rs` (an `f=1` takeover adopting the committed prefix `[r0, r1]`, recovering `r1` which after the owner's death only the surviving holder still holds — Continuity), the `LedgerPromise` wire codec hostile-input tested, the `f=0` degenerate observably identical (R8). The owner-runtime composition now exists too (`fleet.rs`: `FleetNode` composes membership + config group + the owner acceptor, keeping the acceptor authority in step with the configuration version; `ConfigGroup::new(owner, quorum)` is its f-parameterized constructor), with the design-mandated N=1≡fleet differential (R8) as a named test and a head commit driven live over the transport (`fleet_live.rs`) — refuting the earlier "gated on scatter width/hardware" reading (the register core is f-parameterized; fleet semantics are testable in-process over the sim). The register object is now the creator-routable 128-bit `ObjectId` (high half = creator host), unifying with the catalog `VolumeId` and enabling routing-by-id with no catalog (§4.8 "Lookup"; `d580be0`), replacing the `object: u64` shortcut. That change surfaced and fixed two real bugs: **placement collision** — `placed_state` truncated the 128-bit `VolumeId` to its high 8 bytes (the creator alone) as the u64 object, so every volume of one creator collided to one placement object; now the object is the full `VolumeId`. **Rendezvous avalanche** — plain FNV-1a left the last fed byte unmixed, so object ids sharing a creator half and differing by one low byte all picked the same holder; added the MurmurHash3 fmix64 finalizer so rendezvous spreads. **Fleet integration built (2026-09-10, Ada's "build all of fleet"):** the **object→owner routing view** (`cluster::routing::Routing` — per-object `object→owner`, no directory (D-14), `take_over` via `rendezvous_first`, `db04059`); it is **composed into `FleetNode`** so `observe` returns `Observed { config_changed, takeovers }` and a death drives takeover (`69f09c6`); the **detector→fleet bridge** `sync_membership` (folds confirmed deaths → takeover and alive joins, `6d2cdad`); and the whole **membership → takeover path proven live over the transport** (`tests/membership_takeover.rs`: a silent peer's real probe timeout → the detector declares it dead → `sync_membership` → the survivor takes over its objects, `fae1aa1`). **`FleetNode` is wired into the daemon** (`bf427c4`): each shard's `ShardState` holds one (`FleetNode::solo` at boot), and the daemon's placement authority (place/region_placed/await_placed/host_epoch) now flows from `fleet.configuration()` — the register/placement path runs the fleet's configuration group, degenerate at N=1 (R8). The **commit and promotion dispatch now consume the progress extension** (§4.8 "late work"): `CommitBudget::with_extension` seeds a `DeadlineExtender` + `ProgressWitness` into the collection loop (`collect_acks`/`collect_promises`), so a commit whose quorum is still filling as it passes its deadline is extended and commits, while a stalled one (no new acknowledgement within the stall window) is left to time out uncertain — one shared `DispatchWait` step replacing both fixed-deadline tasks, proven live over sim UDP (`tests/extend.rs`: a progressing `f=2` commit runs past its base deadline and places, a stalled one expires uncertain within the extension budget), with the `CommitBudget::hard` degenerate reproducing the fixed deadline exactly (R8). The **daemon config now carries the fleet membership**: `DaemonConfig.fleet: Option<FleetMembership>` (the quorum + peer hosts) drives the `FleetNode` boot construction (`init_shard` builds `FleetNode::new(host, quorum, peers)` when set), the laptop leaving it `None` (solo — the `f = 0` degenerate, the same code path, N=1 unchanged with every daemon test green). Owed: the daemon's control shard running the live probe/gossip *loop* over real peer connections (peer-address directory + connection management + the perpetual task driving `sync_membership` and each takeover's phase-one recovery/serve); the **cross-node commit path** (daemon head commit → `FleetNode::commit_head` over peer endpoints, unifying with the local `db` register); the live probe/gossip loop on the control shard over real peer connections and a multi-node daemon test. **`Endpoint::accept` now accepts a peer without knowing its address in advance** — it adopts the source of the first datagram it hears (mutual-TLS-gated by `allowed_clients`), proven by use in `session.rs` (a server that was never told the client's port replies over the session it learned) — which unblocks a **two-node** fleet: each node accepts its one peer on its advertised socket and dials the peer from a separate socket, so the two sessions are cleanly one-directional (no bidirectional-request deadlock). A node serving **many** peers on one socket still needs the connection-ID demux the endpoint marks owed (`endpoint.rs`: the destination connection ID is zero-length). **The two-node daemon membership loop is now BUILT** (`crates/server/src/fleet.rs`, boot step 6): `FleetTransport` (this node's fleet TLS identity, advertised address, and peers) is handed to `Daemon::start_with_fleet` and drives a control-shard loop — a probe task (owns the `Detector`, dials the peer with a bounded-retry handshake since the transport does not retransmit, probes each period, and folds the converged view into the shard's `FleetNode` via `sync_membership`) and a serve task (accepts the peer via `Endpoint::accept` and answers its probes). Proven live by two in-process daemons (`crates/server/tests/fleet.rs`): they form a fleet over real loopback UDP with mutual TLS, and when one is stopped the survivor detects the probe timeouts, ages the suspicion to death, and **retires** the dead peer (`Daemon::fleet_members` observes it) — a transition only the loop can make (the seeded config would hold it alive forever), so the proof is non-vacuous. The **cross-node commit path is now BUILT** (`crates/server/src/fleet.rs` `ship_records`, boot step 6): each node dials its peer's record address at boot alongside the probe session and, each period, commits its unplaced volume heads to the peer holder over that session (`commit_record` — the owner's local hold plus the remote holder, committed at `f + 1`), recording the acknowledging `Placement` in `ShardState.placed_heads` so the verbs' `region_placed`/`await_placed(Region)` report the head placed. Proven live by two in-process daemons over real loopback UDP + mutual TLS (`crates/server/tests/fleet.rs` `a_provisioned_head_replicates_across_the_fleet`): a volume provisioned on one node replicates to the peer holder and reaches the `f = 1` quorum, non-vacuous (at `f = 1` a solo head is not region-placed — only the replicated commit places it). Building it closed a real transport gap: a single-shot request/reply over a **real** datagram socket needs loss recovery, because a dropped packet or acknowledgement leaves no later ack to expose the gap (the probe path tolerated this by retrying each period; a single commit cannot). The transport now drives its tail-loss probe from the estimated PTO — every reliable exchange (`Endpoint::{request, serve_once, send_stream, recv_stream}`) waits for the next packet only up to the probe timeout (`Endpoint::receive_or_probe`, RFC 9002 §6.2.1) and on a timeout retransmits the oldest in-flight packet (`Connection::probe`, already built), and the handshake seeds the RTT estimator (`establish`, RFC 9002 §5.1) so that timeout reflects the real path from the first packet rather than the coarse initial RTT. The fleet frame cap is now derived from the RFC 9000 §14.1 minimum datagram (a whole fleet message in one frame), replacing a value copied from a transport test. The membership loop is now the **general N-peer form**: `run_membership` iterates the transport's peers, binding one serve socket per peer per plane (since `Endpoint::accept` pins one peer per socket) and spawning a probe/serve/ship set per peer, each peer's own detector folding into the shared `FleetNode` via the new **peer-scoped** `cluster::fleet::sync_peer` (which touches only the peer it tracks — the whole-view `sync_membership` would let one peer's detector re-join a peer another has retired, flapping it; `sync_peer` removes that, unit-tested). Proven live at N=2 by the two tests above (the two-node fleet is the single-peer degenerate). **N-node mesh formation is now reliable (2026-09-10):** a fleet of N forms all N·(N−1) probe/record handshakes over `Endpoint::accept` — proven by the new `three_daemons_form_a_full_mesh` (all six three-node sessions establish; 20/20 in measurement), asserted per node by the new `Daemon::fleet_meshed` (every configured peer *actually probed*, distinct from the optimistically seeded membership `fleet_members` reports from boot, recorded in `ShardState.formed_probe_peers` as each `probe_peer` dial completes). Three transport fixes to `Endpoint::establish` made it solid: **(1) handshake confirmation** — TLS 1.3 finishes the client the instant it *sends* its final flight but the server only when it *receives* it, so a dropped final flight stranded the server forever; `confirm_as_client`/`confirm_as_server` add a 1-RTT confirmation (a fresh-numbered `Connection::emit_confirm` packet) the client waits for and the server resends while it still sees the client's flight retransmits (RFC 9001 §4.1.2 / RFC 9000 §19.20 HANDSHAKE_DONE in spirit). **(2) Fast establish retransmission** — the retransmit interval backs off exponentially from the timer granularity rather than a flat two-thirds of a second, so a peer slow to bind its socket during the boot race (a fleet forms as its nodes boot one after another) is reached within milliseconds, capped at the conservative initial PTO (`handshake_probe_ceiling` = `RttEstimator::initial_pto`, not the tiny loopback-seeded estimate a flat cap would collapse to). **(3) Flight dedup** — the aggressive retransmit races the peer's reply, so a re-sent flight identical to one already fed to `read_hs` is recognized and skipped rather than faulting the handshake stream. The N=3 **retirement** test (`three_daemons_..._retire...`) is now **un-ignored and reliable (measured 27/27, and the whole 4-test fleet suite 5/5 with 0 ignored)** — a survivor's SWIM probe of the dead node reliably times out, ages it to death, and both survivors retire it. Reaching that took a **runtime timer-wheel bug fix** on top of formation: the wheel's `cancel` (`crates/rt/src/timer.rs`) unlinked a timer by bare slot index *before* validating the id's generation, so a **stale cancel** — one whose slot had already fired and been reused by a *later* timer (the SWIM probe cancels its deadline task the instant a healthy probe is acked, and the fired slot is reused constantly under a fleet's load) — spliced the *reused* live timer out of its slot list, **orphaning it** in the arena so it never fired. That stranded the next probe's `sleep`, so a survivor's probe of the dead node hung past its deadline. Fix: remove from the arena first (which validates the generation and refuses a stale id) and only then unlink, using the removed entry's own recorded position — regressed by `a_stale_cancel_does_not_orphan_the_timer_that_reused_the_slot`. This was NOT a "noisy machine" flake: a three-node fleet's own task/timer churn triggered it every time (0/12 before, 27/27 after). The formation test waits for the real direct mesh (`Daemon::fleet_meshed`) before the kill so the survivors are retiring peers they actually probed. **Durable held records are now BUILT (2026-09-10, takeover slice 1):** a node backing a peer as a candidate holder now keeps each accepted record in a **durable per-object acceptor** in `ShardState.holder_records` (keyed by `ObjectId`, authority owner = the socket's TLS-authenticated peer), replacing the task-local acceptor that discarded everything past the serve task — so a survivor's phase-one recovery has the newest committed record to read. `crates/server/src/fleet.rs` `serve_peer_records` now serves the peer's commits into that hold via `accept_held_record`, which also tracks the object in the routing view (`FleetNode::track_object`), so the owner's death hands `sync_peer`'s takeover computation the object. One acceptor **per object** (each object has one owner, so one `Authority`) keeps every acceptor within the register's "one authorized owner per generation" model and lets a promotion route to its hold by object id regardless of the peer socket that carried it; the *per-object authority* a single acceptor would need to serve several owners at once stays the owed refinement. Proven by use (`crates/server/tests/fleet.rs` `a_holder_durably_holds_the_owners_replicated_head`): after a head replicates in a two-node fleet, the holder reports it durably holds the owner's head (owner + value) via the new `Daemon::fleet_holder_head` — non-vacuous (the holder holds nothing until the record commit reaches it, and this is distinct from the owner's `fleet_head_placed` quorum view). **The takeover drive is now BUILT (2026-09-10, takeover slice 2):** on a death, the survivor's probe loop records the objects `sync_peer` reassigns to it (`ShardState.pending_takeovers`) and brings every held acceptor's authority into step with the routing view (`reconcile_held_authority` — `install_authority` to the successor, so a holder can answer the new owner's prepare and accept its re-commit); the record-ship task then **drives phase one** over the surviving candidate holder (`drive_takeover`: `promote_record` over the object's durable hold at a bumped epoch, `Promotion::adoption_record` re-committed under the new epoch via `commit_record`, the placement recorded in `placed_heads`); the serve loop dispatches a `Prepare` (fixed 40 bytes) vs a `Record` (longer) on the shared record socket, answering the promotion from the same hold (`serve_held_promotion`). Proven by use (`crates/server/tests/fleet.rs` `three_daemons_take_over_a_dead_owners_head`): a volume provisioned on the node that then dies is taken over by the survivor rendezvous ranks first, which serves the head region-placed **under its own ownership** (`fleet_head_placed`) with the value preserved — non-vacuous (a mere holder has no `placed_heads` entry; the seeded config never reassigns ownership), 8/8. **The takeover is now the general `f > 1` form (2026-09-10, `360582f`, corrected by the same-day review below):** the record plane is one **coordinator** task per node (`run_record_plane`) dispatching over **every** candidate holder's client session — kept up by per-peer link tasks (`establish_record_link`) in `ShardState::record_sessions` and borrowed per dispatch — replacing the former per-peer ship tasks; so a head is committed to **all** its candidates in one `commit_record` (the design's "records sent to all candidates at once", not N independent single-holder commits) and a takeover is promoted over **all** surviving candidate holders (`drive_takeover` borrows every holder session via `take_sessions`/`return_sessions`). This is what lets an `f > 1` promotion assemble its `f + 1` promise quorum over the several survivors one object needs — a per-peer ship task held only its own peer's session and could reach a one-holder (`f = 1`) quorum only. Proven by use (`crates/server/tests/fleet.rs` `five_daemons_take_over_a_dead_owners_head_over_a_multi_holder_quorum`, 8/8 + green in the full suite under load): a five-node `f = 2` fleet, a volume provisioned on the node that dies, the successor takes it over by promoting over a **three-promise** quorum (itself plus two other holders) and serves it region-placed under its own ownership with the value preserved — non-vacuous, and impossible for the old per-peer drive (one remote holder could never reach quorum three). The coordinator survives the owed connection-ID demux unchanged (only the socket count beneath it falls from O(N) to one). **Review of the coordinator (2026-09-10, same day; `docs/bugs/2026-09-10-swim-stale-ack.md` "Review addendum"):** reading the dispatch code rather than trusting the green 5-node run found five defects — three introduced by the consolidation (a straggler's late acknowledgement discarded on a non-quorum round, so it re-shipped forever at `f > 1`; an early quorum cancelling stragglers and dropping their sessions, which the per-peer-socket mesh cannot re-establish; one slow link's handshake blocking the whole record plane) and two pre-existing ones the sweep exposed (the verbs' `status`/`await placed(region)` never read the recorded acknowledgements — `Configuration::place` is the owner alone — so a fleet's head was reported **unplaced forever** at `f ≥ 1`, contrary to what this ledger claimed; and the record plane's owner acceptor was frozen at the boot generation, so after any join or retirement a newly provisioned head failed its own local hold `ForeignGeneration` and never placed). All five fixed and tested by use: `record_acks` merges every round's acknowledgements; the cluster dispatch no longer cancels stragglers but hands them back (`Stragglers`, on `Committed`/`Promoted`/`LedgerPromoted`; `crates/cluster/tests/extend.rs` proves a recovered session is live by placing a second commit only through it); per-peer link tasks keep the sessions in `ShardState::record_sessions` and the coordinator borrows them; `verbs::committed_placement` reads `placed_heads` (the two-node replicate test now also requires the owner's `await placed(region)` verb to answer `placed: true`); the coordinator re-installs the configuration authority each period (the three-node retire test now provisions on a survivor after the retirement and requires it to place). Also from the sweep: a peer whose serve sockets fail to bind/accept at boot was silently skipped — now counted as a `fleet.bind`/`fleet.accept` status refusal, tested. **Content replication and the takeover's content serve are now BUILT (2026-09-10, A-12):** each owned volume's newest snapshot is exported to the D-17 archive by a **resumable, budgeted walk** (`slates_vfs::export::SnapshotArchiver`, its slice derived from the profile's measured BLAKE3 throughput and the shard step budget — `DaemonConfig::archive_slice_bytes`; deterministic, restore-byte-identical, a base-backed entry refused rather than archived as zeros); the archive is put to the content candidates over the record session's own content streams by **missing set** (`slates_cluster::content`: `Offer`→`Missing`, `Put`→`Ack`, `Fetch`→`Have`, hostile-input tested), the first round to `f + 1` and later rounds hedged to the rest; a holder **verifies before it holds** (`ContentHold`: every chunk against its identity, the manifest against its hash, refused unless every referenced chunk is held — §4.10 "placement closure") and acknowledges bound to the object, sequence and manifest; only then does the head naming the manifest and the acknowledging holders ship (`HeadValue`, the head register's value — carrying the catalog essentials too, the split into a distinct catalog register class owed), and the snapshot is recorded placed durably (`SnapshotIdentified` + `SnapshotPlaced`), which `status` and `await placed(snapshot, region)` answer from. After a takeover the successor **serves the content**: it materializes the volume under its original id and name from the archive it holds, or fetches it by identity from a recorded holder (`verbs::materialize_taken_over`). Proven by use over real loopback UDP + mutual TLS + the daemon's real NFS port (`crates/server/tests/fleet.rs`): `a_sealed_snapshots_content_replicates_to_the_holder_and_places` (a file written over NFS, sealed; `await placed(snapshot, region)` true at `f = 1`; the holder holds the manifest whole) and `a_takeover_successor_serves_the_dead_owners_content_over_nfs` (three nodes; the file written on the owner reads back byte for byte over the successor's NFS port after the owner dies). Building it found and fixed a pre-existing reader bug: a volume's **first** snapshot has the id the catalog uses for "no snapshot" (slab slot 0, generation 0 → `SnapshotId { value: 0 }`), so `status`/`await placed` reported the creation head's placement for every first snapshot (`docs/bugs/2026-09-10-first-snapshot-id-is-the-none-sentinel.md`; the discriminator is now the volume's epoch). **The record plane now serves every owner shard (same day; D-7 "one owning shard per volume"):** the control shard alone holds the peer sessions and probes, so it hands each peer state it folds to every other shard's `FleetNode` (all copies of the configuration advance identically — `cluster::fleet::apply_peer_state`) and reaches every owner shard each period through the new `server::xshard` cross-shard call (a typed, deadline-bounded generalization of the spawn-and-spawn-back the verbs and the NFS bridge already use): the seal walk and the head values run on the owner shard, the archives and heads move to the coordinator by value, and the acknowledgements and durable placements are recorded back there; a taken-over volume is materialized on the shard its id routes to (`verbs::owner_of`), carrying the takeover's `PlacedHead` — sequence, **promotion epoch** and holders — so the successor's next seals of the object are written at the epoch the holders fenced it at (a head at the successor's lower host epoch would be refused `StaleEpoch`; proven by `reseal_places` in the takeover test). Runtime shard ids are process-global and never reused, so a partition index is not a shard id: every partition-addressed send maps through the daemon's shard list (the verbs' dispatch and the NFS bridge already did; the fleet's materialization and observers now do). Proven by use with two-shard daemons: `a_volume_on_a_non_control_shard_replicates_its_content_and_places` and `a_takeover_successor_serves_a_volume_on_a_non_control_shard` (the volume placed on the non-control shard by its name's routing, asserted). Owed in §4.10: content-defined chunking and the compress-or-not cost model (D-17; chunks are raw at the CoW chunk size), the hedge trigger from a measured p95 (the round deadline is the trigger), anti-entropy and the healer, erasure coding, remote attach and prefetch, live shipping, migration and mirroring. **The N-node probe was also made robust (2026-09-10, `docs/bugs/2026-09-10-swim-stale-ack.md`):** two defects that flaked the two-node retirement/formation (no gossip redundancy to mask them at N=2) are fixed — (1) the SWIM probe now carries a per-probe **nonce** the acknowledgement must echo, so a stale acknowledgement the reliable transport redelivered on the reused probe stream (a dead peer's buffered ack) no longer passes for a fresh one and keeps the peer looking alive; (2) `probe_once` now drives the request/reply **inline** and **returns the session whatever the outcome**, so a single missed probe (a lost packet, scheduling jitter, a nonce-rejected reply) no longer drops the unrecoverable session and retires a *live* peer — the session is re-probed, a still-live peer refutes the suspicion (SWIM incarnation refutation), and only sustained silence ages a peer to death across the window. Regressed by `crates/cluster/tests/swim.rs` `a_stale_nonce_acknowledgement_is_rejected...`; the two-node test, ~3% flaky before / ~8% with the nonce alone, is 90/90 after both. **The three-node takeover is now reliable under load (2026-09-10, 30/30 full-suite under load, was ~8% flaky):** three more defects closed on top of the probe fix (all in `docs/bugs/2026-09-10-swim-stale-ack.md`) — (a) the **record dispatch** dropped a straggler's session on a commit/promotion timeout (the collection loop cancelled it), which the per-peer-socket mesh cannot re-establish; each holder request now rides `request_within`, a deadline-bounded exchange that returns the endpoint **whatever the outcome**, so a load-timed-out commit or promotion retries over the same warm session (`CommitBudget::max_deadline_ns` bounds it); (b) the **ship session** now drives its handshake on **one persistent socket** (`establish_session`, since also shared by the probe plane — see below), retried each period so the peer's pinned `accept` completes rather than a fresh-port re-dial being ignored; and — the actual root — (c) a head was shipped **only until `f + 1` quorum, not to every candidate**: `unplaced_heads` gated on the object's overall placement, so once one per-peer ship task placed a head every other skipped it and a co-survivor never received it (failing the takeover's "both survivors hold the head" precondition and starving its promotion quorum). The gate is now **per holder** (`unplaced_heads` returns each head with the candidates that have not acked, and the placement's acked set is **merged, not overwritten**), so a head reaches all candidates — the design's "records are sent to all candidates; committed at `f + 1`." The fleet tests also `yield_now()` between poll checks instead of `spin_loop()`, yielding cores to the daemons rather than starving them. **The probe plane now shares the record plane's persistent-socket establishment** (2026-09-10): `probe_peer` previously dialed with a single `establish()` attempt (`dial`) and returned on failure, stranding a probe task for a peer slow to come up (a real scale-up-join gap); it now uses `client_for` + `establish_session` (one handshake attempt per period on the same socket, retried until it completes), the detector ticking only when a probe is actually sent, and `dial` is removed (fully replaced). **Final validation (2026-09-10): the whole fleet scales up and down flawlessly** — the full six-test fleet suite serialized as real `cargo test` (`--test-threads=1`, one process) ran **25/25 green under heavy load** (15 runs under 8 CPU spinners, then 10 oversubscribed with 24 on an 18-core box), `three_daemons_form_a_full_mesh` **60/60 sequential**, on top of the two-node retire/formation 90/90 and the three-node takeover 30/30 under load. A ~2.5 % `three_daemons_form_a_full_mesh` failure seen only when the test binary is run as **several concurrent OS processes** is a **test-harness artifact**, not a fleet defect: separate processes reuse released OS ephemeral ports and reset the process-local `unique()` host-id counter, so their fleet sockets collide and an `accept` pins the wrong source — conditions that cannot arise in real operation (distinct addresses) or the real suite (fleet tests serialize; one process). Documented in the bug doc, deliberately left as-is (a parallel-process-safe harness would need the daemon to accept pre-bound sockets — test-infra scope beyond the fleet). Owed at the transport (flagged): received-packet-number dedup (RFC 9002 §5.3, the deeper cause of the redelivery) and a re-establishable session (with the connection-ID demux / fixed-port mesh) for a *genuinely* broken session. Still owed at N≥3: the connection-ID demux (many peers on one socket, O(N) sockets for the O(N²) mesh); and full multi-process deployment. Owed beyond this: reconnection after a mid-run session loss (a link task re-establishes on a fresh socket, but the peer's pinned accept side rebuilding is owed with the transport's other reconnection work); the connection-ID demux (many peers on one socket, an O(N) socket count for the mesh's O(N²)); and full real (multi-process) network deployment. The **fleet membership config exists** (`DaemonConfig.fleet`, quorum + peers); the fleet TLS material stays out of the Clone-able config (`Identity` is not `Clone`) and enters via the loop when it lands; the loom/shuttle concurrency pass. | GAP-A9-7; AC-8.18, AC-8.20 |
| Wire/distribution (4.9–4.10) | Framing/canonical bodies and protocol primitives built. **The session-plane transport (§4.10a) is now built end-to-end and runs over the real `rt` UDP driver** (`crates/transport`): the RFC 9000/9002-shaped QUIC dialect — TLS 1.3 handshake (`rustls::quic`, pinned certs), ordered multi-stream delivery, packet-number assignment, **multi-range ACKs** (RFC 9000 §19.3), reorder-threshold loss detection + retransmit + probe, **ACK-of-ACK** state bounding (§13.2.4), the **dual-level `MaxStreamData`+`MaxData` credit law**, and **NewReno congestion control** (RFC 9002 §7). `Endpoint` binds a `slates_rt::udp::UdpSocket` and drives handshake + streams + request/reply over the wire, proven end-to-end over loopback UDP (`tests/session.rs`) and by sans-io oracles over any loss + reorder (`connection.rs`). Only empirical *tuning* (initial window, CUBIC-vs-Reno, pacing, ECN, an RTT-derived probe timeout) and fleet-level pieces (connection IDs, MTU coalescing, multi-node placement/routing) remain — Phase 8. See `docs/wip/fleet-transport.md`. | GAP-A9-8, GAP-A9-11; AC-7.7, AC-8.19–8.21 |
| Archive/compression (4.11) | Pure archive/raw-format and missing-set helpers; separate pre-existing archive edits outside A-9 review. | GAP-A9-8, GAP-A9-11; AC-7.7; compression/dedup rest remains planned |
| Agent surfaces (4.12) | Rust client and CLI subset; MCP in `slates-mcp`. The **CLI's read verbs now emit JSON** with a global `--json` switch — `status` (daemon), `status ID`/`volume stat` and `volume list` — reusing the MCP serializers (`slates_mcp::{status_json,summary_json,daemon_json}`, now public) so the CLI and MCP surfaces share **one** schema (§4.12 "consistent JSON", part of GAP-A9-10); the shared `status_json` gained the `nfs_port` field it had been missing. Proven by use in `crates/cli/tests/cli.rs` (`the_verbs_emit_json_with_the_json_flag`, gated `SLATES_TEST_CLI`): each verb's `--json` output is a JSON object/array carrying the volume's real fields, over a live daemon. A **failing** verb under `--json` now emits a structured error too — `{"error": {"kind", "message"}}` on stderr, same exit code — so a harness gets JSON on the error path as well as the success path (GAP-A9-10 "consistent JSON errors"). **`--json` now covers every client verb**, not the read verbs alone (Ada's steer, 2026-09-09: a human scripting the CLI wants every verb to speak JSON — `id=$(slates volume create x --bounded 8MiB --json | jq -r .id)`): the read verbs, the merge queries (versions/changed-since) and outcomes (submit/rebase), and the **volume-lifecycle verbs** — create/snapshot/clone (`{"id"}`, the one key across every creating verb), placed (`{"placed","mirror_age_ns"}`), attach (the MCP `attachment_json` schema), pin (`{"pinned"}`), rewitness (`{"paths"}`), grants and audit (arrays of objects), land (the MCP `slates.land.materialize` schema), and the outcome-only verbs resize/destroy/detach/destroy-snapshot (a uniform `{"ok":true}`). The lifecycle attach/land JSON reuses the MCP serializers (`slates_mcp::{attachment_json,outcome_json,landing_summary_json}`, now public) so the two surfaces stay one schema. The one exception is `base read`, which streams a file's raw bytes with or without `--json`. Each verb is extracted into an `emit_*` helper so `serve()` stays a branch-free dispatcher under the cognitive-complexity gate. Proven by the same live test (each lifecycle verb driven under `--json`, every id captured from its own JSON). Cursors (pagination) are the remaining `--json` gap. And the **MCP surface gained `slates.merge.declare`** — the namespace operations (unlink/rename/mkdir/rmdir/set_mode/symlink/link/set_xattr/remove_xattr) as one tool dispatching `WorkOp` by `op.kind`, so MCP's merge surface now matches the SDKs' (it had content `edit` but not the namespace dimension); proven in `crates/mcp/tests/mcp.rs`'s merge loop, where a work builds a directory tree with a rename, a mode change, a symlink, a hard link and an xattr, then submits cleanly over a real daemon and merge engine. The **Python SDK** now exists (`crates/sdk-python`, Phase 5, D-19): a PyO3 extension over the typed client, imported as `slates`, binding connect/create/snapshot/client_id/reconnects with typed refusals mapped to a `SlatesError` exception; tested by use (stdlib unittest — the module loads and a missing-daemon connect raises the typed refusal); maturin builds the cp39-abi3 wheel. The **TypeScript/Node SDK** now exists too (`crates/sdk-node`, D-19): a napi-rs addon over the same client (the `.node` Node loads), binding the same verbs with typed refusals mapped to a JS `Error` and every FFI integer range-checked; tested by use (Node's built-in `node:test` — the addon loads and a missing-daemon connect throws the typed refusal). napi's GC-owned binding `Rc` is D-8 exception 1, marked in place. **Both SDKs now bind the volume-management, merge and namespace verbs — `status`, `list`, `resize`, `destroy` (§4.4); the merge workflow `create_green`/`create_work`/`edit`/`submit`/`rebase`/`versions`/`changed_since`; and the namespace operations `unlink`/`rename`/`mkdir`/`rmdir`/`chmod`/`symlink`/`link`/`set_xattr`/`remove_xattr` (§4.16)** — alongside the earlier connect/create/snapshot. `submit` and `rebase` return a uniform outcome (`ok`, the new green `version`, and any `conflicts` windows); `edit` is a content splice whose bytes cross as Python `bytes`/a Node Buffer; the namespace verbs are ergonomic wrappers over the design's `declare(WorkOp)` (the `WorkOp` enum stays inside the SDK, never crossing the FFI). `status` returns the volume's placement, byte accounting, attachments, overlay drift and NFS port (the fields `slates status` prints); `list` returns the volumes as id/name/accounting/overlay records; `resize` and `destroy` change and reclaim a volume. The Python SDK returns dicts, the Node SDK `#[napi(object)]`s with napi's camelCase keys, every `u64` range-checked to a JS-safe integer. **Both daemon-spawn round-trip harnesses are built and proven in-sandbox**: `tests/test_sdk.py` and `tests/sdk.test.mjs` each spawn a real anchor+daemon, connect the SDK, and drive create → snapshot → status → list → resize → destroy → list-gone, then the merge loop create_green → create_work → edit → submit → versions → changed_since → rebase, asserting the outcome of each over a real daemon and merge engine (R5). Each spawns the anchor in its own process group and tears the whole group down on teardown, so the supervised daemon goes with the anchor at once (no daemon outlives the test); both still skip loudly where no `slates` binary is present, so the default run stays green off-box. **Both SDKs are now async-primary** (R6, D-19), each an async client peer to the sync one (the thin blocking facade), exposing every sync verb's async counterpart — the volume lifecycle (create/snapshot/status/list/resize/destroy), the whole merge workflow (create_green/create_work/edit/submit/versions/changed_since/rebase), and the namespace operations (unlink/rename/mkdir/rmdir/chmod/symlink/link/set_xattr/remove_xattr) — each an async verb a real event loop drives to completion by the completion fd's readiness — never blocking the loop, no `tokio` and no `pyo3-asyncio`/thread, resting on the `slates-client` async core (`begin`/`spin_reply`/`poll_reply`, one reader multiplexing every in-flight request by id). **Python** (`crates/sdk-python`): `AsyncClient`, a PyO3 pyclass, resolves each verb on the running `asyncio` loop via `loop.add_reader(completion_fd)`. **Node** (`crates/sdk-node`): `AsyncClient`, a JS wrapper (`async.mjs`) over the addon's low-level primitives, returns a Promise per verb resolved by the completion fd wrapped in a libuv-polled `net.Socket`; because `net.Socket` adopts and closes its fd, the addon hands it a **dup** it owns (`enable_async_completion_dup`, a safe dup of the bridge's owned read end), leaving the client's fd intact. Proven by use over a live daemon on this macOS host (`tests/test_sdk_async.py`, `tests/sdk_async.test.mjs`): each awaits the create → snapshot → status lifecycle and drives eight concurrent creates (`asyncio.gather` / `Promise.all`), each returning a distinct id — concurrent awaits multiplexed through the one reader. The async SDKs now have **full verb parity with the sync ones** — every sync verb has an async counterpart, `land` (§4.15) included (it resolves to the landing outcome or the grant-required dict, issuing no grant itself, R10) — proven by an async by-use suite that drives the sync suite's whole surface (lifecycle + merge loop + a namespace tree built and submitted, all awaited). Owed (both SDKs): only the Windows async *bindings* — the transport they rest on, the Windows completion socket, is now built (`completion.rs`'s `CompletionBridge` gained a Windows arm: a client-local loopback `TcpStream` pair whose thread parks on the named Event and nudges the socket on an armed reply, exposed through `ClientEnd`/`Client` as a `RawSocket`; D-10 "asyncio on Windows needs a socket"), lint-clean on the native Windows target and CI-tested by the `windows-latest` `ipc` `rings` lane (`the_completion_socket_becomes_readable_on_an_armed_reply`, `WSAPoll` from quiet to readable at the armed reply). And the **Node/Python async bindings now consume that socket on Windows too** (2026-09-09, the "alongside the SDK bindings that use it" half): the Node addon's `completionFd` returns the completion `SOCKET` as a JS-safe integer that the same `net.Socket({ fd })` reader in `async.mjs` adopts (Node/libuv accept a socket there, so one reader serves both platforms); the Python addon caches the handle as an `i64` and registers it with `loop.add_reader`, which on Windows needs a `SelectorEventLoop` (the Proactor default has no `add_reader` — the README shows the one-line `WindowsSelectorEventLoopPolicy` a caller sets). Both addons cross-lint clean for `x86_64-pc-windows-msvc` (a `pure-hash` feature forwards blake3's pure backend so the addon cross-checks from a host with no MSVC assembler; the CI napi/maturin build uses the SIMD C path), macOS/Linux unaffected. The addons' Windows *runtime* — `net.Socket` adopting a raw `SOCKET`, the selector loop polling it — is the CI/hardware-owed proof (the native Windows runner), the same completion-socket contract the `ipc` `rings` lane already exercises at the transport level. This rests on **slates-rt now building on Windows** (`e3a8ccd`): the IOCP driver gained real socket readiness through an AFD reactor (`afd.rs`, `\Device\Afd`/`IOCTL_AFD_POLL` — the wepoll/mio mechanism), and the datagram socket the QUIC fleet transport rides is cross-platform through a `netsys` seam (rustix on Unix, Winsock 2 on Windows); TCP stays macOS/Linux (the NFS mount server's — Windows mounts via WinFsp, and the fleet is QUIC-over-UDP). So the outward-facing publish is the only SDK piece left. **The packaging is now built** — usage READMEs (`crates/sdk-{python,node}/README.md`, the Python one referenced from `pyproject.toml`; both cover install + connect + the async and sync workflows); the **Python** wheel via maturin (`pyproject.toml`, name `slates`, dynamic version, asyncio classifier, repository url); and the **Node** package (`crates/sdk-node/package.json`, name `slates`, a napi addon with per-platform `optionalDependencies` — `slates-<triple>` for all nine release targets, the dirs generated by `napi create-npm-dirs`), a hand-written data-driven `index.js` loader (picks the local `slates.<triple>.node` or the platform package; napi's own generator is a 3.x CLI against a 2.x crate, so the loader and `index.d.ts` types are maintained by hand), an `index.mjs` ESM entry exporting `Client` + `AsyncClient`, and `async.mjs`'s `connect` made dual-mode (a string instance loads the addon itself; an explicit addon serves the sandbox tests). The loader and ESM entry are verified to load the addon and expose both clients here; the `.node` binaries are built per-platform in CI (gitignored). Only the **PyPI/npm publish itself remains** (outward-facing, Ada-authorized). | GAP-A9-10; AC-5.9–5.11 |
| Security (4.13) | OS credential and channel checks; enrolled consumer/human issuer boundary incomplete. | GAP-A9-9; AC-2.13, AC-5.10 |
| Observability (4.14) | Partial signals/counters; typed absence and causal context not established end to end. The shard **health signals are now a closed registry** (`HealthSignal` in `slates-ipc`, GAP-A9-12): the report is built by mapping `HealthSignal::ALL`, so an unregistered signal cannot be emitted and a registered one cannot be silently dropped — the "nine spans were called seven" miscount is now a compile error, and a doc-truth test (`the_registry_is_closed_and_its_names_are_unique`) pins the canonical names; the daemon still emits them (the CLI flow test asserts `catalog.volumes`). The **chokepoint-span roster is now closed** too (`Chokepoint` in `slates-wire::observe`, GAP-A9-12): a closed enum of the design's nine spans (§4.14 "Span roster"), pinned by a doc-truth test — so the "nine spans were called seven" drift is now a compile/test failure — with the distinct three-id types (`SpanContext` = `RequestId` (routes, deduplicates) + `TraceId` (128-bit) + `SpanId` + optional `CausedBy`, kept separate types so a trace field can never carry the authority a `RequestId` does, the A-9 correction that trace fields never authorize effects). The **span emission foundation is now built** (`slates-wire::observe`): a completed `Span` (the three-id `SpanContext` + a **content-free** bounded dimension code + monotonic start/end), a bounded **shed-first `SpanSink`** — a ring that keeps the most recent spans and **counts every shed span** explicitly (§4.14 "bounded rings report dropped spans"), never growing unbounded (ban 8) — and the `ChokepointRegistry` **health-plane gate** (§2.6): fail-closed, it opens only once every chokepoint in the roster has registered its emitter. The gate is **wired into the daemon boot** — `Daemon::start` builds the roster via `registered_chokepoints()` (each line naming the subsystem that owns the emitter) and refuses with a typed `ServerError::ChokepointsUnregistered { missing }` *before acquiring any resource* if the roster is incomplete, so a daemon never serves with a silently missing span source; proven by `the_daemon_declares_every_chokepoint_so_the_gate_opens` and by every live daemon test still serving (the gate opens in the real boot). By-use tests in `observe` pin the gate (eight of nine keeps it shut and names the one missing; nine opens it), the shed-first sink (five spans into a sink of three keeps the three most recent, the drop count exact — the non-vacuity witness a silently-lossless sink would fail), and the span's own duration (saturating a backwards clock to zero). The **emission path is now live for six chokepoints** (`shard.op`, `log.append`, `ring.request`, `merge.verdict`, `bridge.request`, `land.entry`): each shard owns a bounded `SpanSink` in its `ShardState` (per-shard, thread-local — no lock, R2; capacity = one client ring's depth, `config.region.slots`). `run_recorded` emits a `shard.op` span (the whole verb, content-free read/mutation label) and a `log.append` span (the durable `Db::commit` within it, partition label) around every verb; `serve_client`/`retry_deferred` emit a `ring.request` span (ring read → reply written) for a synchronously-served reply, threading the read time through `Deferred` so a reply deferred by a full ring is still timed; the submit handler emits a `merge.verdict` span (one increment judged), stamped with the request the shard is serving via a per-shard `current_request` context (set in `run_recorded`, so a fine-grained span deep in a verb needs no request id threaded through every handler); and `crate::nfs`'s `serve_local` emits a `bridge.request` span (one NFS bridge call from arrival to reply, procedure label), reaching the shard's clock and sink through `with_state` at the call's edges (outside `serve_call`'s per-operation borrows, so no re-entrancy) — a bridge call carries the default request id (it is not a RIFL-replayed verb). `land.entry` arrives through the **cross-crate span seam** now built: the land engine's `Observer` trait gained an `after_entry(start_ns, end_ns)` callback (primitives only, so `slates-land` stays wire-free), the engine calls it per entry, and the server implements it with a bounded shed-first collector (`SpanObserver` — a large landing never grows it unbounded, `SpanSink::record_dropped` folds its loss into the sink) drained into the shard's sink after the landing, once the landing's borrows release. This is the reusable seam pattern for the remaining lower-crate chokepoints; `SpanObserver`'s bound is unit-tested and the engine's `after_entry` call site is exercised by the land oracle (its own by-use apply test is the Linux `/dev/shm` lane). Each span carries the real `RequestId`, a per-shard span id, and a trace seeded from the request word until cross-boundary propagation is wired; emission is a sink push (no await, no lock) and a full sink sheds the oldest and counts it. The counts (`spans_held`, `spans_dropped`) ride `ShardReport` to `slates status` (text) as `shard N telemetry:`. Proven live by `telemetry_scenario` (`crates/server/tests/daemon.rs`): the held count **moves up** after running verbs — the non-vacuity witness that the registered emitters actually emit (a dead path would keep it at zero while the gate still passed). The emit is within the design's ~200 ns/span budget and a fraction of the 50 µs floor; the AC-2.1 ratchet gates it on a quiesced machine (unmeasurable under this session's load, where the bench is load-dominated at ~237 µs p99). The six live chokepoints are **every one active in the single-node daemon path**. Still owed, but gated on their subsystems being live rather than on the seam (which is built): `ship.record` and `consensus.step` — a laptop runs no replication or consensus at f=0 and `slates-cluster` is not a daemon dep, so they land with fleet integration (§4.8); `archive.chunk` — the archive is not wired into the daemon and its codec is Phase 7, so it lands with §4.10 — each then through the same cross-crate seam (instrumenting them before their subsystems run would be untestable code, R5). Also owed: `ring.request` for a **forwarded** reply (its origin span crosses the shard boundary — `read_ns` 0 marks it owed rather than timing it wrongly); the cross-shard aggregation of the per-shard sinks into the single control-shard sink (the `Control::Spawn` path the bridge queue uses); real cross-boundary trace propagation; and `(value, freshness)` on the daemon-level counters. **Typed absence on the shard health signals is now done** (A-9): `Signal.value` is `Option<u64>` with an `AbsenceIs` (`Unknown`/`Degraded` per signal, `HealthSignal::absence()`), so an absent sample is never conflated with a measured zero — a live `catalog.volumes: 0` renders `0`, a genuinely absent signal `absent/<meaning>`; the six current shard signals are all measurable (`Some`), the `None` path is for a future genuinely-absent signal (a mirror age at f=0, a non-reporting shard), pinned by `every_signal_types_its_absence` (ipc) and a `signal_value` render unit test (cli). | GAP-A9-12; AC-0.11 |
| Landing (4.15) | Engine, server records and Unix control/write integration; CLI issuance and trusted issuer incomplete. | GAP-A9-9–10; AC-5.10 |
| Merge (4.16) | Pure verdict, deriver and splice/engine components; Green/Work service and fleet integration incomplete. | GAP-A9-14; AC-6.13, AC-8.19 |

Research is indexed in [README.md](README.md); the canonical subsystem sections and original
acceptance criteria remain in [SLATES_DESIGN.md](SLATES_DESIGN.md). New acceptance rows supplement
them rather than renumbering or weakening the original gates.

## 2. Decision-open (named owners: the phase that closes each)

- D-O1 compio audit (custom executor is the plan of record) — Phase 0.
- D-O2 `LocalWaker` stabilization on Rust 1.98 — Phase 0.
- D-O3 macOS unprivileged RAM disk for the mount point — Phase 4.
- D-O4 macOS attribute-cache timeout derivation — Phase 4.
- D-O5 FUSE-over-io_uring mixed-size buffers on target kernels — Phase 3.
- D-O6 Erasure coding of chunks for memory-bound fleets — DECIDED 2026-09-04 (A-3 part 3 accepted): a measured cold-content policy; the fragment record kind is in the format now (§4.11); Phase 8 measures the class boundary, (k, m) and reconstruction cost.
- D-O7 TLS 1.3 versus Noise between hosts — RATIFIED 2026-09-04: TLS 1.3 via rustls; Noise is not pursued.
- D-O8 FSKit as a future macOS bridge — superseded by D-O9.
- D-O9 FSKit-first on macOS 26+ with NFSv3 fallback (Amendment A-1) — ACCEPTED 2026-09-04 and applied; the Phase 4 spike records the numbers and the macOS 15.x path.
- D-O10 Replication recast as quorum-multiplexed sealed content + consensus pointers + owner-local live state with auto-seal (Amendment A-2) — ACCEPTED 2026-09-04 and applied.
- D-O11 (opened 2026-09-04) macOS 15.4–15.x path: RAM-disk block resource for the FSKit module versus the NFS fallback — Phase 4 spike.
- D-O12 (opened 2026-09-04) Pointer-group sharding threshold — CLOSED 2026-09-04 by A-6: there is no pointer group; the configuration group commits only on failures and moves. The auto-seal cadence constants are still measured in Phase 8.
- D-O13 — CLOSED 2026-09-04 by A-6: heads and chains are fenced registers in the Vertical Paxos II form; hedged placement adopted; pre-granted placement blocks unnecessary; erasure coding accepted earlier. Original text of the item: volume heads as fenced single-writer records in the content multiplex (consensus only for membership, placement and leases); hedged placement N > W for pointer records; pre-granted placement blocks so fleet `create` stays local; erasure coding as a measured policy for cold sealed content. Its first item (POSIX-native durability points at `fsync`, `snapshot`, `detach`, `archive`, all in RAM) is subsumed by A-4's `fsync` wording. Owner: Phase 8, decided by Ada.
- D-O14 (opened 2026-09-04) FUSE passthrough for untouched base files — CLOSED 2026-09-04: not used; slates never requires `CAP_SYS_ADMIN` or root beyond the OS-provided brokers installed once (`fusermount3`/user namespaces, the FSKit extension, the WinFsp driver); the daemon copy path is the only base read path.
- D-O15 (opened 2026-09-04) The landing fallback where the target filesystem lacks an atomic exchange (verify-then-rename-over with a reported window) versus a staging-directory strategy — Phase 1 measures the window; Phase 4 measures per platform.
- D-O16 (opened 2026-09-04) Landing filters — DECIDED 2026-09-04: the confirmation surface offers suggestions the human toggles (ignore-file-aware); the agent's filter stays explicit; a toggle produces a new manifest hash (§4.15 step 2). Phase 5 builds it.
- D-O17 (opened 2026-09-04) Conflict rate from whole-file tool rewrites versus SDK `edit` operations: if the measured share of conflicts caused by whole-file rewrites on shared files exceeds the operator SLO, reopen the ergonomics (a declared-edit bridge path for editors; stronger skill guidance) — Phase 6 measures.
- D-O18 (opened 2026-09-04) Merge proposer authority: slates departs from hecate's leader-fused proposer (one shared pointer group; partitioned execution) and uses a consensus-issued lease with an epoch check at commit; CLOSED 2026-09-04 by A-6: the owner is the distinguished proposer of its own registers under its host epoch, so no leader-versus-leaseholder split can exist; a resumed stale owner is refused at the first holder (model-checked as StaleNeverCommits and Continuity).

## 3. Undesigned (charter only)

- Security spec — A-8 defined account credentials; A-9 adds enrolled consumers and protected human issuer authority (§4.13). Implementation remains open in GAP-A9-9.
- Observability spec — A-9 corrects trace identities and adds typed absence/freshness (§4.14). Implementation remains open in GAP-A9-12.
- Skills content (the seven SKILL.md documents, including `slates-landing` and `slates-merge`) — owed in Phase 5 and Phase 6.
- The confirmation-surface contract for harnesses other than the terminal (the request stream a harness renders, answered only by a human-operated process through the control channel) — owed in Phase 5 with the terminal surface as the reference.
- Operator documentation (fleet configuration: failure-domain tree, regions and mirror regions, neighbourhood sizing inputs, certificates) — owed in Phase 8.

## 4. Drift (owed-and-forgotten)

A-9 corrects documentation drift: the remote delta-only clone, exclusion of virtio-fs,
quota-as-reservation language, implicit whole-tree snapshot claims, uid-as-consumer authority,
missing writeback barriers, overstated recovery and protocol proof, and CLI grant availability.
The corrected design is still ahead of implementation; every item remains open in §8i until
its observable regression and integration gate pass. Historical source/measurement records
are retained with their scope; no documentation edit is an implementation acceptance.

## 5. Residual literals against the derivation doctrine

- Ratified in Phase 0 (2026-09-04), each at its definition site with a `Shape:` doc line that
  the literal check reads (`crates/machine`): the Kalibera-Jones stopping width (one tenth of the
  median, `stats::CONVERGED_WIDTH_PERMILLE`), the bootstrap resample count (1,000,
  `stats::BOOTSTRAP_RESAMPLES`), the smallest accepted sample (16, `bench::MIN_SAMPLES`), the
  per-probe wall bound (250 ms, `bench::PROBE_WALL_BUDGET`; the whole profile took 472 ms on the
  M5 Max, BENCHMARKS.md), the timer-overhead factor (100, lmbench's one-percent rule,
  `bench::TIMER_OVERHEAD_FACTOR`), the fault probe's region (256 base pages,
  `probes::FAULT_REGION_PAGES`), the full-matrix core limit (32, from §4.1's text), the wake
  probe's convergence batch (64), the zstd candidate levels (1, 3, 9, 19), the cache-line fallback
  (128 B, only when the OS refuses), the hash corpus (1,024 base pages, the large chunk class) and
  the codec corpus (64 base pages, the small class), and clippy's cognitive-complexity threshold
  (10, `clippy.toml`). Every other number in the crate is `Format:` (a layout fact) or derived.
- Ratified in Phase 0 for the runtime and the wire, each at its definition site: the registry's
  shard bound (1,024, `rt::registry::MAX_SHARDS`), the timing wheel's shape (6 levels of 64 slots,
  `rt::timer`, a `Format:` because it fixes the deadline arithmetic), the events drained per driver
  wait (64, the kqueue, epoll and IOCP drivers; it bounds latency, not correctness), and the
  wire's header layout, class words and schema-hash constants (`Format:`). The runtime's tick,
  step budget and ring size come from the profile; the batch bound is one ring until the per-item
  cost is measured (§4.3), which `RuntimeConfig::from_profile` says in its formula string.
- "10 × broadcast RTT p99" for election timeouts is Raft's published rule; ratified as a shape
  constant with the citation.
- The format floor for compression (savings must exceed the chunk's metadata overhead) is
  derived from the format, not a literal.
- The racy-window timestamp granularity per base filesystem (§4.15) is a cited table keyed by
  the filesystem type the OS reports, ratified as a shape table; each row must carry its
  citation and Phase 1 verifies it per filesystem.

## 6. Fit-before-influence milestones

- Phase 0 baseline (first CI run on the reference machines) precedes every ratchet.
- Prefetch and dedup policies run observe-first until the measured sample counts are reached.

## 7. Armed tripwires (metrics must exist from day one)

- Provisioning p99 (spinning) exceeds the ratchet → reopen D-9/D-10.
- Rename rate high enough that per-volume serialization is visible → reopen D-7 (shared index escape hatch).
- Volume skew across shards → reopen D-7 (Silo-style shared index).
- Loss windows exceeding the operator SLO, or put quorums frequently unreachable → reopen D-14 (make live shipping the default for the affected class).
- Configuration commits growing with ordinary write traffic → fail the D-14 control-path invariant; D-O12 remains closed because there is no pointer group.
- FSKit spike fails its go criteria → NFSv3 remains primary on macOS and D-2 is reopened next macOS release.
- NFS fallback coherence test fails at the derived `actimeo` → reopen the fallback's cache posture.
- Hashing backlog persistent → reopen D-6 (hash-on-seal policy).
- Drift checks per second exceeding the measured `stat` capacity of a base (stat storms) → reopen the check cadence in §4.5 (hint-driven checks only, or a coarser listing fingerprint).
- Watcher overflow rate above the operator SLO on a base → reopen the watcher strategy (fanotify mount marks on Linux; the USN journal on Windows).
- Large-class copy-up cost or descriptor use beyond its derived budget → reopen the copy-up class boundary (D-6, §4.5).
- Landings falling back to rename-over (no exchange) above a measured fraction → reopen D-O15 (staging-directory strategy).
- Merged listing cost on the largest base directories above the readdir latency budget → reopen the listing cache (§4.5).
- Any write by a slates process outside a granted target in the tracer → stop the release; it is a rule violation, not a tripwire.
- `StaleEpoch` refusals outside an observed takeover or migration → a fencing or membership bug; fatal in CI, alarm in production (never a tripwire to tune).
- The configuration group's commit rate above its near-zero baseline outside failures and moves → something has put consensus back on a per-write path; investigate before anything else.
- The hedge rate above its derived cap for a class → the p95 estimate or the neighbourhood is wrong; probation and neighbourhood change first, then re-derive the cap.
- The copyset count above its bound at any configuration → a placement bug, fatal in CI.
- `mirror_age` above the operator's mirror SLO → alarm; `await placed(mirror)` callers see it as `NotPlaced{mirror}` at their deadline.
- p99 intervening deltas per merge above the checkpoint spacing's design point → re-derive the checkpoint spacing; a rising conflict rate with base lag → tighten the stream cadence for that green (§4.16).
- Rebase-retry rate on one path region above the derived threshold → the harness is told (contention control lives above the engine, as in hecate MERGE.md §10); slates never serializes work by itself.
- Holder recomputation mismatch anywhere → not a tripwire: a bug; fatal in CI, alarm in production.
- Merge-path p99 or verdict p99 change-point → nightly gate failure (Part 6).
- Destroy slices past the step budget, the clock-check allowance and the measured jitter (the shard's watchdog count, §8c) → a release unit whose cost the weights do not see; re-derive `release_weight` before touching the budget.
- Heap per file above the counted-object budget of the Phase 1 bench at any tree size → an object the formula does not name; add it to the formula, never to the slack.

## 8. External dependencies and port hazards

- WinFsp (GPLv3 with FLOSS exception or commercial license) installed by the user on Windows.
- Linux kernel features by version (5.10 baseline; 6.1; 6.14).
- macOS 14.4 minimum (`os_sync_wait_on_address`); macOS 26 for FSKit URL resources; Apple Developer ID signing, the FSKit entitlement, and an app group for the macOS bundle; Apple's yearly FSKit protocol changes.
- C toolchains for `zstd-sys` on all nine targets.
- The merge engine (A-5) depends on nothing external: fixed-layer ops documents use `slates-wire`; the fleet parts use the consensus group already chosen. Read directly from hecate on 2026-09-04: `MERGE.md`, ADR-0003, ADR-0005, `SERVING.md` §2-§4, `VFS.md` §5, `CONSENSUS.md` §6; hecate's own contradiction on the deriver (`SERVING.md`/`VFS.md` diff versus `MERGE.md` never-diff) is recorded in `research/merge-engine.md` §2 and resolved for never-diff.
- Base and landing primitives (A-4): Linux filesystems with `RENAME_EXCHANGE` and `O_TMPFILE` (ext4, XFS, Btrfs, tmpfs; others fall back); `openat2` (5.6, under the floor); FUSE passthrough only with `CAP_SYS_ADMIN` (6.9+); macOS `RENAME_SWAP` and `clonefile` by volume capability (APFS); Windows 10 1607+ NTFS for POSIX-semantics rename; ReFS for block clone. Items marked "verify" in `research/disk-source-of-truth.md` §7 (batched `statx`, `NtQueryDirectoryFile` classes, `FlushFileBuffers` on directories, fanotify marks, reparse-tag checks, the timestamp-granularity table, `FSCTL_SET_SPARSE`, `F_PREALLOCATE`) are owed verification in Phase 1 and Phase 4.

## 8a. Phase 0 audits (task 6)

- compio (audited 2026-09-04 from its `master` sources): `compio-runtime` holds `Rc<Executor>`,
  `Rc<RefCell<Proactor>>` and `Rc<RefCell<TimerRuntime>>`, and is a thread-local runtime that a
  user assembles into thread-per-core; `compio-driver` stores every operation in a
  `ThinCell<RawOp<dyn Carry>>` (a reference-counted cell, one heap allocation per operation) and
  hands out `std::task::Waker`s from the proactor. That is a reference count and an allocation on
  the request path, which D-8 forbids, and there is no seam for FUSE-over-io_uring or our rings.
  Result: not adopted; the custom executor of `crates/rt` is the plan of record. Re-check per
  release only if compio publishes an allocation-free operation path.
- `LocalWaker` on Rust 1.98.0: still nightly-only (`local_waker`, #118959). The executor uses
  `Waker` with a vtable that is thread-safe by construction over a `Copy` word; `clone` and `drop`
  are no-ops, so nothing is lost. Revisit when it stabilizes (a `ContextBuilder` change only).
- io-uring crate 0.7.14 (tokio-rs): thin syscall wrapper, no reference counting in its core types;
  adopted for the Linux driver with the probe-and-fall-back sequence of D-9.
- Miri: ships only with nightly, which this machine does not have. Installing nightly is a tool
  install and needs Ada's explicit authorization (asked 2026-09-05); until then CI's
  `miri-and-loom` lane runs it on nightly for the `slates-mem` and `slates-wire` unit tests
  (`slates-rt`'s unit tests open a kqueue or an eventfd, which Miri does not model). Local runs
  are loom-only.
- Instruction-count gates (D-20, "iai-callgrind in CI"): iai-callgrind needs valgrind, which is
  non-Rust tooling and therefore banned from CI and this machine without Ada's explicit
  authorization (asked 2026-09-05). The ratchet that exists is `cargo xtask ratchet`
  (`ratchets.toml`): wall-time ceilings keyed by machine identity, hierarchical over runs the
  way Kalibera and Jones prescribe (a ceiling is the highest upper edge across three runs; a
  check fails only when the lowest lower edge across three fresh runs lies above it), tightening
  only; 22 rows recorded on the M5 Max on 2026-09-05 and proven to fail on a planted ceiling.
  Its resolution is the machine's between-run drift, which it prints: on this laptop up to 40%
  on rows under 100 ns (frequency and thermal state between processes) and a full step on the
  1–2 ns header rows (nanosecond quantization), under 3% on rows above a microsecond. That is
  the case for the instruction-count gate: it sees a 1% change the wall clock cannot.
- Cross-target checks: the four shipped crates lint clean for `x86_64-unknown-linux-gnu` and
  `x86_64-pc-windows-msvc` from this machine (the targets were installed; the C dependencies are
  off for those checks behind `slates-machine`'s `codecs` and `pure-hash` features), and CI's
  `cross-lint` lane repeats it. This caught two real defects on 2026-09-05: three Windows imports
  behind an unrequested `Win32_Security` feature and a working-set call in the wrong module.
  Nothing on Windows or Linux has *run* yet: that needs those machines (Phase 1's reference
  boxes).

## 8b. Unsafe, Miri and instruction counts (2026-09-05)

- Unsafe surface, measured by `cargo xtask unsafe` (blocks, functions and impls in shipped
  sources, comments and tests excluded): 161 mentions and 8 `unsafe impl` before the reduction,
  76 sites and 0 `unsafe impl` after. What did it: the rings hold atomic words instead of
  `UnsafeCell` slots (zero unsafe, loom still explores every interleaving); the shard's mutable
  state sits in a `RefCell` with refused, counted nesting instead of a raw pointer behind a flag;
  shard contexts, pair rings, registry entries and the simulation's shared state are leaked
  process-lifetime objects reached through plain `&'static` references; control messages (spawn,
  cancel, shutdown, active) ride a bounded standard channel instead of pointers packed into ring
  words; drivers are built on their own shard's thread from a `Send` seed instead of being sent
  across; `rustix` (the design's syscall surface) replaces raw `libc` for everything it wraps
  and `memmap2` replaces the raw maps, advice and locks. What remains, per crate, is listed with
  its reason in `unsafe-budget.toml`: FFI without a safe wrapper (Apple sysctl, mach, IOKit,
  Win32), the CRC32C intrinsics, the `RawWaker` vtable, and wrappers that are unsafe by signature
  (`kevent`, io_uring's `push`, the file-backed map of the profile segment). The budget only
  tightens. The reduction cost nothing measurable after one recovery: the wall-clock ratchet
  caught the idle step rising from 30 to 37–45 ns (a borrow per phase, a channel poll per step)
  and the step is back at 22–30 ns with one borrow before the polls and one after, the registry
  entry cached, and the control channel polled only behind a pending flag (BENCHMARKS.md).
- Miri (nightly `miri 0.1.0 (0ed41eb414 2026-09-04)`, authorized and installed 2026-09-05):
  `slates-mem` 28 tests and `slates-wire` 19 tests pass with the leak check on; `slates-rt`'s 12
  unit tests and 2 simulation tests pass with `-Zmiri-ignore-leaks`, because the runtime leaks
  its contexts, rings and registry entries on purpose (that is what makes their references
  `&'static` without unsafe code). Tests that need `sysctl`, `mlock` or a kqueue are marked
  ignored under Miri (3 in `mem`, 3 in `rt`). No undefined behaviour was found. CI's
  `miri-and-loom` lane runs the same commands.
- Instruction counts (D-20): `benches/callgrind.rs` in `mem`, `rt` and `wire` under iai-callgrind
  0.16.1, run by CI's `callgrind` lane on Ubuntu with valgrind (authorized 2026-09-05); valgrind
  has no port for macOS on Apple silicon, so the lane is the only place they run. The benches
  compile here (`cargo bench --workspace --bench callgrind --no-run`). The lane prints the counts;
  the comparison against a recorded baseline is the next step once the first run exists.
- New dependencies, accepted for the unsafe reduction: `rustix` 1.1 (the syscall surface named in
  the IPC research §2.4), `memmap2` 0.9 (maps, advice, locks), `toml` (xtask only),
  `iai-callgrind` (dev only; pulls `proc-macro-error2` 2.0.1, which rustc warns will be rejected
  by a future version — a dev-only build dependency, tracked here until iai-callgrind drops it).

## 8c. Phase 1 volume core record (2026-09-05)

What landed: `slates-vfs` (tasks 1–9 of Phase 1): the copy-on-write namespace (radix-16 inode
trie, directory nodes with an inline two-entry form and a copy-on-write B+-tree of 4 KiB
slotted blocks in a store slab beyond it), content as chunk windows (open page-multiple extents
sealed into chunks, copy-on-write per window, holes uncharged), snapshots and clones by birth
epoch with deadlists and a pruned destroy walk, exact accounting as a histogram of content
bytes by birth epoch (`referenced_bytes` is its total, `unique_bytes` its suffix past the newest
shared epoch), bounded and dynamic quotas with pressure events, the op log with a byte budget,
name folding without allocation, the executable model with proptest state-machine tests, the
edge and fault tests, and the baseline bench with its three acceptance gates.

Gates in place: AC-1.1 (the model over 10^6 generated operations, counted, `cargo test -p
slates-vfs --release --test model -- --ignored ac_1_1` in CI), AC-1.3 (snapshot and clone cost
flat from 10^3 to 10^6 files within the timer's resolution), AC-1.4 (five nodes copied for a
create five levels down; one extent copied for a write; one page for a fresh window),
AC-1.5 (heap per file against the counted-object formula at 10^3, 10^5, 10^6), AC-1.6 (inode
numbers never reused, kept by snapshot and clone), AC-1.7 (both counters equal the model's
after every generated step), AC-1.8 (destroy of 10^6 files in clock-cut slices; none past the
budget plus the clock-check allowance plus the measured jitter); T-1.1, 1.2, 1.3, 1.4, 1.5,
1.7, 1.8, 1.9 as named tests; T-1.6 in its one-shard form (a generated interleaving of two
clones; the shuttle form arrives with Phase 2's threads).

Task 14 (the deriver) landed 2026-09-05: the interval algebra as a pure module
(`crates/vfs/src/algebra.rs`: a content map of base and new runs, every declared operation a
splice, hunks as the unique minimal edit against the surviving base runs, 2,000 generated
histories against a byte-level reference applier, disjoint operations proven to commute); the
SDK `edit` on the volume (delete then insert with true positions, journaled as such; the tail
is rewritten, the zero-copy splice is Phase 6's); the deriver (`crates/vfs/src/derive.rs`: the
journal since the base snapshot folded into per-inode maps, the touched paths resolved in both
trees, a state delta over paths with base references, sorted, encoded little-endian, identified
by BLAKE3); a file inode's home (parent and name hash) so a written inode's path costs no walk;
`readdir_in`, `lookup_in`, `resolve_in`, `stat_in`, `readlink_in` reading a snapshot as it was
(`lookup_in` had resolved to the head's node: a latent bug, fixed). Gated: T-1.18 (300 random
histories over random bases: net-apply reproduces the head's files, symlinks and directories
byte for byte; every hunk inside its sources; deriving twice gives the same bytes), T-1.19
(truncate-and-write and write-and-rename give one hunk of the base length and the new length
and byte-identical documents), AC-1.15 (a fixed history's identity
`11476fb81b5bc32d474f28cd2afd2f65b1609c7dc070e1ade41f3c5df4d955c4` pinned for every lane). The
document keys content by post-state path with an explicit base reference rather than by inode,
so a rename over a base path and a rewrite in place read the same; hard-linked inodes are
listed at every path (conservative, as D-27 says).

Task 10 (the base plane) landed 2026-09-05: the read-only host seam (`crates/vfs/src/host`)
with opaque handles, bulk listings carrying fingerprints, `O_NOFOLLOW` opens and watcher hints;
`SimHost`, an in-memory host with outsider edits, a controllable clock and watcher overflow, the
disk leg of the (disk, overlay, witnesses) oracle; the volume's base plane (`crates/vfs/src/base.rs`):
merged lookups and listings validated by the directory's fingerprint on every use, base
entries given inodes on first touch and dropped when the disk loses them, copy-up by size class
with the racy rule, whiteouts and redirects journaled, drift checked first on what the held
descriptor serves (an in-place change marks the body lost and reads refuse with `BaseDrift`)
and then on the path (deleted, replaced, retyped: reported, still served from the held inode),
`read_base`, `rewitness`, `pin`, `status`, hints and overflow re-checks, the diverged set over
loaded nodes; and `slates-base` (`crates/base`), the operating-system host: descriptor-relative
rustix calls on Unix, inotify on Linux and `EVFILT_VNODE` on macOS behind the seam, a
path-relative standard-library form on Windows. Gated: AC-1.9 (one open and one node at 10^3,
10^5 and 10^6 files, and over the workspace's own tree), AC-1.10 and T-1.10 (150 generated
histories of agent and outsider moves over random bases, the diverged set, the drift list and
every readable file compared after each step), AC-1.11, T-1.11, T-1.12 (40,000 entries), T-1.13;
the host's own tests over `crates/` and, in the Linux lane, over tmpfs (descriptor semantics,
`O_NOFOLLOW`, hints). Baselines in BENCHMARKS.md (Phase 1 baseline: the base plane).

Deviations and owed items from task 10:
- The Windows host is path-relative through the standard library and reports no watcher
  (fingerprints alone, the failure matrix's Masked cell); the directory-handle form with
  `FILE_FLAG_OPEN_REPARSE_POINT` opens and `ReadDirectoryChangesW` arrive with the Windows bridge
  (Phase 4). Its timestamp granularity is the table's coarsest until the volume is queried
  through that handle. Compile-checked in the cross-target lint lane; not run here.
- Listings on macOS use `getdents` plus one `statat` per entry (3.4 µs per entry measured);
  `getattrlistbulk` is the design's bulk call for the platform and its gain is owed as a
  measurement before Phase 3's bridge, where listings sit on the `readdirplus` path.
- A large-class copy-up hashes the whole file for its witness identity (6.8 ms measured on a
  file of a few megabytes); D-6's tripwire on large-class copy-up cost stands, and a lazy
  identity (hashed at seal or landing) is the change it would trigger.
- `slates-base` carries two `unsafe` sites (rustix's `kevent`), budgeted.

Tasks 11–13 (the landing) landed 2026-09-05: `slates-land` (`crates/land`), the only crate
that links a write-capable syscall (the structural test's allow-list): the manifest with its
canonical encoding and BLAKE3 hash (`manifest.rs`: creates, replacements and deletes with their
witnessed base, directory renames as one rename, directory creates, recursive removals, a
`Clear` for a base directory the overlay removed and recreated opaque, symlinks; the filter;
the summary), the pure verdict of §4.15's table (`verdict.rs`, every row and every conflict
class in one table test), in-process grants and the single-holder lease (`grant.rs`; a session
grant covers later landings of the same volume into the same target), the online ramp policy
(`ramp.rs`), the state machine (`engine.rs`: present with a preliminary verdict pass, grant,
lease, capability probe inside the granted target, validate, sweep, write by class, sync,
advance, report, with an audit ring), and the write seam over the operating system (`os.rs`:
`O_TMPFILE` linked through `/proc/self/fd` on Linux and hidden-name temporaries elsewhere,
`renameat2(RENAME_EXCHANGE)` and `renameatx_np(RENAME_SWAP)`, `fdatasync` and
`F_BARRIERFSYNC`, `F_FULLFSYNC` as the media barrier, `futimens`, `fchmod`, containment by
`O_NOFOLLOW` per component with the ownership check). The seam gained the write verbs
(`LandFs`) and `SimHost` implements them with crash injection at every write instruction,
a switch for the exchange and one for unnamed temporaries, and a count of every seam call.

Gated (`crates/land/tests/oracle.rs`, over `SimHost`): the worked example of §4.15 with both
of its failures (a conflict refused with nothing written, then `read_base`, rewrite,
`rewitness`, a new manifest; a compare-and-swap lost to an outsider, exchanged back, `Undone`,
the report `Partial`, the outsider's bytes kept, the entry still in the overlay); AC-1.14 (the
sixteen entries take the same seam calls over 10^3 and 10^5 base entries); T-1.14 (eight
seeded runs of random outsider rewrites in both forms, every loss detected at the swap, none
silently applied); T-1.15 and AC-1.13 (a crash at every one of the writer's 96 write
instructions over a delta with every action class: every path old or new after each, the
resume with the same landing id sweeps the siblings, reaches the reference disk, and a
further plan is empty); T-1.16 (no exchange: verify-then-rename, the window in the outcome,
`NoExchange` reported, an outsider edit still refused at the verify); T-1.12 (the 40k-entry
directory: one `Clear` and two creates, exactly two entries after); stage-and-exchange for an
empty target (1,010 entries in a hidden sibling, one exchange, the scratch volume an overlay
after, reads then following the disk); a populated target in place with `CreateCreate`;
grant mismatch, held lease, consumed and session grants, the audit log. Over a real
directory (`crates/land/tests/os.rs`, Linux lane on `/dev/shm`, loud skip elsewhere): the
worked example's shape on the disk, containment refusals, staging, and T-1.15's real `kill -9`
(a child lands round after round until killed; every file is a whole round; the parent resumes
with the child's landing id and sweeps). Baselines in BENCHMARKS.md (Phase 1 baseline: the
landing).

Found by the landing oracle and fixed as rules (each with its test): a merged directory's link
count ignored its base subdirectories, so removing a base subtree bottom-up drove the parent's
count to zero one step early and its own `rmdir` refused `NotFound` (the count is now two plus
the subdirectories, overlay and base, at listing load); a cleared directory renamed aside then
recreated left the name absent between the two steps (now a fresh directory exchanged with the
old one, verified, the displaced tree removed); a resumed landing met its own fresh directory
and its finished rename and called them conflicts (the verdict now knows a directory holding
only what the manifest creates beneath it, and a rename whose destination holds the witnessed
directory); a crash inside the directory syncs reported `Done` (a failed sync now aborts, an
aborted landing advances nothing, and every entry's directory is synced on the resume); the
simulated host's `st_mode` lacked the type bits a real `stat` carries, so a removed directory
planned as a file delete.

Deviations and owed items from tasks 11–13:
- Entries run one at a time; the ramp records the depth it would have chosen (`ramp_depth` in
  the report). Concurrent entries arrive with the runtime's pool in Phase 2; the linked
  io_uring chains and arena-page writes with Phase 4's Linux bridge (bytes are read from the
  volume into a buffer and written through the seam until then).
- Stage-and-exchange runs for an empty target only. A populated target needs every existing
  entry linked into the stage, a hard-link verb the seam gains with its measured cost (the
  break-even policy `LandingCosts::prefers_staging` is written and tested against the
  formula; the remembered costs come from each landing's report).
- Reflinks are not used (`LandCapabilities.reflink` is probed as `false`); `FICLONE` and
  `clonefile` arrive with the manifest's own hash index of identical files.
- The directory sync strategy is one `fsync` per touched directory; the `syncfs` alternative
  waits on the measured per-directory cost the report now carries (`Durability.dir_sync_ns`).
- Windows has no OS writer yet (`FileRenameInfoEx` with `POSIX_SEMANTICS`, the sharing-mode
  verify): the Windows bridge of Phase 4; the crate compiles there without the `os` module.
- `TargetIsVolume` is not checked: there is no mount table before Phase 3.
- The real-target tests and the OS rows of the bench run in the Linux lane; on this machine
  they skip loudly (no RAM disk authorized), so their first numbers are the lane's.
- `slates-land` carries three `unsafe` sites (the macOS libc calls rustix does not wrap:
  `F_BARRIERFSYNC`, `F_FULLFSYNC`, `renameatx_np`), budgeted.

Open in Phase 1: none. AC-1.2's harness (`crates/vfs/tests/differential.rs`)
and policy (`docs/wip/EQUIVALENCE.md`) are written; it runs in the Linux lane against
`/dev/shm` (2,000 histories) and skips loudly elsewhere, since a macOS RAM disk is a
system-state change Ada has not authorized; its first run is the lane's, not a local one.

Deviations from the §4.5 text, each measured (BENCHMARKS.md, Phase 1 baseline) and applied to
the design in A-7:
- `DirNode.parent` is the parent's inode number, not a node handle, and every directory inode
  carries `Body::Directory(current node)`: a handle held by a node shared with a snapshot goes
  stale after a copy, and the model found the root losing entries when a stale parent was
  copied (the second path copy rebuilt the root from the old node).
- Every node carries its own name, so the path for the op log and the parent re-pointing after
  a copy cost no scan of the parent.
- The indexed representation is one tree keyed by `(hash, folded name)`; the hash side index
  of D-4 is not built because the descent already probes by the leading hash word and the
  measured lookup is the fold and the compare.
- The ordered node is a 4 KiB block, not "entries per two cache lines": a block holds the
  measured directory (36 entries of 49-byte names) whole, and it is the unit the slab hands
  out and a copy moves; the small form is inline in the node up to the measured cut-over of 2.
- Clone pins are released by the owner of both volumes (`Volume::unpin`), not by the clone's
  destroy, because volumes hold no reference to each other (D-8: ownership by handle).
- `destroy_step` takes a time budget on the volume's clock, not an object count, and weighs a
  release by what it frees; the count form put 2.5% of slices over budget.
- The write path charges the materialized delta of the chunk-window rule and the model encodes
  that rule; a byte-precise charge left the counter drifting from the extents.

Residual literals: none in `src/`; the bench's shape constants (files per directory, name
bytes, groups, the 4 KiB block) carry their measurements. The example targets of the four
benched crates share the name `bench`; cargo warns of the output collision and may make it an
error, so a rename to `<crate>-bench` is owed before the Phase 2 crates add theirs.

## 8d. Phase 2 record (2026-09-05)

Task 1 (the anchor) landed 2026-09-05: `slates_mem::SharedObject` (`crates/mem/src/shared.rs`),
the shared memory object of §4.7 created without a filesystem entry (`memfd_create`, `shm_open`
under the 31-character limit, a `Local\` section), handed to another process by an inherited
descriptor or a name, mapped whole, with atomic views of the words two processes touch; and
`slates-anchor` (`crates/anchor`): the segment layout (a header with the magic, version,
machine identity, generation and the persisted geometry; a supervision block of atomic words;
the profile, the per-partition log rings, two snapshot slots per partition, the audit ring and
the landing slots, every region page-aligned and every size a derivation the daemon passes in),
create and attach with the seqlock rule (a torn header or payload is refused, a foreign
identity is refused with both hashes, a wrong length is refused), publish and read of payloads,
and the supervisor (start with the handoff in the child's environment, non-blocking `step`,
restart on exit, a restart bound derived from the recovery budget and the measured daemon start
p99, the crash loop recorded in the segment as the health plane's `daemon.alive` input).
Gated: `crates/anchor/tests/anchor.rs` (create, attach through the handoff, the payloads, the
refusals; a real child process, the test binary re-invoked, attaches from its environment,
beats, exits, is restarted three times and refused the fourth, with every step visible in the
segment). Owed from task 1: the profile is published by the daemon's boot (task 2 wires
`MachineProfile` in); `slates anchor` as a CLI command arrives with task 5; held descriptors
(the FUSE fd, the NFS socket) with Phases 3 and 4.

Task 2 (the database) landed 2026-09-05: `slates-db` (`crates/db`): the records of §4.8 with
one canonical `Wire` encoding each (`catalog.rs`: volumes with policy, base path, head, epoch,
accounting, state, lease, owner and access list; snapshots with placement; lineage edges;
attachments; completion records; grants; landing leases; landing records; audit records);
the operations (`op.rs`, 24 kinds, every local mutation); the adaptive radix tree of the
indexes (`art.rs`: four node shapes growing and shrinking, path compression, values at inner
nodes so keys need not be prefix-free; model-tested against an ordered map over 400
histories); the partition (`partition.rs`) with the guard-then-apply split: `check` refuses
what the log must never record (a duplicate name, a missing record, a held or stale lease, a
stale completion) and `apply` is the unconditional transition both the live path and replay
run, so a recorded operation applies the same way forever; leases indexed by holder with the
runtime's timing wheel for expiry; the log record over the segment's ring (`record.rs`: a
32-byte header with magic, length, sequence, CRC32C over sequence, schema and body, the
schema hash; records wrap; the tail is released after the bytes; replay verifies each and
stops at the first that fails); recovery (`replay.rs`: the newest valid snapshot slot then the
records after its sequence, the torn tail cut and overwritten, the replay timed) and the
snapshot cadence derived from the recovery budget and the measured replay throughput
(`SnapshotPolicy`, bytes per microsecond), with a full ring snapshotting and retrying rather
than refusing.

Gated (`crates/db/tests/model.rs`): 60 generated histories of every operation kind with the
database dropped and recovered at random points and random snapshot cadences, the recovered
partition equal to the live one after every crash and refused operations never recorded
(AC-2.3's durability half); the torn tail (a byte flipped in the last record: cut off, the
sequence reused, the next mutation lands over it); four hostile record shapes (a length of
`u32::MAX`, a foreign magic, a bad checksum, a truncated header) refused with everything
earlier intact; lease fencing and expiry through the wheel with epoch + 1 for the next holder
and the wheel rebuilt by recovery (AC-2.4's core); completions exactly-once across recovery;
snapshots trimming the log and recovery restoring a snapshot plus its tail. Baselines in
BENCHMARKS.md (Phase 2 baseline: the database): recovery of 10^4 volumes from 10^6 records in
96 ms against the 1 s budget (AC-2.7), 206 ns per mutation, 95 ns per replayed record, the
tree at 53 ns per insert and 18 ns per lookup at 10^5 keys.

Found by the model test on its first run: a completion recorded at a sequence the client had
already acknowledged (a stale retry) was retained live but released when the state was
restored from a snapshot, so a recovery differed from the live partition. The guard now
refuses it (`StaleCompletion`) and the window itself drops such a record.

Owed from task 2: the register and held-record tables of §4.8 arrive with task 7 (f=0) and
Phase 8; the `put_wal` with Phase 8; the chains, deltas and last-changed index with Phase 6;
the recovery budget is a ratified default (GAPS §5) until the CLI takes the operator's value.

Task 3 (the IPC) landed 2026-09-05: `slates-ipc` (`crates/ipc`): the 64-byte slot (sequence
word, kind, length, request word, 40 payload bytes) and the single-producer single-consumer
ring of slots with per-slot sequences (`slot.rs`; a hostile kind or length is a typed refusal
and the slot is released, so a bad message never wedges the ring; an oversized payload is
refused before the ring is touched); the client region over a shared object (`region.rs`: a
header with the geometry the daemon derived, the wake word, the client's and the daemon's
parked flags, the doorbell, the command and completion rings, the bulk area); the wake word
per OS (`wake.rs`: a shared futex on Linux; `os_sync_wait_on_address(SHARED)` on macOS, two
`unsafe` sites budgeted; Windows waits on the named Event of Phase 4); the two ends
(`endpoint.rs`: the client spins for the published window, sets its parked flag, re-checks the
slot to close the race, and waits on the word; the daemon bumps the word per reply and wakes
only a parked client; the client rings the doorbell per request while the daemon's shard is
parked); and the rendezvous per OS (`rendezvous.rs`: Linux, an abstract-namespace socket named
from the uid and the instance, `SO_PEERCRED` refusing another uid and counting it, the region
descriptor and the completion eventfd sent with `SCM_RIGHTS`, the socket kept as the control
channel; macOS and Windows, a bootstrap object with claim slots taken by compare-and-swap, the
object's per-user name and mode as the authentication, the region's name and length written
into the slot and released to the waiting client). Discovery: `SLATES_ENDPOINT`, then
`default`.

Gated (`crates/ipc/tests/rings.rs`, two mappings of one region on two threads): the round trip
while spinning (no park, no wake), the late reply (one park, one wake), the ring's credit
(`RingFull`, nothing dropped, the order kept), the hostile slot (refused, released, the ring
flows), the deadline, the doorbell; (`crates/ipc/tests/rendezvous.rs`) the test binary
re-invoked as the client connects through the real rendezvous, receives its region, completes
a round trip and exits 0, and a client with no daemon is refused `DaemonUnavailable` quickly.
Both branches lint clean for Linux and Windows from this machine (the Linux tests run in the
CI lane). Baselines in BENCHMARKS.md (Phase 2 baseline: IPC): 278 ns per spinning round trip,
1.05 µs per parked-and-woken round trip.

The completion-fd mechanism is now built end to end and proven on macOS (`endpoint.rs`): the
daemon end nudges a completion fd on a reply to a *parked* client, under the same parked check
as the futex wake, so a spinning client pays for neither the wake nor the nudge; the client end
carries an owned completion fd it exposes as `completion_fd` (a raw fd an async SDK loop polls)
and `drain_completion` (a non-blocking read that clears its readiness), no `Arc` and no lock —
one owner per end, dup'd once at accept. `a_reply_to_a_parked_client_nudges_the_completion_fd`
drives it over a socketpair on this macOS host: the fd is quiet before the reply, readable after,
the reply waiting in the ring. The Linux rendezvous dups its `SCM_RIGHTS` eventfd into the daemon
end and hands the client its own (`rendezvous.rs`: `Accepted::completion_dup`,
`Connected::take_completion`); `daemon.rs` sets it on the daemon end at accept. The **macOS
completion fd is now live too**, by a different mechanism the design forces there (D-10 forbids
the Linux one on macOS — Mach messages and filesystem-named sockets are both refused, so no fd can
cross the `shm` rendezvous): `completion.rs`'s `CompletionBridge` is a client-owned thread that
parks on the very wake word the daemon already signals and, on an *armed* reply, writes a
client-local self-pipe the async SDK polls (§4.7 "signal the completion fd (eventfd / pipe /
socket)"). The daemon is unchanged (its macOS completion stays `None`) and the fast path is
untouched: a disarmed reply — taken during the spin, the client never parked — wakes neither the
word nor the pipe, so the async fast path adds no event-loop wakeup (§4.7 worked example). One
thread per async client, owned by the `ClientEnd` and joined on drop (the `slates-server`
`DoorbellThread` pattern; the stop flag a `Box::leak`'d `&'static AtomicBool`, no `Arc`, R2).
`ClientEnd::{enable_async_completion, arm_async, disarm_async}` are the uniform seam over both
mechanisms (Linux eventfd written by the daemon, macOS pipe written by the bridge);
`the_completion_bridge_signals_only_an_armed_reply_and_stops_clean` drives it on this macOS host —
the fd quiet for a disarmed reply, readable for an armed one, the thread joining clean on drop,
and the Linux branch lints clean cross-target. The **Rust async core the bindings drive is now
built** (`slates-client`, splitting the sync round trip so a host event loop drives the wait):
`Client::begin` sends without waiting and returns the request id; `spin_reply` is the fast path (a
reply taken within the daemon's spin window, no event loop — §4.7's worked example); `poll_reply`
takes a reply by id once the completion fd signals, buffering another request's reply so a reply
that arrives out of order (a deferred verb) or unawaited (an acknowledgement) never blocks
another's — the buffer bounded at twice the ring (item 8, no unbounded growth); `take_ready` drains
every ready reply for a pump that serves every in-flight request through one reader (a loop allows
one reader per fd); and `enable_async_completion` / `arm_async` / `disarm_async` /
`drain_completion` are the completion-fd seam. Proven by use over an in-process daemon
(`crates/client/tests/async_core.rs`): the create's reply taken by the spin fast path, the
snapshot's by the completion fd (armed before the send, so the daemon signals the fd exactly as for
a request parked on the loop), and two in-flight requests each routed to their own reply by id.
The **Python `asyncio` binding is now built on this core** (`crates/sdk-python`): `AsyncClient` (a
peer to the sync `Client`, which is the thin blocking facade) exposes each verb as an `async`
method a real event loop drives — the fast path returns a ready awaitable without touching the loop
(§4.7 worked example: it "returns the reply without ever touching the event loop"), the slow path
arms the completion signal, registers the fd with `loop.add_reader`, and resolves the awaiting
future when the fd fires; one reader per client serves every request in flight, replies matched to
requests by id, and `begin_ack_if_due` keeps the daemon's records bounded (§4.9). No external
runtime — no `tokio` (banned), no `pyo3-asyncio`: the loop is the user's, the readiness is the
completion fd's. Verbs bound async: create/snapshot/status (the rest follow the same three-line
shape). Proven by use on this macOS host (`crates/sdk-python/tests/test_sdk_async.py`, over a live
daemon): the awaited create → snapshot → status lifecycle resolves on a real `asyncio` loop, and
eight `asyncio.gather`-ed creates each return a distinct id — concurrent awaits multiplexed through
the one reader. The **Node `uv_poll` binding is now built too** (`crates/sdk-node`): `AsyncClient`
(a JS wrapper, `async.mjs`, over the same low-level primitives on the napi addon) returns a Promise
per verb, resolved by the completion fd wrapped in a `net.Socket` that libuv polls — the fd become
readable fires `'data'` and the pump resolves the awaiting Promise, never blocking the loop; the
socket is `unref`'d so it never holds the process open. Node's `net.Socket` *adopts and closes* the
fd it wraps (unlike `asyncio`, which only polls it), so the addon hands it a **dup** it owns
(`ClientEnd::enable_async_completion_dup`, a safe dup of the bridge's owned read end) — the client's
own fd is untouched, no double close. Proven by use on this macOS host
(`crates/sdk-node/tests/sdk_async.test.mjs`, over a live daemon): the awaited create → snapshot →
status lifecycle and eight `Promise.all`-ed creates, each a distinct id. The **full volume lifecycle
is now async in both SDKs** — create/snapshot/status/**list/resize/destroy** — proven by use to the
same shape as the sync suite (create → snapshot → status → list → resize → destroy → list-gone, then
the concurrent creates); the unit verbs (resize/destroy) carry a `bool` "done" sentinel through the
typed poll so the async pump tells "done" from "not yet" and resolves them to `None`/`undefined`.
The **merge workflow core is async in both SDKs too** — create_green/create_work/edit/submit — proven
by use (a green, a work, a content edit, a clean submit), so the async merge outcome (`{ok, version,
conflicts}`) and the tuple `{id, base}` both cross correctly; the sync verbs were refactored to share
the same shape builders (`submitted_to_py`/`submit_outcome`, `work_dict`/`work_volume`). The **whole
merge loop is now async** — versions/changed_since/rebase too (`extract_versions`/`changed`/`rebased`
+ `rebased_to_py`/`rebase_outcome`) — so both async by-use tests drive the sync suite's exact merge
loop (create_green → create_work → edit → submit → versions → changed_since → rebase, each awaited).
The **namespace operations are async too now** — unlink/rename/mkdir/rmdir/chmod/symlink/link/
set_xattr/remove_xattr, each declaring one `WorkOp` (built in the SDK, never crossing the FFI) and
resolving to `None`/`undefined` — so the **async SDKs have full verb parity with the sync ones**:
both async by-use tests drive the sync suite's whole surface (lifecycle + merge loop + a namespace
tree built and submitted). And **`land` is async too** now (§4.15) — resolving to the landing outcome
or the grant-required dict, the SDK still issuing no grant itself (R10) — so **every sync verb has an
async counterpart**: the async surface is complete. The **Windows cross-process wake is now built**
(`wake.rs`, `region.rs`, `endpoint.rs`): a named auto-reset [`Event`] per client, derived from the
region's object name (`{name}-wake`) so both ends `CreateEventW` the one Event with no handle
passing — `WaitOnAddress` on the wake word is process-local (D-10), so the endpoint's late-reply
wake signals and waits on the Event on Windows (`ClientRegion::wake_signal`/`wake_wait`), the wake
word still carrying the spin and the parked flag; the auto-reset semantics carry a signal made before
the wait, closing the park race the word's value re-check closes on Linux/macOS. It **lints clean on
the native Windows target and is now CI-tested**: the `windows-latest` job runs `slates-mem` and the
`ipc` `rings` test (the two-thread park/late-reply round trip — no daemon or rendezvous needed), so
the Event wake is exercised on a real Windows runner, not just linted. The **Windows completion
transport is now built too** — the socket the async loop the Event wake was the last piece of needs
(D-10 "asyncio on Windows needs a socket"): `completion.rs`'s `CompletionBridge` is now paired
`#[cfg]` (module gate `any(macos, windows)`), the macOS self-pipe arm byte-identical under its cfg
and a Windows arm beside it — a client-local **loopback `TcpStream` pair** (a Windows `SOCKET` that
libuv's `uv_poll` and a Python selector both poll), whose bridge thread parks on this same named
Event (`ClientRegion::wake_wait`, since `WaitOnAddress` on the word is process-local) and, on an
*armed* reply, writes the socket — the daemon's `reply()` already signals the Event on a parked
client, so the async fast path adds no wakeup exactly as on macOS. `ClientEnd`/`Client` gained the
Windows `completion_socket`/`enable_async_completion[_dup]`/`drain_completion` returning a
`RawSocket` (the `dup` a safe `try_clone` for Node's socket-owning consumer), the uniform seam over
all three mechanisms (Linux eventfd, macOS pipe, Windows socket). The bridge adds **no `unsafe`** (it
is all safe `std::net`). It **lints clean on the native Windows target and is now CI-tested**: the
same `windows-latest` `ipc` `rings` lane runs `the_completion_socket_becomes_readable_on_an_armed_reply`,
which drives a real region and a real reply and `WSAPoll`s the loopback socket from quiet to readable
exactly at the armed reply (the transport's whole contract), the reply then taken id-matched off the
ring. Still owed on Windows: the **SDK bindings that consume this socket** (the Node/Python async
methods on Windows — the transport they rest on is now here, so this is the "alongside" half Ada
sequenced) and the full Win32 rendezvous (Phase 4) that a client connects through; and separately the
runtime's async TCP/UDP (`slates-rt`'s `tcp.rs`/`udp.rs`) are still Unix-only, so the *daemon* on
Windows awaits the IOCP driver — a concern of the server, not this client-side completion transport. The Rust client parks on the word and needs
none. The Windows named Event per client (Phase
4, with the section-and-Event rendezvous compile-checked now); the doorbell thread that turns a
client's wake of a parked macOS shard into the driver's kick, and the heartbeat slot that
tells the daemon a client died where no socket closes, both with the server's integration in
task 4; the bulk region's use by streams (Phase 5); ring depth and spin window are the
daemon's derivation at rendezvous (task 4 wires the profile in).

Task 4 (the server) landed 2026-09-05: `slates-server` (`crates/server`): the daemon's
configuration as derivations from the profile (`config.rs`: clients per shard from Little's
law, the shard reserve, the table and store caps, ring slots and the spin window, the segment
geometry; every derivation logged with its inputs); the shard state in a thread-local cell on
its shard's thread (`state.rs`: volumes with their base host and reservation, the partition,
the store, the clients, the reserve, the deferred replies, the listings in flight); the verbs
of §4.4 (`verbs.rs`: create scratch and overlay with the bounded reservation or the host's
live memory as the dynamic quota's pressure source, snapshot, clone, attach with the lease
taken or renewed under D-16's epoch rule and a read intent needing none, detach releasing the
holder's last lease, resize moving the reservation, destroy in cooperative slices of half the
step budget, status with the drift list, list as a scatter-gather over every shard, base
verbs, acknowledgement, and the grant kind refused by channel and counted); the completion
record of every reply appended before it is sent (RIFL); the rights of §4.13 checked per verb;
a volume-bound verb from a client on another shard forwarded to the owner shard named by the
id's first bytes and the reply routed back (route by id, no index); the daemon (`daemon.rs`:
the segment created or attached from the anchor's environment, one mapping per shard, the
profile published, each shard's partition recovered and its state installed by a task on that
shard, the server loop as a poller of its clients' rings that idles otherwise and marks its
clients' regions parked, the control shard's rendezvous loop woken by the doorbell thread and
handing a client to its shard as a spawned task, the heartbeat at a tenth of the liveness
budget, stop joining everything); the doorbell thread (`doorbell.rs`: Linux waits on the
listening socket's readiness; macOS and Windows wait on the bootstrap object's word and kick
every shard; the value last acted on is what the wait compares against, so a ring during a
kick is never lost). The runtime gained pollers (`ShardContext::register_poller`: a task woken
by the loop whenever its ring says so) and `futures::idle` (yield without re-queue); the IPC a
shared protocol (`protocol.rs`: the bodies with one schema hash each, inline or through the
bulk chunk the slot's ring index owns) and the doorbell handed at rendezvous (the shard's kick
descriptor on Linux; the bootstrap word elsewhere); the volume core a `resize`.

Gated (`crates/server/tests/daemon.rs`, a two-shard daemon in the test process over a fresh
segment, one client at a time through the real rendezvous): the lifecycle (create, the
duplicate refused with the original's id, snapshot, clone, attach with epoch 1, status, list,
detach releasing the lease, resize, destroy completing in slices with the clone surviving);
exactly-once (a retry under the same id returns the retained reply without executing, an
acknowledgement releases it and a later retry is a stale duplicate); leases (a read attachment
takes none; the same principal renews with its epoch); AC-2.8 (the grant kind refused on the
ring and counted); a 200-byte name and an overlay over this crate's own source tree through
the bulk area, `read_base` reading the disk, `pin`, and a missing base refused
`BaseUnavailable`. Linux and Windows branches of the new crates lint clean from this machine.

Found by the daemon's tests and fixed: a task spawned from a task is joinable and stays in
the arena after it ends, so a daemon's perpetual tasks held its shutdown (they are detached at
spawn now); the doorbell thread re-read its word before each wait and lost a ring that landed
while it was kicking (it compares against the value last acted on); a client handed to a shard
whose loop already idled found the parked flag clear on its fresh region and never rang (the
hand-off wakes the loop, which marks the new client before idling again); a volume-bound verb
from a client on another shard was refused `NotFound` (forwarded to the owner now).

Found by the first cross-target lint of the Phase 1 base crate (a lane the Phase 0 crates had
and Phase 1's did not): on Linux rustix's `stat` nanosecond fields are unsigned and the
fingerprint's widening refused to compile; on Windows the fingerprint used unstable
standard-library metadata (`windows_by_handle`, `windows_change_time`). Both fixed in the
same change, and the cross-target lane now lints every shipped crate.

Owed from task 4: the file verbs over the ring (Phase 5's SDKs; until then content is
reachable in-process only); the Windows named-Event wake (Phase 4); one principal per uid
until the fleet's certificates (Phase 8); the clients-per-shard and ring-depth derivations
re-derived from measured rates at the first `status` (task 6 measures); `TargetIsVolume`
and the bridge path in `Attached` (Phase 3); the health signals of §4.14 exported through
`status` (task 6 with the histogram).

Task 5 (the Rust client and the CLI) landed 2026-09-05: `slates-client` (`crates/client`):
`Client::connect` through the rendezvous, one request in flight (the body framed inline or
through the slot's bulk chunk, the client spinning for the daemon's published window and then
parked on the wake word), request ids `(client id, sequence)`, and the two uses of exactly-once
(§4.9): a reply stalled past the reply deadline with the daemon found gone (`Liveness`, §4.7's
"control channel reset": the Linux control socket's peer end, or the bootstrap object's start
stamp on macOS and Windows) makes the client reconnect under its own id and resend, so the
retry meets its completion record; and `Session` lets a later process resume the id and the
sequence. Deadlines are derived (`Deadlines::derive`: the reply deadline is the anchor's
liveness budget, the reconnect budget the recovery budget plus one reply). Every verb of §4.4
is a typed method; refusals are the wire taxonomy as `ClientError::Refused`; the channel's
own refusals are `Stalled`, `DaemonGone` and `SessionTaken`. The rendezvous gained the wanted
id (`connect_as`; the daemon's `accept_pending` takes an in-use predicate and honours a free
id), the typed `TooManyClients` at the daemon's derived client bound (AC-2.6), and the
liveness check. `slates-cli` (`crates/cli`, the `slates` binary): `anchor` (the profile
measured, the segment created, the profile published, `slates daemon` supervised with the
segment and the anchor's pid in its environment; a daemon that never beats inside the recovery
budget or whose heartbeat lapses is killed and the policy decides; the restart bound
re-derived from the longest measured start), `daemon` (attaches and reads the published
profile, or measures and creates a segment when run alone; leaves when its anchor dies:
`PR_SET_PDEATHSIG` on Linux, a parent watch at the heartbeat cadence, a job object on
Windows), `profile`, and the client verbs with a stable plain output (one `key: value` per
line, one record per line for `list`) and exit codes for the taxonomy (0 done, 1 refused, 2
usage, 3 no daemon, 4 failed); a hand-written grammar with every flag listed once
(`args.rs`). The server gained `SegmentSource::Handoff` (an anchor in the same process), the
control channel held for a client's life, the client → shard mapping by the id's residue over
the partitions (a reconnect lands on the partition holding its records), routing by the
persistent partition index rather than the runtime's shard id, the name's owner partition by a
stable hash (`owner_of_name`, FNV-1a; every create of one name lands on one partition, so
uniqueness is that partition's to keep, with no global index), and the rebuild of recovered
volumes at start (`rebuild_recovered`: a live tree again, the reservation retaken, local-only
snapshots and attachments reconciled out of the catalog as recorded operations). `slates-mem`
keeps the object's name on macOS for opened objects, so an attached process can hand the
segment on.

Gated (`crates/client/tests/client.rs`, 2 tests, 1.2 s): the typed verbs over a two-shard
daemon (create, the duplicate refused with the original's id, snapshot, clone, attach with
epoch 1, status, list, detach, resize, destroy in slices, acknowledge; parks never exceed
replies); and a session outliving a daemon restart over one segment with the test as the
anchor: the first daemon stopped, a second started over the same handoff, the client's next
call stalling, finding the daemon gone, reconnecting under its id (one reconnect counted) and
served by the restarted daemon with the volume rebuilt, the retry of its earlier create
answered from the replayed completion record with the same id and no second volume, the
local-only snapshot reconciled away, new work continuing under the session's sequence, and a
second client refused the live session. (`crates/cli/tests/cli.rs`, 2 tests, 1.2 s): a real
`slates anchor` supervising a real `slates daemon`, the binary driven through create (the id
and the path line), the duplicate refused with exit 1, list, snapshot, clone, stat, attach,
status with and without `--drift`, detach, resize, destroy, the usage refusals with exit 2, a
missing volume with exit 1, then the anchor killed with SIGKILL and the daemon leaving so the
instance answers exit 3; `profile --quick` and the usage. The grammar and the value formats
have unit tests. All gates green on 2026-09-05 (`cargo xtask ratchet`: 82 rows, 0
regressions).

Found by the tests on their first runs: (1) routing used the runtime's shard id, which is
process-local (a second runtime in one process numbers its shards after the first's), so a
restarted daemon could reach none of its recovered volumes; volume ids and client ids now
route by the partition index, which recovery keeps (`ShardState::partition`); (2) a volume's
name was unique per shard only: two clients on different shards created one name twice
(T-2.1 across clients), which is what the CLI does on every invocation; (3) on macOS a shared
object opened by name refused to hand itself on, so a daemon attached from the anchor could
not map the segment on its shards and exited, which the anchor restarted and then refused as a
crash loop, exercising that path for real; (4) `detach` found a holder's other attachments by
encoding the whole partition (`to_snapshot`), replaced by `attachments_of`.

The dead-client reclaim landed 2026-09-05 (the daemon's side of §4.7's failure matrix, T-2.3):
the rendezvous carries the peer's process id (`SO_PEERCRED` on Linux; the claim slot's pid
elsewhere); every shard runs a sweep task at the liveness cadence (`reap_loop`, the same
budget the anchor allows the daemon's heartbeat) that expires leases by the wheel with nobody
asking and asks about every client silent for the budget: `peer.rs` (paired `#[cfg]`) peeks
the control socket on Linux (end of stream is the kernel closing the dead client's end),
probes the pid with signal 0 on macOS (`ESRCH` dead, `EPERM` reused by another user), and
waits on the process handle with a zero timeout on Windows. A gone client's attachments leave
the catalog as recorded operations, its deferred replies are dropped, its region and control
channel close with its slot, and its id returns to the control shard's live set (a thread-local
on that shard, reached by a spawned task: sharing by move); its leases keep their terms and
expire by the wheel, since a paused client is not a dead one and the term is the fence (D-16).
The operator's failover SLO moved into `DaemonConfig` (`failover_slo_ns`, ten seconds until
`slates anchor` takes a value; `with_failover_slo` for tests). The clock is read once per serve
round to mark the clients served in it. Gated (`crates/client/tests/reap.rs`, 3.8 s): the test
binary re-invoked as the victim connects, attaches for writing (epoch 1), prints its id and
parks; the parent kills it with `SIGKILL`; the attachment is reclaimed inside the lease term
(observed within two liveness budgets), the lease still shows epoch 1 after the reclaim, the
daemon's reaped counter moved by one, a session under the victim's id resumes (the id is free
again), the lease then expires by its three-second test term with nobody asking, and the
observing client never reconnected. Gotcha kept in the test: a re-invoked test binary prints
libtest's banner on stdout before the role runs, so the victim's line is tagged.

Owed from task 5: the CLI's grant surface
(`slates grant`, `grants`, `land`, `audit`) with task 8; a daemon-wide `slates status` with
task 6's health signals (`CLIENTS_REFUSED`, `RECOVERY_SKIPPED`, `HANDOFF_LOST`,
`INIT_FAILURES` are counted now and printed nowhere); the daemon start p99 for the restart
bound is the longest start measured in this anchor's life until a histogram of starts exists;
the Windows console handler, job object and section-and-Event paths are lint-checked from this
machine and run first in the Windows lane.

Task 6 (the provisioning histogram, exactly-once as a durable atom, admission and the wake
strategy) landed 2026-09-05. `crates/client/examples/provision_bench.rs` (R9, AC-2.1, T-2.6):
a volume created from the Rust client through the real rendezvous and rings against an
in-process daemon, sampled p50/p99/p999/max in a spinning form (the client spins for the 50 us
floor, so the reply lands without a wake when the daemon meets it) and a parked form (paced
past the shard's park, so each pays the doorbell and two wakes), at 1, 8 and 64 concurrent
clients. The 50 us floor is gated on the single-client spinning p99 (the latency claim); every
runnable concurrency's rows are recorded so the ratchet catches regressions; a run with more
client threads than the machine has cores past its shards is informational (it measures the
scheduler, not the path). Baselines (Apple M5 Max, macOS 26.4.1, best-of-3, all shown): one
client p50 9 us / p99 25 us / p999 31 us against the floor; eight clients p99 34-45 us;
sixty-four (oversubscribing thirteen runnable cores) p99 about 2 ms, not gated; the parked form
p99 about 250 us; a status round trip p99 about 9 us. The ratchet holds 102 rows.

Exactly-once became a durable atom: a verb's effects and its completion record now go into one
log record (`Db::begin` opens a transaction over the partition, `mutate` inside it applies and
queues, `commit` writes one `LogEntry` of every queued operation, a full log snapshots
instead), so `kill -9` between the effect and the record can no longer leave one without the
other (AC-2.3). A forwarded verb records its completion at its owner partition, and the reply
travels back already recorded; an acknowledgement is a scatter over every partition that may
hold the client's records; the client acknowledges on its own every half ring of replies, so
the daemon's retained records stay bounded without the caller (§4.9).

Admission and backpressure (AC-2.6): `clients_per_shard` is the client share of the shard's
reserve over a region's bytes (not the request rate, which sizes only what one client holds in
flight); the task arena and control channel are sized from that times the cross-shard traffic
per client plus the shard's own loops; a connect past the daemon-wide bound is refused
`TooManyClients`; a full owner-shard control channel makes a forward wait in the bounded
`pending_forwards` (retried each round) and, past the clients' credit, refuses
`Overloaded{shard}` without starting the verb. The daemon raises its descriptor soft limit to
the hard one at start (no privilege). A daemon-wide `slates status` (a scatter-gather like
`list`) reports each shard's counters and the health signals of 4.14 and the anchor's view.

The wake strategy was completed (4.7): a runtime shard sets a parked flag before it waits and
re-checks its inbox, and a sender kicks only a parked shard, so a message to a spinning shard
costs no syscall (the flag and the message are sequentially consistent, so a lost wake needs
both to miss, which the total order forbids); the server loop keeps polling for a derived idle
window after its last work, so an active client's next request never pays a wake; the client
can spin for a latency floor of its own (`Client::spin_for`).

Found under the histogram: (1) the create verb's completion record and its effect were two log
records, a `kill -9` between them a durability hole, closed by the transaction; (2) a shared
object whose name exceeded the macOS 31-character limit was truncated, so two clients' regions
could collapse onto one object under the bench's long names, now hashed when they would not fit
(`crates/mem/src/shared.rs`); (3) `status` and `detach` walked the whole partition
(`to_snapshot`) to count a volume's attachments, replaced by `attachments_of`; (4) a burst of
concurrent connects exhausted the bootstrap object's claim slots, so the client retries the
rendezvous on `RingFull` as on `DaemonUnavailable`.

Owed from task 6: the histogram runs on the reference machines in CI (this baseline is the
laptop's); the write-tracer hermeticity assertion (AC-2.2, T-2.9) and the simulation crash at
every instruction (AC-2.3's simulation half) arrive with the chaos harness; the cross-uid
security test (T-2.7) needs a second uid, gated on CI (the rendezvous refuses and counts it
now); a completion fd for parked SDK event loops is Phase 5.

Task 7 (the register protocol at f=0) landed 2026-09-05: `crates/db/src/register.rs`, the pure
core of §4.8 parameterized by the fault tolerance `f` so the laptop is the degenerate of one
formula (R8), never a mode: `Quorum` (`2f+1` candidates, commit at `f+1`, `f=0` giving one
candidate and a commit of one, the local append); rendezvous (highest-random-weight) candidate
selection, owner-first and deterministic, so every host computes the same holder set from an
object id with no directory; `Fence`, a holder's monotonic authority for a host (a record under
a host epoch below the highest seen is refused `StaleEpoch`, so a resumed stale owner never
commits); and `Configuration`, the one-voter oracle (`solo`: version 0, one member, `f=0`, no
mirror; `check_version` refuses a stale version with the current one; `await_placed(scope)`
returns for the region — the local append at f=0 — and refuses the absent mirror `Unsupported`).
Every reply carries the placement from the first version so Phase 8 changes no interface: the
wire gained `PlacedState` (`region`, `mirror_age_ns`, `host_epoch`) on `StatusReport`, the
`Scope` enum, and the `AwaitPlaced` request with the `Placed` reply; the server holds a
`Configuration::solo` per shard built from the machine identity's host id, records a snapshot's
placement through it (placed at f=0), reports the head's placement and the host epoch in
`status`, and serves `await_placed`; the client has `await_placed` and the CLI `slates volume
placed ID [--snapshot N] [--mirror]` with the placement fields in `status`.

Gated (`crates/db/src/register.rs` tests): the commit rule is the same code at f=0 and a
simulated f=1 (one candidate vs three, both `placed`), the observable differing only by the
quorum's own count (AC-2.5's register slice); a stale host epoch is refused at every f
(`StaleNeverCommits`); a stale configuration version is refused with the current one; `await
placed(region)` returns and the absent mirror is refused; rendezvous placement is
deterministic, owner-first and spread. The client and CLI tests assert the f=0 placement over
the real rings: `status` shows `placed=true`, `host_epoch=1`, no mirror; `await_placed(region)`
returns `(true, None)` and the mirror is refused `Unsupported`.

Found by the CLI test on its first run: `detach` carried only an attachment id and ran on the
detaching client's shard, but the attachment record lives on the volume's owner shard; it had
passed only because earlier client ids happened to land on the owner shard, and task 7's extra
clients shifted them. Attachment ids now carry their owner partition in the high 16 bits
(`attachment_id`/`owner_of_attachment`) and `detach` routes to it, like a volume id (§4.8
'ids route to owners, no global index'); a latent cross-shard `detach` bug closed.

Owed from task 7 (updated 2026-09-05): the holders, commit at `f+1`, takeover and phase-one
adoption are now implemented as the pure fenced ledger register simulation (§8h); still Phase 8
are the server put path (hedged placement over the real holders, recorded holder sets in the head
record), the healer and probation, mirroring and `await placed(mirror)`, migration on a
write-intent attachment, and the SWIM membership (the register core is f-parameterized so they
raise `f` without a new shape); the host epoch is persisted only as the constant 1 until the
takeover path is wired into the server (Phase 8); a chain is a register written in sequence,
which arrives with the merge engine (Phase 6).

Task 8 (grants and landings through the server) — the durable records, the ring verbs and the
CLI landed 2026-09-05; the control-channel grant transport and the write execution are in the
Linux lane. `crates/server/src/landing.rs` wires the Phase 1 landing engine (`slates-land`)
through the server: a `RequestBody::Land` plans the manifest from the snapshot's diverged
entries and, without a grant, replies `GrantRequired` with the manifest hash, its summary and
the preliminary conflicts, recording a `LandingRecord` (AwaitingGrant) and a `LandingPlanned`
audit record; a grant that binds the manifest lets the landing take the target's lease (one
holder per target, AC-2.9), validate and write through `OsLand`, after which the landing
record, the consumed grant and the audit trail are persisted (all §4.8 ops, so the
accountability replays after a crash, AC-2.10). The grant is never created on the ring or MCP
(R10, AC-2.8): the ring's grant kind stays refused, and `issue_grant` (the control-channel
entry) binds the manifest a human saw, refusing `GrantMismatch` when a re-planned landing's
hash differs. The wire gained `Land`/`Grants`/`Audit` requests, `GrantRequired`/`Landed`
replies, the `LandingSummary`/`LandingOutcome`/`GrantSummary`/`AuditEntry` shapes, the `Filter`
and `GrantScope`, and the refusals `TargetUnavailable`, `LandingConflict`, `LandingLeaseHeld`,
`GrantMismatch`, `GrantInvalid`. The client has `land`/`grants`/`audit`; the CLI has `slates
land ID TARGET`, `slates grants`, `slates audit`. The landing execution and the `os` writer are
Unix-only, so the write path's test runs in the Linux CI lane; the daemon suite here checks the
off-ring refusal, a landing into a target that cannot be opened (refused `TargetUnavailable`
with no write), and the empty grants and audit reads.

Found while wiring: `detach`'s cross-shard routing bug (task 7) had a sibling — a landing id, a
grant id and an attachment id all need to route to the partition that holds their record;
attachment ids now carry the owner partition (task 7), and the landing/grant records live on
the volume's owner shard, reached by the volume-bound `Land` request. A bench-harness flake
surfaced under the omnibus ratchet on a loaded machine and was fixed: the size-independence
check (`vfs_bench` ac-1.3) compared the median growth against the bare timer resolution, so a
lucky-fast small-size sample read as per-file scaling; it now allows the two measurements' own
bootstrap-interval widths (a real per-file term over three decades still fails). The IPC
parked-round-trip bench asserted the client parked exactly once per trip; a spurious futex
wakeup can add a park, so it now asserts at least once per trip.

Owed from task 8: the control-channel grant transport (the Linux control socket reader in the
daemon and `slates grant`/`slates grant --watch`; the socket is the rendezvous control channel,
Unix, with macOS and Windows on Phase 5's control socket) and the Linux landing execution test
(the full plan-grant-write flow into `/dev/shm`, AC-2.9's two-session serialization, and
AC-2.10's audit replay after `kill -9`); the per-entry audit records (`EntryWritten`,
`EntryRefused`) beyond the plan and the terminal record; the grant scatter for a daemon-wide
`slates grants`/`slates audit` (served on the shard now); the write-tracer hermeticity
assertion (AC-2.2, T-2.9) with the chaos harness. The provisioning histogram
(`crates/client/examples/provision_bench.rs`) was pulled out of the omnibus `cargo xtask
ratchet` (it spawns a daemon and needs a quiescent machine; back-to-back with the microbenches
its p99 measured contention, not the path) and is its own recorded command / CI lane for
AC-2.1; its rows were removed from `ratchets.toml`.

## 8e. Phase 3 record (2026-09-05)

Phase 3 (the Linux FUSE bridge) task 1's first piece landed 2026-09-05: `slates-bridge-fuse`
(`crates/bridge-fuse`), the FUSE ABI codec — the pure, transport-free layer. It parses the
kernel's `fuse_in_header` and the opcode-specific bodies slates serves (`request.rs`,
`abi.rs`: the opcode set as `#[repr(u32)]` discriminants that are the wire values, so an
unserved opcode is a typed miss the daemon answers `ENOSYS`), encodes the daemon's replies
(`reply.rs`: `fuse_out_header`, `fuse_attr`, `fuse_entry_out`, `fuse_attr_out`, `fuse_open_out`,
`fuse_write_out`, and a bounded `readdir` buffer), and computes the `FUSE_INIT` negotiation
(`init.rs`: the intersection of the flags slates wants — writeback cache, parallel dirops,
readdirplus, explicit data invalidation, big writes — and the kernel's, the minor version
bounded to slates' 7.31 floor, and the sizes it will use). The writeback-cache flag's advertised
bit was corrected to the Linux ABI value `1 << 16` (`<linux/fuse.h>`); it had been `1 << 8`
(`FUSE_SPLICE_MOVE`), so writeback never negotiated — the source audit's BUG-6, fixed with a
kernel-vector test. Every field is read and written in
order through a bounds-checked sequential reader/writer (`wire.rs`), so no byte offset is a
literal and a truncated or oversized message is a typed refusal, never a panic or an
out-of-bounds read; the crate holds no `unsafe`.

Gated (`crates/bridge-fuse/tests/codec.rs`, 12 tests, on every host — the codec is pure): a
`LOOKUP` parses to its header and name; an unserved opcode is `None` not a panic; hostile
headers (truncated, a length below the header, a length past the buffer) and hostile bodies (an
unterminated name, a short read, a write whose declared data runs past the body) are refused
without a panic (§4.9); read and write bodies parse; error and success replies encode to the
exact wire bytes with an undersized buffer refused; the `readdir` buffer packs 8-byte-padded
entries and stops before it exceeds the request's size; `FUSE_INIT` keeps the flag intersection
and handles a version mismatch and a short body. Golden byte checks stand in for kernel vectors
until the transport test runs a real mount.

Owed from Phase 3 task 1 (the rest of the driver, all Linux-only, CI lane): the `/dev/fuse`
transport (request read, reply write, notifications), `FUSE_DEV_IOC_CLONE` per shard and the
io_uring command path with the read/write fallback, mount establishment (the new mount API when
permitted, `fusermount3` otherwise) with the fd held by the anchor and the restart handoff, the
`Bridge` trait implementation over the volume core with inode `(no, gen)` and invalidation on
every mutation, `slates exec` (the launcher), the conformance suites (pjdfstest, fsx, fsstress)
and the workload harnesses, and the base-files read path through the mount. These are Phase 3
tasks 1b–8; the codec is the foundation they build on.

The bridge dispatch landed 2026-09-05 (still Phase 3 task 1, pure): `bridge.rs` defines the
`Bridge` trait (§4.6's methods, one real implementation to come in the daemon over the volume
core) and `dispatch(message, bridge, out)` — the seam between the wire and the semantics. It
parses a request, calls the matching method, and encodes the reply or the errno: `INIT`
negotiates, `LOOKUP`/`GETATTR`/`OPEN`/`OPENDIR`/`READ`/`WRITE`/`READDIR`/`CREATE`/`RELEASE`/
`FLUSH`/`FORGET` reach the bridge, a `Result::Err(errno)` becomes the kernel's negated errno, a
parse failure is `EIO`, and an unserved opcode is `ENOSYS` without reaching the bridge. Gated
(`crates/bridge-fuse/tests/dispatch.rs`, 3 tests, every host): a mock one-file bridge driven
through the dispatch — LOOKUP and its ENOENT, READ returning a slice and WRITE mutating the
file, READDIR packing an entry, INIT negotiating, FORGET reaching the bridge with no reply, and
an unserved opcode answered ENOSYS. The transport (the `/dev/fuse` read/write loop) is the thin
Linux-only layer over this dispatch.

The Bridge implementation over the volume core landed 2026-09-05 (still Phase 3 task 1, pure):
`volume_bridge.rs` (`VolumeBridge`) borrows a `Volume` and its `Store` and turns the kernel's
requests, by FUSE node id (the inode number, node id 1 the root), into volume operations —
lookup, getattr, open/opendir, read, write, readdir, create, release, forget, flush — with a
small file-handle table naming the inode a handle was opened on, and the volume core's refusals
mapped to the Linux errno the kernel expects. The volume core gained by-inode-number wrappers
(`root_inode`, `lookup_no`, `readdir_no`, `create_file_no`) so the bridge speaks inode numbers,
not handles. Gated (`crates/bridge-fuse/tests/volume_bridge.rs`, 2 tests, every host — a scratch
volume is pure RAM): a whole FUSE round trip through the real volume core (CREATE a file, WRITE
to it, LOOKUP it, GETATTR its size, OPEN and READ the bytes back, READDIR the root lists it) and
the typed errnos (a missing name is ENOENT, a stale handle is EINVAL). The metadata ops followed the same day: the Bridge trait, the dispatch and the VolumeBridge
gained mkdir, unlink, rmdir, rename (and rename2), symlink, readlink, setattr (size → truncate,
mode → chmod, by the `valid` mask) and statfs, with the volume core's by-inode-number wrappers
(`mkdir_no`, `symlink_no`, `unlink_no`, `rmdir_no`, `rename_no`) and the setattr/rename/statfs
codec. Gated (two more tests in `volume_bridge.rs`): mkdir then a file inside it, rmdir refused
non-empty (ENOTEMPTY), unlink then rmdir; and rename moving a file, setattr truncating it, and
statfs answering. The FUSE bridge's semantic surface is now complete and pure-tested; the
`/dev/fuse` transport (Linux) is the only remainder of task 1. A pre-existing anchor
test-isolation bug was fixed in the same change: `supervised_child` set process-global env vars
that raced into its own in-process test thread under a concurrent `cargo test --workspace`; it
is now `#[ignore]`d and the parent spawns it with `--ignored`, so cargo never runs it in
process. The `/dev/fuse` transport landed 2026-09-05 (Phase 3 task 1b, Linux): `channel.rs` (Linux-only)
owns the device descriptor and turns it into the request/reply stream — `FuseChannel::open`
opens `/dev/fuse` (a character device, structurally allowed as a non-disk-file open), `from_device`
adopts the fd the anchor hands back on a restart, `read_request`/`write_reply` are the device
I/O (a disconnect is `ENODEV`, typed), and `serve_blocking` is the fallback loop the design
names: read a request, `dispatch` it to the bridge, write the reply, until the kernel unmounts.
No `unsafe` (rustix's I/O-safe wrappers over the owned descriptor). It compiles and cross-lints
for Linux from this machine; the serve loop runs against a real mount in the CI Linux lane. Owed
with the rest of the driver: the io_uring command path, `FUSE_DEV_IOC_CLONE` per shard and the io_uring command path (which drops the request copy
the blocking loop makes), generation-tracked node-id reuse after `forget` (§4.6 `(no, gen)`),
`link` and xattrs, the kernel invalidation notifications, `slates exec`, and the conformance and
workload suites.

Mount establishment landed 2026-09-05 (Phase 3 task 2, Linux): `mount.rs` mounts a slates
connection through `fusermount3`, the OS-shipped setuid helper, so no privilege is required
(R10, D-2): the daemon makes a socket pair, spawns `fusermount3 -o default_permissions,fsname=…`
with one end in `_FUSE_COMMFD`, and receives the `/dev/fuse` descriptor the helper sends back
with `SCM_RIGHTS` (the same fd-passing rustix path the rendezvous uses), returning a `Mount`
whose `FuseChannel` serves it; `unmount` tears it down with `fusermount3 -u`. No `unsafe`. It
compiles and cross-lints for Linux here; the handshake runs against a real `fusermount3` in the
CI Linux lane. Owed: the new mount API (`fsopen`/`fsconfig`/`fsmount`/`move_mount`) where the
daemon has `CAP_SYS_ADMIN` in its user namespace, and the anchor holding the fd across a
restart (§2.6 step 4).

The invalidation notifications landed 2026-09-05 (Phase 3, §4.6 "Cache posture"): `notify.rs`
encodes the unsolicited messages the daemon writes to `/dev/fuse` to drop kernel cache on a
mutation — `inval_inode` (an inode's attributes and a data range), `inval_entry` (a cached
name → node mapping) and `delete` (an entry removed). They are pure encoders (the header with a
zero unique and the notification code in `error`, then the body), tested on every host with
golden byte checks (4 tests). The driver writes them before it acknowledges a mutating request,
so a second process never reads stale attributes after the mutating call returns (AC-3.3); wiring
them into the mutating dispatch paths is the driver's, with the transport.

Base files through the bridge landed 2026-09-05 (Phase 3 task 8's read path, §4.6 "Base files",
AC-3.9): `VolumeBridge::with_base` holds the overlay's read-only `OsHost`, and lookup, getattr,
readdir and read route through `Volume::with_host` (the overlay path that serves untouched base
entries from the disk), the volume core gaining `Overlay::lookup_no`/`readdir_no`. Gated
(`crates/bridge-fuse/tests/base_overlay.rs`, on any Unix host — the base is a real read-only
directory, this crate's own `src`): an overlay over `src`, served through the bridge, lists its
base files, looks `lib.rs` up, and reads it back byte-identical to reading the file straight
from disk, writing nothing. Owed: a standalone `LOOKUP` of an unlisted base entry loads the
directory's base listing on demand (today the listing loads on `readdir`, which the kernel does
first; the realistic sequence is verified); `mmap` of a base file and the splice reply path (both with the
transport). Base writes' copy-up through the bridge is done and verified: a write to a base file
routes through `Overlay::write`, copies the base up into the overlay's RAM, and leaves the base
directory on disk untouched (`base_overlay.rs` second test — the write is visible on a later
read, the rest is the base bytes, and the disk file is byte-for-byte unchanged, R1).

The launcher `slates exec` landed 2026-09-05 (Phase 3 task 4, Linux): `crates/cli/src/exec.rs`
makes a volume visible at a caller-named path for one command, in a new user and mount namespace,
without privilege (D-2, R10) and without writing disk (AC-3.5): it enters `CLONE_NEWUSER |
CLONE_NEWNS`, maps its own uid and gid, makes the mount tree recursively private, bind-mounts the
volume's directory (under the daemon's root, `SLATES_ROOT`) onto the chosen path, and execs the
command — so the parent shell's view is unchanged and the bind lives only in the command's
namespace. An unsatisfiable path is refused with the exact missing directory, never created. One
budgeted `unsafe` (`unshare_unsafe`, sound in the single-threaded pre-exec launcher). The `--`
splits the flags from the command; the parsing is unit-tested on every host, and the launcher
runs against a real daemon mount in the CI Linux lane. Owed: the daemon publishing its root
mount so `SLATES_ROOT` need not be set by hand, and the AppArmor-profile detection with the exact
remedy message (a generic hint is given now).

The xtask unsafe counter was made word-boundary aware in the same change: it counted the
substring `unsafe` inside identifiers like `unshare_unsafe`; it now counts the `unsafe` keyword
as a whole token. The end-to-end CLI flow test (`crates/cli/tests/cli.rs`) is gated behind
`SLATES_TEST_CLI` and runs as its own CI step: it spawns a real anchor and daemon whose shards
spin, and a busy parallel `cargo test --workspace` starves them; on its own (the CI step passes
`--test-threads=1`) it is reliable.

The bridge seam became the one shared operation layer 2026-09-05 (§4.6 "Bridge trait (one VFS operation layer)", D-2): the `Bridge` trait, its neutral data types (`NodeAttr`, `DirEntry`, `FsStat`, `SetAttr`, `RenameFlags`) and the single `VolumeBridge` implementation moved from `bridge-fuse` into a new `slates-bridge-core` crate (in the `bridge-*` lint family), so every OS transport — FUSE, the now-first-class virtio-fs (GAP-A9-5, RQ-20), the macOS FSKit module and its NFS fallback, WinFsp — dispatches its wire protocol onto one seam and encodes the neutral results back, never a second parallel operation layer (Part 2 item 7). The trait is neutral by construction: addressed by real inode numbers (the FUSE "node id 1 is the root" convention resolved at the FUSE edge through `Bridge::root`, so NFS, which mints its root handle the same way, shares the implementation), returning neutral attributes, and refusing with the volume core's own `VfsError`, which each transport maps to its wire error (a Linux errno, an `nfsstat3`). `bridge-fuse` keeps the FUSE ABI codec and becomes the wire edge over the seam (`bridge.rs`: node id to inode, `VfsError` to errno, neutral to `fuse_attr`), re-exporting the trait so its tests' import paths hold; all 26 of its tests pass unchanged, evidence the move preserved the FUSE behavior byte for byte.

Three A-9 bridge findings were fixed in the same change, at the seam so every transport inherits the fix instead of the flagged narrow signature (the audit names `Bridge::rename` and "the reduced Bridge signature" directly; Ada's steer, 2026-09-05). BUG-8: `setattr` carries every POSIX field (`SetAttr { size, mode, uid, gid, atime, mtime }`) and the volume core gained `chown`/`set_times`, so a requested field is applied, never an ignored field acknowledged (§4.6 "never acknowledge an ignored setattr field"). BUG-10: `rename` carries the `renameat2` flags (`RenameFlags`), honoring `RENAME_NOREPLACE` (an existing destination is `EEXIST`) and refusing `RENAME_EXCHANGE` with `EINVAL` — the errno `renameat2` itself returns where a flag is unsupported (D-26) — never silently downgrading to a replacing rename. BUG-4: open handles live in a bounded generational `slates-mem` slab, so a released handle is stale (never a wrong inode) and its slot is reused, and the table refuses at its bound (`EMFILE`) instead of growing without end. Gated (`crates/bridge-core/tests/volume_bridge.rs`, 5 tests, every host, no mount): a `setattr` of ownership and times takes effect while a later mode-only `setattr` leaves them alone; NOREPLACE refuses an existing destination and takes a free one; EXCHANGE is refused, not downgraded; a thousand open/release cycles leak no handles; and — the later *root:wheel* fix (2026-09-09, `docs/bugs/2026-09-09-root-wheel-mount.md`) — a created object is owned by the mounting user (the subject's uid) with its parent's group, not the born `root:wheel` default, across all three creating verbs. The mounted verification (independent kernel vectors, real setattr/rename semantics under writeback, `UTIME_NOW`/`UTIME_OMIT` resolution, and open/close beyond the arena bound) is owed to AC-3.10/AC-3.12, which need a real mount; the remaining bridge findings — BUG-5 base-aware lookup, BUG-7 READDIRPLUS/FSYNC/LINK dispatch, BUG-9 truthful statfs — stay open under GAP-A9-3. `slates-bridge-core` unsafe budget 0; clippy (all targets), literal, structural (19 crates), cross (linux, windows) and fmt gates clean.

## 8f. Phase 6 groundwork (2026-09-05)

The merge engine's deterministic verdict — the pure core of §4.16 (D-27) — landed 2026-09-05 as
`slates-merge` (`crates/merge`), ahead of the rest of Phase 6, because it is a self-contained
pure function that needs none of the fleet or the green-volume integration to be correct and
directly confirmable. `range.rs` is the byte range and the per-path `RangeSet` (sorted,
non-overlapping, with the half-open overlap rule that an insert at the very edge of a range does
not conflict). `verdict.rs` is the two passes: `path_verdict` sweeps the increment's ranges
against the intervening deltas' effect ranges on one path — disjoint ranges `Accept`, an
identical span becomes a candidate for pass two, an insert anchored inside a change or two
inserts at one point are `SamePositionDiffering`, any other overlap or containment is `Overlap`;
a structural class the sweep cannot see (rename/create/type/meta/delete-vs-modify) is passed in
and returned directly. `compare_bytes` is pass two (a memcmp: equal bytes `AcceptIdentical`,
else a conflict). `fast_path` is the whole-increment shortcut: every touched path last changed
at or before the base means `Accept` with no range work, and it never decides a conflict. The
sweep is a linear merge of two sorted lists; it does no I/O, reads no clock, draws no randomness,
and its hot comparison allocates nothing (a lint and a no-alloc test to pin that are owed).

Gated (`crates/merge/tests/verdict.rs`, 9 tests, every host — the verdict is pure; the hecate
M-matrix as range cases): disjoint accepts, no-intervening-change accepts, overlap and
containment conflict, an identical span becomes a candidate that pass two resolves to
identical-or-conflict, an insert inside a change conflicts while one at the edge accepts, two
inserts at one point are resolved by pass two, every structural class is returned directly, and
the fast path accepts an untouched basis.

The ops document — the canonical serialization of an increment's declared operations, whose BLAKE3 is half the increment's identity (§4.16 "Data model", "its identity is the test") — landed the same day (`ops_doc.rs`). It is the fixed op kinds (§4.16 `OpKind`, wire values 0–14; `from_wire` reads them off the kinds' own list, so no arm is a bare number), a per-document path table that interns paths in insertion order while building and sorts them canonically at `canonicalize` (remapping every op's path index), and one fixed little-endian record per operation (kind, flags, path index, offset, length, source offset). Bytes are never in the document; content an operation adds is named by a source offset into the post-state chunks the splice resolves later. `encode` is a sequential writer, never a struct transmute, so no host-dependent padding enters; `identity` is the BLAKE3 of that encoding. Gated (`crates/merge/tests/ops_doc.rs`, 5 tests, every host): the same operations declared in different orders produce byte-identical encodings and one identity (the determinism gate), encoding is stable and the identity is its hash, different work has a different identity, every op kind round-trips through its wire value, and interning is stable while canonicalize sorts the table and remaps the ops; and a golden vector pins the identity of a fixed document (`67613f8e…`) so a change to the canonical encoding is caught across versions, not only within a run (the identity is what an increment's id is built on). A caller canonicalizes before taking the identity; the deriver's terminal step will, and that property is what the determinism test pins.

The content deriver — the pure core of "Composition at seal" — landed the same day (`derive.rs`). One path's declared content operations (`ContentOp`: overwrite, extend, truncate, insert, delete), in journal order, compose by interval algebra into the canonical net op set relative to the base content, never by comparing bytes (D-27's never-diff clause: the deriver reads no content). Composition tracks the file as a piece list in current-file coordinates — runs of surviving base bytes and runs of new bytes — that each operation splits and rewrites; a single readout pass then emits the net ops in base coordinates, naming added content by its offset in the sealed post-state (the final file, `slates`' ground truth for bytes), which the splice resolves later. The canonical form is minimal: overlapping overwrites merge into one, an equal-length in-place replacement is one overwrite, an unequal one is a delete then an insert (the whole-file-rewrite shape), an addition past the base end is an extend and within it an insert, a removal to the base end is a truncate and within it a delete. Gated (`crates/merge/tests/derive.rs`, 9 tests, every host): the four worked cases (overlapping overwrites merge, a truncate cancels operations beyond it, an insert then an overlapping delete cancels, a whole-file rewrite is a delete then an insert), an append is an extend, an empty journal is the identity, and three proptests over random in-bounds journals — the deriver oracle (applying the net ops to the base, drawing added bytes from the post-state by source offset, reproduces the post-state exactly, so composition is correct without any byte comparison), determinism (the same journal composes to the same ops), and the net set is ordered by base offset. The oracle draws base bytes and added bytes from two different position-varying patterns, so a misplaced op, a wrong length or a wrong source offset diverges. A `Vec` piece list makes a front split O(pieces); the journal a submit composes is bounded, and a balanced structure is the measured replacement if a length benchmark shows the piece count dominating (owed, not guessed).

Position mapping — "canonical rebase" (§4.16 "Position mapping") — landed the same day (`map.rs`). An increment declared against a base version is mapped forward through the canonical deltas of every version in `(base, head]`, one direction, per path, before the verdict and the splice. For one delta: an intervening operation that turns `old_len` base bytes at `at` into `new_len` shifts every later position by `new_len - old_len`; a range entirely before it is only shifted; a range that overlaps a touched span is returned as `Overlaps` for the verdict to classify (the mapper does not decide conflicts); an insert at the very edge of a range does not overlap it (the range's edge rule), so a range whose neighbourhood only grew or shrank maps cleanly. Maps compose: mapping through `(base, head]` feeds each delta's shifted range into the next. Content effects come from the ops' kinds (overwrite touches its span with zero size change, insert and extend add, delete and truncate remove); namespace ops have no byte-coordinate effect. Gated (`crates/merge/tests/map.rs`, 9 tests, every host): an insert before a range shifts it and after it leaves it, a delete before shifts it left, an overwrite hitting it overlaps, an insert at the edge is clean, maps compose across two deltas, and a later delta meeting the shifted range overlaps — plus a provenance oracle (apply the deltas to a vector recording each head byte's base origin; a range maps cleanly exactly when its bytes all survived contiguously, and then to their position) over random single-op delta sequences, and determinism. Owed here: folding old deltas into checkpoint deltas so a distant base maps in O(log) lookups (the same composition applied ahead of time; the measured optimization when a base-lag benchmark shows the raw walk dominating).

The whole-volume deriver — composing a work volume's journal of content, create and unlink into one ops document — landed the same day (`increment.rs`, replacing the content-only assembler). Per path the journal is a state machine (`VolumeOp`): a create makes a fresh empty file, an unlink removes it, content operations accumulate against whatever file is present. Composition follows §4.16: a create then an unlink of a new file cancels (nothing declared); a base file unlinked is one `Unlink`; a base file edited in place is its content net ops; a base path whose content was replaced (unlinked then recreated, or created over) is a delete of the base and the new content, with no create because the path existed at base; a new file that survives is one `Create` and its bytes as inserts. It then groups the paths, composes each with the content deriver, and lays the post-state out in sorted-path order (each path a contiguous region, an op's source its per-path offset plus the region base), so the identity is independent of the journal's declaration order. An operation the journal could not have produced on a valid volume (content on a missing file, a create over an existing one, an unlink of a missing one) is a typed `DeriveError`, never a panic. Gated (`crates/merge/tests/increment.rs`, 6 tests, every host): create-then-unlink cancels, unlinking a base file is one unlink, creating-and-writing is a create and an insert, recreating a base file replaces its content (a delete and an insert, no create), content on a missing file refuses, and a whole-filesystem oracle over three paths with random valid journals — reconstructing the filesystem from the increment (creating, removing, and applying content drawing from the post-state) reproduces the model the journal produced, base bytes (0-127) and added bytes (128-255) disjoint so any wrong path, offset, source or missing create/unlink diverges.

Rename composition landed the same day, extending the deriver to a moving-entity model (`increment.rs`): each file is an entity carrying its origin, base length and content ops; a create makes one, an unlink kills one, a rename moves one and content accumulates on whichever entity is at a path. §4.16's rename cases: a base file renamed to a fresh path is one `Rename` whose source is the base path, plus its content; a base file renamed over another base file is one `Rename` that replaces the target; a new file renamed over a base file is the write-and-rename pattern — it composes to the destination's content being replaced (a delete and the new bytes), not a rename. A gone base path is unlinked only when no surviving file occupies it, so a rename or replacement onto a path covers its removal and a base file recreated to the same content leaves no trace. The rare rename onto, or create at, a base path already consumed this increment is a typed `Unsupported` refusal (owed). The oracle now generates renames too and reconstructs them (a renamed path's base comes from the rename's source), skipping the owed refusal; 7 tests including worked rename cases (base to a fresh path, write-and-rename replacing a base file's content).

The splice landed the same day (`splice.rs`): once the verdict accepts a path's net ops, the new version's extent list is built from the base version's extents with each op's range replaced by a reference into the post-state chunks — no byte is copied, an unchanged run keeps pointing at its base chunk and an added run points at the post-state chunk (`src`). It is a single walk over the base extents and the (base-ordered) net ops: copy the base extents up to the next op, then place the op's replacement; `Source` is `Base` or `PostState` (a chunk store and offset). Gated (`crates/merge/tests/splice.rs`, 5 tests, every host): an overwrite splits the base and keeps the rest base-sourced (a non-vacuity check that unchanged bytes are not copied), no ops leaves the base extents untouched, a truncate to zero leaves none, and two proptests — the splice oracle (chunk the base into extents, splice a random journal's net ops, read the extents back from the base and post-state buffers, reproduce the final content exactly) and well-formedness (the extents cover exactly the final length, none empty).

Directory composition landed the same day, extending the deriver's input to a `Base` snapshot that knows the base's files and directories (`increment.rs`). Directory operations compose independently of files (a path is a file or a directory, never both at once): a base directory removed is one `Rmdir`; a new directory that survives is one `Mkdir`; a mkdir then an rmdir cancels; a base directory removed then recreated is unchanged (directories have no content). A path used as both a file and a directory this increment, a mkdir over a present directory, and an rmdir of an absent one are typed refusals (`PathIsFileAndDirectory`, `MkdirOverExisting`, `RmdirMissing`). Gated (`crates/merge/tests/increment.rs`, now 15 tests, every host): seven directory worked cases (make, remove, mkdir-then-rmdir cancels, remove-then-recreate is nothing, the file/directory collision, the two refusals) and a directory oracle over random valid mkdir/rmdir journals (applying the document's Mkdir/Rmdir to the base directory set reproduces the set the journal produced). The file deriver's seven tests are unchanged (no regression).

Mode composition landed the same day (`SetMode`, extending `Base` with the base mode per path). A file or directory's mode composes independently, last-write-wins, emitting a `SetMode` (the new mode carried in the op's `len`) only where the mode differs from the base and the path is present at seal — a surviving file (including an untouched base file) or a present directory. A mode set to the base mode declares nothing (minimality); a mode on a path present nowhere is `SetModeMissing`; chmod then rename of one path is the owed `Unsupported` refusal. Gated (`crates/merge/tests/increment.rs`, now 22 tests, every host): setting a base file's mode, setting it to the base mode (nothing), last-write-wins, a base directory's mode, a new file's mode, the missing-path refusal, and the chmod-then-rename refusal.

Symlink composition landed the same day (`Symlink`, composed independently like directories, with `Unlink` on a symlink path routed to it; the target is interned in the path table and named by the op's `src`). A new or retargeted symlink is one `Symlink`; a base symlink removed is one `Unlink`; a symlink created then unlinked, or a base symlink removed then recreated to the same target, is nothing; retargeting a base symlink (unlink then symlink) is one `Symlink` because the survivor rule suppresses the removal. A symlink over a present symlink, and a path used as conflicting kinds (file/directory/symlink), are typed refusals (`SymlinkOverExisting`, `PathKindConflict`). The base grew a `symlinks` list (path to target). Gated (`crates/merge/tests/increment.rs`, now 30 tests, every host): create, symlink-then-unlink cancels, base-symlink removal, retarget, recreate-to-same-target (nothing), symlink-over-existing, symlink at a base file path, and a write on a base symlink. This work reused the file oracle, which surfaced a latent write-and-rename bug: a new file renamed over a base file that had itself been renamed to that path was wrongly treated as a content replacement of a non-existent base path; the fix restricts the content-replacement to a base file at its own path, so the other case is a new file at the destination with the clobbered base file's original name unlinked (the analogous path in `apply_create` already checked the own-path case). The oracle now passes across repeated proptest seeds.

Xattr composition landed the same day (`SetXattr`/`RemoveXattr`, composed like `SetMode` but against the base value). Per `(path, name)` the final state is the last set value or removed; a `SetXattr` is emitted only where the value differs from the base and a `RemoveXattr` only where a base xattr is removed, and only where the path is present at seal. The op names the file in `path` and the attribute name (interned in the path table) in `at`; a set xattr's value is laid out in the post-state after the file content, in sorted `(path, name)` order, and named by the op's `src`/`len`. The base grew an `xattrs` list `(path, name, value)`. An xattr on a renamed-away path is the owed `Unsupported` refusal; one on a path present nowhere is `XattrMissing`. Gated (`crates/merge/tests/increment.rs`, now 36 tests, every host): setting to a new value, setting to the base value (nothing), removing a base xattr, a new xattr's value laid in the post-state, set-then-remove of a base xattr, and the missing-path refusal.

Hard link composition landed the same day (`Link`, composed like symlinks: an independent per-path state machine with `Unlink` routed to it, the target file named in the path table by the op's `src`). A new or retargeted link is one `Link`; a base link removed is one `Unlink`; link-then-unlink cancels; a base link removed then recreated to the same target is nothing. Whether the shared content survives an unlink is the merge's concern at apply time, not the composition's; a write through a hard link is refused as a kind conflict (owed). A link over an existing link, and a link sharing a path with a file/directory/symlink, are typed refusals (`LinkOverExisting`, `PathKindConflict`). The base grew a `hardlinks` list. **With this, the deriver composes every §4.16 declared operation kind** — content (overwrite/extend/truncate/insert/delete), create, unlink, rename, mkdir, rmdir, setmode, symlink, setxattr, removexattr, and link. Gated (`crates/merge/tests/increment.rs`, 41 tests, every host): create, link-then-unlink cancels, base-link removal, link-over-existing, and a link/file conflict.

The merge engine on one node landed the same day (`engine.rs`), composing the pure pieces into the submit pipeline (§4.16 "The verdict", "Splice", "Commit"). A `Green` holds the head content per file, the committed deltas per version (for position mapping), the last version each path changed at (the fast-path index), a `seen` set for idempotent retries, and the head. `submit(increment)` is idempotent by identity; for each changed file it takes the fast path when the path is unchanged since the increment's base (counted, with a non-vacuity test), else maps each edited range forward through the intervening deltas — a range disjoint from every intervening change is accepted and its edit re-applied at the shifted position, a range that meets one is a conflict unless the agent produced exactly the green's current bytes for that file (both made the same edit, which accepts). All files accepting commits a new version; any conflict returns byte-exact windows and changes nothing, and the agent rebases onto the head. Gated (`crates/merge/tests/engine.rs`, 9 tests, every host): a submit accepts and updates the green, disjoint files both accept, disjoint ranges of one file both accept (the range merge), an intervening insert shifts a later edit, overlapping edits conflict, an identical edit accepts, the fast path fires and is counted, a resubmit is idempotent, and a rebase after a conflict accepts. This is the laptop-degenerate engine. It now merges namespace changes too: a `PathChange` is a modify, a create, or a remove; create/create with different bytes conflicts (identical bytes accept), a modify of an intervening-deleted file or a remove of an intervening-modified file is a delete/modify conflict, and a remove of an already-gone file is a no-op — each conflict carries its `MergeConflictClass`. Six more tests: create/create conflict, identical create, delete-versus-modify, a remove accepts, removing an already-removed file, and remove of a modified file. The engine now merges the first namespace dimensions too, as independent per-path dimensions the way the deriver composes them: a directory creation (`Mkdir`) and a mode change (`SetMode`). A file and a directory at one path is a `TypeChanged` conflict (both directions: mkdir over a file, create over a directory, modify of a path now a directory); two agents making the same directory accept; two differing mode changes on one path are a `MetaMeta` conflict while an identical one accepts; a mode change on a deleted path is a delete/modify conflict; and a content edit and a mode change on one path are independent (they do not conflict), each tracked by its own last-changed index. Ten more tests (`crates/merge/tests/engine.rs`): mkdir creates a directory, two mkdirs accept, mkdir over a file conflicts, create over a directory conflicts, setmode sets a file's and a directory's mode, two differing setmodes conflict, identical setmodes accept, setmode on a deleted file conflicts, content and mode are independent, and a modify of a path now a directory conflicts. Symlink merges followed the same pattern: a symbolic link (`Symlink`) merges per path, an identical target accepting, two differing targets a `CreateCreate` conflict, and a file-versus-symlink or directory-versus-symlink at one path a `TypeChanged` conflict in both directions; four more tests (now 29 engine tests) — a symlink is created, identical symlinks accept, differing targets conflict, and a symlink and a file at one path conflict either way. File rename merges followed (`Rename { from }`, keyed at the destination and naming its source): the source's current content is captured at merge time (so an intervening edit to the source follows the move) and the source is removed; a source an intervening change renamed away or removed, or a destination an intervening change occupied, is a `RenameRename` conflict, and a destination that is a directory or symlink a `TypeChanged` conflict. Removes apply before sets in a commit, so a chained rename (a→b and b→c in one increment) rotates correctly without losing the moved content. Six more tests (now 35 engine tests): a rename moves a file, a rename carries an intervening edit, a rename of a moved source conflicts, a rename onto an occupied destination conflicts, a rename over a base file replaces it, and a chained rename is correct. Directory removal (`Rmdir`) merges next: the increment's cleared set (its removes, rmdirs and rename sources) is computed once, and a directory is removable when it is empty once that set is applied — so a directory whose children the same increment removes, rmdirs or renames away can be removed, while a live child it does not clear (including one an intervening change added) is a delete/modify conflict; a file or symlink at the path is a `TypeChanged` conflict, an absent directory a no-op. Six more tests (now 41 engine tests): rmdir removes an empty directory, a directory emptied in the same increment is removed, a non-empty directory conflicts, an rmdir of a file conflicts, an rmdir of an absent directory is a no-op, and an intervening child blocks the removal. The engine was then refactored (`6164ef9`) to consume the deriver's canonical ops document plus its sealed post-state, rather than one `PathChange` per path: `Increment { id, base, doc: OpsDoc, post_state }`, resolved into per-dimension groups by path and decided against the intervening history. That closed the remaining kinds. Hard link merges as a namespace edge (`Link`; identical accepts, differing is a `CreateCreate` conflict, file/link is `TypeChanged` — its content sharing is the volume's concern at apply time, §4.16, not the merge's). Xattrs merge per `(path, name)` (`SetXattr`/`RemoveXattr`; identical accepts, differing is `MetaMeta`, names are independent). A directory move needs no special case — it is the deriver's child renames plus mkdir and rmdir, all of which already merge. And several dimensions on one path in one increment (an edit and a chmod and an xattr) now merge together, which one change per path could not express. The green keeps a small per-path content history to reconstruct a base version's bytes for the identity check (the design's chain shares it copy-on-write — the measured optimization, owed). The engine tests were rewritten to build increments as ops documents through a small builder (the shape the deriver emits); 23 tests cover all prior content and namespace behaviors plus hard-link, xattr, several-dimensions-on-one-path, and a directory move as child ops. Per-range identity within a mixed file (the design's two-pass memcmp verdict; the engine's identity check is whole-file today), the fully general intra-increment coordination (a path both renamed away and recreated in one increment), the checkpoint folding of the canonical deltas, and the green chain and holder recomputation in a fleet remain owed.

Owed (the rest of Phase 6): the deriver's interaction edges (a file/directory/symlink/link transition at one path, a metadata or link then a rename of that path, symlink rename, a write through a hard link, and the rare reused-base-path rename combination above), each a typed refusal today; the engine now consumes the whole ops document (`6164ef9`), so every namespace kind merges through it — directory creation, mode, symlink, file rename, directory removal, hard link and xattr — and several dimensions on one path in one increment merge together; the fully general intra-increment coordination (a path both renamed away and recreated in one increment) remains; per-range identity in the engine's verdict; the checkpoint folding above; the fenced ledger-register commit and holder recomputation in a fleet (the single-node commit is an in-memory chain append; the fleet register, mirror and reconfiguration protocols are now simulated, §8h); these build on the
green-volume data model (§4.5's journal is in place; the fleet version chain is not yet).

## 8g. Phase 7 groundwork (2026-09-05)

The archive format — the pure core of D-17 and §2.6 of `research/compression-archive-dedup.md` — landed 2026-09-05 as `slates-archive` (`crates/archive`), ahead of the rest of Phase 7, because the container is a self-contained, in-RAM byte format that needs none of the codec, dedup or CDC work to be correct and directly confirmable, and slates never writes it to disk (R1). An archive is one streamable, content-addressed byte sequence: a fixed header (magic, major/minor, flags, page and chunk-size parameters, the manifest's BLAKE3, chunk count, raw and stored byte totals, volume and snapshot ids, the name-policy id and Unicode version), the chunk records in manifest order (each a BLAKE3 identity, raw and stored lengths, an encoding and level, a dictionary identity, and the payload), the manifest bytes, a seek table (a zstd skippable frame mapping each chunk's identity to its offset), and a trailer (the section index, the whole-archive BLAKE3, and the tail magic). `wire.rs` is a bounds-checked sequential reader/writer, so no byte offset is a literal and a short read is a typed refusal; `encode` is deterministic (the same snapshot yields the same bytes), and `decode` verifies the magic, the major, the whole-archive hash (catching truncation or any alteration), each chunk's identity, and the manifest's identity; `chunk_by_identity` locates one chunk through the seek table without scanning. Every malformed stream is a typed `ArchiveError` (bad magic, unsupported major, unknown required flag, truncated, bad trailer, archive-hash mismatch, chunk-identity mismatch, manifest-hash mismatch, bad seek table), never a panic.

Gated (`crates/archive/tests/archive.rs`, 11 tests, every host): an archive round-trips (T-7.1), encoding is deterministic, the seek table finds a chunk by identity (and reports an unknown one absent), a chunk that fails its identity is refused and named (AC-7.3), a bad magic and an unknown major are refused, a flipped body byte is caught by the whole-archive hash, a short stream is refused; and three hostile proptests (T-7.4): every truncation is refused, any single byte flip is caught (never the original snapshot decoded), and arbitrary bytes never panic. The LZ4 codec landed the same day (D-17's probe and hot-path codec, `lz4_flex`, pure Rust and safe-only so the unsafe budget stays 0): `compressed_chunk` compresses a chunk and keeps the LZ4 form only when it is smaller than the raw bytes (the format-derived floor), else stores raw; `content` and the reader decode an LZ4 chunk to its declared raw length and verify the decoded bytes against the chunk's identity, refusing an undecodable payload with a typed `BadPayload`. Two more tests (T-7.3): a repetitive blob is stored LZ4 and shrinks and round-trips, and a tiny unique blob stays raw. The zstd codec landed 2026-09-05 (D-17's ratio-bearing codec, `zstd`/`zstd-sys`, the C library built here through `cc` with the system clang): `zstd_chunk` compresses and keeps the zstd form only when it is smaller than the raw bytes (the format floor, level 0 = the library default; the calibrated per-chunk level and the LZ4-vs-zstd-vs-raw cost model are owed, they need the boot profile so R3 forbids fixing them here), `content`/`decode_payload` decode a zstd chunk to its declared raw length and verify the decoded bytes against the chunk's identity (`BadPayload` on an undecodable payload), and it uses only the safe `zstd::bulk` API so the archive unsafe budget stays 0. Because `zstd-sys` needs a C toolchain that the Windows/Linux cross-lint lane does not have, zstd is a **default feature**: native builds and tests use it, the `--no-default-features` cross gate compiles without it (a zstd chunk is then a typed refusal, the owed `ruzstd` decode-only fallback). Three more tests, feature-gated (T-7.3): a repetitive blob is stored zstd and shrinks and round-trips through the whole archive, a tiny blob stays raw, and zstd beats LZ4 on structured data (a non-vacuity ratio check). Owed: zstd static contexts carved from arenas and the calibrated cost model (the Btrfs sampler, the LZ4-to-zstd regression, per-volume observation), the `ruzstd` decode-only fallback for no-C-toolchain targets, dictionaries (FastCover training, identity, embedding, GC), the background identity pass and the per-shard hash-prefix partitioning and server wiring of the content index, FastCDC for the measured large-file class, and the export stream through the SDKs/MCP. LZ4 and BLAKE3 need no C toolchain; zstd needs one only where the feature is on.

The content-addressed store with deduplication landed the same day (`store.rs`), the pure core of Phase 7 task 1's content index. Chunks are keyed by their BLAKE3 identity, so identical chunks fold to one stored copy with a reference count; `unique_bytes` (the distinct chunks' stored bytes) shrinks as duplicates fold in while `referenced_bytes` (the undeduplicated total) is unchanged — the accounting the design's worked example names; `release` evicts a chunk on its last reference. Gated (`crates/archive/tests/store.rs`, 4 tests, every host): identical chunks deduplicate with exact accounting, the two-clones-share-an-output worked example, eviction on the last release, and a property test over random chunks with duplicates (byte-exact reads, exact accounting, a non-vacuity check that deduplication shrank the count). The per-shard hash-prefix partitioning, the background identity pass on idle time, and the server wiring are the runtime integration (owed).

The manifest tree landed the same day (`manifest.rs`), the archive's canonical, sorted, Merkle-hashed directory tree (§2.6 item 4). A directory node holds its entries (a name and a child) sorted by name; a file node holds its extent list (offset, length, chunk identity, chunk offset), a hole a zero-chunk extent. Each node's BLAKE3 identity is a Merkle hash over an encoding that names its children by identity, so the root identity fingerprints the whole tree and any change to any node changes it. Two canonical, deterministic encodings: the Merkle encoding (children by identity) defines the identity; the tree encoding (children inlined) is stored and parsed back through the bounds-checked reader, a truncated or malformed tree a typed `ManifestError`. Gated (`crates/archive/tests/manifest.rs`, 8 tests, every host): a tree round-trips, the identity and encoding are independent of entry order, changing a leaf changes the root, a file and a directory differ, a truncated tree and an unknown kind are refused, arbitrary bytes never panic, and generated trees round-trip with stable identities. The archive container now embeds the tree: its manifest section stores the tree's canonical encoding and the header's manifest hash is the tree's Merkle root, which `decode` recomputes and verifies (the archive's 13 tests use a `Node` manifest). Per-node metadata landed the same way: each directory `Entry` carries a `NodeMeta` (inode number, mode, modification and change times in nanoseconds, size, hard-link count, and an xattr-present flag — the field list of §2.6 item 4), written into both encodings and hashed into the Merkle identity, so a change to any entry's mode or times changes the root identity as a content change does; the archive carries, hashes and round-trips it, while applying it to a host path is the landing engine's job under a grant (§4.15). This bumped the format minor to 1 (a v1.0 and a v1.1 tree of the same shape have different roots). Two more tests (`crates/archive/tests/manifest.rs`, now 10): the metadata round-trips through the canonical encoding, and a mode change changes the root identity while identical metadata gives one identity. A golden vector pins the sample tree's Merkle root (`1a1634ff…`), so any change to the node encoding (including the metadata) is caught across versions, not only within a run. The root directory has no naming entry, so its own metadata is not carried; every named node's is (owed only for the root, a minor gap).

Restore landed the same day (`restore.rs`): reconstruct a volume's files from an archive by walking the manifest tree and resolving each file's extents against the chunks — a normal extent reads its bytes from the named chunk (decoded and identity-verified), a zero-chunk extent is a hole of zeros, a multi-extent file concatenates a sub-range of each chunk. An extent naming a chunk the archive does not hold is a typed `MissingChunk` refusal. Restore also surfaces each named node's metadata by path (`Restored.metadata`, the `NodeMeta` from each entry), so a granted landing can apply the mode and times; restore itself reconstructs only the in-memory tree. Gated (`crates/archive/tests/restore.rs`, 8 tests, every host): a file restores to its bytes, a hole restores as zeros, a multi-extent file concatenates its chunks, a tree restores files by path and records directories, a missing chunk is refused, restore works after an encode/decode round trip, an LZ4-compressed chunk restores correctly, and per-node metadata is surfaced by path and survives the stream. The eager whole-tree restore is here; lazy restore (attach after a metadata-only pass, decompress on first read, AC-7.4) is the runtime's job on top of it (owed).

Resumable transfer by the missing set landed the same day (`transfer.rs`; §2.6): the receiver reports the chunk identities it holds, `missing_set` returns the archive's chunks not in that set (sorted, so a resumed transfer computes the same set), and `chunks_for` packages exactly those — the basis of replication and clone-from-archive. Gated (`crates/archive/tests/transfer.rs`, 3 tests, every host): the missing set is everything with nothing held and empty with all held; a receiver holding a previous version's chunk receives only the changed one and combining held with shipped restores the whole archive; duplicate chunks ship once.

## 8h. Phase 8 groundwork (2026-09-05)

`crates/db/src/ledger.rs`, `mirror.rs` and `reconfig.rs` contain pure, direct-call simulations
of ledger adoption, prefix shipping and holder-set changes. The earlier record lists 10 ledger,
6 mirror and 7 reconfiguration tests; those counts were not rerun for A-9. The tests are useful
component evidence but do not establish a correct fleet or a refinement of the TLA models.

The audit at `a1059ed` found a committed-prefix counterexample in `Holder::reconcile`: an
identical record retained its older accepted epoch, allowing later adoption of a conflicting
intermediate-epoch proposal (BUG-12). Separate commit `d9cb6e5` fixes that refresh and reports
its before/after regression plus 40 passing DB tests. It also removes the forced candidate-zero
reachability restriction (BUG-13) and pins a shrunk seed. The direct check immediately after
takeover still compares adopted length rather than values; later committed-prefix comparison
exists. Message-level histories and complete adoption checks remain owed. No tests were rerun
in this documentation pass. `Owner::replicate` also needs its extension precondition enforced. Highest epoch
cannot be learned from unavailable holders' hidden state in a real protocol.

`Mirror::lag` is a count of records, not a duration; identity-only shipping does not close
`await placed(mirror)` for filesystem contents. Reconfiguration simulations cover their local
transition model; message delay, consumer/read leases, bytes, host-local capacity, state-transfer
publication and configuration consensus wiring remain open. These modules provide starting
points for the same N=1/fleet semantics, not proof that all integration work is transport only.

## 8i. A-9 contract correction and open implementation gaps (2026-09-05)

Separate workspace work advanced HEAD through archive commit `540fb5b` and ledger fix
`d9cb6e5` during this pass. BUG-12 is fixed there with recorded before/after regression evidence;
BUG-13's reachability restriction is removed, but direct adoption-value/message-level checks
remain owed. This docs pass inspected those changes and the commit's reported tests without
rerunning them. Source findings retain their explicit `a1059ed` baseline.

All rows below are **open**. Their design is now specified in A-9; acceptance requires the
named behavior tests and relevant original phase gates. Source findings BUG-1–BUG-14 are in
[the audit](../bugs/2026-09-05-system-contract-audit.md). Hecate applicability and deliberate
departures are in [the contract review](research/hecate-contract-review.md). No new measurement
or executable regression was performed by this docs change.

| Id | Gap and source finding | Design contract | Closure gate / owning phase |
|---|---|---|---|
| GAP-A9-1 | Locked flag without locked store, mapped-vs-usable capacity, dynamic allocations competing with bounded claims (BUG-1–3). Uncharged metadata/transient/retained bytes can defeat the cap. | §4.2: atomic all-cost per-host admission; disjoint shard credits; protect outstanding entitlement through resize, pressure and recovery. | AC-0.10/T-0.10; AC-2.11/T-2.13; Phases 0/2. |
| GAP-A9-2 | Live bases and immutable complete snapshots conflated; remote delta-only clone drops untouched state; base capture not implemented. | §4.4/§4.15/§4.10: retained BaseRef and explicit coverage; stable-source requirement for complete atomic capture. | AC-1.16/T-1.20; AC-8.21/T-8.19; Phases 1/8. |
| GAP-A9-3 | Base lookup depends on listing, FUSE flag mismatch, undispatched advertised operations, ignored metadata/rename flags, false statfs (BUG-5–10); coherence delivery and metadata copy-up need sweep. | §4.5–§4.6: complete shared operations, independent ABI checks, truthful capacity, real invalidations and mounted conformance. | AC-1.17/T-1.21; AC-3.10/T-3.13; Phases 1/3. |
| GAP-A9-4 | Attachment record is not a mounted path; dirty client caches lack a seal barrier; handles grow and helper error exits can orphan children (BUG-4/14). | §4.4/§4.6: authenticated binding, ready path/device, barriers, generations, bounded drain/revoke/reuse. | AC-3.11–3.12/T-3.14–3.15; Phase 3. |
| GAP-A9-5 | No virtio-fs device or demonstrated OCI/VMM integration; native macOS/Windows remain unimplemented. | §4.6: owned FUSE-over-virtio/custom-runtime seam, host and guest containers, device admission, immutable-only isolated DAX if offered. | AC-4.11–4.12/T-4.13–4.14; Phase 4. |
| GAP-A9-6 | Restart reconstructs empty scratch contents and loses local snapshots (BUG-11). Metadata completion tests cannot establish filesystem recovery. | §2.6/§4.8/D-18: recover bytes, roots, witnesses, source identity, rights and capacity with effect/completion publication. | AC-2.12/T-2.14; Phase 2. |
| GAP-A9-7 | BUG-12 acceptance-epoch fix and BUG-13 reachability correction landed separately in `d9cb6e5`; complete adoption-value/extension checks, message faults, read authority and real configuration core remain unestablished. | §4.8: accepted/proposed/promise separation, exact historical prefix, distinct quorums, safe leases and bounded epochs; configuration only on cold changes. | AC-8.18/T-8.16; AC-8.20/T-8.18; Phase 8. Model refinement/revalidation also owed, not run. |
| GAP-A9-8 | Identity-only replication, mirror lag in records, no byte-complete holder admission, repair or cross-region service. | §4.10/§4.16: verified placed reference graph, real holder capacity, atomic generations, recomputation and time lag. | AC-8.19/T-8.17; original fleet gates; Phase 8. |
| GAP-A9-9 | Same-uid agents share ambient authority; channel class is not evidence of a human approval. | §4.13: trusted consumer enrollment, scoped rights before effects, protected grant issuer and manifest-bound approval; private content sharing scopes. | AC-2.13/T-2.15; AC-5.10/T-5.12; Phases 2/5. |
| GAP-A9-10 | No MCP/SDKs; CLI ids/manual root setup, partial help/output and no grant issuance; schema parity and actual path readiness incomplete. | §4.12: one operation descriptor; scoped names, consistent JSON/help/errors/cursors; discoverable native/guest flows and exact capability status. | AC-5.9–5.11/T-5.11–5.13; Phase 5. |
| GAP-A9-11 | Separate frame classes do not bound CPU, device, arena or full-object transfer costs; resumable helper is not bounded end-to-end ingest. | §4.2/§4.9: all-resource QoS, bounded quanta/credits, verified named and unknown-length transfer, cancellation and release. | AC-0.10/T-0.10; AC-7.7/T-7.8; Phases 0/7/8. |
| GAP-A9-12 | Signal names/absence and request-vs-trace causation not enforced end to end; nine spans were called seven. | §4.14: closed registry, absence/freshness semantics, distinct identities and telemetry loss markers. | AC-0.11/T-0.11; Phase 0 foundation with surface/fleet integration. |
| GAP-A9-13 | Clean-file digest export and bounded cache discovery not implemented; stale source knowledge must not imply clean content. | §4.15: verified current digest only; invalidate before mutation; watcher hints backed by revalidation. | AC-1.17/T-1.21; Phase 1 core and bridge integration. |
| GAP-A9-14 | Pure merge core lacks service-level Work/Green roles, CLI/MCP flow, pinned attachments and distributed recomputation. | §4.16: complete immutable green base, writeback barrier, declared operations only, input retention and placed-before-reference. | AC-6.13/T-6.15; AC-8.19/T-8.17; Phases 6/8. |
| GAP-A9-15 | Native/guest POSIX, strict RAM residency and grant-only disk effects lack complete transport-specific evidence. Historical stages overstate coverage. | §4.6, Part 6 and EQUIVALENCE.md: explicit semantics/boundaries; report skipped lanes and limited adapters honestly. | AC-9.7/T-9.1 plus original conformance/workload gates; Phase 9, prerequisite gates run in their owning phase. |

**Additional decisions/evidence owed.** Phase 4 must establish the supported VMM/device seam,
immutable mapping capability and guest cache residency for each host; no guessed support
matrix. Phase 1/4 must establish which read-only source facilities can produce complete atomic
bases without a write or privilege, otherwise refuse that request. Phase 2/5 must establish
the protected enrollment/confirmation channel with the harness; a uid-only demonstration
cannot close it. Derived headroom/retention bounds and provisioning costs need new measured
records; the formulas in the design are contracts, not measurements.

**Prohibited-path review.** Old research recommendations of parallel pure-TS SDK fallbacks,
FSKit compatibility shims or automatic degraded bridge substitution are not authorization to
implement them. Remove such runtime paths if found; requested semantics must be met or refused.
A limited NFS adapter cannot satisfy the full POSIX contract by expanding an expected-failure
list. No subagent, external runtime, checker installation, privilege or disk write is approved
by this ledger.

## 8j. Phase 4 groundwork — the NFS loopback bridge wire codec (2026-09-05)

The macOS NFSv3 loopback bridge (§4.6, D-2 — the FSKit fallback for macOS 14.4/older and where FSKit is disabled, and the differential oracle against the other bridges) began 2026-09-05 as `slates-bridge-nfs` (`crates/bridge-nfs`), the wire codec first, because it is pure and directly confirmable on every host with no socket and no mount, exactly as the FUSE ABI codec was (§8e). Two modules. `xdr` (External Data Representation, RFC 4506) is a bounds-checked sequential reader and writer — big-endian, four-byte aligned; a variable opaque or string is a length then the bytes then the padding; a declared length is checked against a caller-supplied cap and the bytes that remain before any allocation, so a hostile length is a typed `XdrError`, never an allocation or a panic. `rpc` (ONC RPC, RFC 1057/5531) is record marking over TCP (a four-byte per-fragment header, the last-fragment flag plus the length), the call and reply messages, and the `AUTH_NONE` credential the loopback server sends (it trusts the peer of a socket only it created and the kernel connects); `read_record` distinguishes a partial stream (`Incomplete` — wait for more bytes) from a malformed one and refuses a fragment past the message cap (`RecordTooLarge`) before accumulating, `parse_call` reads the call header (program, version, procedure; the credentials validated for shape and skipped) and positions a reader at the arguments, and `reply_bytes` builds an accepted reply. Every wire number is a `size_of` or a named `Format` constant (the RFC message-type and accept-status values, the record-marking flag and length mask), so the literal check stays clean; the unsafe budget is 0.

Gated (`crates/bridge-nfs/tests/wire.rs`, 11 tests, every host): XDR scalars round-trip; a variable opaque is length-prefixed and padded and the reader skips the padding; a string round-trips and non-UTF-8 is refused; a hostile length and a truncated buffer are refused before allocating; a record frames and deframes with the right consumed count; a partial stream is `Incomplete`; an oversized record is refused; a NFSv3 NULL call header parses to its program, version and procedure; a non-call is refused and garbage does not panic; and a successful reply and a program-mismatch reply build to the exact bytes the kernel expects (golden vectors).

The NFSv3 core data types followed (`nfs.rs`, RFC 1813): the status codes (`Nfsstat3`), file types (`Ftype3`), timestamps (`Nfstime3`), the fixed 84-byte attribute structure (`Fattr3`), optional post-operation attributes (`PostOpAttr`), and the file handle (`Nfsfh3`, opaque capped at the 64-byte `NFS3_FHSIZE`), each with XDR encode and decode and every wire value a `#[repr(u32)]` discriminant or a named `Format` constant. Gated (`crates/bridge-nfs/tests/nfs.rs`, 5 tests, every host): `fattr3` and `post_op_attr` round-trip, a file handle round-trips and one past the cap is refused, an unknown file type is refused, and the status and time wire values are golden.

The file-handle codec followed (`handle.rs`), the design's `(volume, inode no, gen)` identity encoded into the opaque `nfs_fh3`: a version byte then the 16-byte `VolumeId`, the inode number and the generation big-endian, 33 bytes, within the 64-byte `NFS3_FHSIZE`. NFS is stateless, so the handle names its object with no server-side table across daemon restarts; the generation makes a reused inode a distinct handle (a stale handle is refused, not answered from whatever now holds the number), and the version byte refuses a handle from an incompatible build rather than misreading it — the discipline `crates/db/src/catalog.rs` applies to Wire types and the Linux in-kernel NFS server applies to its handles (evidence C). Gated (`crates/bridge-nfs/tests/handle.rs`, 5 tests, every host): a handle round-trips within the size cap, its encoding is golden, a reused inode with a new generation is a different handle, and a wrong length or an unknown version is a typed refusal. 21 bridge-nfs tests total.

The MOUNT protocol (`mount.rs`, RFC 1813 Appendix I) and the minimal portmap responder (`portmap.rs`, RFC 1833) followed — the two helper RPC programs the server answers alongside NFS. MOUNT: the `mountstat3` status codes, the `MNT` request path (capped at `MNTPATHLEN` = 1024), and the `mountres3` reply as a `MountReply` enum (correct by construction — a success carries the root file handle and the accepted auth flavors, a failure only its status). Portmap: the `mapping` argument and the bare-port GETPORT reply; slates passes explicit `port`/`mountport` options so a client need not query it, but the responder exists for those that do. Gated (`crates/bridge-nfs/tests/mount.rs`, 4 tests, every host): a successful MNT reply carries the handle and flavors, a failure is only its status, a mount path round-trips and one past the cap is refused, and a portmap mapping round-trips with the bare-port reply. 25 bridge-nfs tests total; the crate's pure wire surface (XDR, ONC RPC, NFSv3 types, file handle, MOUNT, portmap) is complete and confirmable on every host with no socket.

The NFSv3 procedures began over the shared operation layer (`procedures.rs`, §4.6, Phase 4 task 3), the NFS analogue of the FUSE `dispatch`: an `Export` holds a `&mut dyn Bridge` (the `slates-bridge-core` seam the FUSE mount also uses) and a `VolumeId`, decodes a file handle to an inode (a foreign-volume handle is `NFS3ERR_STALE`, a malformed one `NFS3ERR_BADHANDLE`), calls the same `Bridge`, and mints handles for the objects it returns — stateless, no server-side open table. This slice serves the metadata walk a client does first: MOUNT `MNT` (the export's root handle from `Bridge::root`), `NULL`, `GETATTR` and `LOOKUP`, with the neutral translations `NodeAttr` → `fattr3` and `VfsError` → `nfsstat3` (the same neutral error the FUSE edge maps to an errno). Gated (`crates/bridge-nfs/tests/procedures.rs`, 3 tests, every host, no socket): MNT → LOOKUP → GETATTR walks a scratch volume through a real `VolumeBridge` and GETATTR names the same inode LOOKUP returned; a foreign or malformed handle is a typed status; a missing name is NOENT. 28 bridge-nfs tests. A design fork is owed on READ and WRITE: §4.6 keys them by inode ("requests carry (volume, inode no, gen)") but the current `Bridge` keys them by an open handle, so serving stateless NFS read/write wants either an inode-keyed read/write on the shared seam or an NFS-side inode→handle cache with GC. The post-mount query procedures followed — ACCESS (grants the requested access; slates is not a sandbox, a non-goal), FSSTAT (the volume's space from the seam's `statfs`) and FSINFO (static transfer sizes matched to the arena chunk, the maximum file size, one-nanosecond time granularity, and the link/symlink/homogeneous/cansettime capabilities); 29 bridge-nfs tests. The rest of the namespace and directory procedures (READLINK, CREATE, MKDIR, REMOVE, RMDIR, RENAME, SYMLINK, READDIR/READDIRPLUS, SETATTR, PATHCONF, COMMIT) and the RPC record/program routing over the socket follow.

Owed (the rest of Phase 4): the TCP loopback listener held by the anchor; the NFSv3, MOUNT and portmap procedures over the volume core (LOOKUP, GETATTR, ACCESS, READ, WRITE, READDIR/READDIRPLUS, CREATE, MKDIR, REMOVE, RMDIR, RENAME, SYMLINK, and the file handles that name volume inodes); the root mount at an existing user-owned mount point via `mount_nfs` (non-root, no kernel extension, no privilege — R10); the attribute-cache timeout derived from the loopback round trip; and the differential-oracle harness comparing this server against the FUSE bridge over the same volume. FSKit remains the primary macOS 26+ path (§4.6, D-2); this NFS server is the fallback and the oracle, and its wire codec is reusable whichever native bridge is built next. The real mount runs in the macOS lane.

The socket and the server followed, closing most of that owed list. First the blocking `serve_connection` (`src/server.rs`) — the transport loop behind the codec: it reads ONC RPC records off any `Read + Write` stream, dispatches portmap/MOUNT/NFSv3 onto an `Export`, and writes framed replies; `tests/loopback.rs` mounts `/` and reads a seeded file back byte-for-byte over a real socket in CI, and the `nfs_loopback` example serves a real `mount_nfs` (a live kernel mount of a RAM-only volume — no signing, no kernel extension, no privilege beyond the mount, R10). Then the **production async server** (2026-09-09): `serve_connection_async` serves a connection over slates's own runtime, reads and writes awaiting the shard's driver through the rt's new async `TcpStream` (§4.3 — `Driver::register_writable`, the `EVFILT_WRITE`/`EPOLLOUT` sibling of `register_readable`, and `tcp::{TcpListener, TcpStream}`; `TcpStream::write_all` awaits write-readiness so a stalled client yields the shard rather than blocking it), sharing the RPC engine (`dispatch`) and record codec with the blocking form — one engine, two transport adapters, not a second path. A volume is `!Send` (it holds a `Box<dyn Clock>`), so the serve loop reaches its shard by the daemon's own idiom (a `Send` boot task through `spawn_on`, then `futures::spawn` for the non-`Send` loop). Proven by use in CI with no privilege: `tests/async_loopback.rs` mounts and reads a seeded file back byte-for-byte from the async server on the runtime, driven by the same hand-rolled ONC RPC client as the blocking test; the `nfs_async` example serves a real `mount_nfs`.

Multi-volume routing followed (2026-09-09), the first step toward the design's single-root-mount model (§4.6 line 128, "the single kernel mount point per host under which volumes appear as directories"): the server was one volume at `/`, and `Export::mnt` itself named "the path-to-volume resolution of a multi-volume export is owed". `MultiExport` (`src/multi.rs`) now serves many volumes from one server, routing each request to the volume its file handle names. It needs no table: every served NFSv3 procedure begins with a file handle, and the handle already encodes `(volume, inode, gen)` (`handle.rs`), so the router reads the leading handle's volume id through a fresh reader (a new `XdrReader::rest`) and hands the *untouched* request to that volume's `Export`, which re-decodes and validates the handle exactly as for a single volume; a handle for a volume the server does not hold is `NFS3ERR_STALE`. The seam is `NfsService` (`serve_mount`/`serve_procedure`), implemented for both `Export` and `MultiExport`, and `dispatch`/`serve_connection`/`serve_connection_async` now work over `&mut dyn NfsService` — so one server serves one volume or many with no transport change, and the existing single-volume callers coerce unchanged (all prior tests green). Proven by `tests/multi.rs`: one server, two independent volumes, a client mounts each by name over one connection and reads its file, and the bytes never cross (the routing proof). The **synthetic root directory** followed, completing the design's single-root-mount model at the NFS layer: `MultiExport` now serves a read-only root whose entries are the volumes, so one `MNT /` lets a client `ls` the volumes and `cd` into any. `MNT /` returns the root handle; `GETATTR`/`ACCESS` describe it; `READDIR`/`READDIRPLUS` list the volume names (budgeted against the client's `count`, `NFS3ERR_TOOSMALL` if it cannot hold one entry); `LOOKUP` a volume name returns that volume's own root handle and attributes (via a new `Export::root_object`), so descending into it crosses into the volume (a distinct `fsid`, as at any mount point); `FSINFO`/`FSSTAT` answer for the pseudo-filesystem; and every mutation, `READ` and `READLINK` on the root is a typed refusal in the failing procedure's own reply shape (`NFS3ERR_ROFS`/`ISDIR`/`INVAL`), so the stream never desynchronises. Proven by `tests/multi.rs`'s browse test over a real socket: `mount /` → READDIRPLUS lists `alpha`+`beta` → LOOKUP `alpha` → LOOKUP `hello.txt` inside it → READ, byte-for-byte.

The serving core was then reshaped to the **daemon's storage model** (2026-09-09): the first cut held an `Export` per volume, each owning its own store — but a shard holds many volumes sharing *one* store, so a volume must be served through a *transient* `VolumeBridge` built per request (the design's "marshal each operation into the bridge queue of the owning shard", §4.6 line 1341; the shape `bridge-fskit`'s `MountSession` already takes). A `VolumeSet` trait is that seam — `entries`, `serve(volume, …)`, `root_object(volume, …)` — and `MultiExport<V: VolumeSet>` carries the routing and the synthetic root above it. The daemon will implement `VolumeSet` over its `ShardState` (one store, a volume slab); a test implements `OwnedVolumeSet` (one store, several volumes), so the shared-store path the daemon uses is what the tests now drive — `tests/multi.rs` puts two volumes in one store (distinct inode prefixes, as a shard assigns) and both the routing and the browse test pass unchanged. The daemon-side wiring followed, and the daemon now serves NFS over its own provisioned volumes: `crates/server/src/nfs.rs` binds a loopback listener at boot (port on `Daemon::nfs_port`), serves it on the control shard, and `ShardVolumeSet` implements `VolumeSet` over the shard's `ShardState` — a request resolves its volume through `state::with_state` and is served by a transient `VolumeBridge::attached` (the daemon's serve path: it lends the volume slot's base host and a fresh handle slab, since NFS keeps no open state across requests). Each connection is a detached task, so connections are concurrent. Proven with no privilege by `crates/server/tests/nfs_mount.rs`: a single-shard daemon starts, a client provisions a volume through the real rendezvous, then over the daemon's NFS port a client mounts it, **creates a file, writes bytes, and reads them back** — client → NFS → `ShardVolumeSet` → the shard's real volume and back, byte-for-byte. The **cross-shard bridge queue** followed (§4.3, D-7 "bridge queues pinned to the owner"), so the daemon serves volumes on *any* shard, not just the accepting one: a request naming a volume this shard does not own is routed by the volume's owner partition (`verbs::owner_of` mapped to a shard) to run the same `serve_call` on the owner shard — spawned there exactly as the client path forwards a verb (`Control::Spawn`) — and the owner spawns a task back on the accepting shard that hands the reply to the awaiting connection task through a per-shard, thread-local pending map (no new runtime primitive, no lock; a lost-wake-safe slot on a single-threaded shard). `tests/nfs_mount.rs` proves it: a two-shard daemon mounts a volume that lives on a shard other than the NFS listener's and writes then reads a file back byte-for-byte over the bridge queue. And `cd`-ing into a volume from the single host root spans shards: a root `LOOKUP` routes by the looked-up name (a volume id in hex), so `mount /` then `cd <id>` reaches a volume on any shard (`nfs_mount.rs` `a_client_mounts_the_host_root_and_reaches_a_remote_volume_by_id`). And the host root's *listing* now gathers every shard's volumes: a root `READDIR`/`READDIRPLUS` scatters an entry-gather to each other shard and lists them all, so `mount /` then `ls /` shows every volume on the host (`nfs_mount.rs` `the_host_root_listing_gathers_volumes_from_every_shard`; a remote volume's per-entry attributes are absent in READDIRPLUS, filled by a cross-shard `LOOKUP`). **The whole browse — `mount /`, `ls /`, `cd <id>`, read/write — spans shards.** And each request runs as the mounting user: the daemon reads the uid from the call's `AUTH_SYS` credential (`subject_of` over bridge-nfs's `auth_sys_uid`; `AUTH_NONE` → root, §4.13), and the subject rides to the owner shard on a cross-shard call (`bridge-nfs/tests/auth.rs` covers the parse). Now owed on this path (minor refinements): a friendly chosen-path mount name (the id's hex is used now, §4.6 "Chosen path"); the anchor-held listener for restart survival (§4.6, line 509; the daemon binds it now); the attribute-cache timeout from the measured loopback RTT; and the differential-oracle harness against the FUSE bridge over the same volume (the root-listing gather now fans out to the shards in parallel).

## 9. Blocking order toward first light

1. Correct capacity/residency admission and acknowledged-content recovery (GAP-A9-1/6).
2. Complete base routing, FUSE semantics, barriers and owned attachment teardown (2/3/4).
3. Establish trusted consumer/grant boundaries and a usable local CLI flow (9/10). First light
   means a real tool at an actual attached path with correct bytes and isolation, not an
   attachment row. It does not by itself certify full POSIX or fleet durability.
4. Add the virtio-fs/VMM and OCI attachment forms over that same core (5), then the native
   platform-specific gates. Complete Work/Green and MCP/SDK flows with their phase prerequisites.
5. Retain the fixed takeover regression and complete the protocol oracle before fleet wiring (7), then
   verify byte placement, capacity, mirroring and remote bases (8), with bounded transfer/QoS.
6. Close transport-specific conformance, workload, fault, residency and release evidence (15).

These are dependencies for implementation, not work authorized by the documentation request.
Every phase retains its original acceptance gates plus the A-9 additions.

## 10. Model-checking record (A-6)

| Model | Configuration | Result | States | Depth | Date |
|---|---|---|---|---|---|
| `models/Reconfig.tla` | Old {a1,a2,a3} → New {a2,a3,a4}, three records | no error (ReadSafety, NoLoss, TypeOK) | 20,478 distinct | 19 | 2026-09-04 |
| `models/FencedRegister.tla` | 3 holders, 2 hosts, 2 epochs, 2 records per epoch | no error (TotalOrder, Continuity, StaleNeverCommits, ReadSafety, TypeOK) | 1,432,929 distinct | 27 | 2026-09-04 |
| `models/FencedRegister.tla` | 3 holders, 2 hosts, 3 epochs, 2 records per epoch | not completed: stopped after 3 h 7 min with 83 GB of queued states on disk; needs symmetry reduction (TLC symmetry sets over Acceptors and Hosts) and a bounded record alphabet before it is feasible; the two-epoch result stands (one takeover plus a resumed stale owner) | — | — | 2026-09-04 |

The models are architecture artifacts, not CI jobs. These historical runs apply only to the
listed models and configurations. The Rust simulations are evidence with known gaps, not a
proof or established refinement (BUG-12/13). A-9 corrects §4.8; refinement/revalidation remains
owed before GAP-A9-7 closes. Running a checker requires separate explicit tooling authorization;
none ran for A-9. Any authorized rerun must bound work and keep state outside the project tree: TLC's disk-backed state queue otherwise lands in `docs/wip/models/states`, which
is what happened on 2026-09-04 (83 GB, removed).

Two modelling bugs were found and fixed before the runs passed: the first draft let an owner issue two
different records for one sequence number (a model error, not a design error), and its Fencing
invariant was stronger than Paxos promises (a record partially acknowledged before a promotion may
still complete; the correct property is Continuity: the successor's base is at least as new as any
such record). Both are recorded so the implementer knows exactly what the models guarantee.
