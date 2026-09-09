#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The daemon serves a provisioned volume over NFS (§4.6, R5, AC-3.10/3.12 in spirit — with no kernel
//! mount, so it runs in CI on any host): a single-shard daemon is started, a client provisions a volume
//! through the real rendezvous, and then — over the daemon's own NFS loopback port — a client mounts the
//! volume, creates a file in it, writes bytes, and reads them back. The bytes travel client → NFS →
//! `ShardVolumeSet` → the shard's real volume and back, so this proves the daemon's NFS transport
//! reaches the volumes it provisioned, not a demo volume. A real `mount_nfs localhost:PORT` would do the
//! same over the kernel; the hand-rolled ONC RPC client here needs no privilege.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use slates_ipc::protocol::{
  Direction, NamePolicy, ReplyBody, RequestBody, SizeClass, pack, unpack,
};
use slates_ipc::{ClientEnd, IpcError, connect};
use slates_machine::{MachineProfile, ProfileOptions};
use slates_server::{Daemon, DaemonConfig, SegmentSource};
use slates_wire::request::RequestId;

/// Shape: the probe budget of the quick profile (milliseconds); an input to derivations, not a gate.
const PROBE_MS: u64 = 5;
/// Shape: the reply deadline (nanoseconds): five seconds, far past any served verb.
const DEADLINE_NS: u64 = 5_000_000_000;
/// Shape: how long a client waits for the daemon or a full ring before giving up.
const CREDIT_WAIT: Duration = Duration::from_secs(5);
/// Format: the MOUNT program number (RFC 1813).
const MOUNT_PROGRAM: u32 = 100_005;
/// Format: the NFS program number (RFC 1813).
const NFS_PROGRAM: u32 = 100_003;

fn profile() -> MachineProfile {
  MachineProfile::measure(ProfileOptions {
    budget_per_probe: Duration::from_millis(PROBE_MS),
    codecs: false,
    core_matrix: false,
  })
}

// --- The client side of the daemon's own rendezvous (as in tests/daemon.rs). ---

struct Client {
  end: ClientEnd,
  client: u32,
  sequence: u32,
}

impl Client {
  fn connect(instance: &str) -> Client {
    let started = Instant::now();
    loop {
      match connect(instance) {
        Ok(connected) => {
          let client = connected.region.client_id();
          return Client {
            end: ClientEnd::with_doorbell(connected.region, connected.doorbell),
            client,
            sequence: 0,
          };
        }
        Err(IpcError::DaemonUnavailable { .. }) if started.elapsed() < CREDIT_WAIT => {
          std::hint::spin_loop();
        }
        Err(e) => panic!("{e}"),
      }
    }
  }

  fn call(&mut self, body: &RequestBody) -> ReplyBody {
    self.sequence += 1;
    let id = RequestId {
      client: self.client,
      sequence: self.sequence,
    };
    let index = self.end.next_request_index();
    let slot = pack(
      self.end.region_mut(),
      Direction::Request,
      index,
      id.word(),
      body,
    )
    .unwrap();
    let started = Instant::now();
    loop {
      match self.end.send(&slot) {
        Ok(()) => break,
        Err(IpcError::RingFull) if started.elapsed() < CREDIT_WAIT => std::hint::spin_loop(),
        Err(e) => panic!("{e}"),
      }
    }
    let reply = self.end.wait(Some(DEADLINE_NS)).unwrap();
    unpack(self.end.region(), reply.kind, &reply.payload).unwrap()
  }
}

fn single_shard_daemon(name: &str) -> (Daemon, String) {
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  // One shard, so every provisioned volume lands on the shard the NFS listener is served on (R8, the
  // laptop-degenerate case the daemon-side NFS serve covers; cross-shard is owed).
  let config = DaemonConfig::derive(&profile, &instance).with_shards(1);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}"),
    },
  )
  .unwrap();
  (daemon, instance)
}

fn two_shard_daemon(name: &str) -> (Daemon, String) {
  let profile = profile();
  let instance = format!("srv-{name}-{}", std::process::id());
  // Two shards, so a volume can land on a shard other than the one the NFS listener is served on,
  // exercising the cross-shard bridge queue.
  let config = DaemonConfig::derive(&profile, &instance).with_shards(2);
  let daemon = Daemon::start(
    &profile,
    config,
    SegmentSource::Create {
      name: format!("slates-seg-{name}"),
    },
  )
  .unwrap();
  (daemon, instance)
}

fn scratch(name: &str) -> RequestBody {
  RequestBody::Create {
    name: name.to_owned(),
    size: SizeClass::Bounded { limit: 1 << 20 },
    names: NamePolicy::Exact,
    require_locked: false,
    base: None,
  }
}

