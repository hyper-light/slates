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
//! Owed: the synthetic root's *listing* still shows only the accepting shard's volumes (a cross-shard
//! scatter for the full host root is owed; a mount of a specific volume by id works across shards
//! now). Per-mount authentication (§4.13) is owed — every request runs as root read-write; a friendly
//! chosen-path mount name (§4.6 "Chosen path") is owed — the id's hex is the name today; and the
//! anchor-held listener for restart survival is owed — the daemon binds it now.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

use slates_bridge_core::{Rights, VolumeBridge, new_handle_store};
use slates_bridge_nfs::mount::{MOUNT_PROGRAM, MOUNTPROC3_MNT};
use slates_bridge_nfs::nfs::{Fattr3, Nfsfh3};
use slates_bridge_nfs::procedures::{
  Export, NFS_MAXNAMELEN, NFS_PROGRAM, NFSPROC3_LOOKUP, NFSPROC3_READDIR, NFSPROC3_READDIRPLUS,
};
use slates_bridge_nfs::xdr::XdrReader;
use slates_bridge_nfs::{
  AcceptStatus, MultiExport, RpcError, VolumeSet, parse_call, read_record, reply_bytes,
  request_volume, root_volume, serve_call, write_record,
};
use slates_db::catalog::{Principal, VolumeId};
use slates_rt::control::Control;
use slates_rt::task::SpawnRequest;
use slates_rt::tcp::{TcpListener, TcpStream};
use slates_rt::{futures, registry};

use crate::state::{self, ShardState};
use crate::verbs::owner_of;

