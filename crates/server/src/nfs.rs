//! The daemon's NFS transport (§4.6): one loopback listener whose connections serve the daemon's own
//! volumes over the shared operation layer, so a `mount_nfs localhost:PORT` reaches every volume this
//! daemon provisioned — the signing-free macOS mount path (D-O9), run against real state.
//!
//! A volume lives on its owning shard (`§4.8`: ids route to owners; a volume id names its owner
//! *partition*, [`crate::verbs::owner_of`], which the daemon maps to a shard). The connection is
//! accepted on the control shard, so a request for a volume that shard owns is served locally, and a
//! request for a volume on another shard is routed over the **cross-shard bridge queue** to its owner
//! and the reply routed back:
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
//! The whole browse spans shards: `mount /<name>` (or `/` for the host root), `ls /` (a `READDIR` of
//! the host root scatters an entry-gather to every shard and lists every volume by its friendly name),
//! `cd <name>` (a root `LOOKUP` routes across shards by `owner_of_name`), read/write. Each request runs
//! as the mounting user ([`subject_of`] reads the uid from the `AUTH_SYS` credential, §4.13; `AUTH_NONE`
//! falls back to root).
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
//! port is stable across restarts; a standalone daemon (tests) still binds its own. Owed here (one
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
//! The §4.6 differential oracle (line 1368) is *not* owed here: it
//! mounts the same volume via FSKit *and* via NFS and compares the abstract states — two real
//! kernel mounts — so it is gated on the FSKit mount, hence on the Apple Developer entitlement that
//! item (1) needs and this sandbox cannot hold. A synthetic FUSE-dispatch-vs-NFS-dispatch stand-in
//! would not be it: both legs dispatch onto one `VolumeBridge`, so their agreement is tautological
//! (R5: no vacuous oracle).

use slates_bridge_core::{Rights, VolumeBridge, new_handle_store};
use slates_bridge_nfs::mount::{MOUNT_PROGRAM, MOUNTPROC3_MNT};
use slates_bridge_nfs::nfs::{Fattr3, Nfsfh3};
use slates_bridge_nfs::procedures::{
  Export, NFS_MAXNAMELEN, NFS_PROGRAM, NFSPROC3_COMMIT, NFSPROC3_CREATE, NFSPROC3_LINK,
  NFSPROC3_LOOKUP, NFSPROC3_MKDIR, NFSPROC3_MKNOD, NFSPROC3_READDIR, NFSPROC3_READDIRPLUS,
  NFSPROC3_REMOVE, NFSPROC3_RENAME, NFSPROC3_RMDIR, NFSPROC3_SETATTR, NFSPROC3_SYMLINK,
  NFSPROC3_WRITE, io_failure_reply, is_unstable, status_failure_reply, write_stable_how,
};
use slates_bridge_nfs::rpc::RecordReader;
use slates_bridge_nfs::xdr::XdrReader;
use slates_bridge_nfs::{
  AcceptStatus, MultiExport, Nfsstat3, UnixGroups, VolumeSet, auth_sys_identity, parse_call,
  reply_bytes, request_volume, root_volume, serve_call, write_record,
};
use slates_db::catalog::{Principal, VolumeId};
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
const RECORD_CHUNK: usize = 1 << 16;
/// Format: the largest mount path the router reads before deciding a route (RFC 1813 `MNTPATHLEN`).
const MNT_PATH_MAX: usize = 1024;

// ---------------------------------------------------------------------------- the shard's volumes

/// The shard's volumes as an NFS [`VolumeSet`]: resolved through [`state::with_state`], each served by
/// a transient bridge over the shard's store and the routed volume's slot. A zero-size handle — the
/// state it reads is the current shard's, so it stays thread-local. On the owner shard of a routed
/// request (reached over the bridge queue), `with_state` is the owner's state, which holds the volume.
struct ShardVolumeSet;

