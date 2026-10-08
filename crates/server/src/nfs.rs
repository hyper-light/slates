//! The daemon's NFS transport (§4.6): one loopback listener whose connections serve the daemon's own
//! volumes over the shared operation layer, so a `mount_nfs localhost:PORT` reaches every volume this
//! daemon provisioned — the signing-free macOS mount path (D-O9), run against real state.
//!
//! A volume lives on its owning shard (`§4.8`: ids route to owners; a volume id names its owner
//! *partition*, [`crate::verbs::owner_of`], which the daemon maps to a shard). The connection is
//! accepted on the control shard. At its first NFSv3 call naming a volume another shard owns, the
//! **connection moves** to that owner shard (its descriptor and read state, by move; [`migrate`]) and is
//! served locally there from then on: a mount's calls name one volume, and a call forwarded per request
//! paid two cross-shard hops and two thread wakes, each of which a busy machine can delay (2026-10-04:
//! service p99 115 µs → 29 µs at rest, 1.18 ms → about 50 µs with a spinner per core;
//! docs/wip/BENCHMARKS.md "The VFS on a busy machine"). A call that still names another shard's volume
//! — a `MNT` routed by name, an NFSv4 compound (its session state is per shard), a host-root listing —
//! is routed over the **cross-shard bridge queue** to its owner and the reply routed back:
//!
//! * **Local** (the volume this shard owns, or the synthetic root): the request runs through the
//!   bridge's `serve_call` on a [`MultiExport`] over [`ShardVolumeSet`], which resolves the volume
//!   through [`state::with_state`] and serves it with a transient [`VolumeBridge::attached`] (the
//!   daemon's serve path — it lends the volume slot's base host and a fresh per-request handle slab,
//!   since NFS keeps no open state across requests).
//! * **Remote** (a volume another shard owns): the request runs the same `serve_call` on the *owner*
//!   shard, spawned there the way the client path forwards a verb (`Control::Spawn`, §4.3), and the
//!   owner spawns a task back on this shard that hands the reply to the awaiting connection task
//!   through a per-shard pending map. No new runtime primitive — the cross-shard spawn is the one the
//!   daemon already uses; the pending map is thread-local, so it needs no lock.
//!
//! The whole browse spans shards: `mount /<name>@<capability>` (or `/@<capability>` for the host root
//! scoped to that capability), `ls /` (a `READDIR` of the host root scatters an entry-gather to every
//! shard and lists the volume the capability authorizes by its friendly name), `cd <name>` (a root
//! `LOOKUP` routes across shards by `owner_of_name`), read/write. Each request runs as the mounting user
//! ([`subject_of`] reads the uid from the `AUTH_SYS` credential, §4.13; `AUTH_NONE` falls back to root)
//! for the POSIX permission rules — but **authorization is the mount capability, never the uid**
//! (§4.13; AUD-01): a supplied uid and loopback reachability identify no consumer, so every volume is
//! served only through the capability `attach` returned — `<attachment_hex>.<token_hex>` in the mount
//! path, stamped into the root handle and every handle derived from it, and validated against the
//! attachment record on every request. A bare `/` lists nothing and enters nothing. The attachment a
//! host mount rides on is the **mount's** (`Consumer::Bridge`): it outlives the client that attached
//! and a daemon restart (the kernel's handles must keep validating), and it ends with the kernel's
//! `UMNT` of the mount path ([`unmount_capability`]), a `detach`, or the volume's destroy.
//!
//! A volume appears under its **provisioned name** (§4.6 "Chosen path"): the slot carries the name
//! ([`crate::state::VolumeSlot`]), [`ShardVolumeSet::entries`] lists it, and a root `LOOKUP`/`MNT` of a
//! name routes to the name's owning partition ([`route_by_name`] over `owner_of_name` — the same
//! partition the create routed to and the id encodes, so a name reaches its volume with no global
//! index, D-14) where the owner shard resolves it against its own slots (`MultiExport` matches the
//! name in `entries`). The root-listing gather fans out to the shards in parallel. The **listener
//! survives a daemon restart** (§4.6): a supervising anchor holds it and hands its descriptor over in
//! the environment, and the daemon adopts it (`crate::daemon`'s `nfs_listener` over
//! `slates_rt::tcp::TcpListener::from_fd` and `slates_anchor::ENV_NFS_LISTENER`) rather than binding a
//! fresh ephemeral one, so the loopback
//! port is stable across restarts; a standalone daemon (tests) still binds its own. **Every connection survives it
//! too (A-113):** the anchor holds a duplicate of each from its accept, a request is consumed only once its reply is in
//! the send queue, and replies leave in whole records, so a successor answers what the dead daemon left on the same
//! socket and the kernel client never reconnects (on macOS its reconnect under repeated kills panicked the kernel,
//! docs/bugs/2026-10-06-macos-nfs-client-panics-when-its-server-restarts.md; Linux does not hold connections yet). Owed here (one
//! minor, situational refinement): the attribute-cache timeout from the measured loopback GETATTR RTT.
//!
//! **The data-plane barrier (§4.8, D-18).** A mutation served here changes the shard's volumes
//! without a control verb, so nothing else republishes the shard's recovery image; every mutating
//! procedure that succeeded therefore runs [`crate::verbs::publish_shard`] on the owner shard
//! *before* its reply is sent ([`barrier`]), so the reply's stability claim — a `FILE_SYNC` write, a
//! COMMIT, a create or a rename the protocol defines as stable — is true for daemon-restart survival.
//! The one exception is an `UNSTABLE` write, answered `UNSTABLE` and made stable by the client's
//! later COMMIT; the write verifier ([`crate::state::ShardState::write_verifier`], per boot) is how a
//! client learns a restart lost the unstable writes it still holds and re-sends them (RFC 1813
//! §3.3.7). A refused publish replaces the reply with `NFS3ERR_IO` — the effect is in the volume but
//! not stable, and the client is told so rather than promised survival.
//!
//! **NFSv4.1/4.2 (A-35).** Version 4 of the NFS program is served on the same listener: the v4 state
//! (clients, sessions, opens) lives on the listener's shard, created with the first v4 call under the
//! bounds `config::nfs_v4_caps` derives, and each operation of a `COMPOUND` becomes an NFSv3 call that
//! [`serve_v3`] routes to the volume's owner shard as it routes any v3 call — authorization, the
//! barrier and the write verifier included — so the two versions share one semantics.
//!
//! The §4.6 differential oracle (line 1368) is *not* owed here: it
//! mounts the same volume via FSKit *and* via NFS and compares the abstract states — two real
//! kernel mounts — so it is gated on the FSKit mount, hence on the Apple Developer entitlement that
//! item (1) needs and this sandbox cannot hold. A synthetic FUSE-dispatch-vs-NFS-dispatch stand-in
//! would not be it: both legs dispatch onto one `VolumeBridge`, so their agreement is tautological
//! (R5: no vacuous oracle).

use slates_bridge_core::{Rights, VolumeBridge, new_handle_store};
use slates_bridge_nfs::mount::{MOUNT_PROGRAM, MOUNTPROC3_MNT, MOUNTPROC3_UMNT};
use slates_bridge_nfs::nfs::{Fattr3, Nfsfh3};
use slates_bridge_nfs::procedures::{
  Dialect, Export, NFS_MAXNAMELEN, NFS_PROGRAM, NFS_VERSION, NFSPROC3_COMMIT, NFSPROC3_CREATE,
  NFSPROC3_LINK, NFSPROC3_LOOKUP, NFSPROC3_MKDIR, NFSPROC3_MKNOD, NFSPROC3_READDIR,
  NFSPROC3_READDIRPLUS, NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR, NFSPROC3_SETATTR,
  NFSPROC3_SYMLINK, NFSPROC3_WRITE, io_failure_reply, is_unstable, status_failure_reply,
  write_stable_how,
};
use slates_bridge_nfs::rpc::{MAX_MESSAGE, RecordReader};
use slates_bridge_nfs::v4::compound::{self, Server as V4Server};
use slates_bridge_nfs::v4::session::Limits;
use slates_bridge_nfs::v4::session::{CallbackState, DepartedSession};
use slates_bridge_nfs::v4::types::ChannelAttrs;
use slates_bridge_nfs::v4::types::SessionId;
use slates_bridge_nfs::v4::{NFS_V4, NFSPROC4_COMPOUND, NFSPROC4_NULL, Nfsstat4};
use slates_bridge_nfs::xdr::{XdrReader, XdrWriter};
use slates_bridge_nfs::{
  AcceptStatus, MultiExport, Nfsstat3, UnixGroups, VolumeSet, auth_sys_identity, parse_call,
  reply_bytes, request_volume, root_volume, serve_call, write_record,
};
use slates_db::catalog::{AttachForm, Consumer, Principal, VolumeId};
use slates_db::register::ObjectId;
use slates_rt::tcp::{TcpListener, TcpStream};
use slates_rt::{futures, registry};
use slates_vfs::clock::Clock;
use slates_vfs::host::HostFs;
use slates_wire::observe::Chokepoint;
use slates_wire::request::RequestId;

use crate::state::{self, ShardState};
use crate::verbs::{owner_of, owner_of_name};

/// Shape: bytes read from a connection per `read` when more of an RPC record is needed (see the
/// blocking server in bridge-nfs for the reasoning): one large transfer fits, the assembler stitches
/// any split, so this bounds syscalls per record, not correctness.
pub(crate) const RECORD_CHUNK: usize = 1 << 16;
/// Format: the largest mount path the router reads before deciding a route (RFC 1813 `MNTPATHLEN`).
const MNT_PATH_MAX: usize = 1024;
/// Format: the radix of the hexadecimal digits a mount capability is written in (`<attachment_hex>` and
/// the 32-digit `<token_hex>` of a capability mount path).
const HEX_RADIX: u32 = 16;

// ---------------------------------------------------------------------------- the shard's volumes

/// A mount capability as a request presents it (§4.13; AUD-01): the attachment id and the 16-byte token
/// `attach` returned to the authorized consumer — in the `MNT` path for the mount itself, and in the file
/// handle of every later request (the root handle a `MNT` returns and every handle derived from it carry
/// it). The owner shard validates it against the attachment record on every request, so a handle
/// self-authorizes with no per-connection state: the loopback edge's substitute for peer-credential
/// identity, since a supplied `AUTH_SYS` uid and loopback reachability are not consumer authority.
type MountCapability = (u64, [u8; 16]);

/// The shard's volumes as an NFS [`VolumeSet`]: resolved through [`state::with_state`], each served by
/// a transient bridge over the shard's store and the routed volume's slot. The state it reads is the
/// current shard's, so it stays thread-local. On the owner shard of a routed request (reached over the
/// bridge queue), `with_state` is the owner's state, which holds the volume. It carries the mount
/// capability the request presented, so the owner-shard authorization validates it and the export stamps
/// it into every handle it mints (AUD-01).
#[derive(Clone, Copy, Default)]
struct ShardVolumeSet {
  capability: Option<MountCapability>,
}

impl VolumeSet for ShardVolumeSet {
  fn entries(&self) -> Vec<(String, VolumeId)> {
    let capability = self.capability;
    state::with_state(|s| {
      // List each volume under its provisioned mount name (its slot's `name`), so `ls /` shows friendly
      // names and `cd <name>` resolves by matching it (`MultiExport`) — but only the volume the presented
      // mount capability authorizes (§4.13; AUD-01). Without a capability nothing is listed: a volume
      // cannot be discovered by an unbound or unrelated caller and then reached by handle.
      let mut out = Vec::with_capacity(s.by_id.len());
      for (id, &handle) in &s.by_id {
        if let Ok(slot) = s.volumes.get(handle)
          && listable(s, *id, capability)
        {
          out.push((slot.name.clone(), *id));
        }
      }
      out
    })
    .unwrap_or_default()
  }

  fn capability(&self) -> MountCapability {
    self.capability.unwrap_or((0, [0u8; 16]))
  }

  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    _rights: Rights,
    groups: Option<UnixGroups>,
    dialect: Dialect,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    let capability = self.capability;
    state::with_state(|s| {
      if !s.consensus_ready {
        return Some(io_failure_reply(procedure));
      }
      // The authorization gate (§4.13; AUD-01): the request runs under the rights its mount capability
      // was granted — the attachment `attach` created after checking the volume's access list for the
      // caller's principal — validated here against the attachment record for *this* volume. A request
      // with no capability, a wrong or forged one, or one for another volume — an unbound TCP client, a
      // wrong consumer, any uid — is refused `NFS3ERR_ACCES` before any effect, where the edge used to
      // fabricate unconditional read/write from the uid alone. It comes first, before the lease: an
      // unauthorized caller is told the same thing whatever the lease's state, so a lapse cannot tell it
      // which volumes this node holds.
      let Some((capability, rights)) = authorized_rights(s, volume, capability) else {
        return Some(Some(status_failure_reply(Nfsstat3::Acces, procedure)));
      };
      // The owner-lease gate (§4.8 "Leases and reads"; AUD-08): the mount serves the volume's **live tree**
      // — its latest state — so while this node's authority over the object is unconfirmed (cut off, paused
      // past the lease bound, or superseded by a newer configuration) every procedure answers `NFS3ERR_JUKEBOX`,
      // the retry-later status, rather than a stale view a successor may have advanced. This runs on the
      // owner shard, so the lease read here is the owner's; a client mounting elsewhere reaches this owner
      // through `serve_remote`. Only a volume in this shard's set is gated: `with_export` serves nothing else,
      // and the router answers such a handle `NFS3ERR_STALE` (a destroyed volume's), which a lapse must not
      // turn into a retry-later
      // (docs/bugs/2026-09-29-the-lease-gate-refused-volumes-the-node-did-not-hold.md).
      if s.by_id.contains_key(&volume)
        && crate::verbs::lease_refusal(s, ObjectId(volume.bytes)).is_some()
      {
        return Some(Some(status_failure_reply(Nfsstat3::Jukebox, procedure)));
      }
      let requester = (subject, groups, dialect);
      let reply = with_export(s, volume, requester, rights, capability, |export| {
        export.serve_nfs(procedure, args)
      });
      // A file state procedure's changes are durable before its reply leaves (§4.6 A-37); a change
      // that could not be recorded has rebuilt the state from its records, and the call is refused.
      if crate::nfs_state::record_files(s) {
        reply
      } else {
        Some(Some(status_word(Nfsstat4::Serverfault)))
      }
    })
    .flatten()
    .flatten()
  }

  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    _rights: Rights,
    groups: Option<UnixGroups>,
    dialect: Dialect,
  ) -> Option<(Nfsfh3, Option<Fattr3>)> {
    let capability = self.capability;
    state::with_state(|s| {
      // Establishing the volume's root at `MNT`/`LOOKUP` is itself gated (AUD-01): a caller without the
      // volume's capability gets no root handle, so no volume can be mounted or entered without it. The
      // root handle returned carries the capability, so every later request self-authorizes.
      let (capability, rights) = authorized_rights(s, volume, capability)?;
      let requester = (subject, groups, dialect);
      let (handle, attr) = with_export(s, volume, requester, rights, capability, |export| {
        export.root_object()
      })??;
      // The owner-lease gate (§4.8 "Leases and reads"; AUD-08), as `serve` applies it: the root's
      // attributes are the volume's latest state, withheld while this node's authority over it is
      // unconfirmed; the handle is not state, and the client's `GETATTR` through it meets `serve`'s gate
      // (docs/bugs/2026-09-29-the-lease-gate-refused-volumes-the-node-did-not-hold.md). `with_export`
      // answered, so the volume is in this shard's set.
      let serves_latest = crate::verbs::lease_verdict(s, ObjectId(volume.bytes)).holds();
      Some((handle, serves_latest.then_some(attr)))
    })
    .flatten()
  }

  fn serve_file_state(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Vec<u8> {
    serve_file_state_here(procedure, args.rest())
  }
}