/// Shape: bytes read from a connection per `read` when more of an RPC record is needed (see the
/// blocking server in bridge-nfs for the reasoning): one large transfer fits, the assembler stitches
/// any split, so this bounds syscalls per record, not correctness.
const RECORD_CHUNK: usize = 1 << 16;
/// Format: the base of a hexadecimal digit, for parsing a volume id out of a mount path.
const HEX_RADIX: u32 = 16;
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
    state::with_state(|s| s.by_id.keys().map(|id| (hex(id), *id)).collect()).unwrap_or_default()
  }

  fn serve(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    state::with_state(|s| {
      with_export(s, volume, subject, rights, |export| {
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
  ) -> Option<(Nfsfh3, Fattr3)> {
    state::with_state(|s| with_export(s, volume, subject, rights, |export| export.root_object()))
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
  f: impl FnOnce(&mut Export<'_>) -> R,
) -> Option<R> {
  let handle = *s.by_id.get(&volume)?;
  let ShardState { store, volumes, .. } = s;
  let slot = volumes.get_mut(handle).ok()?;
  let mut handles = new_handle_store();
  let mut bridge = VolumeBridge::attached(
    volume,
    &mut slot.volume,
    store,
    &mut handles,
    slot.host.as_mut(),
  );
  let mut export = Export::new(&mut bridge, volume, subject, rights).ok()?;
  Some(f(&mut export))
}

/// The mount name a volume appears under in the root listing: its id in hex. A friendly chosen-path
/// name (§4.6 "Chosen path") is owed; the id is unique and stable, so `ls /` and `cd <id>` work today.
fn hex(id: &VolumeId) -> String {
  use std::fmt::Write as _;
  let mut out = String::with_capacity(id.bytes.len().saturating_mul(2));
  for byte in id.bytes {
    // Two lowercase hex digits per byte; the write into a String cannot fail.
    let _ = write!(out, "{byte:02x}");
  }
  out
}

/// The credentials every NFS request runs under until per-mount authentication lands (§4.13): the
/// machine's root, read-write, so a mount can read and write the volumes it reaches.
fn mount_rights() -> Rights {
  Rights {
    read: true,
    write: true,
  }
}

/// The volume id encoded in a 32-hex-digit mount name, or `None` if the name is not one (the root, or
/// a not-yet-supported friendly name).
fn parse_hex(name: &str) -> Option<VolumeId> {
  let bytes: Vec<u8> = (0..name.len())
    .step_by(2)
    .map(|index| {
      name
        .get(index..index + 2)
        .and_then(|pair| u8::from_str_radix(pair, HEX_RADIX).ok())
    })
    .collect::<Option<Vec<u8>>>()?;
  Some(VolumeId {
    bytes: bytes.try_into().ok()?,
  })
}

// ---------------------------------------------------------------- routing and the bridge queue

/// The owner shard's runtime id a call must be routed to, or `None` to serve it on this shard (the
/// volume is local, the call names the synthetic root, or it carries no volume — portmap, `MNT /`, a
/// bad handle). A volume's `owner_of` is its owning *partition* (§4.8: ids route to owners); a request
/// whose owner partition is not this shard's is routed to that partition's shard over the bridge queue.
fn route(program: u32, procedure: u32, args: &[u8]) -> Option<u16> {
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

/// The volume a call is *about*, for routing: the file handle's volume for most NFS procedures; the
/// looked-up name's volume for a `LOOKUP` under the synthetic root (its name is a volume's id in hex,
/// so `cd <id>` at the host root reaches a volume on any shard); the mount path's volume for `MNT`.
fn target_volume(program: u32, procedure: u32, args: &[u8]) -> Option<VolumeId> {
  match program {
    NFS_PROGRAM if procedure == NFSPROC3_LOOKUP => {
      let dir = request_volume(&XdrReader::new(args))?;
      if dir == root_volume() {
        root_lookup_target(args)
      } else {
        Some(dir)
      }
    }
    NFS_PROGRAM => request_volume(&XdrReader::new(args)),
    MOUNT_PROGRAM if procedure == MOUNTPROC3_MNT => mount_path_volume(args),
    _ => None,
  }
}

/// The volume a root `LOOKUP` names: the id its name encodes (in hex), or `None` for a name that is not
/// an id (served locally, where it is a miss or a local volume).
fn root_lookup_target(args: &[u8]) -> Option<VolumeId> {
  let mut reader = XdrReader::new(args);
  let _dir = Nfsfh3::decode(&mut reader).ok()?;
  let name = reader.string(NFS_MAXNAMELEN).ok()?;
  parse_hex(name)
}

/// The volume a MOUNT `MNT` path names (`/<id-hex>`), or `None` for the root (`/`) or a name that is
/// not an id.
fn mount_path_volume(args: &[u8]) -> Option<VolumeId> {
  let path = XdrReader::new(args).string(MNT_PATH_MAX).ok()?;
  parse_hex(path.trim_matches('/'))
}

/// Serves one call locally, on this shard's volumes and synthetic root.
fn serve_local(program: u32, procedure: u32, args: &[u8], port: u16) -> (AcceptStatus, Vec<u8>) {
  let mut service = MultiExport::new(ShardVolumeSet, Principal::Uid { uid: 0 }, mount_rights());
  serve_call(
    &mut service,
    program,
    procedure,
    &mut XdrReader::new(args),
    port,
  )
}

/// Serves one call on the volume's `owner` shard over the bridge queue: spawns the serve on the owner
/// (the same cross-shard spawn the client forward uses, §4.3), which spawns a task back on `origin`
/// that hands the reply to this awaiting task. Returns a system error if the owner shard is gone.
async fn serve_remote(
  owner: u16,
  origin: u16,
  program: u32,
  procedure: u32,
  args: Vec<u8>,
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  let call_id = register_pending();
  let owner_task = SpawnRequest::new(
    Box::pin(async move {
      let outcome = serve_local(program, procedure, &args, port);
      let back = SpawnRequest::new(
        Box::pin(async move {
          deliver_reply(call_id, outcome);
        }),
        None,
      );
      let _ = registry::send_control(origin, Control::Spawn(Box::new(back)));
    }),
    None,
  );
  if registry::send_control(owner, Control::Spawn(Box::new(owner_task))).is_err() {
    cancel_pending(call_id);
    return (AcceptStatus::SystemErr, Vec::new());
  }
  BridgeCall { call_id }.await
}

// ------------------------------------------------------------------------- the per-shard pending map

thread_local! {
  /// Bridge calls this shard has sent to an owner and is awaiting the reply for, by call id; single
  /// this shard's thread touches it, so it needs no lock (D-7: no locks on data paths).
  static PENDING: RefCell<BTreeMap<u64, PendingReply>> = const { RefCell::new(BTreeMap::new()) };
  /// The next bridge-call id this shard hands out.
  static NEXT_CALL: Cell<u64> = const { Cell::new(1) };
}

/// A bridge call awaiting its reply: the reply once it lands, and the waker of the task awaiting it.
struct PendingReply {
  reply: Option<(AcceptStatus, Vec<u8>)>,
  waker: Option<Waker>,
}

/// Registers a new pending bridge call and returns its id.
fn register_pending() -> u64 {
  let id = NEXT_CALL.with(|next| {
    let id = next.get();
    next.set(id.wrapping_add(1));
    id
  });
  PENDING.with(|map| {
    map.borrow_mut().insert(
      id,
      PendingReply {
        reply: None,
        waker: None,
      },
    )
  });
  id
}

/// Forgets a pending bridge call whose owner could not be reached.
fn cancel_pending(id: u64) {
  PENDING.with(|map| map.borrow_mut().remove(&id));
}

/// Hands a bridge call's reply to the awaiting task and wakes it (run on the origin shard, off the
/// task the owner spawned back).
fn deliver_reply(id: u64, outcome: (AcceptStatus, Vec<u8>)) {
  PENDING.with(|map| {
    if let Some(pending) = map.borrow_mut().get_mut(&id) {
      pending.reply = Some(outcome);
      if let Some(waker) = pending.waker.take() {
        waker.wake();
      }
    }
  });
}

/// The future a task awaits for a routed call's reply: ready when the reply lands in the pending map,
/// otherwise pending with the task's waker recorded. A vanished entry (the owner was unreachable) is a
/// system error, never a hang.
struct BridgeCall {
  call_id: u64,
}

impl Future for BridgeCall {
  type Output = (AcceptStatus, Vec<u8>);

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    PENDING.with(|map| {
      let mut map = map.borrow_mut();
      let ready = match map.get_mut(&self.call_id) {
        Some(pending) => match pending.reply.take() {
          Some(outcome) => Some(outcome),
          None => {
            pending.waker = Some(cx.waker().clone());
            None
          }
        },
        None => return Poll::Ready((AcceptStatus::SystemErr, Vec::new())),
      };
      match ready {
        Some(outcome) => {
          map.remove(&self.call_id);
          Poll::Ready(outcome)
        }
        None => Poll::Pending,
      }
    })
  }
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
    procedure: u32,
    args: &mut XdrReader<'_>,
  ) -> Option<Vec<u8>> {
    ShardVolumeSet.serve(volume, subject, rights, procedure, args)
  }

  fn root_object(
    &mut self,
    volume: VolumeId,
    subject: Principal,
    rights: Rights,
  ) -> Option<(Nfsfh3, Fattr3)> {
    ShardVolumeSet.root_object(volume, subject, rights)
  }
}

thread_local! {
  /// Cross-shard entry gathers this shard is awaiting, by id; single this shard's thread touches it.
  static PENDING_ENTRIES: RefCell<BTreeMap<u64, PendingEntries>> =
    const { RefCell::new(BTreeMap::new()) };
}

/// A gather of one shard's volume entries, awaiting its answer.
struct PendingEntries {
  entries: Option<Vec<(String, VolumeId)>>,
  waker: Option<Waker>,
}

/// Registers a pending entry gather and returns its id (shares the bridge-call id space).
fn register_entries() -> u64 {
  let id = NEXT_CALL.with(|next| {
    let id = next.get();
    next.set(id.wrapping_add(1));
    id
  });
  PENDING_ENTRIES.with(|map| {
    map.borrow_mut().insert(
      id,
      PendingEntries {
        entries: None,
        waker: None,
      },
    )
  });
  id
}

/// Forgets a pending gather whose shard could not be reached.
fn cancel_entries(id: u64) {
  PENDING_ENTRIES.with(|map| map.borrow_mut().remove(&id));
}

/// Hands a gather's entries to the awaiting task and wakes it (run on the origin shard).
fn deliver_entries(id: u64, entries: Vec<(String, VolumeId)>) {
  PENDING_ENTRIES.with(|map| {
    if let Some(pending) = map.borrow_mut().get_mut(&id) {
      pending.entries = Some(entries);
      if let Some(waker) = pending.waker.take() {
        waker.wake();
      }
    }
  });
}

/// The future a task awaits for one shard's gathered entries; an empty answer if the shard is gone.
struct EntriesCall {
  id: u64,
}

impl Future for EntriesCall {
  type Output = Vec<(String, VolumeId)>;

  fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
    PENDING_ENTRIES.with(|map| {
      let mut map = map.borrow_mut();
      let ready = match map.get_mut(&self.id) {
        Some(pending) => match pending.entries.take() {
          Some(entries) => Some(entries),
          None => {
            pending.waker = Some(cx.waker().clone());
            None
          }
        },
        None => return Poll::Ready(Vec::new()),
      };
      match ready {
        Some(entries) => {
          map.remove(&self.id);
          Poll::Ready(entries)
        }
        None => Poll::Pending,
      }
    })
  }
}

/// Gathers the volume entries of one other `shard` over the bridge queue (the spawn/back-spawn the
/// bridge calls use); an empty list if the shard is gone.
async fn gather_from_shard(shard: u16, origin: u16) -> Vec<(String, VolumeId)> {
  let id = register_entries();
  let task = SpawnRequest::new(
    Box::pin(async move {
      let entries = ShardVolumeSet.entries();
      let back = SpawnRequest::new(
        Box::pin(async move {
          deliver_entries(id, entries);
        }),
        None,
      );
      let _ = registry::send_control(origin, Control::Spawn(Box::new(back)));
    }),
    None,
  );
  if registry::send_control(shard, Control::Spawn(Box::new(task))).is_err() {
    cancel_entries(id);
    return Vec::new();
  }
  EntriesCall { id }.await
}

/// Gathers the volumes of every shard for the host root listing: this shard's directly, each other
/// shard's over the bridge queue.
async fn gather_all_entries() -> Vec<(String, VolumeId)> {
  let (mine, shards) =
    state::with_state(|s| (s.partition, s.shards.clone())).unwrap_or((0, Vec::new()));
  let origin = registry::current_shard().unwrap_or(0);
  let mut all = ShardVolumeSet.entries();
  for (partition, shard) in shards.iter().enumerate() {
    if partition == usize::from(mine) {
      continue;
    }
    let mut remote = gather_from_shard(*shard, origin).await;
    all.append(&mut remote);
  }
  all
}

/// Whether a call is a `READDIR`/`READDIRPLUS` of the synthetic host root (which must list every
/// shard's volumes, not just this shard's).
fn is_root_listing(program: u32, procedure: u32, args: &[u8]) -> bool {
  program == NFS_PROGRAM
    && (procedure == NFSPROC3_READDIR || procedure == NFSPROC3_READDIRPLUS)
    && request_volume(&XdrReader::new(args)) == Some(root_volume())
}

/// Serves a host-root `READDIR`/`READDIRPLUS` over the gathered entries of every shard.
fn serve_root_listing(
  procedure: u32,
  args: &[u8],
  entries: Vec<(String, VolumeId)>,
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  let mut service = MultiExport::new(
    GatheredVolumeSet { entries },
    Principal::Uid { uid: 0 },
    mount_rights(),
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
    if let Ok(task) = futures::spawn(serve_one(stream, port)) {
      let _ = futures::detach(task);
    }
  }
}

/// Serves one accepted connection: reads RPC records, routes each call local or remote, and writes the
/// framed reply, until the client closes the connection. Owned request data is taken out before the
/// serve so the read buffer is free to drain while a remote call awaits its owner.
/// Produces the RPC reply payload for one parsed call, routing it locally, to a volume's owner shard
/// over the bridge queue, or (a host-root listing) across every shard. A `program` of 0 is a garbage
/// call whose reply carries xid 0.
async fn reply_for(
  this: u16,
  xid: u32,
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
    let entries = gather_all_entries().await;
    let (status, results) = serve_root_listing(procedure, &args, entries, port);
    return reply_bytes(xid, status, &results);
  }
  let (status, results) = match route(program, procedure, &args) {
    Some(owner) => serve_remote(owner, this, program, procedure, args, port).await,
    None => serve_local(program, procedure, &args, port),
  };
  reply_bytes(xid, status, &results)
}

async fn serve_one(stream: TcpStream, port: u16) {
  let this = registry::current_shard().unwrap_or(0);
  let mut buffer: Vec<u8> = Vec::new();
  let mut chunk = [0u8; RECORD_CHUNK];
  loop {
    // (xid, program, procedure, args, consumed); a program of 0 marks a garbage call (no real program
    // is 0), whose xid is unknown so the reply carries 0.
    let parsed: Option<(u32, u32, u32, Vec<u8>, usize)> = match read_record(&buffer) {
      Ok((body, consumed)) => match parse_call(&body) {
        Ok((call, args)) => Some((
          call.xid,
          call.program,
          call.procedure,
          args.rest().to_vec(),
          consumed,
        )),
        Err(_) => Some((0, 0, 0, Vec::new(), consumed)),
      },
      Err(RpcError::Incomplete) => None,
      Err(_) => return,
    };
    match parsed {
      Some((xid, program, procedure, args, consumed)) => {
        let reply = reply_for(this, xid, program, procedure, args, port).await;
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
