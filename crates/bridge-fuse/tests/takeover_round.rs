//! A restored mount's takeover round (A-158), delivered through the public serve step over an in-memory device: one end
//! of a Unix sequenced-packet socket pair stands in for `/dev/fuse`, so every notification the mount's notify thread
//! writes arrives as one packet, in order, and the test reads exactly what a kernel would have been told. No mount, no
//! disk: the pair is anonymous.
//!
//! What the round must be, by the design's words (§4.6, A-158): the epoch bump first where the kernel takes it, then an
//! inode invalidation with all data for the root (node 1) and for every inode the volume's takeover set names, each
//! once, sent in slices no longer than the owner's budget. A zero budget sends one inode a slice, so the slicing is
//! exercised on every inode.
#![cfg(target_os = "linux")]
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

use std::collections::BTreeSet;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use rustix::net::{AddressFamily, SocketFlags, SocketType};
use slates_bridge_core::{Attachments, Rights, View, VolumeBridge};
use slates_bridge_fuse::abi::OUT_HEADER_LEN;
use slates_bridge_fuse::channel::{FuseChannel, ServeState, Step, Wake, serve_step};
use slates_bridge_fuse::notify::Notify;
use slates_bridge_fuse::session::Session;
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::ids::RefOwner;

mod common;
use common::{store, volume};

/// Shape: the files the restored mount's kernel had looked up — enough slices to cross many round boundaries, few
/// enough for the scratch store's inode table.
const LOOKED_UP: usize = 40;
/// Shape: the durable attachment id the restored references belong to.
const OWNER: u64 = 41;
/// Shape: how long the test waits for the notify thread to write the whole round.
const WAIT: Duration = Duration::from_secs(10);
/// Shape: how long the test keeps turning a mount that has told the whole set, to see it send nothing more: thousands
/// of times one slice's write here, which is a few packets on an in-memory socket.
const QUIET: Duration = Duration::from_millis(50);
/// Format: `S_IFREG | 0644`.
const FILE_MODE: u32 = 0o100_644;
/// Format: the largest notification the round can carry, past which a packet would be cut.
const PACKET_BYTES: usize = 256;
/// Format: where `fuse_out_header`'s notification code and `fuse_notify_inval_inode_out`'s inode sit.
const AT_CODE: usize = 4;

/// One notification as the device received it: its code, and the inode an inode invalidation names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Told {
  Epoch,
  Inode(u64),
}

/// The packets waiting on `peer`, decoded, without blocking.
fn drain(peer: &OwnedFd, told: &mut Vec<Told>) {
  let mut packet = [0u8; PACKET_BYTES];
  loop {
    match rustix::io::read(peer, &mut packet) {
      Ok(0) | Err(rustix::io::Errno::AGAIN) => return,
      Ok(n) => {
        assert!(n >= OUT_HEADER_LEN, "a whole header, got {n} bytes");
        let length = u32::from_le_bytes(packet[..4].try_into().unwrap()) as usize;
        assert_eq!(length, n, "the header's length is the packet's");
        let code = i32::from_le_bytes(packet[AT_CODE..AT_CODE + 4].try_into().unwrap());
        assert_eq!(&packet[8..16], &[0; 8], "a notification's unique is zero");
        told.push(if code == Notify::IncEpoch.code() {
          assert_eq!(n, OUT_HEADER_LEN, "the epoch bump has no body");
          Told::Epoch
        } else {
          assert_eq!(
            code,
            Notify::InvalInode.code(),
            "only epoch bumps and inode invalidations"
          );
          let body = &packet[OUT_HEADER_LEN..n];
          let offset = i64::from_le_bytes(body[8..16].try_into().unwrap());
          assert_eq!(offset, 0, "all data, from the start");
          Told::Inode(u64::from_le_bytes(body[..8].try_into().unwrap()))
        });
      }
      Err(e) => panic!("reading the device's peer: {e}"),
    }
  }
}

/// What a restored mount told its device over its takeover ([`takeover_round`]).
struct Delivered {
  /// Every notification, in the order the device received it.
  told: Vec<Told>,
  /// The inodes each turn's slice invalidated: one turn sends at most one slice, since a slice waits for the round
  /// before it.
  slices: Vec<u64>,
  /// The takeover counts: epoch bumps, unsupported bumps, inodes invalidated.
  counts: (u64, u64, u64),
  /// The looked-up files' inodes.
  files: Vec<u64>,
}