/// The mount capability and the rights an NFS request runs under for `volume`, or `None` if the caller
/// may not touch it (§4.13; AUD-01): the presented `(attachment, token)` must name an attachment record
/// on this shard whose token matches and whose volume is `volume`, and the rights are those the attachment
/// was granted from the volume's access list when `attach` admitted it — the owner's every right for the
/// owner, exactly what was shared for a shared consumer or uid, read only for a green pin. Nothing else
/// authorizes: not the `AUTH_SYS` uid, not loopback reachability, not a capability for another volume.
fn authorized_rights(
  s: &ShardState,
  volume: VolumeId,
  capability: Option<MountCapability>,
) -> Option<(MountCapability, Rights)> {
  let (attachment, token) = capability?;
  if token == [0u8; 16] {
    return None;
  }
  let record = s.db.partition().attachment(attachment)?;
  if record.token != token || record.volume != volume {
    return None;
  }
  // The attachment's granted rights (catalog read/write/admin) map to the mount edge's read/write.
  let rights = Rights {
    read: record.rights.read,
    write: record.rights.write,
  };
  rights.read.then_some(((attachment, token), rights))
}

/// Whether a request may see `volume` in a root `ls /` (§4.13; AUD-01): only under a mount capability
/// that authorizes that volume, so a volume cannot be discovered by an unbound or unrelated caller and
/// then reached by handle.
fn listable(s: &ShardState, volume: VolumeId, capability: Option<MountCapability>) -> bool {
  authorized_rights(s, volume, capability).is_some()
}

