//! A blocking NFSv3 loopback server that mounts a real slates volume (§4.6, the macOS fallback).
//!
//! It binds a TCP loopback port and serves portmap/MOUNT/NFSv3 over it — the socket-and-mount half the
//! bridge-nfs codec was built to sit behind (`lib.rs`) — against a `VolumeBridge` over a RAM-only
//! volume seeded with `/hello.txt` and `/subdir`. The serve loop and RPC dispatch live in the library
//! ([`slates_bridge_nfs::serve_connection`]); this example is the thin runner that sets up a demo
//! volume and prints the port. Run it, then `mount_nfs` the printed port to see a real kernel mount of
//! a slates volume. It serves one connection at a time — enough for a mount and file access.
//!
//! Run: `cargo run -p slates-bridge-nfs --example nfs_loopback`
//! Then (macOS): `sudo mount_nfs -o vers=3,tcp,port=PORT,mountport=PORT,nolocks,noresvport,soft
//!               localhost:/ /path/to/mountpoint`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::net::TcpListener;

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

/// The served volume's id (any stable 16 bytes; the export reports its leading half as the fsid).
const VOL_ID: VolumeId = VolumeId { bytes: [0x11; 16] };
/// The content of the demo file, so a live `cat` shows the mount really reaches the RAM volume.
const MESSAGE: &[u8] = b"hello from slates, served live over NFS from a RAM-only volume\n";

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

fn make_cx() -> OpContext {
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

fn rights() -> Rights {
  Rights {
    read: true,
    write: true,
  }
}

/// Seeds the volume so a mount has something to show: `/hello.txt` with [`MESSAGE`], and `/subdir`.
fn populate(store: &mut Store, volume: &mut Volume) {
  let cx = make_cx();
  let mut bridge = VolumeBridge::new(VOL_ID, volume, store);
  let root = bridge.root(&cx).unwrap();
  let (attr, fh) = bridge
    .create(ObjectId::new(root, 0), &cx, "hello.txt", 0o644, 0)
    .unwrap();
  bridge
    .write(ObjectId::new(attr.ino, 0), &cx, 0, MESSAGE)
    .unwrap();
  bridge.release(ObjectId::new(attr.ino, 0), &cx, fh).ok();
  bridge
    .mkdir(ObjectId::new(root, 0), &cx, "subdir", 0o755)
    .unwrap();
}

fn main() {
  let mut store = make_store();
  let mut volume = make_volume(&mut store);
  populate(&mut store, &mut volume);

  let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind loopback");
  let port = listener.local_addr().unwrap().port();
  println!("SLATES_NFS_PORT={port}");
  println!(
    "mount: sudo mount_nfs -o vers=3,tcp,port={port},mountport={port},nolocks,noresvport,soft \
     localhost:/ <mountpoint>"
  );
  std::io::stdout().flush().ok();

  let mut bridge = VolumeBridge::new(VOL_ID, &mut volume, &mut store);
  let mut export =
    Export::new(&mut bridge, VOL_ID, Principal::Uid { uid: 0 }, rights()).expect("export");
  for mut stream in listener.incoming().flatten() {
    serve_connection(&mut stream, &mut export, port);
  }
}
