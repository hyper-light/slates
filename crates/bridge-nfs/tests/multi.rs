#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! One NFS server serves several volumes, routing each request to the volume its file handle names
//! (§4.6, R5): a `MultiExport` holds two independent volumes, and over one connection a client mounts
//! `alpha`, reads its seeded file, then mounts `beta` and reads its file — the bytes never cross,
//! which proves the request routed to the volume the handle carried, not to a single fixed export.
//! This is the multi-volume routing the design's "one root mount, volumes appear as directories"
//! (§4.6) rests on, driven here through the blocking `serve_connection` over a real socket in CI.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_nfs::procedures::Export;
use slates_bridge_nfs::{MultiExport, serve_connection};
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

const VOL_A: VolumeId = VolumeId { bytes: [0x11; 16] };
const VOL_B: VolumeId = VolumeId { bytes: [0x22; 16] };
const MSG_A: &[u8] = b"alpha volume: served from the volume its handle named\n";
const MSG_B: &[u8] = b"beta volume: a different volume behind the same server\n";
const MOUNT_PROGRAM: u32 = 100_005;
const NFS_PROGRAM: u32 = 100_003;

fn rights() -> Rights {
  Rights {
    read: true,
    write: true,
  }
}

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

fn write_cx(volume: VolumeId) -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(volume, View::Current, Principal::Uid { uid: 0 }, rights())
    .unwrap();
  attachments.context(id).unwrap()
}

/// Seeds `volume` (of id `vol_id`) with one file named `filename` holding `message`.
fn populate(
  store: &mut Store,
  volume: &mut Volume,
  vol_id: VolumeId,
  filename: &str,
  message: &[u8],
) {
  let cx = write_cx(vol_id);
  let mut bridge = VolumeBridge::new(vol_id, volume, store);
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge
    .create(ObjectId::new(root, 0), &cx, filename, 0o644, 0)
    .unwrap();
  bridge
    .write(ObjectId::new(attr.ino, 0), &cx, 0, message)
    .unwrap();
  bridge.release(ObjectId::new(attr.ino, 0), &cx, fh).ok();
}

// --- The client side: a minimal ONC RPC/NFSv3 client (as in `loopback.rs`). ---

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

/// Reads a file by mounting `export_name`, looking up `filename`, and reading it — the three RPCs a
/// client makes, over one connection, so the same server serves both volumes in the test.
fn read_file(stream: &mut TcpStream, export_name: &str, filename: &str, xid: u32) -> Vec<u8> {
  let mut mnt_args = Vec::new();
  opaque(export_name.as_bytes(), &mut mnt_args);
  let mnt = call(stream, MOUNT_PROGRAM, 1, &mnt_args, xid);
  assert_eq!(status(&mnt), 0, "MNT {export_name} succeeded");
  let (root_fh, _) = read_opaque(&mnt, 4);

  let mut lookup_args = Vec::new();
  opaque(&root_fh, &mut lookup_args);
  opaque(filename.as_bytes(), &mut lookup_args);
  let lookup = call(stream, NFS_PROGRAM, 3, &lookup_args, xid + 1);
  assert_eq!(status(&lookup), 0, "LOOKUP {filename} in {export_name}");
  let (file_fh, _) = read_opaque(&lookup, 4);

  let mut read_args = Vec::new();
  opaque(&file_fh, &mut read_args);
  read_args.extend_from_slice(&0u64.to_be_bytes());
  read_args.extend_from_slice(&200u32.to_be_bytes());
  let read = call(stream, NFS_PROGRAM, 6, &read_args, xid + 2);
  assert_eq!(status(&read), 0, "READ in {export_name}");
  let mut off = 4;
  let follows = u32::from_be_bytes(read[off..off + 4].try_into().unwrap());
  off += 4;
  if follows == 1 {
    off += 84;
  }
  off += 8;
  let (data, _) = read_opaque(&read, off);
  data
}

/// One `MultiExport` serves two volumes over one connection; the client mounts each by name and reads
/// its file, and the bytes match the volume each handle named — proof the request routed by the
/// handle's volume id, not to one fixed export.
#[test]
fn one_server_routes_requests_to_the_volume_each_handle_names() {
  let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
  let port = listener.local_addr().unwrap().port();

  let server = std::thread::spawn(move || {
    let mut store_a = make_store();
    let mut vol_a = make_volume(&mut store_a);
    populate(&mut store_a, &mut vol_a, VOL_A, "hello.txt", MSG_A);
    let mut store_b = make_store();
    let mut vol_b = make_volume(&mut store_b);
    populate(&mut store_b, &mut vol_b, VOL_B, "world.txt", MSG_B);

    let mut bridge_a = VolumeBridge::new(VOL_A, &mut vol_a, &mut store_a);
    let mut bridge_b = VolumeBridge::new(VOL_B, &mut vol_b, &mut store_b);
    let export_a = Export::new(&mut bridge_a, VOL_A, Principal::Uid { uid: 0 }, rights()).unwrap();
    let export_b = Export::new(&mut bridge_b, VOL_B, Principal::Uid { uid: 0 }, rights()).unwrap();

    let mut multi = MultiExport::new();
    multi.mount("alpha", VOL_A, export_a);
    multi.mount("beta", VOL_B, export_b);
    assert_eq!(multi.len(), 2);

    if let Ok((mut stream, _)) = listener.accept() {
      serve_connection(&mut stream, &mut multi, port);
    }
  });

  let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
  let from_alpha = read_file(&mut stream, "/alpha", "hello.txt", 1);
  let from_beta = read_file(&mut stream, "/beta", "world.txt", 10);

  assert_eq!(
    from_alpha, MSG_A,
    "the alpha handle routed to the alpha volume"
  );
  assert_eq!(
    from_beta, MSG_B,
    "the beta handle routed to the beta volume"
  );
  assert_ne!(
    from_alpha, from_beta,
    "the two volumes served distinct content"
  );

  drop(stream);
  server.join().unwrap();
}