/// Builds a transient export for `volume` over the shard's store and the volume's slot, and runs `f`
/// with it; `None` if the shard does not hold the volume or its attachment cannot be admitted. The
/// handle slab is fresh per request — NFS keeps no open state across requests — and the volume's base
/// host (for an overlay) is lent from the slot, both through [`VolumeBridge::attached`]. The validated
/// mount `capability` is stamped into every handle the export mints (AUD-01), so the client's later
/// requests carry it.
fn with_export<R>(
  s: &mut ShardState,
  volume: VolumeId,
  (subject, groups, dialect): (Principal, Option<UnixGroups>, Dialect),
  rights: Rights,
  capability: MountCapability,
  f: impl FnOnce(&mut Export<'_>) -> R,
) -> Option<R> {
  let handle = *s.by_id.get(&volume)?;
  crate::verbs::relieve_deferred(s);
  let write_verifier = s.write_verifier;
  // The request is admitted under the registry attachment its mount capability rides on (§4.4; GAP-A9-4):
  // admitted on the capability's first request, so a barrier the owner runs over the volume sees this
  // request in flight and closes the mount's generation with the others. The subject is the attachment
  // record's principal, never the request's uid.
  let admitted = admit_mount(s, volume, capability, rights)?;
  if s.nfs_v4_files.is_none() {
    s.nfs_v4_files = crate::nfs_state::file_state(s);
  }
  // A scoped mount presents one directory: its bridge answers nothing outside it (AUD-29-76). A bound mount
  // serves one identity; any other caller is a stranger to it (§4.6 A-115).
  let form = s
    .db
    .partition()
    .attachment(capability.0)
    .map(|record| (record.form.scope(), record.form.bound_uid()));
  let (scope, bound_uid) = form.unwrap_or((None, None));
  let stranger = bound_uid.is_some_and(|bound| subject != Principal::Uid { uid: bound });
  let ShardState {
    store,
    volumes,
    attachments,
    nfs_v4_files,
    snapshot_views,
    ..
  } = s;
  let mut handles = new_handle_store();
  // A snapshot mount serves its attachment's read-only view; every other mount, the volume's head.
  let mut bridge = match snapshot_views.get_mut(&capability.0) {
    Some(view) => VolumeBridge::attached(volume, &mut view.volume, store, &mut handles, None),
    None => {
      let slot = volumes.get_mut(handle).ok()?;
      VolumeBridge::attached(
        volume,
        &mut slot.volume,
        store,
        &mut handles,
        slot.host.as_mut().map(|host| host as &mut dyn HostFs),
      )
    }
  };
  attachments.begin(admitted).ok()?;
  let mut scoped;
  let served: &mut dyn slates_bridge_core::Bridge = match scope {
    Some(scope) => {
      scoped = slates_bridge_core::scoped::ScopedBridge::new(&mut bridge, scope);
      &mut scoped
    }
    None => &mut bridge,
  };
  let mut export = Export::over(served, volume, subject, attachments, admitted);
  export.set_groups(groups);
  if stranger {
    export.set_stranger();
  }
  export.set_dialect(dialect);
  // The per-boot write verifier (§4.6, RFC 1813 §3.3.7): a client compares it across a restart to
  // learn its unstable writes were lost and re-send them.
  export.set_write_verifier(write_verifier);
  export.set_capability(capability);
  if let Some(files) = nfs_v4_files.as_mut() {
    export.lend_file_state(files);
  }
  let served = f(&mut export);
  drop(export);
  attachments.end(admitted);
  Some(served)
}

/// The registry attachment a mount capability's requests are admitted under: the one recorded for the
/// capability's catalog attachment, or a fresh one admitted now with the record's principal and its
/// granted rights (`NFS` mounts claim the server-visible boundary, §4.6: the kernel client buffers
/// acknowledged writes until its `COMMIT`). `None` when the registry refuses (its bound).
fn admit_mount(
  s: &mut ShardState,
  volume: VolumeId,
  capability: MountCapability,
  rights: Rights,
) -> Option<slates_bridge_core::AttachmentId> {
  if let Some(mount) = s.mount_attachments.get(&capability.0) {
    return Some(mount.registry);
  }
  let record = s.db.partition().attachment(capability.0)?;
  // A record naming a snapshot is served only through its view (`crate::snapshot_view`), never as the head
  // (AUD-29-76): without a view it is admitted nowhere.
  if record.snapshot.is_some() && !s.snapshot_views.contains_key(&capability.0) {
    return None;
  }
  let subject = record.principal.clone();
  let registry = s
    .attachments
    .attach(volume, slates_bridge_core::View::Current, subject, rights)
    .ok()?;
  s.mount_attachments.insert(
    capability.0,
    crate::state::MountAttachment {
      registry,
      boundary: slates_ipc::protocol::SnapshotBoundary::ServerVisible,
    },
  );
  Some(registry)
}

/// Whether a served call's effect must be in the shard's recovery image before its reply goes out
/// (the §4.8 barrier, D-18): a mutating NFS procedure that succeeded — every one except an `UNSTABLE`
/// write, which its client makes stable with a later COMMIT (itself a barrier). A refused call
/// changed nothing, and a read never needs one.
fn needs_barrier(program: u32, procedure: u32, args: &[u8], results: &[u8]) -> bool {
  if program != NFS_PROGRAM || !nfs_ok(results) {
    return false;
  }
  // A state-carrying I/O (A-36) is its NFSv3 procedure behind a state status: with the state clear,
  // the inner result and the NFSv3 arguments (a prefix of the call's) decide as the plain call would.
  if let Some(inner) = slates_bridge_nfs::procedures::state_io_inner(procedure) {
    let Some(inner_results) = results.get(size_of::<u32>()..) else {
      return false;
    };
    return needs_barrier(program, inner, args, inner_results);
  }
  match procedure {
    NFSPROC3_WRITE => {
      write_stable_how(&mut XdrReader::new(args)).is_none_or(|stable| !is_unstable(stable))
    }
    NFSPROC3_SETATTR | NFSPROC3_CREATE | NFSPROC3_MKDIR | NFSPROC3_SYMLINK | NFSPROC3_MKNOD
    | NFSPROC3_REMOVE | NFSPROC3_RMDIR | NFSPROC3_RENAME | NFSPROC3_LINK | NFSPROC3_COMMIT => true,
    // The v4 front end's attribute changes (A-35) publish as any mutation does.
    slates_bridge_nfs::procedures::extension::XATTR_SET
    | slates_bridge_nfs::procedures::extension::XATTR_REMOVE => true,
    _ => false,
  }
}

/// Whether an NFS reply's leading status is `NFS3_OK` (RFC 1813: every NFS result starts with its
/// `nfsstat3`).
fn nfs_ok(results: &[u8]) -> bool {
  XdrReader::new(results)
    .u32()
    .is_ok_and(|status| status == Nfsstat3::Ok as u32)
}

/// The barrier after a served mutation (§4.8, D-18): publishes the shard's recovery image on this
/// (owner) shard so the effect survives a daemon restart before the reply claims it does. A refused
/// publish — the image did not fit its slot, or the slot could not be written — replaces the reply
/// with `NFS3ERR_IO` (the effect is in the volume, not stable; the client is told so, never promised
/// survival). A publish that committed without the touched `volume` (a volume it could not image: its
/// base unreadable, or the image refused) is counted as a
/// refused barrier ([`crate::daemon::BARRIER_UNCAPTURED`]) and returns `NFS3ERR_IO`. No volume id
/// or no publication also refuses: absence of a proof never becomes a stability guarantee.
fn barrier(
  procedure: u32,
  volume: Option<VolumeId>,
  reply: (AcceptStatus, Vec<u8>),
) -> (AcceptStatus, Vec<u8>) {
  let outcome = state::with_state(crate::verbs::publish_shard);
  publication_reply(procedure, volume, outcome, reply)
}

/// The barrier's observable reply, separated from publication so refusal paths can be exercised
/// without a live mount (§4.8, AC-2.12).
fn publication_reply(
  procedure: u32,
  volume: Option<VolumeId>,
  outcome: Option<Result<crate::verbs::Published, slates_vfs::VfsError>>,
  reply: (AcceptStatus, Vec<u8>),
) -> (AcceptStatus, Vec<u8>) {
  if let Some(Ok(published)) = outcome {
    if volume.is_some_and(|touched| published.captured(touched)) {
      return reply;
    }
    crate::daemon::BARRIER_UNCAPTURED.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
  }
  match io_failure_reply(procedure) {
    Some(failed) => (AcceptStatus::Success, failed),
    None => (AcceptStatus::SystemErr, Vec::new()),
  }
}

/// The rights every NFS request runs under (§4.13): read-write, so a mount can read and write the
/// volumes it reaches. Which *user* the request runs as comes from its credential ([`subject_of`]).
fn mount_rights() -> Rights {
  Rights {
    read: true,
    write: true,
  }
}

/// The mounting user a request runs as: the authenticated `subject` (uid-only, §4.13) it authorizes
/// as, and its `groups` — the `AUTH_SYS` primary group a created object takes and, with the
/// supplementary groups, the group class of every permission check; `None` when the mount named none
/// (`AUTH_NONE`: a caller in no group whose created objects inherit the parent's group). Both are read
/// from the one credential and ride together to a volume's owner shard, so they travel as one value
/// rather than two parallel arguments through the routing.
#[derive(Clone)]
struct Requester {
  subject: Principal,
  groups: Option<UnixGroups>,
  /// The mount capability this request presented (§4.13; AUD-01): from the `MNT` path for a mount, from
  /// the leading file handle for every other call. Rides to the volume's owner shard, which validates it
  /// against the attachment record — the loopback edge's substitute for a peer credential. `None` when
  /// the call presented none; a call's `AUTH_SYS` uid never sets it.
  capability: Option<MountCapability>,
  /// The protocol the request answers for ([`Dialect`]): an NFSv3 wire request is this host's loopback
  /// client's (AppleDouble views only for the macOS client, §4.6 A-33); an NFSv4 operation's v3 call is
  /// the front end's (no views; change counters, A-38).
  dialect: Dialect,
  /// The NFSv4 client an operation's v3 call acts for (the client its compound's `SEQUENCE` named, A-80), so the
  /// owner's recall gate passes a delegation holder's own change; `None` for every other call.
  client: Option<u64>,
}

impl Requester {
  /// The mounting user a call runs as, from its `AUTH_SYS` credential (uid and groups, §4.13, set by
  /// the kernel on a loopback mount — so a real `mount_nfs` runs as the mounting user and stamps the
  /// user's own group, not root:wheel), or the machine root when the call carries no such credential
  /// (`AUTH_NONE`). The call's mount capability is folded in by the serve loop.
  fn of(body: &[u8]) -> Requester {
    match auth_sys_identity(body) {
      Some(identity) => Requester {
        subject: Principal::Uid { uid: identity.uid },
        groups: Some(identity.groups),
        capability: None,
        dialect: Dialect::LOOPBACK_NFS3,
        client: None,
      },
      None => Requester::root(),
    }
  }

  /// The fallback requester for a call whose header would not parse (a garbage call), and for a call
  /// with no credential: machine root, in no group (parent-inherited). Never authorizes anything a real
  /// credential would not.
  fn root() -> Requester {
    Requester {
      subject: Principal::Uid { uid: 0 },
      groups: None,
      capability: None,
      dialect: Dialect::LOOPBACK_NFS3,
      client: None,
    }
  }

  /// Folds the mount capability this call presented into the requester, so it rides to the owner
  /// shard's authorization (§4.13; AUD-01).
  fn with_capability(mut self, capability: Option<MountCapability>) -> Requester {
    self.capability = capability;
    self
  }
}

// ---------------------------------------------------------------- routing and the bridge queue

/// The owner shard's runtime id a call must be routed to, or `None` to serve it on this shard (the
/// volume is local, the call names the synthetic root, or it carries no volume — portmap, `MNT /`, a
/// bad handle). A volume's `owner_of` is its owning *partition* (§4.8: ids route to owners); a request
/// whose owner partition is not this shard's is routed to that partition's shard over the bridge queue.
fn route(program: u32, procedure: u32, args: &[u8]) -> Option<u16> {
  // A root `LOOKUP` or a `MNT` names a volume by its friendly mount name, which — unlike the id — is
  // not self-describing, so it routes by the name's owning partition (`owner_of_name`, the partition
  // the create routed to and the volume's id encodes, so the two agree with no global index, D-14).
  if let Some(name) = root_mount_name(program, procedure, args) {
    return route_by_name(&name);
  }
  // Every other call names its volume by a file handle; route by the volume's owning partition.
  let volume = target_volume(program, procedure, args)?;
  if volume == root_volume() {
    return None;
  }
  // The volume core and the wire id share the same 16 bytes; `owner_of` reads the owning partition.
  let owner_partition = owner_of(slates_ipc::protocol::VolumeId {
    bytes: volume.bytes,
  });
  state::with_state(|s| {
    if owner_partition == s.partition {
      return None;
    }
    s.shards.get(usize::from(owner_partition)).copied()
  })
  .flatten()
}

/// The shard for the partition that owns `name` (`owner_of_name` over `state.shards.len()`, the same
/// count the create used), or `None` when that partition is this shard's. A friendly-name root
/// `LOOKUP`/`MNT` routes here to the shard holding the named volume, which resolves the name against
/// its own slots (`MultiExport` matches it in `ShardVolumeSet::entries`).
fn route_by_name(name: &str) -> Option<u16> {
  state::with_state(|s| {
    let owner_partition = owner_of_name(name, s.shards.len());
    if owner_partition == s.partition {
      return None;
    }
    s.shards.get(usize::from(owner_partition)).copied()
  })
  .flatten()
}

/// The volume a handle-addressed call is *about*, for routing: the leading file handle's volume. A
/// root `LOOKUP` (by name) and a `MNT` (by path) route by name through [`root_mount_name`] before
/// this, so they do not reach here; every other NFS call names its volume by its file handle.
fn target_volume(program: u32, _procedure: u32, args: &[u8]) -> Option<VolumeId> {
  match program {
    // A LOOKUP inside a volume routes to that volume; a LOOKUP under the synthetic root and a `MNT`
    // route by name ([`root_mount_name`]) before this, so they never reach here as the root.
    NFS_PROGRAM => request_volume(&XdrReader::new(args)),
    _ => None,
  }
}

/// The friendly mount name a root `LOOKUP` (a name under the synthetic root) or a `MNT` names, to be
/// routed by [`route_by_name`]. `None` for the host root itself (`MNT /`), a LOOKUP inside a volume
/// (routed by its handle through [`target_volume`]), or any other call.
fn root_mount_name(program: u32, procedure: u32, args: &[u8]) -> Option<String> {
  match program {
    NFS_PROGRAM if procedure == NFSPROC3_LOOKUP => {
      // Only a LOOKUP whose directory is the synthetic root routes by name.
      if request_volume(&XdrReader::new(args))? != root_volume() {
        return None;
      }
      let mut reader = XdrReader::new(args);
      let _dir = Nfsfh3::decode(&mut reader).ok()?;
      Some(reader.string(NFS_MAXNAMELEN).ok()?.to_owned())
    }
    // A `MNT` and the kernel's `UMNT` of the same path both route to the name's owner: the mount is served
    // there, and the unmount ends the mount's attachment there (AUD-01).
    MOUNT_PROGRAM if is_mount_request(program, procedure) => {
      let path = XdrReader::new(args).string(MNT_PATH_MAX).ok()?;
      let name = path.trim_matches('/');
      if name.is_empty() {
        None
      } else {
        Some(name.to_owned())
      }
    }
    _ => None,
  }
}

/// Whether a call is a `MOUNT` `MNT` or `UMNT` — the calls whose path presents a mount capability
/// (§4.13; AUD-01): the mount to be served under it, the unmount to end its attachment.
fn is_mount_request(program: u32, procedure: u32) -> bool {
  program == MOUNT_PROGRAM && (procedure == MOUNTPROC3_MNT || procedure == MOUNTPROC3_UMNT)
}

/// The kernel's `UMNT` of `/<name>` ends the attachment of the mount it unmounts (§4.6, §4.13; AUD-01),
/// once the kernel's mount table confirms that mount is gone. The path is the mount's source name, with
/// no capability (§4.6 A-34), so a `UMNT` alone proves nothing: any local process could send one. On the
/// name's owner shard every host mount (`Consumer::Bridge`) of the named volume bound to a mount point is
/// watched by [`confirm_unmount`]: an attachment ends (as a `detach` would, `verbs::end_attachment`) when
/// its mount point no longer holds this volume's mount, and one whose mount is still there when the
/// deadline passes is a mount nobody unmounted, left as it is. `UMNT` has no status (RFC 1813 §5.2.3),
/// so the reply is void; `args` is `/<name>`.
fn unmount_capability(_capability: Option<MountCapability>, args: &[u8]) {
  let Ok(path) = XdrReader::new(args).string(MNT_PATH_MAX) else {
    return;
  };
  let name = path.trim_matches('/').to_owned();
  if name.is_empty() || name.contains('@') {
    return;
  }
  let mounts: Vec<(u64, String)> = state::with_state(|s| {
    let Some(volume) = s.db.partition().volume_by_name(&name).map(|v| v.id) else {
      return Vec::new();
    };
    s.db
      .partition()
      .attachments_of(volume)
      .into_iter()
      .filter_map(|record| match (&record.consumer, &record.form) {
        (Consumer::Bridge, AttachForm::ChosenPath { path })
        | (
          Consumer::Bridge,
          AttachForm::ScopedMount {
            mount_point: Some(path),
            ..
          }
          | AttachForm::BoundMount {
            mount_point: Some(path),
            ..
          },
        ) => Some((record.id, path.clone())),
        _ => None,
      })
      .collect()
  })
  .unwrap_or_default();
  if mounts.is_empty() {
    let _ = state::with_state_counted(|s| s.count("nfs.unmount_refused", 1));
    return;
  }
  match futures::spawn(confirm_unmount(name, mounts)) {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => {
      crate::fleet::count_refusal("nfs.unmount_refused");
    }
  }
}

/// Derived: how long a `UMNT` waits for its mount to leave the kernel's mount table: one liveness
/// budget ([`crate::daemon::LIVENESS_BUDGET_NS`]). The kernel sends `UMNT` while it unmounts, so the
/// mount leaves the table within the unmount's own few steps; one still there after the budget the
/// anchor gives the daemon to beat was not being unmounted.
const UNMOUNT_CONFIRM_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;

/// Ends each of `mounts` (attachment, mount point) whose point no longer holds `name`'s mount, checking at
/// the fleet's poll interval for at most [`UNMOUNT_CONFIRM_NS`] ([`unmount_capability`]). The check stops
/// early once one mount has gone: a `UMNT` is one unmount.
async fn confirm_unmount(name: String, mounts: Vec<(u64, String)>) {
  let began = futures::now_ns();
  let poll = crate::daemon::HEARTBEAT_NS / crate::fleet::POLL_PER_PERIOD;
  loop {
    let gone: Vec<u64> = mounts
      .iter()
      .filter(|(_, point)| !holds_mount_of(point, &name))
      .map(|(attachment, _)| *attachment)
      .collect();
    if !gone.is_empty() {
      let _ = state::with_state_counted(|s| {
        for attachment in gone {
          let ended = s
            .db
            .partition()
            .attachment(attachment)
            .cloned()
            .is_some_and(|record| {
              crate::verbs::end_attachment(s, &record, crate::verbs::Ending::Otherwise).is_ok()
            });
          if !ended {
            s.count("nfs.unmount_refused", 1);
          }
        }
      });
      return;
    }
    if futures::now_ns().saturating_sub(began) >= UNMOUNT_CONFIRM_NS {
      let _ = state::with_state_counted(|s| s.count("nfs.unmount_unconfirmed", 1));
      return;
    }
    if futures::sleep(poll).await.is_err() {
      // Off a shard no poll can be timed: the confirmation is given up, counted, never spun on.
      let _ = state::with_state_counted(|s| s.count("nfs.unmount_unconfirmed", 1));
      return;
    }
  }
}

/// Whether `mount_point` is still where `name`'s loopback mount is: the kernel's mount table
/// (`getfsstat(MNT_NOWAIT)`, `slates_bridge_oci::mount_table`) lists a mount exactly there whose source is
/// `slates:/<name>` (§4.6 A-34). The table is read, never the mount: a `statfs` of the point would send
/// an NFS request to this very daemon, and on a single shard the shard that must answer is the one
/// waiting (the hazard `slates-bridge-oci`'s module doc records). A table that cannot be read counts as
/// the mount still being there, so nothing ends on a read error.
#[cfg(target_os = "macos")]
fn holds_mount_of(mount_point: &str, name: &str) -> bool {
  let Ok(entries) = slates_bridge_oci::mount_table::mount_table() else {
    return true;
  };
  let source = format!("slates:/{name}");
  entries
    .iter()
    .rev()
    .find(|entry| entry.mount_point.trim_end_matches('/') == mount_point.trim_end_matches('/'))
    .is_some_and(|entry| entry.source == source)
}

/// Off macOS a loopback NFS mount is not how volumes are mounted (Linux uses FUSE, which ends its
/// attachment when the kernel closes the device); a `UMNT` there is never taken as proof of an unmount.
#[cfg(not(target_os = "macos"))]
fn holds_mount_of(_mount_point: &str, _name: &str) -> bool {
  true
}

/// Splits a mount path of the form `<name>@<attachment_hex>.<token_hex>` — or `@<attachment_hex>.<token_hex>`
/// for the host root scoped to that capability — into the volume name (empty for the root) and the mount
/// capability it presents (§4.13; AUD-01): the attachment id (a routable counter) and its 16-byte secret
/// token. `None` when the path carries no capability (a plain `<name>` or `/`, which mounts nothing but
/// an empty root), or when the capability is malformed. The token is 32 lowercase hex digits; the
/// attachment id is hex. The `@` and `.` are outside a volume name's character set, so they cannot occur
/// in a real name.
fn split_mount_capability(args: &[u8]) -> Option<(String, MountCapability)> {
  let path = XdrReader::new(args).string(MNT_PATH_MAX).ok()?;
  let path = path.trim_matches('/');
  let (name, capability) = path.rsplit_once('@')?;
  let (attachment_hex, token_hex) = capability.split_once('.')?;
  let attachment = u64::from_str_radix(attachment_hex, HEX_RADIX).ok()?;
  let hex = token_hex.as_bytes();
  if hex.len() != size_of::<[u8; 16]>().saturating_mul(2) {
    return None;
  }
  let mut token = [0u8; 16];
  let (pairs, _) = hex.as_chunks::<2>();
  for (byte, &[hi, lo]) in token.iter_mut().zip(pairs) {
    let hi = char::from(hi).to_digit(HEX_RADIX)?;
    let lo = char::from(lo).to_digit(HEX_RADIX)?;
    *byte = u8::try_from(hi.checked_mul(HEX_RADIX)?.checked_add(lo)?).ok()?;
  }
  Some((name.to_owned(), (attachment, token)))
}

/// Encodes a bare volume name (or the empty root) as the `MNT` call's `dirpath` argument (an XDR string),
/// so the capability stripped from the path (`split_mount_capability`) leaves the routing and the mount
/// serving `<name>` exactly as an ordinary `MNT /<name>` would (§4.13; AUD-01).
fn encode_mount_path(name: &str) -> Vec<u8> {
  let mut writer = XdrWriter::new();
  writer.opaque(format!("/{name}").as_bytes());
  writer.into_bytes()
}

/// The mount capability one call presents (§4.13; AUD-01): a `MNT`'s from its path, every other call's
/// from the leading file handle it names — the handle the daemon minted with the capability stamped in.
/// For a `MNT` with a capability the path is rewritten to the bare name, so the routing and the mount
/// serve it as an ordinary `MNT /<name>`; the returned root handle then carries the capability.
fn presented_capability(
  program: u32,
  procedure: u32,
  args: &mut Vec<u8>,
) -> Option<MountCapability> {
  if program == MOUNT_PROGRAM && procedure == MOUNTPROC3_UMNT {
    // The kernel's `UMNT` names the volume by the mount's source (`slates:/<name>` gives `/<name>`), with
    // no capability (§4.6 A-34); it presents none, and the unmount is confirmed against the kernel's
    // mount table before anything ends ([`unmount_capability`]).
    return None;
  }
  if is_mount_request(program, procedure) {
    let (name, capability) = split_mount_capability(args)?;
    *args = encode_mount_path(&name);
    return Some(capability);
  }
  slates_bridge_nfs::request_capability(&XdrReader::new(args))
}

/// Counter: NFSv4 `GETATTR`s answered `NFS4ERR_DELAY` while another client's write delegation of the file was
/// recalled (A-80).
const GETATTR_RECALLED: &str = "nfs4.delegation.getattr_recalled";

/// An NFSv4 client's `GETATTR` of a file another client holds a write delegation of, refused `NFS3ERR_JUKEBOX`
/// (`NFS4ERR_DELAY` to the client) while that delegation is recalled (RFC 8881 §10.4.3; A-80); `None` for every
/// other call, which is served. Only a client is told to wait: no path outside NFSv4 has a delegation's promise to
/// keep, and a write delegation's zero space limit has its holder flush at every close, so what those paths read is
/// what they would read without one.
fn refused_for_write_delegation(
  requester: &Requester,
  program: u32,
  procedure: u32,
  args: &[u8],
) -> Option<(AcceptStatus, Vec<u8>)> {
  let client = requester.client?;
  if program != NFS_PROGRAM || procedure != slates_bridge_nfs::procedures::NFSPROC3_GETATTR {
    return None;
  }
  let fh = Nfsfh3::decode(&mut XdrReader::new(args)).ok()?;
  let now = futures::now_ns();
  let waiting = state::with_state(|s| {
    if s.store.recall_gate.is_open() {
      return false;
    }
    let waiting = s
      .nfs_v4_files
      .as_mut()
      .is_some_and(|files| files.check_read_conflicts(&fh, client, now));
    if waiting {
      s.count(GETATTR_RECALLED, 1);
      crate::delegation::drain(s);
    }
    waiting
  })?;
  waiting.then(|| {
    (
      AcceptStatus::Success,
      status_failure_reply(Nfsstat3::Jukebox, procedure),
    )
  })
}

/// Serves one call locally, on this shard's volumes and synthetic root, as `requester` (the mounting
/// user's subject and the group its created objects take).
fn serve_local(
  requester: Requester,
  xid: u32,
  program: u32,
  procedure: u32,
  args: &[u8],
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  // The `bridge.request` chokepoint span (§4.14): one NFS bridge call from arrival to reply, served on
  // this shard's volumes, opened as a root — the kernel's call is the entry point of its trace. Its
  // request identity for replay is the RPC transaction id under the mount's port (an NFS client
  // retransmits a call under the same xid, exactly what a replay identity names); the label is the NFS
  // procedure — content-free (an operation code, never a path or bytes). The clock and ring are reached
  // through `with_state`, taken at the call's edges — outside `serve_call`'s own per-operation borrows,
  // so there is no re-entrant borrow.
  if let Some(refused) = refused_for_write_delegation(&requester, program, procedure, args) {
    return refused;
  }
  let request = RequestId {
    client: u32::from(port),
    sequence: xid,
  };
  let open = state::with_state(|s| {
    let start_ns = s.clock.monotonic_ns();
    s.tracer
      .open_root(request, Chokepoint::BridgeRequest, start_ns)
  });
  // The kernel's unmount ends the mount's attachment here, on the name's owner shard (AUD-01); the
  // bridge then answers the void `UMNT` reply.
  if program == MOUNT_PROGRAM && procedure == MOUNTPROC3_UMNT {
    unmount_capability(requester.capability, args);
  }
  let requester_dialect = requester.dialect;
  let mut service = MultiExport::new(
    ShardVolumeSet {
      capability: requester.capability,
    },
    requester.subject,
    mount_rights(),
    requester.groups,
  );
  // The recall gate passes a change by the only holders of a file's delegations: the NFSv4 client this call acts
  // for, if any (A-80). Named for this call alone, and cleared after it.
  let _ = state::with_state_counted(|s| s.store.recall_gate.act_as(requester.client));
  // Only NFSv3 reaches here: an NFSv4 call is served by the v4 front end before routing.
  let served = serve_call(
    &mut service,
    requester_dialect,
    program,
    NFS_VERSION,
    procedure,
    &mut XdrReader::new(args),
    port,
  );
  let _ = state::with_state_counted(|s| s.store.recall_gate.act_as(None));
  // The barrier (§4.8, D-18): a mutation's effect is published into anchor-owned RAM before its
  // reply leaves this shard, so the reply's stability claim is true for daemon-restart survival.
  let result = if matches!(served.0, AcceptStatus::Success)
    && needs_barrier(program, procedure, args, &served.1)
  {
    barrier(procedure, target_volume(program, procedure, args), served)
  } else {
    served
  };
  if let Some(open) = open {
    let _ = state::with_state_counted(|s| {
      let end_ns = s.clock.monotonic_ns();
      crate::telemetry::emit(s, open.end(procedure, end_ns));
    });
  }
  // A change this call was refused for a delegation asked for a recall; it is sent now (A-79).
  let _ = state::with_state_counted(crate::delegation::drain);
  result
}

/// Serves one call on the volume's `owner` shard over the bridge queue: spawns the serve on the owner
/// (the same cross-shard spawn the client forward uses, §4.3), which spawns a task back on `origin`
/// that hands the reply to this awaiting task. Returns a system error if the owner shard is gone. The
/// mounting user's `subject` rides to the owner, so the request runs as the same user there (§4.13).
#[allow(clippy::too_many_arguments)] // one RPC call's whole identity: where, who, its xid, program, procedure, args, port
async fn serve_remote(
  owner: u16,
  origin: u16,
  requester: Requester,
  xid: u32,
  program: u32,
  procedure: u32,
  args: Vec<u8>,
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  let call = crate::xshard::call_on(origin, owner, move || {
    Some(serve_local(requester, xid, program, procedure, &args, port))
  });
  let Ok(call) = call else {
    return (AcceptStatus::SystemErr, Vec::new());
  };
  crate::xshard::within(call, crate::daemon::LIVENESS_BUDGET_NS)
    .await
    .unwrap_or((AcceptStatus::SystemErr, Vec::new()))
}

// ------------------------------------------------------- the host root's cross-shard listing

/// A [`VolumeSet`] for serving the host root's *listing* across shards: its `entries` are the volumes
/// of every shard, gathered over the bridge queue, so `READDIR`/`READDIRPLUS` of the root lists them
/// all. Attributes and handles (`READDIRPLUS`) come from the local [`ShardVolumeSet`], so a volume on
/// another shard lists by name with its attributes absent — a client fills them with a `LOOKUP`, which
/// routes across shards (built). Only the listing needs this; every other root op is unchanged.
struct GatheredVolumeSet {
  entries: Vec<(String, VolumeId)>,
  /// The mount capability the request presented, carried so a served entry authorizes under it (AUD-01).
  capability: Option<MountCapability>,
}

impl VolumeSet for GatheredVolumeSet {
  fn entries(&self) -> Vec<(String, VolumeId)> {
    self.entries.clone()
  }

  fn capability(&self) -> MountCapability {
    self.capability.unwrap_or((0, [0u8; 16]))
  }

  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    groups: Option<UnixGroups>,
    dialect: Dialect,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    ShardVolumeSet {
      capability: self.capability,
    }
    .serve(volume, subject, rights, groups, dialect, procedure, args)
  }

  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    groups: Option<UnixGroups>,
    dialect: Dialect,
  ) -> Option<(Nfsfh3, Option<Fattr3>)> {
    ShardVolumeSet {
      capability: self.capability,
    }
    .root_object(volume, subject, rights, groups, dialect)
  }

  fn serve_file_state(&mut self, procedure: u32, args: &mut XdrReader<'_>) -> Vec<u8> {
    serve_file_state_here(procedure, args.rest())
  }
}