impl VolumeSet for ShardVolumeSet {
  fn entries(&self) -> Vec<(String, VolumeId)> {
    state::with_state(|s| {
      // List each volume under its provisioned mount name (its slot's `name`), so `ls /` shows
      // friendly names and `cd <name>`/`mount /<name>` resolve by matching it (`MultiExport`).
      let mut out = Vec::with_capacity(s.by_id.len());
      for (id, &handle) in &s.by_id {
        if let Ok(slot) = s.volumes.get(handle) {
          out.push((slot.name.clone(), *id));
        }
      }
      out
    })
    .unwrap_or_default()
  }

  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    groups: Option<UnixGroups>,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    state::with_state(|s| {
      if !s.consensus_ready {
        return Some(io_failure_reply(procedure));
      }
      // The owner-lease gate (§4.8 "Leases and reads"; AUD-08): the mount serves the volume's **live tree**
      // — its latest state — so while this node's authority over the object is unconfirmed (cut off, paused
      // past the lease bound, or superseded by a newer configuration) every procedure answers `NFS3ERR_JUKEBOX`,
      // the retry-later status, rather than a stale view a successor may have advanced. This runs on the
      // owner shard (`with_export` serves only a volume this shard holds), so the lease read here is the
      // owner's; a client mounting elsewhere reaches this owner through `serve_remote`.
      if crate::verbs::lease_unconfirmed(s, ObjectId(volume.bytes)).is_some() {
        return Some(Some(status_failure_reply(Nfsstat3::Jukebox, procedure)));
      }
      with_export(s, volume, subject, rights, groups, |export| {
        export.serve_nfs(procedure, args)
      })
    })
    .flatten()
    .flatten()
  }

  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    groups: Option<UnixGroups>,
  ) -> Option<(Nfsfh3, Fattr3)> {
    state::with_state(|s| {
      with_export(s, volume, subject, rights, groups, |export| {
        export.root_object()
      })
    })
    .flatten()
    .flatten()
  }
}