// --- The NFS client over the daemon's loopback port. ---

fn opaque(bytes: &[u8], out: &mut Vec<u8>) {
  out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
  out.extend_from_slice(bytes);
  out.extend(std::iter::repeat_n(0u8, (4 - bytes.len() % 4) % 4));
}

fn read_opaque(buf: &[u8], off: usize) -> (Vec<u8>, usize) {
  let len = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
  let start = off + 4;
  (
    buf[start..start + len].to_vec(),
    start + len + (4 - len % 4) % 4,
  )
}

fn call(stream: &mut TcpStream, program: u32, procedure: u32, args: &[u8], xid: u32) -> Vec<u8> {
  let mut body = Vec::new();
  for field in [xid, 0, 2, program, 3, procedure, 0, 0, 0, 0] {
    body.extend_from_slice(&field.to_be_bytes());
  }
  body.extend_from_slice(args);
  let marker = 0x8000_0000u32 | u32::try_from(body.len()).unwrap();
  stream.write_all(&marker.to_be_bytes()).unwrap();
  stream.write_all(&body).unwrap();

  let mut marker_buf = [0u8; 4];
  stream.read_exact(&mut marker_buf).unwrap();
  let len = (u32::from_be_bytes(marker_buf) & 0x7fff_ffff) as usize;
  let mut reply = vec![0u8; len];
  stream.read_exact(&mut reply).unwrap();
  let verf_len = u32::from_be_bytes(reply[16..20].try_into().unwrap()) as usize;
  let accept_off = 20 + verf_len + (4 - verf_len % 4) % 4;
  assert_eq!(
    u32::from_be_bytes(reply[accept_off..accept_off + 4].try_into().unwrap()),
    0,
    "RPC accepted"
  );
  reply[accept_off + 4..].to_vec()
}

fn status(results: &[u8]) -> u32 {
  u32::from_be_bytes(results[0..4].try_into().unwrap())
}

/// MOUNT MNT `path` → the root file handle.
fn mount(stream: &mut TcpStream, path: &str, xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(path.as_bytes(), &mut args);
  let reply = call(stream, MOUNT_PROGRAM, 1, &args, xid);
  assert_eq!(status(&reply), 0, "MNT {path}");
  read_opaque(&reply, 4).0
}

/// NFS LOOKUP `name` in `dir_fh` → the child's file handle.
fn lookup(stream: &mut TcpStream, dir_fh: &[u8], name: &str, xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  opaque(name.as_bytes(), &mut args);
  let reply = call(stream, NFS_PROGRAM, 3, &args, xid);
  assert_eq!(status(&reply), 0, "LOOKUP {name}");
  read_opaque(&reply, 4).0
}

/// NFS CREATE (UNCHECKED) `name` in `dir_fh` with mode 0644 → the new file's handle.
fn create(stream: &mut TcpStream, dir_fh: &[u8], name: &str, xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  opaque(name.as_bytes(), &mut args);
  args.extend_from_slice(&0u32.to_be_bytes()); // createmode3 UNCHECKED
  // sattr3: mode set to 0644, everything else unset, times unchanged.
  args.extend_from_slice(&1u32.to_be_bytes()); // set_mode: yes
  args.extend_from_slice(&0o644u32.to_be_bytes());
  args.extend_from_slice(&0u32.to_be_bytes()); // set_uid: no
  args.extend_from_slice(&0u32.to_be_bytes()); // set_gid: no
  args.extend_from_slice(&0u32.to_be_bytes()); // set_size: no
  args.extend_from_slice(&0u32.to_be_bytes()); // atime: DONT_CHANGE
  args.extend_from_slice(&0u32.to_be_bytes()); // mtime: DONT_CHANGE
  let reply = call(stream, NFS_PROGRAM, 8, &args, xid);
  assert_eq!(status(&reply), 0, "CREATE {name}");
  // CREATE3resok: obj (post_op_fh: follows bool, then the handle), then attributes and dir wcc.
  assert_eq!(
    u32::from_be_bytes(reply[4..8].try_into().unwrap()),
    1,
    "CREATE returned a handle"
  );
  read_opaque(&reply, 8).0
}

/// NFS WRITE `data` at offset zero to `file_fh` (FILE_SYNC).
fn write(stream: &mut TcpStream, file_fh: &[u8], data: &[u8], xid: u32) {
  let mut args = Vec::new();
  opaque(file_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // offset
  args.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes()); // count
  args.extend_from_slice(&2u32.to_be_bytes()); // stable: FILE_SYNC
  opaque(data, &mut args);
  let reply = call(stream, NFS_PROGRAM, 7, &args, xid);
  assert_eq!(status(&reply), 0, "WRITE succeeded");
}