/// Gathers every owner's entries under one liveness budget (§4.8 lookup). Any failed admission,
/// missing reply or timeout refuses the entire listing. Calls own their registrations, so early return
/// or cancellation also releases the gathers still in flight (AUD-04/AUD-17).
async fn gather_all_entries(
  capability: Option<MountCapability>,
) -> Option<Vec<(String, VolumeId)>> {
  let (origin, shards) = state::with_state(|s| (s.shard, s.shards.clone()))?;
  gather_entries(origin, &shards, capability).await
}

async fn gather_entries(
  origin: u16,
  shards: &[u16],
  capability: Option<MountCapability>,
) -> Option<Vec<(String, VolumeId)>> {
  let deadline = futures::now_ns().saturating_add(crate::daemon::LIVENESS_BUDGET_NS);
  let calls = shards
    .iter()
    .map(|shard| {
      // Each shard filters its own entries to what the presented mount capability authorizes (AUD-01).
      crate::xshard::call_on(origin, *shard, move || {
        state::with_state(|_| ())?;
        Some(ShardVolumeSet { capability }.entries())
      })
    })
    .collect::<Result<Vec<_>, _>>()
    .ok()?;
  let mut all = Vec::new();
  for call in calls {
    let remaining = deadline.saturating_sub(futures::now_ns());
    all.extend(crate::xshard::within(call, remaining).await?);
  }
  Some(all)
}

/// Whether a call is a `READDIR`/`READDIRPLUS` of the synthetic host root (which must list every
/// shard's volumes, not just this shard's).
fn is_root_listing(program: u32, procedure: u32, args: &[u8]) -> bool {
  program == NFS_PROGRAM
    && (procedure == NFSPROC3_READDIR || procedure == NFSPROC3_READDIRPLUS)
    && request_volume(&XdrReader::new(args)) == Some(root_volume())
}

/// Serves a host-root `READDIR`/`READDIRPLUS` over the gathered entries of every shard, as `requester`
/// (the listing is read-only, so the group only rides for uniformity — the root creates nothing).
fn serve_root_listing(
  requester: Requester,
  procedure: u32,
  args: &[u8],
  entries: Vec<(String, VolumeId)>,
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  let requester_dialect = requester.dialect;
  let mut service = MultiExport::new(
    GatheredVolumeSet {
      entries,
      capability: requester.capability,
    },
    requester.subject,
    mount_rights(),
    requester.groups,
  );
  serve_call(
    &mut service,
    requester_dialect,
    NFS_PROGRAM,
    NFS_VERSION,
    procedure,
    &mut XdrReader::new(args),
    port,
  )
}

// ------------------------------------------------------------------------------------ the serve loop

/// Serves NFS/MOUNT/portmap over `listener` on the current shard until the daemon stops: each connection is admitted
/// ([`admit`]: its buffers sized, its sends made whole, and handed to the anchor to hold, A-113) and becomes a
/// detached task, so connections are concurrent. `port` answers a portmap `GETPORT`; `bound` is the most connections
/// the anchor holds ([`crate::config::DaemonConfig::held_connection_bound`]).
pub async fn serve(listener: TcpListener, port: u16, bound: usize) {
  while let Ok(stream) = listener.accept().await {
    if let Some(connection) = admit(stream, port, bound) {
      spawn_connection(connection);
    }
    futures::yield_now().await;
  }
}

/// Serves `connection` as a detached task on this shard; a task the arena refuses ends the connection (the kernel
/// client reconnects) and is counted, never silent (banned item 9).
fn spawn_connection(connection: Connection) {
  let ending = connection.ending();
  match futures::spawn(serve_connection(connection)) {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => {
      crate::fleet::count_refusal(SERVE_SPAWN_REFUSED);
      ending.release();
    }
  }
}

/// The status refusal count under which the mount listener records a connection whose serve task the
/// shard's arena refused.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
pub(crate) const SERVE_SPAWN_REFUSED: &str = "nfs.serve_spawn";

/// One call of the NFS program's `version`, MOUNT or portmap (every program but NFSv4, which
/// [`reply_v4`] serves): its RPC reply payload. A `program` of 0 is a garbage call whose reply carries
/// xid 0; an NFS version other than 3 or 4 is `PROG_MISMATCH` naming the versions served.
#[allow(clippy::too_many_arguments)] // one RPC call's whole identity: where, its xid, who, program, version, procedure, args, port
async fn reply_for(
  this: u16,
  xid: u32,
  requester: Requester,
  program: u32,
  version: u32,
  procedure: u32,
  args: Vec<u8>,
  port: u16,
) -> Vec<u8> {
  if program == 0 {
    return reply_bytes(0, AcceptStatus::GarbageArgs, &[]);
  }
  if program == NFS_PROGRAM && version != NFS_VERSION {
    let mismatch = AcceptStatus::ProgMismatch {
      low: NFS_VERSION,
      high: NFS_V4,
    };
    return reply_bytes(xid, mismatch, &[]);
  }
  let (status, results) = serve_v3(this, xid, requester, program, procedure, args, port).await;
  reply_bytes(xid, status, &results)
}

/// Serves one NFSv3, MOUNT or portmap call, routing it locally, to a volume's owner shard over the
/// bridge queue, or (a host-root listing) across every shard, all as `requester` (the mounting user,
/// §4.13). The v4 front end serves each of its operations through here too, so both versions route,
/// authorize and publish alike.
async fn serve_v3(
  this: u16,
  xid: u32,
  requester: Requester,
  program: u32,
  procedure: u32,
  args: Vec<u8>,
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  if is_root_listing(program, procedure, &args) {
    // The host root lists the volume the presented capability authorizes, gathered over the bridge queue
    // (AUD-01: nothing is listed to a caller with no capability).
    let Some(entries) = gather_all_entries(requester.capability).await else {
      return (AcceptStatus::SystemErr, Vec::new());
    };
    return serve_root_listing(requester, procedure, &args, entries, port);
  }
  let started = slates_machine::clock::monotonic_ns();
  let started_cpu = thread_cpu_ns();
  let (forwarded, served) = match route(program, procedure, &args) {
    Some(owner) => (
      true,
      serve_remote(owner, this, requester, xid, program, procedure, args, port).await,
    ),
    None => (
      false,
      serve_local_held(requester, xid, program, procedure, &args, port).await,
    ),
  };
  let elapsed = slates_machine::clock::monotonic_ns().saturating_sub(started);
  let on_cpu = thread_cpu_ns().saturating_sub(started_cpu);
  let _ = state::with_state_counted(|s| {
    if program == NFS_PROGRAM
      && let Some(calls) = usize::try_from(procedure)
        .ok()
        .and_then(|procedure| s.nfs_service.calls.get_mut(procedure))
    {
      *calls = calls.saturating_add(1);
    }
    if forwarded {
      s.nfs_service.forwarded.record(elapsed);
    } else {
      s.nfs_service.local.record(elapsed);
      s.nfs_service
        .local_off_cpu
        .record(elapsed.saturating_sub(on_cpu));
    }
  });
  served
}

/// Counter: NFSv3 calls the recall gate refused that were held for the delegation's return rather than answered
/// `NFS3ERR_JUKEBOX` (A-79), one per wait.
const V3_HELD: &str = "nfs.v3.held_for_recall";

/// Serves one call locally ([`serve_local`]), holding an NFSv3 call the recall gate refused until the delegation it
/// waits on is returned or revoked (RFC 8881 §10.2: the server "may either delay responding to conflicting requests
/// or respond to them with NFS4ERR_DELAY"; A-79). An NFSv3 client's own retry of `NFS3ERR_JUKEBOX` backs off for
/// seconds (measured 4,033 ms for one macOS write, docs/wip/BENCHMARKS.md), while a recall is answered in a round
/// trip, so the call waits here, parked on the gate, and is served again at each release. The hold is bounded by one
/// lease past the recall: a delegation not returned by then is revoked (§10.4.5), which the drain before the last
/// attempt does, so the last attempt proceeds. A NFSv4 front end's sub-call is never held: its client is told
/// `NFS4ERR_DELAY` at once, since the holder changing its own file would otherwise block the connection its
/// `DELEGRETURN` must arrive on. Any other refusal is answered as it is.
async fn serve_local_held(
  requester: Requester,
  xid: u32,
  program: u32,
  procedure: u32,
  args: &[u8],
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  let holdable = program == NFS_PROGRAM && matches!(requester.dialect, Dialect::Nfs3 { .. });
  let mut deadline: Option<u64> = None;
  loop {
    let seen = state::with_state(|s| s.store.recall_gate.refusals());
    let served = serve_local(requester.clone(), xid, program, procedure, args, port);
    let gate = state::with_state(|s| {
      (
        s.store.recall_gate.refusals(),
        s.store.recall_gate.generation(),
        s.config.failover_slo_ns,
      )
    });
    let (Some(seen), Some((refusals, generation, lease))) = (seen, gate) else {
      return served;
    };
    let refused_by_gate = holdable
      && refusals != seen
      && served.0 == AcceptStatus::Success
      && leading_status(&served.1) == Some(Nfsstat3::Jukebox as u32);
    let now = futures::now_ns();
    // §10.4.5's "after a lease" is strict, and the recall was stamped after this first refusal began.
    let until = *deadline.get_or_insert(now.saturating_add(lease).saturating_add(1));
    if !refused_by_gate || now >= until {
      return served;
    }
    let _ = state::with_state_counted(|s| s.count(V3_HELD, 1));
    let parked = futures::within(
      until.saturating_sub(now),
      std::future::poll_fn(|cx| {
        let released =
          state::with_state(|s| s.store.recall_gate.wait_for_release(generation, cx.waker()));
        match released {
          Some(false) => std::task::Poll::Pending,
          _ => std::task::Poll::Ready(()),
        }
      }),
    )
    .await;
    match parked {
      Ok(Some(())) => {}
      // The lease passed: revoke the lapsed delegation so the last attempt proceeds.
      Ok(None) => {
        let _ = state::with_state_counted(crate::delegation::drain);
      }
      Err(_) => return served,
    }
  }
}

/// The leading `nfsstat3` of an NFSv3 procedure's results.
fn leading_status(results: &[u8]) -> Option<u32> {
  XdrReader::new(results).u32().ok()
}