/// Builds a transient export for `volume` over the shard's store and the volume's slot, and runs `f`
/// with it; `None` if the shard does not hold the volume or its attachment cannot be admitted. The
/// handle slab is fresh per request — NFS keeps no open state across requests — and the volume's base
/// host (for an overlay) is lent from the slot, both through [`VolumeBridge::attached`].
fn with_export<R>(
  s: &mut ShardState,
  volume: VolumeId,
  subject: Principal,
  rights: Rights,
  groups: Option<UnixGroups>,
  f: impl FnOnce(&mut Export<'_>) -> R,
) -> Option<R> {
  let handle = *s.by_id.get(&volume)?;
  let write_verifier = s.write_verifier;
  let ShardState { store, volumes, .. } = s;
  let slot = volumes.get_mut(handle).ok()?;
  let mut handles = new_handle_store();
  let mut bridge = VolumeBridge::attached(
    volume,
    &mut slot.volume,
    store,
    &mut handles,
    slot.host.as_mut().map(|host| host as &mut dyn HostFs),
  );
  let mut export = Export::new(&mut bridge, volume, subject, rights).ok()?;
  export.set_groups(groups);
  // The per-boot write verifier (§4.6, RFC 1813 §3.3.7): a client compares it across a restart to
  // learn its unstable writes were lost and re-send them.
  export.set_write_verifier(write_verifier);
  Some(f(&mut export))
}

/// Whether a served call's effect must be in the shard's recovery image before its reply goes out
/// (the §4.8 barrier, D-18): a mutating NFS procedure that succeeded — every one except an `UNSTABLE`
/// write, which its client makes stable with a later COMMIT (itself a barrier). A refused call
/// changed nothing, and a read never needs one.
fn needs_barrier(program: u32, procedure: u32, args: &[u8], results: &[u8]) -> bool {
  if program != NFS_PROGRAM || !nfs_ok(results) {
    return false;
  }
  match procedure {
    NFSPROC3_WRITE => {
      write_stable_how(&mut XdrReader::new(args)).is_none_or(|stable| !is_unstable(stable))
    }
    NFSPROC3_SETATTR | NFSPROC3_CREATE | NFSPROC3_MKDIR | NFSPROC3_SYMLINK | NFSPROC3_MKNOD
    | NFSPROC3_REMOVE | NFSPROC3_RMDIR | NFSPROC3_RENAME | NFSPROC3_LINK | NFSPROC3_COMMIT => true,
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
/// survival). A publish that committed without the touched `volume` (a volume the image cannot yet
/// hold: an overlay with base-backed inodes, whose recovery is the owed base gate) is counted as an
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
}

impl Requester {
  /// The mounting user a call runs as, from its `AUTH_SYS` credential (uid and groups, §4.13, set by
  /// the kernel on a loopback mount — so a real `mount_nfs` runs as the mounting user and stamps the
  /// user's own group, not root:wheel), or the machine root when the call carries no such credential
  /// (`AUTH_NONE`).
  fn of(body: &[u8]) -> Requester {
    match auth_sys_identity(body) {
      Some(identity) => Requester {
        subject: Principal::Uid { uid: identity.uid },
        groups: Some(identity.groups),
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
    }
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
    MOUNT_PROGRAM if procedure == MOUNTPROC3_MNT => {
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
  let request = RequestId {
    client: u32::from(port),
    sequence: xid,
  };
  let open = state::with_state(|s| {
    let start_ns = s.clock.monotonic_ns();
    s.tracer
      .open_root(request, Chokepoint::BridgeRequest, start_ns)
  });
  let mut service = MultiExport::new(
    ShardVolumeSet,
    requester.subject,
    mount_rights(),
    requester.groups,
  );
  let served = serve_call(
    &mut service,
    program,
    procedure,
    &mut XdrReader::new(args),
    port,
  );
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
    let _ = state::with_state(|s| {
      let end_ns = s.clock.monotonic_ns();
      crate::telemetry::emit(s, open.end(procedure, end_ns));
    });
  }
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
}

impl VolumeSet for GatheredVolumeSet {
  fn entries(&self) -> Vec<(String, VolumeId)> {
    self.entries.clone()
  }

  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    groups: Option<UnixGroups>,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    ShardVolumeSet.serve(volume, subject, rights, groups, procedure, args)
  }

  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    groups: Option<UnixGroups>,
  ) -> Option<(Nfsfh3, Fattr3)> {
    ShardVolumeSet.root_object(volume, subject, rights, groups)
  }
}

/// Gathers every owner's entries under one liveness budget (§4.8 lookup). Any failed admission,
/// missing reply or timeout refuses the entire listing. Calls own their registrations, so early return
/// or cancellation also releases the gathers still in flight (AUD-04/AUD-17).
async fn gather_all_entries() -> Option<Vec<(String, VolumeId)>> {
  let (origin, shards) = state::with_state(|s| (s.shard, s.shards.clone()))?;
  gather_entries(origin, &shards).await
}

async fn gather_entries(origin: u16, shards: &[u16]) -> Option<Vec<(String, VolumeId)>> {
  let deadline = futures::now_ns().saturating_add(crate::daemon::LIVENESS_BUDGET_NS);
  let calls = shards
    .iter()
    .map(|shard| {
      crate::xshard::call_on(origin, *shard, || {
        state::with_state(|_| ())?;
        Some(ShardVolumeSet.entries())
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
  let mut service = MultiExport::new(
    GatheredVolumeSet { entries },
    requester.subject,
    mount_rights(),
    requester.groups,
  );
  serve_call(
    &mut service,
    NFS_PROGRAM,
    procedure,
    &mut XdrReader::new(args),
    port,
  )
}

// ------------------------------------------------------------------------------------ the serve loop

/// Serves NFS/MOUNT/portmap over `listener` on the current shard until the daemon stops: each
/// connection becomes a detached task, so connections are concurrent. `port` answers a portmap
/// `GETPORT`.
pub async fn serve(listener: TcpListener, port: u16) {
  while let Ok(stream) = listener.accept().await {
    match futures::spawn(serve_one(stream, port)) {
      Ok(task) => {
        let _ = futures::detach(task);
      }
      // The arena refused the connection's serve task: the connection is dropped (the kernel client
      // reconnects) and the refusal counted, never silent (banned item 9).
      Err(_) => {
        crate::fleet::count_refusal(SERVE_SPAWN_REFUSED);
      }
    }
  }
}

/// The status refusal count under which the mount listener records a connection whose serve task the
/// shard's arena refused.
/// Format: a refusal name in the daemon's status report, alongside the verbs' refusal kinds.
const SERVE_SPAWN_REFUSED: &str = "nfs.serve_spawn";

/// Produces the RPC reply payload for one parsed call, routing it locally, to a volume's owner shard
/// over the bridge queue, or (a host-root listing) across every shard, all as `requester` (the
/// mounting user, §4.13). A `program` of 0 is a garbage call whose reply carries xid 0.
async fn reply_for(
  this: u16,
  xid: u32,
  requester: Requester,
  program: u32,
  procedure: u32,
  args: Vec<u8>,
  port: u16,
) -> Vec<u8> {
  if program == 0 {
    return reply_bytes(0, AcceptStatus::GarbageArgs, &[]);
  }
  if is_root_listing(program, procedure, &args) {
    // The host root lists every shard's volumes, gathered over the bridge queue.
    let Some(entries) = gather_all_entries().await else {
      return reply_bytes(xid, AcceptStatus::SystemErr, &[]);
    };
    let (status, results) = serve_root_listing(requester, procedure, &args, entries, port);
    return reply_bytes(xid, status, &results);
  }
  let (status, results) = match route(program, procedure, &args) {
    Some(owner) => serve_remote(owner, this, requester, xid, program, procedure, args, port).await,
    None => serve_local(requester, xid, program, procedure, &args, port),
  };
  reply_bytes(xid, status, &results)
}

/// Serves one accepted connection: reads RPC records, routes each call, and writes the framed reply,
/// until the client closes the connection. Owned request data (including the mounting user from the
/// `AUTH_SYS` credential, §4.13) is taken out before the serve, so the read buffer is free to drain
/// while a remote call awaits its owner.
async fn serve_one(stream: TcpStream, port: u16) {
  let this = registry::current_shard().unwrap_or(0);
  let mut buffer: Vec<u8> = Vec::new();
  let mut records = RecordReader::default();
  let mut chunk = [0u8; RECORD_CHUNK];
  loop {
    // (xid, requester, program, procedure, args, consumed); a program of 0 marks a garbage call (no
    // real program is 0), whose xid is unknown so the reply carries 0. The requester is the mounting
    // user (subject and group), read from the one `AUTH_SYS` credential.
    let parsed: Option<(u32, Requester, u32, u32, Vec<u8>, usize)> = match records.read(&buffer) {
      Ok((Some(body), consumed)) => match parse_call(&body) {
        Ok((call, args)) => Some((
          call.xid,
          Requester::of(&body),
          call.program,
          call.procedure,
          args.rest().to_vec(),
          consumed,
        )),
        Err(_) => Some((0, Requester::root(), 0, 0, Vec::new(), consumed)),
      },
      Ok((None, consumed)) => {
        buffer.drain(..consumed);
        None
      }
      Err(_) => return,
    };
    match parsed {
      Some((xid, requester, program, procedure, args, consumed)) => {
        let reply = reply_for(this, xid, requester, program, procedure, args, port).await;
        if stream.write_all(&write_record(&reply)).await.is_err() {
          return;
        }
        buffer.drain(..consumed);
      }
      None => match stream.read(&mut chunk).await {
        Ok(0) | Err(_) => return,
        Ok(count) => buffer.extend_from_slice(&chunk[..count]),
      },
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// AC-2.12 / T-2.14, AUD-05: omit a touched volume from a committed image. The NFS caller
  /// must receive an error instead of a stable successful mutation; captured volumes still succeed.
  #[test]
  fn an_omitted_volume_cannot_receive_a_stable_reply() {
    let omitted = VolumeId { bytes: [1; 16] };
    let captured = VolumeId { bytes: [2; 16] };
    let published = crate::verbs::Published {
      volumes: vec![captured],
      skipped: vec![omitted],
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
          let entries = gather_entries(origin.0, &[u16::MAX]).await;
          sent.try_send(entries).unwrap();
        }),
        None,
      )
      .unwrap();
    runtime.run_until_idle();
    assert_eq!(received.try_recv().unwrap(), None);
  }
}
