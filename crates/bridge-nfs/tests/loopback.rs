#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! The NFS loopback server end to end over a real socket (§4.6, R5): a client thread mounts the export
//! and reads a file it seeded, with no kernel mount and no privilege — exactly what a `mount_nfs` client
//! does over the wire, driven here by a hand-rolled ONC RPC client so it runs in CI on any host. The
//! `nfs_loopback` example serves the same `serve_connection` to a real `mount_nfs`.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_nfs::procedures::Export;
use slates_bridge_nfs::serve_connection;
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const VOL_ID: VolumeId = VolumeId { bytes: [0x11; 16] };
const MESSAGE: &[u8] = b"hello from slates, served live over NFS from a RAM-only volume\n";
const MOUNT_PROGRAM: u32 = 100_005;
const NFS_PROGRAM: u32 = 100_003;

// --- The server side: a demo volume, seeded, served on one connection. ---

fn make_store() -> Store {
  let page = 4096;
  let region_pages = 8192;
  let mut arena = ChunkArena::new(page);
  arena
    .add_region(Region::map(page * region_pages, page, false).unwrap())
    .unwrap();
  Store::new(
    &StoreConfig {
      page,
      cache_line: 128,
      max_dirs: 256,
      max_inodes: 1024,
      max_chunks: region_pages,
      max_dir_blocks: 256,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

fn make_volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(HostClock::default()),
    },
  )
  .unwrap()
}

fn write_cx() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      VOL_ID,
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.context(id).unwrap()
}

fn populate(store: &mut Store, volume: &mut Volume) {
  let cx = write_cx();
  let mut bridge = VolumeBridge::new(VOL_ID, volume, store);
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "hello.txt", 0o644, 0)
    .unwrap();
  bridge
    .write(ObjectId::new(attr.ino, 0), &cx, 0, MESSAGE)
    .unwrap();
  bridge.release(ObjectId::new(attr.ino, 0), &cx, fh).ok();
}

// --- The client side: a minimal ONC RPC/NFSv3 client over the socket. ---

fn opaque(bytes: &[u8], out: &mut Vec<u8>) {
  out.extend_from_slice(&u32::try_from(bytes.len()).unwrap().to_be_bytes());
  out.extend_from_slice(bytes);
  out.extend(std::iter::repeat_n(0u8, (4 - bytes.len() % 4) % 4));
}

fn read_opaque(buf: &[u8], off: usize) -> (Vec<u8>, usize) {
  let len = u32::from_be_bytes(buf[off..off + 4].try_into().unwrap()) as usize;
  let start = off + 4;
  let data = buf[start..start + len].to_vec();
  (data, start + len + (4 - len % 4) % 4)
}

/// Sends one RPC call (AUTH_NONE) and returns the accepted reply's results (after the RPC header).
fn call(stream: &mut TcpStream, program: u32, procedure: u32, args: &[u8], xid: u32) -> Vec<u8> {
  let mut body = Vec::new();
  for field in [xid, 0, 2, program, 3, procedure, 0, 0, 0, 0] {
    body.extend_from_slice(&field.to_be_bytes()); // call header + AUTH_NONE cred and verf
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
  // Header: xid, mtype, reply_stat, verf.flavor, verf.len, verf.data(pad), accept_stat, then results.
  let verf_len = u32::from_be_bytes(reply[16..20].try_into().unwrap()) as usize;
  let accept_off = 20 + verf_len + (4 - verf_len % 4) % 4;
  assert_eq!(
    u32::from_be_bytes(reply[accept_off..accept_off + 4].try_into().unwrap()),
    0,
    "RPC accepted (SUCCESS)"
  );
  reply[accept_off + 4..].to_vec()
}

fn status(results: &[u8]) -> u32 {
  u32::from_be_bytes(results[0..4].try_into().unwrap())
}

/// The NFS loopback server serves a mount and a read over a real socket: a client mounts `/`, looks up
/// the seeded file, and reads back its exact bytes — the whole socket→RPC→dispatch→bridge→volume path.
#[test]
fn a_client_mounts_and_reads_a_file_over_a_real_socket() {
  let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
  let port = listener.local_addr().unwrap().port();

  let server = std::thread::spawn(move || {
    let mut store = make_store();
    let mut volume = make_volume(&mut store);
    populate(&mut store, &mut volume);
    let mut bridge = VolumeBridge::new(VOL_ID, &mut volume, &mut store);
    let mut export = Export::new(
      &mut bridge,
      VOL_ID,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
    if let Ok((mut stream, _)) = listener.accept() {
      serve_connection(&mut stream, &mut export, port);
    }
  });

  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();

  // MOUNT MNT "/" -> the root file handle (after the mountstat3 Ok status word).
  let mut mnt_args = Vec::new();
  opaque(b"/", &mut mnt_args);
  let mnt = call(&mut stream, MOUNT_PROGRAM, 1, &mnt_args, 1);
  assert_eq!(status(&mnt), 0, "MNT succeeded");
  let (root_fh, _) = read_opaque(&mnt, 4);
  assert!(!root_fh.is_empty(), "MNT returned a root handle");

  // NFS LOOKUP(root, "hello.txt") -> the file handle.
  let mut lookup_args = Vec::new();
  opaque(&root_fh, &mut lookup_args);
  opaque(b"hello.txt", &mut lookup_args);
  let lookup = call(&mut stream, NFS_PROGRAM, 3, &lookup_args, 2);
  assert_eq!(status(&lookup), 0, "LOOKUP found hello.txt");
  let (file_fh, _) = read_opaque(&lookup, 4);

  // NFS READ(file, 0, 200) -> the file's bytes.
  let mut read_args = Vec::new();
  opaque(&file_fh, &mut read_args);
  read_args.extend_from_slice(&0u64.to_be_bytes()); // offset
  read_args.extend_from_slice(&200u32.to_be_bytes()); // count
  let read = call(&mut stream, NFS_PROGRAM, 6, &read_args, 3);
  assert_eq!(status(&read), 0, "READ succeeded");
  // READ3resok: file_attributes (post_op_attr), count, eof, data. Skip the attributes.
  let mut off = 4;
  let follows = u32::from_be_bytes(read[off..off + 4].try_into().unwrap());
  off += 4;
  if follows == 1 {
    off += 84; // one fattr3
  }
  off += 8; // count and eof
  let (data, _) = read_opaque(&read, off);
  assert_eq!(
    data, MESSAGE,
    "the client read back the exact bytes the server seeded, over a real socket"
  );

  drop(stream);
  server.join().unwrap();
}