/// The calling thread's CPU time in nanoseconds (`CLOCK_THREAD_CPUTIME_ID`), so a local serve's wall time splits
/// into the time it ran and the time the operating system held the shard's thread off a core.
fn thread_cpu_ns() -> u64 {
  /// Format: nanoseconds per second.
  const NS_PER_SECOND: u64 = 1_000_000_000;
  let reading = rustix::time::clock_gettime(rustix::time::ClockId::ThreadCPUTime);
  u64::try_from(reading.tv_sec)
    .unwrap_or(0)
    .saturating_mul(NS_PER_SECOND)
    .saturating_add(u64::try_from(reading.tv_nsec).unwrap_or(0))
}

/// How long a shard took to serve the NFS calls it read (§4.14): from the call parsed to its results built,
/// split by whether the volume's owner was this shard or another, which the call was forwarded to and back.
#[derive(Clone, Debug, Default)]
pub struct ServiceTimes {
  /// Calls served on this shard.
  pub local: crate::histogram::DurationHistogram,
  /// Calls forwarded to their volume's owner shard and answered back.
  pub forwarded: crate::histogram::DurationHistogram,
  /// Of each local call, the wall time the shard's thread spent off a core (its wall time less its CPU time): a
  /// synchronous serve never waits, so this is the operating system's preemption, not the serve's work.
  pub local_off_cpu: crate::histogram::DurationHistogram,
  /// Calls served, by NFSv3 procedure number (an NFSv4 compound counts each operation's v3 call): how many round
  /// trips each client operation cost, which a client's own counters cannot say on a shared machine (`nfsstat` is
  /// machine-wide). Reported per shard (`ShardReport::nfs_calls`).
  pub calls: [u64; V3_PROCEDURES],
}

/// Format: the NFSv3 procedures, numbered 0 (`NULL`) to 21 (`COMMIT`), RFC 1813 §3.
pub const V3_PROCEDURES: usize = 22;

/// What [`ServiceTimes`] measured, summed over a daemon's shards (`Daemon::nfs_service_times`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServiceQuantiles {
  /// Calls served on the shard that read them.
  pub local: crate::histogram::Quantiles,
  /// Calls forwarded to their volume's owner and answered back.
  pub forwarded: crate::histogram::Quantiles,
  /// The off-core share of each local call.
  pub local_off_cpu: crate::histogram::Quantiles,
}

// ------------------------------------------------------------------------------ NFSv4 (A-35)

/// One NFSv4 call (§4.6 A-35): `NULL`, or a `COMPOUND` served by the v4 front end over this shard's v4
/// state, each of its operations becoming an NFSv3 call through [`serve_v3`].
async fn reply_v4(this: u16, call: Call, port: u16) -> Vec<u8> {
  serve_v4(this, this, call, port).await.0
}

/// Whether `call` is an NFSv4 `COMPOUND` (the calls a session places, A-76).
fn is_v4_compound(call: &Call) -> bool {
  call.program == NFS_PROGRAM && call.version == NFS_V4 && call.procedure == NFSPROC4_COMPOUND
}

/// The shard an NFSv4 compound must be served on when it is not this one (A-76): a session's compound where the
/// session is held — the home routes one that departed to the shard it went to — and a client-table compound at the
/// connection's `home`. A session this shard neither holds nor (as home) knows departed is answered here, with
/// `NFS4ERR_BADSESSION`, so a connection never bounces between shards: the client makes a new session at home.
fn v4_elsewhere(this: u16, home: u16, call: &Call) -> Option<u16> {
  match compound::placement(&call.args) {
    compound::Placement::Session(sessionid) => state::with_state(|s| {
      let server = s.nfs_v4.as_ref()?;
      if server.sessions.holds_session(&sessionid) || this != home {
        return None;
      }
      server.sessions.away(&sessionid).filter(|&to| to != this)
    })
    .flatten(),
    compound::Placement::Clients => (this != home).then_some(home),
    compound::Placement::Anywhere => None,
  }
}

/// Serves an NFSv4 call for a connection whose client table is at `home`: the reply, and the owner shard the
/// compound's v3 calls were forwarded to, if any (where its session would rather be, A-76).
async fn serve_v4(this: u16, home: u16, call: Call, port: u16) -> (Vec<u8>, Option<u16>) {
  let Call {
    xid,
    requester,
    procedure,
    args,
    request_bytes,
    ..
  } = call;
  match procedure {
    NFSPROC4_NULL => (reply_bytes(xid, AcceptStatus::Success, &[]), None),
    NFSPROC4_COMPOUND => {
      // Each operation's v3 call answers for the front end: no AppleDouble views (a v4 client carries
      // attributes itself, §4.6 A-33) and the change counters its attributes need (A-38).
      let requester = Requester {
        dialect: Dialect::Nfs4,
        ..requester
      };
      let mut backend = RoutedBackend {
        this,
        xid,
        requester,
        port,
        home,
        forwarded_to: None,
      };
      note(NFS4_COMPOUNDS);
      let results = compound::serve(&mut backend, &args, request_bytes).await;
      (
        reply_bytes(xid, AcceptStatus::Success, &results),
        backend.forwarded_to,
      )
    }
    _ => (reply_bytes(xid, AcceptStatus::ProcUnavail, &[]), None),
  }
}

/// The counter of compounds answered `NFS4ERR_DELAY` at operation `opnum` (§4.14): the operations a recall or a
/// lease gate tells clients to retry, each of which costs the client a backoff (Linux: at least 100 ms,
/// `NFS4_POLL_RETRY_MIN`).
fn delayed_counter(opnum: u32) -> &'static str {
  /// Format: the operation numbers named (RFC 7863 `nfs_opnum4`).
  const OPEN: u32 = 18;
  /// Format: see [`OPEN`].
  const DELEGRETURN: u32 = 8;
  /// Format: see [`OPEN`].
  const GETATTR: u32 = 9;
  /// Format: see [`OPEN`].
  const WRITE: u32 = 38;
  /// Format: see [`OPEN`].
  const SETATTR: u32 = 34;
  /// Format: see [`OPEN`].
  const REMOVE: u32 = 28;
  /// Format: see [`OPEN`].
  const RENAME: u32 = 29;
  match opnum {
    OPEN => "nfs4.delay.open",
    DELEGRETURN => "nfs4.delay.delegreturn",
    GETATTR => "nfs4.delay.getattr",
    WRITE => "nfs4.delay.write",
    SETATTR => "nfs4.delay.setattr",
    REMOVE => "nfs4.delay.remove",
    RENAME => "nfs4.delay.rename",
    _ => "nfs4.delay.other",
  }
}

/// After a compound on this shard (A-76): the drops a home owes the guests of clients it dropped, and the notes a
/// guest owes the home of sessions destroyed here.
fn v4_after(this: u16, home: u16) {
  let (drops, forgotten) = state::with_state(|s| {
    s.nfs_v4.as_mut().map(|server| {
      (
        server.sessions.take_guest_drops(),
        server.sessions.take_forgotten(),
      )
    })
  })
  .flatten()
  .unwrap_or_default();
  for (sessionid, guest) in drops {
    send_note(this, guest, NFS4_NOTE_LOST, move || {
      with_v4_sessions(|sessions| {
        sessions.drop_guest(&sessionid);
      })
    });
  }
  for sessionid in forgotten {
    send_note(this, home, NFS4_NOTE_LOST, move || {
      with_v4_sessions(|sessions| sessions.forget_away(&sessionid))
    });
  }
}

/// Departs the session of a compound that was forwarded to `forwarded_to` for that shard, moving with the connection
/// (A-76): from then on its compounds run where their volume lives, with no hop. Only a session this shard holds as
/// its own departs (`Sessions::depart`), and only when no slot is still being served. The owner shard to move to.
fn depart_with(
  this: u16,
  connection: &mut Connection,
  placement: compound::Placement,
  forwarded_to: Option<u16>,
) -> Option<u16> {
  let (compound::Placement::Session(sessionid), Some(owner)) = (placement, forwarded_to) else {
    return None;
  };
  if owner == this {
    return None;
  }
  let departed = with_v4_sessions(|sessions| sessions.depart(&sessionid, owner)).flatten()?;
  note(NFS4_SESSIONS_MOVED);
  connection.carry = Some((sessionid, departed));
  Some(owner)
}

/// Takes in a session that moved here with its connection (A-76), creating this shard's NFSv4 table if it has none.
fn adopt_session(sessionid: SessionId, departed: DepartedSession) {
  let now = futures::now_ns();
  let adopted = state::with_state(|s| {
    if s.nfs_v4.is_none() {
      s.nfs_v4 = crate::nfs_state::server(s);
    }
    s.nfs_v4
      .as_mut()
      .map(|server| server.sessions.arrive(sessionid, departed, now))
  })
  .flatten();
  if adopted.is_none() {
    note(NFS4_SESSION_UNADOPTED);
  }
}

/// Runs `f` on this shard's NFSv4 session table, if it has one.
fn with_v4_sessions<R>(
  f: impl FnOnce(&mut slates_bridge_nfs::v4::session::Sessions) -> R,
) -> Option<R> {
  state::with_state(|s| s.nfs_v4.as_mut().map(|server| f(&mut server.sessions))).flatten()
}

/// Sends `work` to shard `to` and awaits its outcome in a detached task of this shard, counting `lost` when it could
/// not be delivered or did not answer within the liveness budget (never a silent drop, banned item 9).
fn send_note(
  this: u16,
  to: u16,
  lost: &'static str,
  work: impl FnOnce() -> Option<()> + Send + 'static,
) {
  let Ok(call) = crate::xshard::call_on(this, to, work) else {
    note(lost);
    return;
  };
  match futures::spawn(async move {
    if crate::xshard::within(call, crate::daemon::LIVENESS_BUDGET_NS)
      .await
      .is_none()
    {
      note(lost);
    }
  }) {
    Ok(task) => {
      let _ = futures::detach(task);
    }
    Err(_) => note(lost),
  }
}

/// Counter: NFSv4 `COMPOUND`s this shard served (§4.14): the denominator of the per-compound call counts below.
const NFS4_COMPOUNDS: &str = "nfs4.compounds";
/// Counter: v3 calls a compound's operations made that this shard served itself.
const NFS4_V3_LOCAL: &str = "nfs4.v3.local";
/// Counter: v3 calls a compound's operations made that went to the volume's owner shard and back (two thread
/// wakes each; the cost a compound run where its volume lives would not pay).
const NFS4_V3_FORWARDED: &str = "nfs4.v3.forwarded";
/// Counter: file-state calls (open, close, lock records; A-36) served on this shard.
const NFS4_STATE_LOCAL: &str = "nfs4.state.local";
/// Counter: file-state calls that went to the file's owner shard and back.
const NFS4_STATE_FORWARDED: &str = "nfs4.state.forwarded";

/// Counter: callbacks the client refused (an RPC reply that was not accepted: a credential it would not take).
const NFS4_CALLBACK_REFUSED: &str = "nfs4.callback.refused";
/// Counter: callbacks the client did not answer within their deadline.
const NFS4_CALLBACK_TIMEOUT: &str = "nfs4.callback.timeout";
/// Counter: callbacks that could not be sent (no carrier, one already in flight, the connection gone).
const NFS4_CALLBACK_UNSENT: &str = "nfs4.callback.unsent";
/// Counter: back channels that answered their probe (RFC 8881 §10.2).
const NFS4_CALLBACK_UP: &str = "nfs4.callback.up";
/// Counter: back channels that did not answer their probe, or whose probe could not start.
const NFS4_CALLBACK_DOWN: &str = "nfs4.callback.down";
/// Counter: NFSv4 sessions that moved with their connection to the shard their volume lives on (A-76).
const NFS4_SESSIONS_MOVED: &str = "nfs4.sessions.moved";
/// Counter: sessions that arrived with a connection but could not be taken in (no NFSv4 table on the shard).
const NFS4_SESSION_UNADOPTED: &str = "nfs4.sessions.unadopted";
/// Counter: notes between a session's home and its guest (a renewal, a drop, a forget) that were not delivered.
const NFS4_NOTE_LOST: &str = "nfs4.notes.lost";

/// Counts one `counter` on this shard; off a shard (no state) nothing is counted.
fn note(counter: &'static str) {
  let _ = state::with_state_counted(|s| s.count(counter, 1));
}

/// The v4 front end's backend in the daemon: each v3 call it makes presents the capability of the
/// handle it names and is routed to the volume's owner shard as an NFSv3 call is; the v4 state is this
/// shard's, reached through [`state::with_state`] and borrowed only inside each closure.
struct RoutedBackend {
  this: u16,
  xid: u32,
  requester: Requester,
  port: u16,
  /// Where the connection's client table lives (A-76): a guest's renewal notes go there.
  home: u16,
  /// The owner shard a v3 call of this compound was forwarded to, if any.
  forwarded_to: Option<u16>,
}

impl compound::Backend for RoutedBackend {
  fn call_v3(
    &mut self,
    procedure: u32,
    mut args: Vec<u8>,
  ) -> impl std::future::Future<Output = Vec<u8>> {
    let (this, xid, port) = (self.this, self.xid, self.port);
    let requester = self.requester.clone();
    let forwarded = route(NFS_PROGRAM, procedure, &args).filter(|&owner| owner != this);
    if forwarded.is_some() {
      self.forwarded_to = forwarded;
    }
    async move {
      note(if forwarded.is_some() {
        NFS4_V3_FORWARDED
      } else {
        NFS4_V3_LOCAL
      });
      let capability = presented_capability(NFS_PROGRAM, procedure, &mut args);
      let requester = requester.with_capability(capability);
      match serve_v3(this, xid, requester, NFS_PROGRAM, procedure, args, port).await {
        (AcceptStatus::Success, results) => results,
        // A call the v3 layer could not serve decodes as malformed, which the front end reports as
        // `NFS4ERR_SERVERFAULT`.
        _ => Vec::new(),
      }
    }
  }

  fn call_owner(
    &mut self,
    owner: u16,
    procedure: u32,
    args: Vec<u8>,
  ) -> impl std::future::Future<Output = Vec<u8>> {
    let this = self.this;
    async move {
      // The owner partition a state id names is served by that partition's shard (D-14); a name no
      // shard carries is a state id this daemon never minted.
      let Some(shard) = state::with_state(|s| s.shards.get(usize::from(owner)).copied()).flatten()
      else {
        return status_word(Nfsstat4::BadStateid);
      };
      note(if shard == this {
        NFS4_STATE_LOCAL
      } else {
        NFS4_STATE_FORWARDED
      });
      let Ok(call) = crate::xshard::call_on(this, shard, move || {
        Some(serve_file_state_here(procedure, &args))
      }) else {
        return status_word(Nfsstat4::Delay);
      };
      crate::xshard::within(call, crate::daemon::LIVENESS_BUDGET_NS)
        .await
        .unwrap_or_else(|| status_word(Nfsstat4::Delay))
    }
  }

  fn owners(&self) -> Vec<u16> {
    state::with_state(|s| {
      (0..s.shards.len())
        .filter_map(|partition| u16::try_from(partition).ok())
        .collect()
    })
    .unwrap_or_default()
  }