/// What a restored mount over a volume whose takeover set is `LOOKED_UP` files tells its device: drives Change turns
/// with a zero slice budget until the device has received the whole set, then keeps turning for [`QUIET`].
fn takeover_round(kernel_bumps_epoch: bool) -> Delivered {
  let mut store = store();
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let mut files = Vec::new();
  for n in 0..LOOKED_UP {
    let no = vol
      .create_file_no(&mut store, root, &format!("f{n}"), FILE_MODE)
      .unwrap();
    vol
      .reference_for(&store, no, RefOwner::Attachment(OWNER))
      .unwrap();
    files.push(no.0);
  }
  let volume_id = VolumeId { bytes: [3; 16] };
  let mut attachments = Attachments::new();
  let attachment = attachments
    .attach(
      volume_id,
      View::Current,
      Principal::Uid { uid: 0 },
      Rights {
        read: true,
        write: true,
      },
    )
    .unwrap();
  attachments.set_coherence(attachment, slates_bridge_core::CacheCoherence::Invalidated);
  attachments.set_owner(attachment, OWNER);
  let (device, peer) = rustix::net::socketpair(
    AddressFamily::UNIX,
    SocketType::SEQPACKET,
    SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
    None,
  )
  .unwrap();
  let mut channel = FuseChannel::nonblocking(device).unwrap();
  let mut state = ServeState::restored(
    Session {
      expire_only: true,
      kernel_resends: true,
      kernel_bumps_epoch,
    },
    None,
  );
  state.slices_within(0);
  let mut told = Vec::new();
  let mut slices = Vec::new();
  let mut counts = (0, 0, 0);
  let whole = LOOKED_UP + 1;
  let packets = whole + usize::from(kernel_bumps_epoch);
  let mut turn = || {
    let step = {
      let mut bridge = VolumeBridge::new(volume_id, &mut vol, &mut store);
      serve_step(
        &mut channel,
        &mut bridge,
        &mut attachments,
        attachment,
        &mut state,
        Wake::Change,
      )
      .unwrap()
    };
    assert!(
      matches!(step, Step::Change(_)),
      "a change turn delivers: {step:?}"
    );
    let (bumped, unsupported, invalidated) = state.take_takeover_counts();
    (
      bumped,
      unsupported,
      invalidated,
      state.owes_takeover_round(),
    )
  };
  let began = Instant::now();
  while told.len() < packets {
    assert!(
      began.elapsed() < WAIT,
      "the round is told within {WAIT:?}: {told:?}"
    );
    let (bumped, unsupported, invalidated, _) = turn();
    counts = (
      counts.0 + bumped,
      counts.1 + unsupported,
      counts.2 + invalidated,
    );
    if invalidated > 0 {
      slices.push(invalidated);
    }
    drain(&peer, &mut told);
    std::thread::yield_now();
  }
  let quiet = Instant::now();
  let mut owes = true;
  while quiet.elapsed() < QUIET {
    let (bumped, unsupported, invalidated, owes_now) = turn();
    assert_eq!(
      (bumped, unsupported, invalidated),
      (0, 0, 0),
      "nothing more is counted"
    );
    drain(&peer, &mut told);
    assert_eq!(told.len(), packets, "nothing more is told: {told:?}");
    owes = owes_now;
    std::thread::yield_now();
  }
  assert!(!owes, "a mount that has told the whole set owes nothing");
  Delivered {
    told,
    slices,
    counts,
    files,
  }
}

/// A-158 (the takeover round, sliced). Do: restore a mount whose owner holds references to 40 files, with a kernel that
/// takes the epoch bump, and turn it with a zero slice budget until the device has the whole set, then a while longer.
/// Expect the epoch bump first and only once, then an all-data invalidation of the root (node 1) and of every file,
/// each exactly once and nothing else; 40 slices, the first carrying the root and one file and each after it one file;
/// the counts to say one bump and 41 inodes; and nothing more sent or owed after.
#[test]
fn a_restored_mount_tells_its_kernel_the_whole_takeover_set_in_slices_each_inode_once() {
  let Delivered {
    told,
    slices,
    counts,
    files,
  } = takeover_round(true);
  assert_eq!(
    told.first(),
    Some(&Told::Epoch),
    "the epoch bump opens the first round"
  );
  assert_eq!(
    told.iter().filter(|told| **told == Told::Epoch).count(),
    1,
    "one epoch bump"
  );
  let inodes: Vec<u64> = told
    .iter()
    .filter_map(|told| match told {
      Told::Inode(ino) => Some(*ino),
      Told::Epoch => None,
    })
    .collect();
  let distinct: BTreeSet<u64> = inodes.iter().copied().collect();
  assert_eq!(distinct.len(), inodes.len(), "each inode once: {inodes:?}");
  let expected: BTreeSet<u64> = files.iter().copied().chain(std::iter::once(1)).collect();
  assert_eq!(
    distinct, expected,
    "the root and every looked-up file, nothing else"
  );
  let mut sliced = vec![2];
  sliced.extend(std::iter::repeat_n(1, LOOKED_UP - 1));
  assert_eq!(
    slices, sliced,
    "a zero budget: the root and one file, then one file a slice"
  );
  assert_eq!(counts, (1, 0, u64::try_from(LOOKED_UP + 1).unwrap()));
}

/// A-158 (a kernel before FUSE 7.44). Do: the same restore, with a kernel that cannot bump its epoch. Expect no epoch
/// bump at all, the root and every file each once, the same slices, and the counts to say the bump was unsupported
/// once.
#[test]
fn a_kernel_that_cannot_bump_its_epoch_still_gets_every_inode_and_is_counted() {
  let Delivered {
    told,
    slices,
    counts,
    files,
  } = takeover_round(false);
  assert!(
    !told.contains(&Told::Epoch),
    "no epoch bump to a kernel before 7.44"
  );
  let inodes: BTreeSet<u64> = told
    .iter()
    .filter_map(|told| match told {
      Told::Inode(ino) => Some(*ino),
      Told::Epoch => None,
    })
    .collect();
  assert_eq!(inodes.len(), told.len(), "each inode once");
  assert_eq!(
    inodes,
    files.iter().copied().chain(std::iter::once(1)).collect(),
    "the root and every looked-up file"
  );
  assert_eq!(slices.len(), LOOKED_UP, "the same slices");
  assert_eq!(counts, (0, 1, u64::try_from(LOOKED_UP + 1).unwrap()));
}
