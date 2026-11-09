//! A revoked mount's round (AUD-29-84's residual), delivered through the public serve step over an in-memory device: one
//! end of a Unix sequenced-packet socket pair stands in for `/dev/fuse`, so the test plays the kernel. It sends the
//! mount's lookups and reads every reply and every notification the mount's notify thread writes, one packet each, in
//! order. No mount, no disk: the pair is anonymous.
//!
//! What the round must be, by the fix's words (`slates_bridge_fuse::channel`'s revoked turn): once the attachment admits
//! no more requests, an inode invalidation with all data for the root (node 1) and for every node the kernel still
//! references, each once, in slices no longer than the owner's budget; every request read meanwhile answered `ENOTCONN`
//! without reaching the volume; a node the kernel forgets meanwhile skipped; and the mount ended only once the last
//! slice is written. A zero budget sends one node a slice, so the slicing is exercised on every node.
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
use slates_bridge_core::{AttachmentId, Attachments, Rights, View, VolumeBridge};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode};
use slates_bridge_fuse::channel::{FuseChannel, REVOKED_ERRNO, ServeState, Step, Wake, serve_step};
use slates_bridge_fuse::notify::Notify;
use slates_db::catalog::{Principal, VolumeId};
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{store, volume};

/// Shape: the files the mount's kernel looks up before the revocation: enough slices to cross many round boundaries,
/// few enough for the scratch store's inode table.
const LOOKED_UP: usize = 40;
/// Shape: the slice after which the kernel sends a `READ` and a `FORGET`, mid-round.
const MID_ROUND: usize = 10;
/// Shape: how long the test waits for the mount to end.
const WAIT: Duration = Duration::from_secs(10);
/// Format: `S_IFREG | 0644`.
const FILE_MODE: u32 = 0o100_644;
/// Format: the largest packet the test reads: an entry reply or a notification.
const PACKET_BYTES: usize = 512;
/// Format: `fuse_read_in`'s length.
const READ_IN_BYTES: usize = 40;
/// Format: the unique of the mid-round `READ`; the lookups take 1 to `LOOKED_UP`.
const READ_UNIQUE: u64 = 1_000;
/// Format: the unique of the mid-round `FORGET`.
const FORGET_UNIQUE: u64 = 1_001;

/// One packet the device received.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Packet {
  /// A reply: its unique, its errno (positive), and the node an entry reply names (zero for any other reply).
  Reply { unique: u64, errno: i32, node: u64 },
  /// An inode invalidation of all data, naming the node.
  Inode(u64),
}

/// The packets waiting on `peer`, decoded, without blocking.
fn drain(peer: &OwnedFd) -> Vec<Packet> {
  let mut packets = Vec::new();
  let mut packet = [0u8; PACKET_BYTES];
  loop {
    match rustix::io::read(peer, &mut packet) {
      Ok(0) | Err(rustix::io::Errno::AGAIN) => return packets,
      Ok(n) => {
        assert!(n >= OUT_HEADER_LEN, "a whole header, got {n} bytes");
        let word = i32::from_le_bytes(packet[4..8].try_into().unwrap());
        let unique = u64::from_le_bytes(packet[8..16].try_into().unwrap());
        packets.push(if unique == 0 {
          assert_eq!(word, Notify::InvalInode.code(), "only inode invalidations");
          let body = &packet[OUT_HEADER_LEN..n];
          let offset = i64::from_le_bytes(body[8..16].try_into().unwrap());
          assert_eq!(offset, 0, "all data, from the start");
          Packet::Inode(u64::from_le_bytes(body[..8].try_into().unwrap()))
        } else {
          let node = if n >= OUT_HEADER_LEN + 8 {
            u64::from_le_bytes(
              packet[OUT_HEADER_LEN..OUT_HEADER_LEN + 8]
                .try_into()
                .unwrap(),
            )
          } else {
            0
          };
          Packet::Reply {
            unique,
            errno: word.saturating_neg(),
            node,
          }
        });
      }
      Err(e) => panic!("reading the device's peer: {e}"),
    }
  }
}