  fn root_handle(&self) -> Nfsfh3 {
    // The pseudo root with no capability: it lists and enters nothing until a LOOKUP presents one
    // (§4.13; AUD-01).
    slates_bridge_nfs::multi::root_handle_with(NO_CAPABILITY)
  }

  fn principal(&self) -> u32 {
    // A subject that is not a Unix user is no uid, and never root's: the bridge's convention for one
    // (`access::INVALID_UID`, which owns nothing). Every NFS requester is a uid today (`Requester::of`).
    match self.requester.subject {
      Principal::Uid { uid } => uid,
      _ => slates_bridge_nfs::access::INVALID_UID,
    }
  }

  fn now_ns(&self) -> u64 {
    futures::now_ns()
  }

  fn owns_file(&self, fh: &Nfsfh3) -> bool {
    let mut args = XdrWriter::new();
    fh.encode(&mut args);
    route(
      NFS_PROGRAM,
      slates_bridge_nfs::procedures::NFSPROC3_GETATTR,
      args.as_slice(),
    )
    .is_none()
  }

  fn revoked_state(&mut self, clientid: u64) -> bool {
    state::with_state(|s| {
      s.nfs_v4_files
        .as_ref()
        .is_some_and(|files| files.has_revoked(clientid))
    })
    .unwrap_or(false)
  }

  fn act_for(&mut self, clientid: u64) {
    self.requester.client = Some(clientid);
  }

  fn finished(&mut self, opnum: u32, status: Nfsstat4) {
    if status == Nfsstat4::Delay {
      note(delayed_counter(opnum));
    }
  }

  fn renew_home(&mut self, clientid: u64) {
    let now = futures::now_ns();
    send_note(self.this, self.home, NFS4_NOTE_LOST, move || {
      with_v4_sessions(|sessions| sessions.renew(clientid, now))
    });
  }

  fn with_v4<R>(&mut self, f: impl FnOnce(&mut V4Server) -> R) -> Result<R, Nfsstat4> {
    state::with_state(|s| {
      if s.nfs_v4.is_none() {
        s.nfs_v4 = crate::nfs_state::server(s);
      }
      let result = s.nfs_v4.as_mut().map(f)?;
      // The client table's changes are durable before the call goes on (§4.6 A-37); a change that
      // could not be recorded has rebuilt the table from its records, and the call is refused.
      crate::nfs_state::record_clients(s).then_some(result)
    })
    .flatten()
    .ok_or(Nfsstat4::Serverfault)
  }
}

/// Format: the capability a handle carries when none was presented (attachment 0, an all-zero token),
/// which authorizes nothing.
const NO_CAPABILITY: MountCapability = (0, [0u8; 16]);

/// The NFSv4 limits this shard's listener runs under (§4.6 A-35, `config::nfs_v4_caps`): the lease the
/// operator's failover SLO (a client silent through a whole failover may be evicted when the table is
/// full); requests and replies up to the v3 transfer ceiling plus one compound header, and a kept reply
/// up to one compound header (a larger reply is an idempotent READ or READDIR, whose retry is resent as
/// new).
pub(crate) fn v4_limits(s: &ShardState) -> Limits {
  let caps = s.config.nfs_v4;
  let size = slates_bridge_nfs::procedures::MAX_TRANSFER + compound::COMPOUND_HEADER_BYTES;
  Limits {
    max_clients: caps.clients,
    max_sessions_per_client: crate::config::NFS_V4_SESSIONS_PER_CLIENT,
    offer: ChannelAttrs {
      header_pad: 0,
      max_request: size,
      max_response: size,
      max_response_cached: compound::COMPOUND_HEADER_BYTES,
      max_operations: size / compound::MIN_OPERATION_BYTES,
      max_requests: caps.slots,
    },
    lease_ns: s.config.failover_slo_ns,
  }
}

/// Serves an id-only NFSv4 state procedure on this shard's file state (§4.6 A-36), created on first
/// use.
fn serve_file_state_here(procedure: u32, args: &[u8]) -> Vec<u8> {
  state::with_state(|s| {
    if s.nfs_v4_files.is_none() {
      s.nfs_v4_files = crate::nfs_state::file_state(s);
    }
    let reply = s.nfs_v4_files.as_mut().map(|files| {
      slates_bridge_nfs::v4::files::serve_by_id(files, procedure, &mut XdrReader::new(args))
    })?;
    // The changes are durable before the reply leaves (§4.6 A-37).
    crate::nfs_state::record_files(s).then_some(reply)
  })
  .flatten()
  .unwrap_or_else(|| status_word(Nfsstat4::Serverfault))
}

/// A reply of only a status word.
fn status_word(status: Nfsstat4) -> Vec<u8> {
  status.wire().to_be_bytes().to_vec()
}

/// One parsed RPC call, its data owned so the read buffer can drain while the call is served. A
/// `program` of 0 marks a garbage call (no real program is 0), whose xid is unknown so the reply
/// carries 0. The requester is the mounting user (subject and groups), from the `AUTH_SYS` credential.
struct Call {
  xid: u32,
  requester: Requester,
  program: u32,
  version: u32,
  procedure: u32,
  args: Vec<u8>,
  /// The whole call's encoded size, its RPC headers included (not its record marker): what an NFSv4
  /// session's request size bounds (RFC 8881 §18.36.3).
  request_bytes: usize,
}

impl Call {
  /// The call a record body holds, or a garbage call if its header does not parse.
  fn parse(body: &[u8]) -> Call {
    match parse_call(body) {
      Ok((call, args)) => Call {
        xid: call.xid,
        requester: Requester::of(body),
        program: call.program,
        version: call.version,
        procedure: call.procedure,
        args: args.rest().to_vec(),
        request_bytes: body.len(),
      },
      Err(_) => Call {
        xid: 0,
        requester: Requester::root(),
        program: 0,
        version: 0,
        procedure: 0,
        args: Vec::new(),
        request_bytes: body.len(),
      },
    }
  }
}

/// The RPC reply payload for one deframed call message: the entry the network export's TLS session serves
/// its plaintext calls through ([`crate::nfs_tls`]), exactly as [`serve_one`] serves the loopback's.
pub(crate) async fn reply_to_message(this: u16, message: &[u8], port: u16) -> Vec<u8> {
  reply_to(this, Call::parse(message), port).await
}

/// The RPC reply payload for one call. An NFSv4 call goes to the v4 front end, which presents a
/// capability per operation inside the compound (A-35). Any other call presents its mount capability
/// (AUD-01): a `MNT`'s from its path (`<name>@<attachment>.<token>`, rewritten to the bare name for the
/// routing), every other call's from the file handle it names. It rides to the owner shard, which
/// validates it against the attachment record; no state is kept per connection, so the kernel's later
/// requests — on this connection or any other — self-authorize through the handles the mount returned.
async fn reply_to(this: u16, mut call: Call, port: u16) -> Vec<u8> {
  if call.program == NFS_PROGRAM && call.version == NFS_V4 {
    return reply_v4(this, call, port).await;
  }
  // The extension procedures (A-35) are the v4 front end's, never the wire's.
  if call.program == NFS_PROGRAM
    && !slates_bridge_nfs::procedures::is_rfc1813_procedure(call.procedure)
  {
    return reply_bytes(call.xid, AcceptStatus::ProcUnavail, &[]);
  }
  let capability = presented_capability(call.program, call.procedure, &mut call.args);
  let requester = call.requester.with_capability(capability);
  reply_for(
    this,
    call.xid,
    requester,
    call.program,
    call.version,
    call.procedure,
    call.args,
    port,
  )
  .await
}

// ------------------------------------------------------------------ a connection that outlives its daemon (A-113)

/// Derived: the most wire bytes one record of a connection may span — the largest message ([`MAX_MESSAGE`]) and its
/// one record marker. The kernel clients send each record as a single fragment (XNU `nfs_send`, Linux
/// `xs_encode_stream_record_marker`); a peer whose record spans more than a connection's receive buffer is refused,
/// since a request must sit whole in the kernel to be peeked.
const RECORD_WIRE_BYTES: usize = MAX_MESSAGE + size_of::<u32>();

/// Derived: each kernel buffer of a connection — two records: the one being served and the next arriving (receive),
/// the one leaving and the next being built (send). A connection is served one call at a time, so a deeper queue adds
/// no concurrency, only memory per connection; a shallower one could not hold a whole request beside its successor.
const CONNECTION_BUFFER_BYTES: usize = 2 * RECORD_WIRE_BYTES;

/// Counter: replies larger than a connection sends whole, answered `SYSTEM_ERR` instead (zero by construction: every
/// reply is capped at [`MAX_MESSAGE`]; a non-zero count is a bug signal).
const NFS_REPLY_PAST_BOUND: &str = "nfs.reply_past_bound";
/// Counter: runs of replies the kernel took in more than one send — moments a connection ended mid-record (zero where
/// sends are whole; a non-zero count on macOS is a bug signal).
const NFS_SEND_COMPLETED: &str = "nfs.send_completed";
/// The status refusal count of connections refused at admission: buffers the kernel would not size, a low-water mark
/// it would not set, the hold bound reached, or a hold the anchor's channel would not carry. The client reconnects.
pub(crate) const HOLD_REFUSED: &str = "nfs.connection_hold_refused";
/// The status refusal count of connections ended because their stream was not a record stream, carried a record
/// larger than the connection's receive buffer, or needed a receive low-water mark the kernel would not keep.
const STREAM_REFUSED: &str = "nfs.stream_refused";
/// The status refusal count of connection releases the anchor's channel would not carry: the anchor keeps a copy of a
/// connection this daemon ended until the next daemon finds it closed and releases it again.
const RELEASE_LOST: &str = "nfs.connection_release_lost";

/// Connections the anchor holds for this daemon, against [`crate::config::DaemonConfig::held_connection_bound`]:
/// raised at a hold, lowered at a release, on whichever shard the connection then lives. A statistic-grade word touched
/// once per connection's admission and end, never per request.
static HELD_CONNECTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

