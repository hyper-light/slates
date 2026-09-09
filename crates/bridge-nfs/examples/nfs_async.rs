//! An async NFSv3 loopback server that mounts a real slates volume on slates's own runtime (§4.6, the
//! macOS fallback) — the production form of the blocking `nfs_loopback` example. It binds a TCP
//! loopback port, then serves portmap/MOUNT/NFSv3 over the runtime's async `TcpStream`: every read and
//! write awaits the shard's driver ([`slates_bridge_nfs::serve_connection_async`], the same dispatch
//! engine the blocking server uses), so a stalled client yields the shard instead of blocking it. The
//! volume is `!Send` (it holds a `Box<dyn Clock>`), so the serve loop is spawned the way the daemon
//! spawns its own perpetual tasks (§4.3): a `Send` boot task reaches the shard through `spawn_on`, then
//! `futures::spawn` runs the non-`Send` serve loop locally on that shard.
//!
//! Run: `cargo run -p slates-bridge-nfs --example nfs_async`
//! Then (macOS): `sudo mount_nfs -o vers=3,tcp,port=PORT,mountport=PORT,nolocks,noresvport,soft
//!               localhost:/ /path/to/mountpoint` — a real kernel mount of a RAM-only volume, no
//! signing, no kernel extension, no privilege beyond the mount (R10). It serves connections serially,
//! one at a time — enough for a mount and file access.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::sync::mpsc::channel;

use slates_bridge_core::{Attachments, Bridge, ObjectId, OpContext, Rights, View, VolumeBridge};
use slates_bridge_nfs::procedures::Export;
use slates_bridge_nfs::serve_connection_async;
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_rt::futures;
use slates_rt::runtime::{Runtime, RuntimeConfig};
use slates_rt::tcp::{Ipv4Addr, SocketAddrV4, TcpListener};
use slates_vfs::clock::HostClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// The served volume's id (any stable 16 bytes; the export reports its leading half as the fsid).
const VOL_ID: VolumeId = VolumeId { bytes: [0x11; 16] };
/// The content of the demo file, so a live `cat` shows the mount really reaches the RAM volume.
const MESSAGE: &[u8] =
  b"hello from slates, served live over NFS on the runtime, from a RAM-only volume\n";
/// Shape: a loopback NFS mount opens a small, bounded number of connections; a handful queued before
/// the accept loop takes them is ample, and the OS clamps the backlog to the system maximum anyway.
const BACKLOG: i32 = 16;

fn config() -> RuntimeConfig {
  RuntimeConfig {
    shards: 1,
    tasks_per_shard: 64,
    timers_per_shard: 64,
    ring_entries: 64,
    step_budget_ns: 1_000_000_000,
    timer_tick_ns: 100_000,
    batch: 64,
    pin: false,
    cores: Vec::new(),
    page_bytes: 4096,
    spin_ns: 0,
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
  // Bind (and listen) on this thread so the port is known before the runtime starts; the listener is
  // then moved onto the shard, where `accept` awaits the driver.
  let listener =
    TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0), BACKLOG).expect("bind loopback");
  let port = listener.local_addr().unwrap().port();
  println!("SLATES_NFS_PORT={port}");
  println!(
    "mount: sudo mount_nfs -o vers=3,tcp,port={port},mountport={port},nolocks,noresvport,soft \
     localhost:/ <mountpoint>"
  );
  std::io::stdout().flush().ok();

  let rt = Runtime::start(&config()).expect("runtime");
  let id = rt.shard_ids()[0];
  rt.spawn_on(id, async move {
    // On the shard thread now: spawn the non-`Send` serve loop locally, as the daemon does.
    let spawned = futures::spawn(async move {
      let mut store = make_store();
      let mut volume = make_volume(&mut store);
      populate(&mut store, &mut volume);
      let mut bridge = VolumeBridge::new(VOL_ID, &mut volume, &mut store);
      let mut export =
        Export::new(&mut bridge, VOL_ID, Principal::Uid { uid: 0 }, rights()).expect("export");
      while let Ok(mut stream) = listener.accept().await {
        let _ = serve_connection_async(&mut stream, &mut export, port).await;
      }
    });
    if let Ok(task) = spawned {
      let _ = futures::detach(task);
    }
  })
  .expect("spawn boot task");

  // Keep the process alive while the runtime's shards serve; the demo runs until killed.
  let (_keep_alive, rx) = channel::<()>();
  let _ = rx.recv();
  rt.shutdown();
}
