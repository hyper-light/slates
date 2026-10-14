//! A slates volume served to a `shards` microVM over shards' virtio-fs socket framing (§4.6, the shards
//! integration; `docs/wip/shards/`). It binds a socket, builds a one-file volume in RAM, and serves slates'
//! [`slates_bridge_fuse::shards_fs`] backend to each shards VM that connects — the same le32-length + FUSE framing
//! shards' fs device speaks (`shards crates/vmm/src/devices/virtio/fs`: `write_frame`/`read_frame`/`Slot::ask`).
//!
//! Pair it with shards' `run --kernel … --slates-fs-fd <N>` (the slot it fills from the connected socket): the guest
//! mounts tag `shards0` and reads the volume with no kernel NFS, no host mount and no DAX window — the achievable
//! integration on a host with no DAX-capable VMM (the DAX window is what shards does not yet have). This is a demo
//! harness, not daemon code: it serves one connection at a time (a microVM opens one), which is all `Slot::ask`
//! drives, and the volume lives only as long as it runs.
//!
//! The backend ([`slates_bridge_fuse::shards_fs`]) is transport-agnostic and cross-platform; this demo uses a
//! Unix-domain socket, so it runs on Linux and macOS. On Windows the same backend is reached over a named pipe or
//! `AF_UNIX`; the Windows demo is a follow-up and the `shards_fs` tests already exercise the backend cross-platform.
//!
//! Run: `cargo run -p slates-bridge-fuse --example shards_fs_serve -- /tmp/slates.sock`

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]

fn main() {
  #[cfg(not(unix))]
  eprintln!(
    "shards_fs_serve: this demo binds a Unix-domain socket (Linux/macOS); the slates fs backend \
     slates_bridge_fuse::shards_fs is cross-platform and on Windows is reached over a named pipe / AF_UNIX."
  );
  #[cfg(unix)]
  unix::serve();
}

#[cfg(unix)]
mod unix {
  use std::os::unix::net::UnixListener;
  use std::path::PathBuf;

  use slates_bridge_core::{Attachments, OpContext, Rights, View};
  use slates_bridge_fuse::shards_fs;
  use slates_bridge_fuse::volume_bridge::VolumeBridge;
  use slates_db::catalog::{Principal, VolumeId};
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;
  use slates_vfs::clock::HostClock;
  use slates_vfs::names::NameEquivalence;
  use slates_vfs::quota::Quota;
  use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

  /// The arena's page; the host page is fine for a RAM store that never maps a DAX window here.
  const PAGE: usize = 4096;
  /// A 16 MiB arena — ample for a demo volume of a few small files.
  const REGION_PAGES: usize = 4096;
  /// The reply buffer bounds a request frame too; sized past shards' `MAX_FRAME` (its `MAX_WRITE` + 4096) so a
  /// negotiated read never exceeds it.
  const REPLY_BYTES: usize = (1 << 20) + (1 << 16);

  pub(crate) fn serve() {
    let path = std::env::args_os()
      .nth(1)
      .map(PathBuf::from)
      .expect("usage: shards_fs_serve <socket-path>");
    // The caller passes a fresh socket path per run (the bind creates it); no stale-file removal — slates writes
    // no disk outside a granted landing (R1), and the lint wall denies `std::fs::remove_file` here.

    // The RAM store and a volume holding one readable file.
    let mut arena = ChunkArena::new(PAGE);
    arena
      .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
      .unwrap();
    let mut store = Store::new(
      &StoreConfig {
        page: PAGE,
        cache_line: 128,
        max_dirs: 64,
        max_inodes: 256,
        max_chunks: REGION_PAGES,
        max_dir_blocks: 64,
        dir_cutover: 16,
      },
      arena,
      0,
    );
    let vid = VolumeId { bytes: [0; 16] };
    let mut vol = Volume::create(
      &mut store,
      VolumeConfig {
        prefix: 1,
        names: NameEquivalence::Exact,
        quota: Quota::Bounded { limit: 1 << 30 },
        journal_bytes: 1 << 16,
        clock: Box::new(HostClock::default()),
      },
    )
    .unwrap();
    let root = vol.root_inode(&store).unwrap();
    let hello = vol.create_file_no(&mut store, root, "hello", 0o644).unwrap();
    vol
      .write(&mut store, hello, 0, b"hello from slates over shards virtio-fs\n")
      .unwrap();

    // The bridge is the one FUSE seam the kernel mount uses too; it borrows the volume for serving.
    let mut bridge = VolumeBridge::new(vid, &mut vol, &mut store);

    // A root principal with read/write on the volume — the context every dispatched op runs under.
    let mut attachments = Attachments::new();
    let id = attachments
      .attach(
        vid,
        View::Current,
        Principal::Uid { uid: 0 },
        Rights {
          read: true,
          write: true,
        },
      )
      .unwrap();
    let cx: OpContext = attachments.context(id).unwrap();

    let listener = UnixListener::bind(&path).expect("bind the slates-fs socket");
    eprintln!(
      "slates: serving a one-file volume on {} — shards `run --kernel … --slates-fs-fd <fd-of-this>` mounts it as tag shards0",
      path.display()
    );
    let mut reply = vec![0u8; REPLY_BYTES];
    // One shards VM at a time (its fs device opens one connection); serve it to EOF, then await the next.
    for conn in listener.incoming() {
      let mut conn = match conn {
        Ok(conn) => conn,
        Err(error) => {
          eprintln!("slates: accept failed: {error}");
          continue;
        }
      };
      eprintln!("slates: a shards VM connected; serving its FUSE requests");
      if let Err(error) = shards_fs::serve(&mut conn, &mut bridge, &cx, &mut reply) {
        eprintln!("slates: serving ended: {error}");
      }
    }
  }
}