std::thread_local! {
  /// This shard's stream buffer: the bytes a peek copies out of a connection's receive queue, and the sink of a
  /// discard where the kernel cannot drop bytes itself. One per shard thread, borrowed only inside a synchronous call,
  /// never across an await, so every connection the shard serves shares it.
  static STREAM_BUFFER: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
  /// The last connection id this shard handed out ([`next_connection_id`]).
  static LAST_CONNECTION_ID: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// A connection id unique across the anchor's life: the host's monotonic clock, which every daemon under one anchor
/// shares and which never runs back (`slates_machine::clock`), made strictly increasing on this shard. Only the
/// listener's shard hands ids out, and an id a dead daemon handed out is older than any reading after it.
fn next_connection_id() -> u64 {
  LAST_CONNECTION_ID.with(|last| {
    let id = slates_machine::clock::monotonic_ns().max(last.get().saturating_add(1));
    last.set(id);
    id
  })
}

/// What ends a connection the anchor holds: its id, if held. Ending one shuts it down (so the client sees it close
/// though the anchor still holds a copy) and releases the anchor's copy.
#[derive(Clone, Copy, Debug)]
struct Ending {
  held: Option<u64>,
}

impl Ending {
  /// Releases the anchor's copy and the hold's place under the bound; without a hold, nothing.
  fn release(self) {
    let Some(connection) = self.held else {
      return;
    };
    HELD_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    if matches!(
      crate::anchor_hold::release_connection(connection),
      Some(Err(_))
    ) {
      crate::fleet::count_refusal(RELEASE_LOST);
    }
  }
}

/// Admits an accepted connection (A-113): sizes its kernel buffers to [`CONNECTION_BUFFER_BYTES`] (a request must sit
/// whole in the receive queue to be peeked), makes its sends of up to [`RECORD_WIRE_BYTES`] whole where the kernel
/// offers that, and — under a supervising anchor that can hold it — hands the anchor a duplicate before its first
/// request is read. A connection whose buffers or low-water mark the kernel refuses, or that the hold bound or the
/// channel refuses, is shut down and counted ([`HOLD_REFUSED`]); the client reconnects. Where the kernel cannot make a
/// send whole (Linux), a connection is served unheld: holding it would let a successor resume mid-record (GAPS, A-113).
fn admit(stream: TcpStream, port: u16, bound: usize) -> Option<Connection> {
  let refuse = |stream: &TcpStream| {
    crate::fleet::count_refusal(HOLD_REFUSED);
    let _ = stream.shutdown();
  };
  if stream.reserve_buffers(CONNECTION_BUFFER_BYTES).is_err() {
    refuse(&stream);
    return None;
  }
  let whole = match stream.make_sends_whole(RECORD_WIRE_BYTES) {
    Ok(whole) => whole,
    Err(_) => {
      refuse(&stream);
      return None;
    }
  };
  let held = if whole == slates_rt::tcp::WholeSends::Guaranteed && crate::anchor_hold::anchored() {
    let admitted = HELD_CONNECTIONS
      .fetch_update(
        std::sync::atomic::Ordering::Relaxed,
        std::sync::atomic::Ordering::Relaxed,
        |held| (held < bound).then_some(held.saturating_add(1)),
      )
      .is_ok();
    if !admitted {
      refuse(&stream);
      return None;
    }
    let connection = next_connection_id();
    if !matches!(
      crate::anchor_hold::hold_connection(connection, stream.as_fd()),
      Some(Ok(()))
    ) {
      HELD_CONNECTIONS.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
      refuse(&stream);
      return None;
    }
    Some(connection)
  } else {
    None
  };
  Some(Connection::new(stream, port, held))
}

/// Serves again the connections a dead daemon left (A-113), each handed back by the anchor
/// ([`crate::anchor_hold::adopt`]): the socket and its buffers and low-water marks are as the dead daemon left them;
/// every request it had not answered is still queued, since a request is consumed only after its reply's send. A
/// connection that cannot be adopted is released, so the anchor closes the last copy and the client reconnects.
pub(crate) fn adopt_held(
  handed: Vec<crate::anchor_hold::HandedConnection>,
  port: u16,
  bound: usize,
) {
  for crate::anchor_hold::HandedConnection { connection, socket } in handed {
    LAST_CONNECTION_ID.with(|last| last.set(last.get().max(connection)));
    let ending = Ending {
      held: Some(connection),
    };
    let previously = HELD_CONNECTIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let Ok(stream) = TcpStream::from_fd(socket) else {
      crate::fleet::count_refusal(HOLD_REFUSED);
      ending.release();
      continue;
    };
    if previously >= bound {
      // More were handed back than this daemon's bound allows (a smaller configuration than its predecessor's).
      crate::fleet::count_refusal(HOLD_REFUSED);
      let _ = stream.shutdown();
      ending.release();
      continue;
    }
    note(NFS_CONNECTIONS_ADOPTED);
    spawn_connection(Connection::new(stream, port, Some(connection)));
  }
}

/// Releases connections the anchor handed back that this daemon cannot serve (it has no listener): the anchor closes
/// its copy, and with the daemon's dropped here the client sees the connection close and reconnects.
pub(crate) fn release_handed(handed: Vec<crate::anchor_hold::HandedConnection>) {
  for crate::anchor_hold::HandedConnection { connection, socket } in handed {
    let _ = rustix::net::shutdown(&socket, rustix::net::Shutdown::Both);
    drop(socket);
    if matches!(
      crate::anchor_hold::release_connection(connection),
      Some(Err(_))
    ) {
      crate::fleet::count_refusal(RELEASE_LOST);
    }
  }
}

/// Counter: connections a dead daemon left that this daemon serves again (A-113).
const NFS_CONNECTIONS_ADOPTED: &str = "nfs.connections_adopted";

/// A connection's serving state, moved whole to the shard that owns the volume its calls name ([`migrate`]). The
/// requests not yet answered live in the socket's receive queue, not here (A-113): each turn peeks them, and a request
/// is consumed only once its reply is in the send queue, so the state a move carries — and a death loses — is no more
/// than the socket.
struct Connection {
  stream: TcpStream,
  port: u16,
  /// The shard that accepted the connection: where its NFSv4 client table lives (A-76).
  home: u16,
  /// An NFSv4 session moving with the connection to the shard its volume lives on (A-76), taken in there.
  carry: Option<(SessionId, DepartedSession)>,
  /// The connection's id while the anchor holds it.
  held: Option<u64>,
  /// The receive low-water mark last set, so it is set only when it changes (0: not known on this shard).
  wanted: usize,
}

impl Connection {
  fn new(stream: TcpStream, port: u16, held: Option<u64>) -> Connection {
    Connection {
      stream,
      port,
      home: registry::current_shard().unwrap_or(0),
      carry: None,
      held,
      wanted: 0,
    }
  }

  /// What ends this connection, kept apart from it for the paths that lose the socket.
  fn ending(&self) -> Ending {
    Ending { held: self.held }
  }

  /// Ends the connection while this daemon lives: shuts it down, so the client sees it close although the anchor's copy
  /// would keep it open, and releases the hold.
  fn end(self) {
    let _ = self.stream.shutdown();
    self.ending().release();
  }

  /// Sets the receive low-water mark to `bytes` if it is not already that; whether the kernel keeps it. A mark it caps
  /// below `bytes` (Linux at half of `tcp_rmem[2]`) would wake the connection before the record is whole, every time,
  /// so the caller ends the connection rather than spin.
  fn want(&mut self, bytes: usize) -> bool {
    if self.wanted == bytes {
      return true;
    }
    if self.stream.want_bytes(bytes).is_err() {
      return false;
    }
    self.wanted = bytes;
    true
  }
}

/// One record at the head of a connection's receive queue: its body, and the queue offset just past it (what a discard
/// consumes to remove it and everything before it).
struct Inbound {
  body: Vec<u8>,
  end: usize,
}

/// What a peek found at the head of a connection's receive queue.
enum Head {
  /// Whole records, in order.
  Records(Vec<Inbound>),
  /// No whole record: wait until this many bytes are queued.
  Waiting(usize),
  /// The client closed the connection.
  Ended,
  /// The bytes are not a record stream, or a record spans more than the buffer; the connection ends.
  Refused,
}

/// Peeks the head of `stream`'s receive queue into this shard's stream buffer and reads the whole records there.
fn peek_head(stream: &TcpStream) -> Head {
  STREAM_BUFFER.with(|cell| {
    let Ok(mut buffer) = cell.try_borrow_mut() else {
      // Never borrowed across an await, so never busy; a busy buffer would be a bug, refused rather than waited on.
      return Head::Refused;
    };
    if buffer.len() < CONNECTION_BUFFER_BYTES {
      buffer.resize(CONNECTION_BUFFER_BYTES, 0);
    }
    match stream.peek_now(&mut buffer) {
      Err(_) | Ok(Some(0)) => Head::Ended,
      Ok(None) => Head::Waiting(1),
      Ok(Some(queued)) => read_head(buffer.get(..queued).unwrap_or_default()),
    }
  })
}

/// The whole records at the head of `queued`, or how many bytes the first needs.
fn read_head(queued: &[u8]) -> Head {
  let mut reader = RecordReader::default();
  let mut at = 0usize;
  let mut records = Vec::new();
  while let Some(rest) = queued.get(at..).filter(|rest| !rest.is_empty()) {
    match reader.read(rest) {
      Err(_) => return Head::Refused,
      Ok((Some(body), used)) => {
        at = at.saturating_add(used);
        records.push(Inbound { body, end: at });
      }
      Ok((None, used)) => {
        at = at.saturating_add(used);
        if records.is_empty() {
          let wanted = at.saturating_add(reader.wanted());
          return if wanted > CONNECTION_BUFFER_BYTES {
            Head::Refused
          } else {
            Head::Waiting(wanted)
          };
        }
        break;
      }
    }
  }
  if records.is_empty() {
    Head::Waiting(at.saturating_add(reader.wanted()).max(1))
  } else {
    Head::Records(records)
  }
}

/// The serve loop of a connection, on whichever shard holds it now. A mount's calls name one volume, so the connection
/// moves to that volume's owner shard at its first call there and is served locally from then on: a call forwarded over
/// the bridge queue costs two cross-shard hops and two thread wakes, each of which a busy machine can delay by a
/// scheduler quantum, and every call of a mount (six RPCs for one `rename(2)` from macOS) paid them (2026-10-04: every
/// one of 185,745 calls was forwarded; their service p99 was 115 µs at rest and 1.18 ms with a spinner per core,
/// `crates/cli/examples/vfs_tails.rs`). Each turn peeks the whole records queued, serves them up to the shard's
/// quantum, sends their replies in whole records and only then consumes their requests (A-113).
/// Counter: an NFS connection the peer closed at a record boundary (`Head::Ended`).
/// Format: a counter name in the daemon's status report.
const CONNECTION_ENDED_BY_PEER: &str = "nfs.connection.ended_by_peer";
/// Counter: an NFS connection the peer closed with an unfinished record queued.
/// Format: a counter name in the daemon's status report.
const CONNECTION_CLOSED_MID_RECORD: &str = "nfs.connection.closed_mid_record";
/// Counter: an NFS connection ended because its idle wait failed, or a callback send on it failed.
/// Format: a counter name in the daemon's status report.
const CONNECTION_WAIT_FAILED: &str = "nfs.connection.wait_failed";
/// Counter: an NFS connection ended because a turn's replies could not be sent.
/// Format: a counter name in the daemon's status report.
const CONNECTION_SEND_FAILED: &str = "nfs.connection.send_failed";

async fn serve_connection(mut connection: Connection) {
  let this = registry::current_shard().unwrap_or(0);
  // The connection's back-channel outbox on this shard (`crate::callback`), released however the loop ends.
  let registration = Registration(crate::callback::register());
  // Whether the last wait ended because the socket became readable: with an unfinished record queued, that is the
  // moment to ask whether the peer closed (a peek never shows the close behind bytes it does not consume).
  let mut woken = false;
  loop {
    match peek_head(&connection.stream) {
      Head::Ended => {
        crate::fleet::count_refusal(CONNECTION_ENDED_BY_PEER);
        connection.end();
        return;
      }
      Head::Refused => {
        crate::fleet::count_refusal(STREAM_REFUSED);
        connection.end();
        return;
      }
      Head::Waiting(bytes) => {
        if woken && bytes > 1 && connection.stream.peer_closed().unwrap_or(true) {
          // The peer closed with an unfinished record queued: it never completes.
          crate::fleet::count_refusal(CONNECTION_CLOSED_MID_RECORD);
          connection.end();
          return;
        }
        if !connection.want(bytes) {
          crate::fleet::count_refusal(STREAM_REFUSED);
          connection.end();
          return;
        }
        woken = false;
        let standing = match idle(&connection, registration.0).await {
          Idle::Readable => {
            woken = true;
            true
          }
          Idle::Send(records) => send_callbacks(&connection, &records).await,
          Idle::Failed => false,
        };
        if !standing {
          crate::fleet::count_refusal(CONNECTION_WAIT_FAILED);
          connection.end();
          return;
        }
      }
      Head::Records(records) => {
        let turn = serve_turn(this, &mut connection, records, registration.0).await;
        if !send_turn(&connection, &turn).await {
          crate::fleet::count_refusal(CONNECTION_SEND_FAILED);
          connection.end();
          return;
        }
        if let Some(owner) = turn.moving {
          migrate(owner, connection);
          return;
        }
        // A ready read/write does not yield: bound a busy connection to one turn per scheduling round.
        futures::yield_now().await;
      }
    }
  }
}

/// One turn's handled records: each record's reply (empty for a callback's answer, which has none) beside the queue
/// offset just past it, and the owner shard the connection must move to before the next call (left queued).
struct Turn {
  handled: Vec<(Vec<u8>, usize)>,
  moving: Option<u16>,
}

/// Serves the whole records peeked at the head of `connection`'s queue, in order, in one turn bounded by the shard's
/// step quantum so a long pipeline still yields to the heartbeat (§4.3, D-18); the replies are gathered for whole-record
/// sends (A-74). A call naming another shard's volume ends the turn and stays queued: the owner peeks it there.
async fn serve_turn(
  this: u16,
  connection: &mut Connection,
  records: Vec<Inbound>,
  registration: Option<u64>,
) -> Turn {
  let started = futures::now_ns();
  let quantum = futures::step_budget_ns().unwrap_or(0);
  let mut turn = Turn {
    handled: Vec::new(),
    moving: None,
  };
  for Inbound { body, end } in records {
    // A reply on the back channel answers one of this shard's callbacks (RFC 8881 §2.10.3.1), never a call.
    if crate::callback::is_reply(&body) {
      crate::callback::deliver(&body);
      turn.handled.push((Vec::new(), end));
      continue;
    }
    let call = Call::parse(&body);
    let elsewhere = if is_v4_compound(&call) {
      v4_elsewhere(this, connection.home, &call)
    } else {
      owner_elsewhere(&call)
    };
    if let Some(owner) = elsewhere {
      turn.moving = Some(owner);
      break;
    }
    let xid = call.xid;
    let reply = if is_v4_compound(&call) {
      let (reply, moving) = serve_v4_on(this, connection, call, registration).await;
      turn.moving = moving;
      reply
    } else {
      reply_to(this, call, connection.port).await
    };
    turn.handled.push((bounded_record(xid, &reply), end));
    if turn.moving.is_some() || futures::now_ns().saturating_sub(started) >= quantum {
      break;
    }
  }
  turn
}

/// `reply` framed as one record, or — past [`RECORD_WIRE_BYTES`], which no reply reaches since every result is capped
/// at the transfer ceiling — a `SYSTEM_ERR` for its `xid`, counted: a connection sends only records it can send whole.
fn bounded_record(xid: u32, reply: &[u8]) -> Vec<u8> {
  let record = write_record(reply);
  if record.len() <= RECORD_WIRE_BYTES {
    return record;
  }
  note(NFS_REPLY_PAST_BOUND);
  write_record(&reply_bytes(xid, AcceptStatus::SystemErr, &[]))
}

/// Sends a turn's replies in runs of whole records no longer than [`RECORD_WIRE_BYTES`], and after each run consumes
/// the requests it answered (and any callback answers among them) from the receive queue: a request leaves the kernel
/// only once its reply is in it, so a daemon that dies anywhere here leaves every unanswered request for its successor,
/// and at worst a reply sent twice, which the client drops by its xid (RFC 5531 §9) (A-113). Whether the connection
/// still stands.
async fn send_turn(connection: &Connection, turn: &Turn) -> bool {
  let mut run: Vec<u8> = Vec::new();
  let mut run_end = 0usize;
  let mut consumed = 0usize;
  let mut answered = 0u64;
  for (reply, end) in &turn.handled {
    if !run.is_empty() && run.len().saturating_add(reply.len()) > RECORD_WIRE_BYTES {
      if !send_run(connection, &run, run_end, &mut consumed).await {
        return false;
      }
      run.clear();
    }
    run.extend_from_slice(reply);
    run_end = *end;
    if !reply.is_empty() {
      answered = answered.saturating_add(1);
    }
  }
  if run_end > consumed && !send_run(connection, &run, run_end, &mut consumed).await {
    return false;
  }
  if answered > 1 {
    let _ = state::with_state_counted(|s| {
      s.count(NFS_REPLIES_BATCHED, answered);
    });
  }
  if answered > 0 {
    // A mount's call is client activity: the shard spins out its idle window after it, so the next call of a burst is
    // read without a kernel wake (§4.7).
    registry::with_current(|ctx| ctx.note_activity());
  }
  true
}

/// Sends one run of whole records (if any), then consumes the receive queue through `through`; whether both succeeded.
async fn send_run(
  connection: &Connection,
  run: &[u8],
  through: usize,
  consumed: &mut usize,
) -> bool {
  if !run.is_empty() {
    match connection.stream.send_records(run).await {
      Ok(slates_rt::tcp::Sent::Whole) => {}
      Ok(slates_rt::tcp::Sent::Completed) => note(NFS_SEND_COMPLETED),
      Err(_) => return false,
    }
  }
  let count = through.saturating_sub(*consumed);
  let discarded = STREAM_BUFFER.with(|cell| {
    cell
      .try_borrow_mut()
      .ok()
      .is_some_and(|mut buffer| connection.stream.discard(count, &mut buffer).is_ok())
  });
  *consumed = through;
  discarded
}

/// Sends callback records queued for this connection, each run whole; whether the connection still stands.
async fn send_callbacks(connection: &Connection, records: &[Vec<u8>]) -> bool {
  let mut run: Vec<u8> = Vec::new();
  for record in records {
    if !run.is_empty() && run.len().saturating_add(record.len()) > RECORD_WIRE_BYTES {
      if connection.stream.send_records(&run).await.is_err() {
        return false;
      }
      run.clear();
    }
    run.extend_from_slice(record);
  }
  run.is_empty() || connection.stream.send_records(&run).await.is_ok()
}

/// A connection's registration in its shard's back-channel table, ended when the serve loop ends (any path, a
/// cancellation included), so the callbacks waiting on it fail `Lost` rather than wait out their deadline.
struct Registration(Option<u64>);

impl Drop for Registration {
  fn drop(&mut self) {
    if let Some(id) = self.0 {
      crate::callback::unregister(id);
    }
  }
}

/// What woke an idle connection: bytes from the client (or its close), callbacks to send it, or a failed wait.
enum Idle {
  Readable,
  Send(Vec<Vec<u8>>),
  Failed,
}

/// Waits for the client's next bytes (as many as the low-water mark asks) or for a callback queued on this connection,
/// whichever comes first.
async fn idle(connection: &Connection, registration: Option<u64>) -> Idle {
  let mut readable = std::pin::pin!(connection.stream.wait_readable());
  std::future::poll_fn(|cx| {
    if let Some(id) = registration {
      let outbound = crate::callback::take_outbound(id, cx.waker());
      if !outbound.is_empty() {
        return std::task::Poll::Ready(Idle::Send(outbound));
      }
    }
    readable.as_mut().poll(cx).map(|waited| match waited {
      Ok(()) => Idle::Readable,
      Err(_) => Idle::Failed,
    })
  })
  .await
}

/// Serves one NFSv4 compound on `connection` (A-76, A-77): binds its session's back channel to the connection, serves
/// it, settles the notes it owes, and departs its session for the owner shard its calls were forwarded to; the reply,
/// and the shard the connection must move to after it.
async fn serve_v4_on(
  this: u16,
  connection: &mut Connection,
  call: Call,
  registration: Option<u64>,
) -> (Vec<u8>, Option<u16>) {
  let placement = compound::placement(&call.args);
  if let (compound::Placement::Session(sessionid), Some(id)) = (placement, registration) {
    bind_back_channel(sessionid, id, compound::minor_version(&call.args));
  }
  let (reply, forwarded_to) = serve_v4(this, connection.home, call, connection.port).await;
  v4_after(this, connection.home);
  (
    reply,
    depart_with(this, connection, placement, forwarded_to),
  )
}

/// Binds connection `id` as the carrier of `sessionid`'s back channel when the session has one (its compounds
/// arrive on the connection its client keeps the back channel on).
fn bind_back_channel(sessionid: SessionId, id: u64, minor: Option<u32>) {
  let Some(back) = with_v4_sessions(|sessions| sessions.back_channel(&sessionid)).flatten() else {
    return;
  };
  if let Some(minor) = minor.filter(|&minor| minor != back.minor) {
    with_v4_sessions(|sessions| sessions.set_callback_minor(&sessionid, minor));
  }
  crate::callback::carry(sessionid, id);
  if back.state == CallbackState::Unproven {
    // Probed once before the first use (RFC 8881 §10.2): marked down while the probe runs, so a second compound does
    // not start another, and up only when the client answers.
    with_v4_sessions(|sessions| sessions.set_callback_state(&sessionid, CallbackState::Down));
    match futures::spawn(probe_back_channel(sessionid)) {
      Ok(task) => {
        let _ = futures::detach(task);
      }
      Err(_) => note(NFS4_CALLBACK_DOWN),
    }
  }
}

/// Probes `sessionid`'s back channel with `CB_SEQUENCE` alone and records whether the client answered `NFS4_OK`
/// within a liveness budget.
async fn probe_back_channel(sessionid: SessionId) {
  let Some(next) = with_v4_sessions(|sessions| sessions.next_callback(&sessionid)).flatten() else {
    return;
  };
  let args = slates_bridge_nfs::v4::callback::probe(&sessionid, next.minor, next.sequence);
  let outcome = crate::callback::call(
    sessionid,
    (next.program, &next.credential),
    &args,
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await;
  if let Err(error) = outcome {
    note(match error {
      crate::callback::CallbackError::Refused => NFS4_CALLBACK_REFUSED,
      crate::callback::CallbackError::Timeout => NFS4_CALLBACK_TIMEOUT,
      _ => NFS4_CALLBACK_UNSENT,
    });
  }
  let answered = outcome
    .ok()
    .and_then(|results| slates_bridge_nfs::v4::callback::status(&results))
    == Some(Nfsstat4::Ok as u32);
  let state = if answered {
    note(NFS4_CALLBACK_UP);
    CallbackState::Up
  } else {
    note(NFS4_CALLBACK_DOWN);
    CallbackState::Down
  };
  with_v4_sessions(|sessions| sessions.set_callback_state(&sessionid, state));
}

/// Counter: replies that left in a write carrying more than one (§4.14): the batched serve's non-vacuity count.
const NFS_REPLIES_BATCHED: &str = "nfs.replies.batched";

/// The shard a call should be served on when it is not this one: an NFSv3 call naming a volume another shard
/// owns. A `MOUNT` call, an NFSv4 call (its session state is this shard's) and a host-root listing stay here, as
/// does a call this shard owns.
fn owner_elsewhere(call: &Call) -> Option<u16> {
  if call.program != NFS_PROGRAM
    || call.version != NFS_VERSION
    || !slates_bridge_nfs::procedures::is_rfc1813_procedure(call.procedure)
    || is_root_listing(call.program, call.procedure, &call.args)
  {
    return None;
  }
  route(call.program, call.procedure, &call.args)
}

/// Moves `connection` to `owner`, where the call that named it — still at the head of the receive queue (A-113) — is
/// peeked and served first, and the loop goes on. The move is a spawn on the owner's control channel carrying the
/// descriptor (sharing by move); refused there, the connection is closed and its hold released, and counted, and the
/// kernel client reconnects, as at a refused accept.
fn migrate(owner: u16, connection: Connection) {
  let ending = connection.ending();
  let Connection {
    stream,
    port,
    home,
    carry,
    held,
    ..
  } = connection;
  let fd = stream.into_fd();
  // A copy of the session stays behind for the refusal path: a move refused before it left returns the session to its
  // home table, so the home never points at a session no shard holds.
  let kept = carry.clone();
  let arrive = slates_rt::task::SpawnRequest::new(
    Box::pin(async move {
      // The session is taken in before the socket is adopted, so a failed adoption leaves it where its home says.
      if let Some((sessionid, departed)) = carry {
        adopt_session(sessionid, departed);
      }
      let Ok(stream) = TcpStream::from_fd(fd) else {
        crate::fleet::count_refusal(MIGRATE_REFUSED);
        ending.release();
        return;
      };
      let connection = Connection {
        stream,
        port,
        home,
        carry: None,
        held,
        wanted: 0,
      };
      spawn_connection(connection);
    }),
    None,
  );
  if registry::send_control(owner, slates_rt::control::Control::Spawn(Box::new(arrive))).is_err() {
    crate::fleet::count_refusal(MIGRATE_REFUSED);
    // The refused spawn dropped the descriptor; releasing the anchor's copy closes the connection.
    ending.release();
    if let Some((sessionid, departed)) = kept {
      let _ = state::with_state_counted(|s| {
        if let Some(server) = s.nfs_v4.as_mut() {
          server.sessions.return_home(sessionid, departed);
        }
      });
    }
  }
}

/// The status refusal count under which a connection's move to its volume's owner shard was refused (the
/// owner's control channel full, or the descriptor not adoptable there): the connection is closed and the
/// kernel client reconnects.
const MIGRATE_REFUSED: &str = "nfs.connection_move_refused";

#[cfg(test)]
mod tests {
  use super::*;

  /// AC-2.6 / T-4.3: queue two complete RPC calls on one connection. Serving the first must
  /// give another task a turn before the second, even though neither socket direction blocks.
  #[test]
  fn queued_rpc_calls_yield_between_replies() {
    use std::future::Future;
    use std::io::{Read, Write};
    use std::task::{Context, Poll, Waker};

    let listener = TcpListener::bind(
      slates_rt::tcp::SocketAddrV4::new(slates_rt::tcp::Ipv4Addr::LOCALHOST, 0),
      1,
    )
    .unwrap();
    let address = listener.local_addr().unwrap();
    let mut client = std::net::TcpStream::connect(address).unwrap();
    client
      .set_read_timeout(Some(std::time::Duration::from_nanos(
        crate::daemon::LIVENESS_BUDGET_NS,
      )))
      .unwrap();
    let mut requests = Vec::new();
    for xid in [1u32, 2] {
      let mut body = Vec::new();
      // ONC RPC call, version 2; NFS version 3 NULL; AUTH_NONE credential and verifier.
      for field in [xid, 0, 2, NFS_PROGRAM, 3, 0, 0, 0, 0, 0] {
        body.extend_from_slice(&field.to_be_bytes());
      }
      requests.extend_from_slice(&write_record(&body));
    }
    client.write_all(&requests).unwrap();
    client.shutdown(std::net::Shutdown::Write).unwrap();
    let mut context = Context::from_waker(Waker::noop());
    let Poll::Ready(Ok(stream)) = std::pin::pin!(listener.accept()).poll(&mut context) else {
      panic!("the established connection must be ready to accept");
    };
    let mut server = std::pin::pin!(serve_connection(Connection::new(
      stream,
      address.port(),
      None
    )));
    for xid in [1u32, 2] {
      assert!(
        server.as_mut().poll(&mut context).is_pending(),
        "the connection drained the next request without yielding"
      );
      let expected = write_record(&reply_bytes(xid, AcceptStatus::Success, &[]));
      let mut received = vec![0; expected.len()];
      client.read_exact(&mut received).unwrap();
      assert_eq!(received, expected);
      client.set_nonblocking(true).unwrap();
      assert_eq!(
        client.peek(&mut [0]).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "the next RPC has not run yet"
      );
      client.set_nonblocking(false).unwrap();
    }
    assert!(server.as_mut().poll(&mut context).is_ready());
  }

  /// Encodes a `MNT` `dirpath` argument (an XDR string) for a test.
  fn mnt_args(path: &str) -> Vec<u8> {
    let mut writer = XdrWriter::new();
    writer.opaque(path.as_bytes());
    writer.into_bytes()
  }

  /// A well-formed capability token, as 32 lowercase hex digits.
  fn sample_token_hex() -> String {
    [0xABu8; 16].iter().map(|b| format!("{b:02x}")).collect()
  }

  /// The mount-capability parser reads a well-formed `<name>@<attachment_hex>.<token_hex>`, and the
  /// host root scoped to a capability (`/@<capability>`, an empty name).
  #[test]
  fn the_mount_capability_parser_reads_a_capability() {
    let token_hex = sample_token_hex();
    let parsed = split_mount_capability(&mnt_args(&format!("/vol@1a.{token_hex}")));
    assert_eq!(parsed, Some(("vol".to_owned(), (0x1a, [0xABu8; 16]))));
    assert_eq!(
      split_mount_capability(&mnt_args(&format!("/@1a.{token_hex}"))),
      Some((String::new(), (0x1a, [0xABu8; 16])))
    );
  }

  /// The parser refuses a path with no capability, a short or long token, a non-hex attachment or
  /// token, an empty token, a missing `.` separator, and truncated XDR — never panicking on a hostile
  /// mount path (AUD-01; a parser of external bytes).
  #[test]
  fn the_mount_capability_parser_refuses_malformed_paths() {
    let token_hex = sample_token_hex();
    let non_hex: String = std::iter::repeat_n('g', 32).collect();
    let malformed = [
      "/vol".to_owned(),                // a plain name: no capability
      "/".to_owned(),                   // the bare root: no capability
      "/vol@1a.abcd".to_owned(),        // a short token
      format!("/vol@1a.{token_hex}ff"), // a long token
      format!("/vol@zz.{token_hex}"),   // a non-hex attachment id
      format!("/vol@1a.{non_hex}"),     // a non-hex token
      "/vol@1a.".to_owned(),            // an empty token
      "/vol@1a".to_owned(),             // no `.` separator
    ];
    for path in malformed {
      assert_eq!(
        split_mount_capability(&mnt_args(&path)),
        None,
        "{path} is refused"
      );
    }
    // Truncated XDR (not a valid string) is refused, not panicked.
    assert_eq!(split_mount_capability(&[0xff, 0xff, 0xff, 0xff]), None);
  }

  /// AC-2.12 / T-2.14, AUD-05: omit a touched volume from a committed image. The NFS caller
  /// must receive an error instead of a stable successful mutation; captured volumes still succeed.
  #[test]
  fn an_omitted_volume_cannot_receive_a_stable_reply() {
    let omitted = VolumeId { bytes: [1; 16] };
    let captured = VolumeId { bytes: [2; 16] };
    let published = crate::verbs::Published {
      volumes: vec![captured],
      skipped: vec![omitted],
      destroying: Vec::new(),
      frame_bytes: 1,
    };
    let success = || {
      (
        AcceptStatus::Success,
        (Nfsstat3::Ok as u32).to_be_bytes().to_vec(),
      )
    };
    let (_, refused) = publication_reply(
      NFSPROC3_COMMIT,
      Some(omitted),
      Some(Ok(published.clone())),
      success(),
    );
    assert!(
      !nfs_ok(&refused),
      "an omitted volume was acknowledged as stable"
    );
    let (_, accepted) = publication_reply(
      NFSPROC3_COMMIT,
      Some(captured),
      Some(Ok(published)),
      success(),
    );
    assert!(nfs_ok(&accepted));
  }

  fn runtime() -> slates_rt::sim::SimRuntime {
    slates_rt::sim::SimRuntime::new(
      &slates_rt::runtime::RuntimeConfig {
        shards: 2,
        tasks_per_shard: 2,
        timers_per_shard: 2,
        ring_entries: 8,
        step_budget_ns: 1_000_000,
        timer_tick_ns: 100_000,
        batch: 8,
        pin: false,
        cores: Vec::new(),
        page_bytes: 4096,
        spin_ns: 0,
        wake_tracking: None,
      },
      17,
    )
    .unwrap()
  }

  /// T-4.3, AC-2.6, §4.3; AUD-04: the control queue admits a bridge request while either the
  /// owner's or the reply's task arena is full. Expect an RPC system error within the caller's
  /// budget, never a stuck call. Neither fault involves control-queue saturation.
  #[test]
  fn an_arena_refusal_completes_the_bridge_call_with_an_error() {
    for refuse_owner in [true, false] {
      let mut runtime = runtime();
      let shards = runtime.shard_ids();
      let origin = shards[0];
      let owner = shards[1];
      if refuse_owner {
        for _ in 0..2 {
          runtime.spawn_on(owner, std::future::pending()).unwrap();
        }
      } else {
        runtime.spawn_on(origin, std::future::pending()).unwrap();
      }
      let (sent, received) = std::sync::mpsc::sync_channel(1);
      runtime
        .context(origin)
        .unwrap()
        .spawn_local(
          Box::pin(async move {
            let reply = serve_remote(
              owner.0,
              origin.0,
              Requester::root(),
              1,
              NFS_PROGRAM,
              0,
              Vec::new(),
              0,
            )
            .await;
            sent.try_send(reply).unwrap();
          }),
          None,
        )
        .unwrap();
      runtime.run_until_idle();
      let (status, _) = received
        .try_recv()
        .expect("the bridge call completed despite refused task admission");
      assert!(matches!(status, AcceptStatus::SystemErr));
      assert!(runtime.now_ns() <= crate::daemon::LIVENESS_BUDGET_NS + 100_000);
    }
  }

  /// T-4.3, §4.8 lookup; AUD-04: a root gather names a shard that has gone away. Expect refusal
  /// of the whole gather, not a successful listing silently omitting that shard's volumes.
  #[test]
  fn a_missing_shard_refuses_the_whole_root_listing() {
    let mut runtime = runtime();
    let origin = runtime.shard_ids()[0];
    let (sent, received) = std::sync::mpsc::sync_channel(1);
    runtime
      .context(origin)
      .unwrap()
      .spawn_local(
        Box::pin(async move {
          let entries = gather_entries(origin.0, &[u16::MAX], None).await;
          sent.try_send(entries).unwrap();
        }),
        None,
      )
      .unwrap();
    runtime.run_until_idle();
    assert_eq!(received.try_recv().unwrap(), None);
  }
}