/// A request as the kernel sends it: the header with `unique` and `nodeid`, then `body`.
fn message(opcode: Opcode, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_wire().to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

/// A mount over a volume of [`LOOKED_UP`] files, with the test as its kernel.
struct Mount {
  store: Store,
  vol: Volume,
  volume_id: VolumeId,
  attachments: Attachments,
  attachment: AttachmentId,
  channel: FuseChannel,
  state: ServeState,
  peer: OwnedFd,
}

/// What the revoked round sent and answered ([`Mount::revoked_round`]).
#[derive(Default)]
struct Round {
  /// The nodes invalidated, in the order the device received them.
  told: Vec<u64>,
  /// The inodes each turn's slice invalidated.
  slices: Vec<u64>,
  /// The requests counted as refused.
  refused: u64,
  /// Every reply the device received.
  replies: Vec<Packet>,
}

impl Mount {
  /// The volume, its attachment for reading, the device pair, and a fresh mount's state with a zero slice budget.
  fn new() -> Mount {
    let mut store = store();
    let mut vol = volume(&mut store);
    let root = vol.root_inode(&store).unwrap();
    for n in 0..LOOKED_UP {
      vol
        .create_file_no(&mut store, root, &format!("f{n}"), FILE_MODE)
        .unwrap();
    }
    let volume_id = VolumeId { bytes: [5; 16] };
    let mut attachments = Attachments::new();
    let attachment = attachments
      .attach(
        volume_id,
        View::Current,
        Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        Rights {
          read: true,
          write: false,
        },
      )
      .unwrap();
    attachments.set_coherence(attachment, slates_bridge_core::CacheCoherence::Invalidated);
    let (device, peer) = rustix::net::socketpair(
      AddressFamily::UNIX,
      SocketType::SEQPACKET,
      SocketFlags::CLOEXEC | SocketFlags::NONBLOCK,
      None,
    )
    .unwrap();
    let mut state = ServeState::new();
    state.slices_within(0);
    Mount {
      store,
      vol,
      volume_id,
      attachments,
      attachment,
      channel: FuseChannel::nonblocking(device).unwrap(),
      state,
      peer,
    }
  }

  /// One serve step after `wake`.
  fn turn(&mut self, wake: Wake) -> Step {
    let mut bridge = VolumeBridge::new(self.volume_id, &mut self.vol, &mut self.store);
    serve_step(
      &mut self.channel,
      &mut bridge,
      &mut self.attachments,
      self.attachment,
      &mut self.state,
      wake,
    )
    .unwrap()
  }

  /// The kernel sends `request` and the mount takes it.
  fn send(&mut self, request: &[u8]) -> Step {
    rustix::io::write(&self.peer, request).unwrap();
    self.turn(Wake::Request)
  }

  /// The kernel looks up every file in the root: each file's node, in order, and every other packet the device got.
  fn look_up_every_file(&mut self) -> (Vec<u64>, Vec<Packet>) {
    let mut files = Vec::new();
    let mut other = Vec::new();
    for n in 0..LOOKED_UP {
      let unique = u64::try_from(n).unwrap() + 1;
      let name = format!("f{n}\0");
      let step = self.send(&message(Opcode::Lookup, unique, 1, name.as_bytes()));
      assert!(
        matches!(step, Step::Request { error: 0, .. }),
        "lookup {n}: {step:?}"
      );
      for packet in drain(&self.peer) {
        match packet {
          Packet::Reply {
            unique: replied,
            errno: 0,
            node,
          } if replied == unique && node != 0 => files.push(node),
          packet => other.push(packet),
        }
      }
    }
    (files, other)
  }

  /// The counts a turn left, onto `round`.
  fn tally(&mut self, round: &mut Round) {
    let (invalidated, refused) = self.state.take_revoked_counts();
    round.refused += refused;
    if invalidated > 0 {
      round.slices.push(invalidated);
    }
  }

  /// Mid-round, the kernel sends a `READ` of `read` and gives back the one lookup it holds on `forgotten`.
  fn read_and_forget(&mut self, round: &mut Round, read: u64, forgotten: u64) -> Step {
    let step = self.send(&message(
      Opcode::Read,
      READ_UNIQUE,
      read,
      &[0u8; READ_IN_BYTES],
    ));
    self.tally(round);
    assert_eq!(
      step,
      Step::Request {
        opcode: Some(Opcode::Read),
        delivered: Default::default(),
        error: REVOKED_ERRNO,
      },
      "a READ mid-round is refused"
    );
    self.send(&message(
      Opcode::Forget,
      FORGET_UNIQUE,
      forgotten,
      &1u64.to_le_bytes(),
    ))
  }

  /// Turns the revoked mount until it ends, sending [`Mount::read_and_forget`] after the [`MID_ROUND`]th slice.
  fn revoked_round(&mut self, read: u64, forgotten: u64) -> Round {
    let mut round = Round::default();
    let mut sent_mid_round = false;
    let began = Instant::now();
    loop {
      assert!(
        began.elapsed() < WAIT,
        "the mount ends within {WAIT:?}: {:?}",
        round.told
      );
      let step = if !sent_mid_round && round.slices.len() == MID_ROUND {
        sent_mid_round = true;
        self.read_and_forget(&mut round, read, forgotten)
      } else {
        self.turn(Wake::Change)
      };
      self.tally(&mut round);
      // The mount ends only once its last round came back from the notify thread, which writes the round before it
      // answers: whatever the mount ended after is already on the device.
      for packet in drain(&self.peer) {
        match packet {
          Packet::Inode(node) => round.told.push(node),
          reply @ Packet::Reply { .. } => round.replies.push(reply),
        }
      }
      if step == Step::Ended {
        return round;
      }
      std::thread::yield_now();
    }
  }
}

/// The slices of a zero budget: the root and one file first, then one node a slice, the root and every file but the
/// forgotten one in all.
fn assert_sliced(slices: &[u64]) {
  assert_eq!(
    slices.first(),
    Some(&2),
    "the root and one file first: {slices:?}"
  );
  assert!(
    slices[1..].iter().all(|&slice| slice == 1),
    "one node a slice after the first: {slices:?}"
  );
  assert_eq!(
    slices.iter().sum::<u64>(),
    u64::try_from(LOOKED_UP).unwrap(),
    "the counts say the root and the 39 files still referenced"
  );
}

/// AUD-29-84's residual (the revoked round, sliced). Do: the kernel looks up 40 files through a mount; the attachment is
/// revoked; the mount is turned with a zero slice budget; after the tenth slice the kernel sends a `READ` of one file and
/// a `FORGET` of the last file's only lookup; the mount is turned until it ends. Expect nothing invalidated before the
/// revocation; the `READ` answered `ENOTCONN`, the `FORGET` answered by nothing; an all-data invalidation of the root
/// (node 1) and of every looked-up file but the forgotten one, each exactly once and nothing else; the root and one file
/// in the first slice and one file in each after; the counts to say so; and the mount ended only once every one of them
/// was written.
#[test]
fn a_revoked_mount_drops_every_node_its_kernel_holds_in_slices_before_it_ends() {
  let mut mount = Mount::new();
  let (files, before) = mount.look_up_every_file();
  assert_eq!(
    files.len(),
    LOOKED_UP,
    "every lookup answered with its node"
  );
  assert_eq!(before, Vec::new(), "nothing else before the revocation");
  assert_eq!(mount.state.take_revoked_counts(), (0, 0));

  mount.attachments.revoke(mount.attachment);
  let forgotten = *files.last().unwrap();
  let round = mount.revoked_round(files[0], forgotten);

  assert_eq!(
    round.replies,
    vec![Packet::Reply {
      unique: READ_UNIQUE,
      errno: REVOKED_ERRNO,
      node: 0,
    }],
    "the READ answered ENOTCONN, the FORGET answered by nothing"
  );
  assert_eq!(round.refused, 2, "both requests counted as refused");
  let distinct: BTreeSet<u64> = round.told.iter().copied().collect();
  assert_eq!(
    distinct.len(),
    round.told.len(),
    "each node once: {:?}",
    round.told
  );
  let expected: BTreeSet<u64> = files[..LOOKED_UP - 1]
    .iter()
    .copied()
    .chain(std::iter::once(1))
    .collect();
  assert_eq!(
    distinct, expected,
    "the root and every node the kernel still referenced, nothing else"
  );
  assert_sliced(&round.slices);
}