/// NFS READ up to 400 bytes at offset zero from `file_fh`.
fn read(stream: &mut TcpStream, file_fh: &[u8], xid: u32) -> Vec<u8> {
  let mut args = Vec::new();
  opaque(file_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // offset
  args.extend_from_slice(&400u32.to_be_bytes()); // count
  let reply = call(stream, NFS_PROGRAM, 6, &args, xid);
  assert_eq!(status(&reply), 0, "READ succeeded");
  let mut off = 4;
  let follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
  off += 4;
  if follows == 1 {
    off += 84; // post_op_attr fattr3
  }
  off += 8; // count and eof
  read_opaque(&reply, off).0
}

/// READDIRPLUS `dir_fh` and return the entry names (skipping each entry's fileid, cookie, optional
/// attributes and optional handle).
fn readdirplus(stream: &mut TcpStream, dir_fh: &[u8], xid: u32) -> Vec<String> {
  let mut args = Vec::new();
  opaque(dir_fh, &mut args);
  args.extend_from_slice(&0u64.to_be_bytes()); // cookie
  args.extend_from_slice(&[0u8; 8]); // cookieverf
  args.extend_from_slice(&512u32.to_be_bytes()); // dircount (advisory)
  args.extend_from_slice(&8192u32.to_be_bytes()); // maxcount
  let reply = call(stream, NFS_PROGRAM, 17, &args, xid);
  assert_eq!(status(&reply), 0, "READDIRPLUS");
  let mut off = 4;
  let dir_follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
  off += 4;
  if dir_follows == 1 {
    off += 84; // dir post_op_attr
  }
  off += 8; // cookieverf
  let mut names = Vec::new();
  loop {
    let follows = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
    off += 4;
    if follows == 0 {
      break;
    }
    off += 8; // fileid
    let (name, next) = read_opaque(&reply, off);
    off = next;
    off += 8; // cookie
    let name_attr = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
    off += 4;
    if name_attr == 1 {
      off += 84; // name_attributes
    }
    let name_handle = u32::from_be_bytes(reply[off..off + 4].try_into().unwrap());
    off += 4;
    if name_handle == 1 {
      let (_handle, next) = read_opaque(&reply, off);
      off = next;
    }
    names.push(String::from_utf8_lossy(&name).into_owned());
  }
  names
}

/// The daemon mounts a volume it provisioned, and a file written over NFS reads back over NFS.
#[test]
fn the_daemon_serves_a_provisioned_volume_over_nfs() {
  let (daemon, instance) = single_shard_daemon("nfsmount");
  let mut client = Client::connect(&instance);

  let ReplyBody::Created { .. } = client.call(&scratch("vol")) else {
    panic!("the volume was not created");
  };
  let port = daemon.nfs_port().expect("the daemon is serving NFS");

  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  // Mount the volume by its provisioned friendly name, not its id.
  let root_fh = mount(&mut stream, "/vol", 1);
  let file_fh = create(&mut stream, &root_fh, "hello.txt", 2);
  let payload = b"written through the NFS mount into a daemon-provisioned volume\n";
  write(&mut stream, &file_fh, payload, 3);
  let got = read(&mut stream, &file_fh, 4);
  assert_eq!(
    got, payload,
    "read back over NFS the bytes written over NFS to the daemon's own volume"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}

/// The daemon serves a volume that lives on a shard OTHER than the one the NFS listener is on, over the
/// cross-shard bridge queue: the request is routed to the volume's owning shard, served there against
/// that shard's real state, and the reply routed back — a write over NFS reads back over NFS.
#[test]
fn the_daemon_serves_a_volume_on_another_shard_over_nfs() {
  let (daemon, instance) = two_shard_daemon("nfsxshard");
  let mut client = Client::connect(&instance);
  // The NFS listener is served on the control shard, whose partition is 0; a volume whose owner
  // partition is not 0 lives on the other shard and is reached over the cross-shard bridge queue.
  let control_partition = 0;

  let mut remote = None;
  for attempt in 0..32 {
    let ReplyBody::Created { id } = client.call(&scratch(&format!("vol-{attempt}"))) else {
      continue;
    };
    if slates_server::verbs::owner_of(id) != control_partition {
      remote = Some(format!("vol-{attempt}")); // the remote volume's friendly name
      break;
    }
  }
  let name = remote.expect("a volume provisioned on a non-control shard");

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  // Mount the remote volume by its friendly name; the MNT routes across shards by `owner_of_name`.
  let root_fh = mount(&mut stream, &format!("/{name}"), 1);
  let file_fh = create(&mut stream, &root_fh, "remote.txt", 2);
  let payload = b"served from a volume on another shard, over the cross-shard bridge queue\n";
  write(&mut stream, &file_fh, payload, 3);
  let got = read(&mut stream, &file_fh, 4);
  assert_eq!(
    got, payload,
    "a volume on another shard served over the cross-shard bridge queue, byte-for-byte"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}

/// A client mounts the single host root `/` and reaches a volume on ANOTHER shard by `cd`-ing into it
/// by its friendly name (a root `LOOKUP` routed across shards by `owner_of_name`): the design's single
/// mount point under which every volume appears, reaching a remote volume, over NFS with no privilege.
#[test]
fn a_client_mounts_the_host_root_and_reaches_a_remote_volume_by_name() {
  let (daemon, instance) = two_shard_daemon("nfsrootname");
  let mut client = Client::connect(&instance);
  let control_partition = 0;

  // Provision a volume on a NON-control shard, deterministically: a create routes by `owner_of_name`
  // (verbs.rs), so we target only names whose owner is a non-control partition and create them until
  // one lands — instead of the old fixed `0..32` probe over arbitrary names, which flaked under load
  // (it silently `continue`d past a remote-owned create that came back non-`Created`, and could exhaust
  // its 32 tries). This skips control-owned names outright and surfaces the actual reply if every
  // remote-owned create is refused, so a real forwarding failure is a loud panic, not a silent give-up.
  let partitions = daemon.shards().len();
  let mut remote = None;
  let mut last_reply = None;
  for attempt in 0..256u32 {
    let candidate = format!("rv-{attempt}");
    if slates_server::verbs::owner_of_name(&candidate, partitions) == control_partition {
      continue; // a control-owned name; not what this test needs
    }
    match client.call(&scratch(&candidate)) {
      ReplyBody::Created { id } => {
        assert_eq!(
          slates_server::verbs::owner_of(id),
          slates_server::verbs::owner_of_name(&candidate, partitions),
          "a volume is created on its name's owner shard"
        );
        assert_ne!(
          slates_server::verbs::owner_of(id),
          control_partition,
          "and that owner is a non-control shard"
        );
        remote = Some(candidate);
        break;
      }
      other => last_reply = Some(format!("{other:?}")),
    }
  }
  let name = remote
    .unwrap_or_else(|| panic!("no remote volume created (last non-Created reply: {last_reply:?})"));

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();

  // Mount the host root, then LOOKUP the remote volume by its name — the root LOOKUP routes across
  // shards by `owner_of_name`, and the owning shard resolves the name against its own volumes.
  let root_fh = mount(&mut stream, "/", 1);
  let volume_root = lookup(&mut stream, &root_fh, &name, 2);
  let file_fh = create(&mut stream, &volume_root, "byname.txt", 3);
  let payload = b"reached a remote volume by cd-ing into it from the single host root\n";
  write(&mut stream, &file_fh, payload, 4);
  let got = read(&mut stream, &file_fh, 5);
  assert_eq!(
    got, payload,
    "mounted the host root and reached a volume on another shard by its name, over NFS"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}

/// The host root's listing gathers volumes from every shard over the bridge queue: a client mounts `/`,
/// lists it (READDIRPLUS), and sees a volume from the control shard AND a volume from another shard —
/// the design's single mount point under which every volume on the host appears.
#[test]
fn the_host_root_listing_gathers_volumes_from_every_shard() {
  let (daemon, instance) = two_shard_daemon("nfsrootlist");
  let mut client = Client::connect(&instance);
  let control_partition = 0;

  // Provision volumes until there is one on the control shard and one on another shard.
  let mut local = None;
  let mut remote = None;
  for attempt in 0..48 {
    let ReplyBody::Created { id } = client.call(&scratch(&format!("lv-{attempt}"))) else {
      continue;
    };
    if slates_server::verbs::owner_of(id) == control_partition {
      local.get_or_insert(format!("lv-{attempt}"));
    } else {
      remote.get_or_insert(format!("lv-{attempt}"));
    }
    if local.is_some() && remote.is_some() {
      break;
    }
  }
  let local = local.expect("a volume on the control shard");
  let remote = remote.expect("a volume on another shard");

  let port = daemon.nfs_port().expect("the daemon is serving NFS");
  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let root_fh = mount(&mut stream, "/", 1);
  let names = readdirplus(&mut stream, &root_fh, 2);
  // The host root lists each volume under its friendly (provisioned) name.
  assert!(
    names.contains(&local),
    "the host root lists the control-shard volume by name: {names:?}"
  );
  assert!(
    names.contains(&remote),
    "the host root lists the other-shard volume by name (gathered over the bridge queue): {names:?}"
  );

  drop(stream);
  drop(client);
  drop(daemon);
}
