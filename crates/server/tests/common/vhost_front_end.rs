//! A vhost-user front end, as a VMM speaks it (the inherited-descriptor form; AUD-29-68): the protocol over one
//! end of a socketpair, the guest's memory a sealed memory object, the kicks and calls eventfds. Linux only.
//!
//! Shared by the daemon's virtio-fs tests (`crates/server/tests/virtiofs.rs`) and the live-shipping flow that plays
//! the VMM `slates vhost-user` runs (`crates/cli/tests/live_fleet.rs`); each includes it beside `guest.rs`, as `super::guest`.
#![allow(clippy::disallowed_methods)]
// memory objects and eventfds: RAM, not a host path (R1).
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::event::{EventfdFlags, PollFd, PollFlags, Timespec, eventfd, poll};
use rustix::net::{
  AddressFamily, RecvAncillaryBuffer, RecvFlags, SendAncillaryBuffer, SendAncillaryMessage,
  SendFlags, SocketFlags, SocketType,
};
use slates_bridge_virtiofs::vhost_user::{
  DAX_PROTOCOL_FEATURES, FLAG_NEED_REPLY, GuestRegions, HEADER_LEN, MappedRegion, OFFERED_FEATURES,
  OFFERED_PROTOCOL_FEATURES, RegionEntry, VERSION, request,
};

use super::guest::{GUEST_RAM, Guest};

/// Shape: where the front end's own mapping of guest memory sits in its address space (any value: the
/// back end translates ring addresses from it).
pub(crate) const USER_BASE: u64 = 0x7f00_0000_0000;
/// Shape: how long the front end waits for one reply or one interrupt.
const WAIT_NS: i64 = 10_000_000_000;

/// A sealed memory object of the guest's RAM, or (with `seal` false) one that may still shrink.
pub(crate) fn guest_ram(seal: bool) -> OwnedFd {
  use rustix::fs::{MemfdFlags, SealFlags};
  let fd =
    rustix::fs::memfd_create("guest-ram", MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING).unwrap();
  rustix::fs::ftruncate(&fd, GUEST_RAM).unwrap();
  if seal {
    rustix::fs::fcntl_add_seals(&fd, SealFlags::SHRINK | SealFlags::GROW).unwrap();
  }
  fd
}

/// The guest's driver over a second mapping of `ram`.
pub(crate) fn guest_over(ram: &OwnedFd) -> Guest<GuestRegions> {
  let region = MappedRegion::map(region_entry(), ram).unwrap();
  Guest::over(GuestRegions::new(vec![region]).unwrap())
}

/// The guest's RAM as the memory table names it: from guest address zero, at [`USER_BASE`] in the front end.
pub(crate) fn region_entry() -> RegionEntry {
  RegionEntry {
    guest_phys: 0,
    size: GUEST_RAM,
    user_addr: USER_BASE,
    offset: 0,
  }
}

/// The front end: its socket, and per queue its kick and call eventfds.
pub(crate) struct FrontEnd {
  socket: OwnedFd,
  pub(crate) kicks: Vec<OwnedFd>,
  pub(crate) calls: Vec<OwnedFd>,
}

/// A connected pair: the front end, and the end the daemon adopts.
pub(crate) fn pair() -> (FrontEnd, OwnedFd) {
  let (ours, theirs) = rustix::net::socketpair(
    AddressFamily::UNIX,
    SocketType::STREAM,
    SocketFlags::CLOEXEC,
    None,
  )
  .unwrap();
  (FrontEnd::over(ours), theirs)
}

impl FrontEnd {
  /// The front end over `socket`, an end whose other end the daemon's device serves: what a VMM holds when it was
  /// handed the end (`slates vhost-user`), with fresh kick and call eventfds for its queues.
  pub(crate) fn over(socket: OwnedFd) -> FrontEnd {
    let eventfds = || {
      (0..2)
        .map(|_| eventfd(0, EventfdFlags::CLOEXEC | EventfdFlags::NONBLOCK).unwrap())
        .collect()
    };
    FrontEnd {
      socket,
      kicks: eventfds(),
      calls: eventfds(),
    }
  }
}

impl FrontEnd {
  pub(crate) fn send(&self, request: u32, flags: u32, payload: &[u8], fds: &[BorrowedFd<'_>]) {
    let mut message = Vec::new();
    message.extend_from_slice(&request.to_le_bytes());
    message.extend_from_slice(&(VERSION | flags).to_le_bytes());
    message.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_le_bytes());
    message.extend_from_slice(payload);
    let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !fds.is_empty() {
      assert!(control.push(SendAncillaryMessage::ScmRights(fds)));
    }
    let sent = rustix::net::sendmsg(
      &self.socket,
      &[std::io::IoSlice::new(&message)],
      &mut control,
      SendFlags::NOSIGNAL,
    )
    .unwrap();
    assert_eq!(sent, message.len());
  }

  /// Whether the back end has closed its end, probed with a `GET_QUEUE_NUM`: a device that ends on a refusal may
  /// close its socket before the probe is sent (the send is refused `EPIPE` or `ECONNRESET`) or after (the probe is
  /// never answered), and both mean closed. Main CI 42083757327 met the first: the probe's send panicked `EPIPE`.
  pub(crate) fn closed(&self) -> bool {
    let mut message = Vec::new();
    message.extend_from_slice(&request::GET_QUEUE_NUM.to_le_bytes());
    message.extend_from_slice(&VERSION.to_le_bytes());
    message.extend_from_slice(&0u32.to_le_bytes());
    match rustix::net::send(&self.socket, &message, SendFlags::NOSIGNAL) {
      Err(rustix::io::Errno::PIPE | rustix::io::Errno::CONNRESET) => true,
      Err(other) => panic!("probing the back end: {other}"),
      Ok(sent) => {
        assert_eq!(sent, message.len());
        self.reply().is_none()
      }
    }
  }

  /// The next reply's payload, or `None` when the back end closed its end.
  pub(crate) fn reply(&self) -> Option<Vec<u8>> {
    let mut header = [0u8; HEADER_LEN];
    if !self.read_exact(&mut header) {
      return None;
    }
    let size = u32::from_le_bytes(header[8..12].try_into().unwrap());
    let mut payload = vec![0u8; usize::try_from(size).unwrap()];
    self.read_exact(&mut payload).then_some(payload)
  }

  fn read_exact(&self, out: &mut [u8]) -> bool {
    let mut at = 0;
    while at < out.len() {
      let mut fds = [PollFd::new(&self.socket, PollFlags::IN)];
      let timeout = Timespec {
        tv_sec: WAIT_NS / 1_000_000_000,
        tv_nsec: 0,
      };
      assert_eq!(
        poll(&mut fds, Some(&timeout)).unwrap(),
        1,
        "a reply in time"
      );
      let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(1))];
      let mut control = RecvAncillaryBuffer::new(&mut space);
      let got = rustix::net::recvmsg(
        &self.socket,
        &mut [std::io::IoSliceMut::new(&mut out[at..])],
        &mut control,
        RecvFlags::CMSG_CLOEXEC,
      );
      match got {
        Ok(received) if received.bytes > 0 => at += received.bytes,
        _ => return false,
      }
    }
    true
  }

  /// A `GET_*` request: its `u64` answer.
  pub(crate) fn get(&self, request: u32) -> u64 {
    self.send(request, 0, &[], &[]);
    u64::from_le_bytes(self.reply().unwrap().try_into().unwrap())
  }

  /// A setting request with a reply-ack asked for: the ack's status (zero for success).
  pub(crate) fn set(&self, request: u32, payload: &[u8], fds: &[BorrowedFd<'_>]) -> u64 {
    self.send(request, FLAG_NEED_REPLY, payload, fds);
    u64::from_le_bytes(self.reply().unwrap().try_into().unwrap())
  }

  /// A vring-state request (index, number).
  pub(crate) fn state(queue: u32, number: u32) -> Vec<u8> {
    let mut payload = queue.to_le_bytes().to_vec();
    payload.extend_from_slice(&number.to_le_bytes());
    payload
  }

  /// Queues the settings and the memory table back to back, asking no reply until the table's: what QEMU
  /// sends before it waits. The table's ack, once the back end reads them.
  pub(crate) fn queue_settings_and_memory(&self, ram: &OwnedFd) {
    self.send(
      request::SET_FEATURES,
      0,
      &OFFERED_FEATURES.to_le_bytes(),
      &[],
    );
    self.send(
      request::SET_PROTOCOL_FEATURES,
      0,
      &OFFERED_PROTOCOL_FEATURES.to_le_bytes(),
      &[],
    );
    self.send(request::SET_OWNER, 0, &[], &[]);
    self.send(
      request::SET_MEM_TABLE,
      FLAG_NEED_REPLY,
      &memory_table(),
      &[ram.as_fd()],
    );
  }

  /// Negotiates features and sends the memory table of `ram` (one region from guest address zero).
  pub(crate) fn negotiate_memory(&self, ram: &OwnedFd) -> u64 {
    assert_eq!(self.get(request::GET_FEATURES), OFFERED_FEATURES);
    self.send(
      request::SET_FEATURES,
      0,
      &OFFERED_FEATURES.to_le_bytes(),
      &[],
    );
    assert_eq!(
      self.get(request::GET_PROTOCOL_FEATURES),
      OFFERED_PROTOCOL_FEATURES
    );
    self.send(
      request::SET_PROTOCOL_FEATURES,
      0,
      &OFFERED_PROTOCOL_FEATURES.to_le_bytes(),
      &[],
    );
    assert_eq!(self.get(request::GET_QUEUE_NUM), 2);
    assert_eq!(self.set(request::SET_OWNER, &[], &[]), 0);
    self.set(request::SET_MEM_TABLE, &memory_table(), &[ram.as_fd()])
  }

  /// Configures each queue of `guest` and enables it; every ack must report success.
  pub(crate) fn configure_queues<M: slates_bridge_virtiofs::memory::GuestMemory>(
    &self,
    guest: &Guest<M>,
  ) {
    for (queue, layout) in guest.layouts().iter().enumerate() {
      let index = u32::try_from(queue).unwrap();
      let acks = [
        self.set(
          request::SET_VRING_NUM,
          &Self::state(index, u32::from(layout.size)),
          &[],
        ),
        self.set(request::SET_VRING_ADDR, &ring_addresses(index, layout), &[]),
        self.set(request::SET_VRING_BASE, &Self::state(index, 0), &[]),
        self.set(
          request::SET_VRING_KICK,
          &u64::from(index).to_le_bytes(),
          &[self.kicks[queue].as_fd()],
        ),
        self.set(
          request::SET_VRING_CALL,
          &u64::from(index).to_le_bytes(),
          &[self.calls[queue].as_fd()],
        ),
        self.set(request::SET_VRING_ENABLE, &Self::state(index, 1), &[]),
      ];
      assert_eq!(acks, [0; 6], "queue {queue} configured");
    }
  }

  /// Negotiates as [`FrontEnd::negotiate_memory`] does, for a device that offers a DAX window of `window` bytes: the
  /// shared-memory and back-end-request features accepted, the window's size read (`GET_SHMEM_CONFIG`), and the
  /// back-end-request channel handed over (`SET_BACKEND_REQ_FD`), as crosvm does. The front end's end of that channel,
  /// on which the device asks it to map.
  pub(crate) fn negotiate_dax(&self, ram: &OwnedFd, window: u64) -> OwnedFd {
    assert_eq!(self.get(request::GET_FEATURES), OFFERED_FEATURES);
    self.send(
      request::SET_FEATURES,
      0,
      &OFFERED_FEATURES.to_le_bytes(),
      &[],
    );
    let protocol = OFFERED_PROTOCOL_FEATURES | DAX_PROTOCOL_FEATURES;
    assert_eq!(
      self.get(request::GET_PROTOCOL_FEATURES),
      protocol,
      "a device with a window offers the shared-memory features"
    );
    self.send(
      request::SET_PROTOCOL_FEATURES,
      0,
      &protocol.to_le_bytes(),
      &[],
    );
    assert_eq!(self.get(request::GET_QUEUE_NUM), 2);
    assert_eq!(self.set(request::SET_OWNER, &[], &[]), 0);
    self.send(request::GET_SHMEM_CONFIG, 0, &[], &[]);
    let config = self.reply().unwrap();
    assert_eq!(
      u32::from_le_bytes(config[0..4].try_into().unwrap()),
      1,
      "one shared-memory region: the window"
    );
    assert_eq!(
      u64::from_le_bytes(config[8..16].try_into().unwrap()),
      window,
      "the window's size is the attach's"
    );
    let (ours, theirs) = rustix::net::socketpair(
      AddressFamily::UNIX,
      SocketType::STREAM,
      SocketFlags::CLOEXEC,
      None,
    )
    .unwrap();
    assert_eq!(
      self.set(request::SET_BACKEND_REQ_FD, &[], &[theirs.as_fd()]),
      0,
      "the back-end-request channel is taken"
    );
    assert_eq!(
      self.set(request::SET_MEM_TABLE, &memory_table(), &[ram.as_fd()]),
      0
    );
    ours
  }

  /// Kicks `queue`.
  pub(crate) fn kick(&self, queue: usize) {
    rustix::io::write(&self.kicks[queue], &1u64.to_le_bytes()).unwrap();
  }

  /// Waits for `queue`'s interrupt.
  pub(crate) fn wait_call(&self, queue: usize) {
    let mut fds = [PollFd::new(&self.calls[queue], PollFlags::IN)];
    let timeout = Timespec {
      tv_sec: WAIT_NS / 1_000_000_000,
      tv_nsec: 0,
    };
    assert_eq!(
      poll(&mut fds, Some(&timeout)).unwrap(),
      1,
      "an interrupt in time"
    );
    let mut counter = [0u8; 8];
    rustix::io::read(&self.calls[queue], &mut counter).unwrap();
  }

  /// Kicks `queue` and waits for its interrupt.
  pub(crate) fn kick_and_wait(&self, queue: usize) {
    self.kick(queue);
    self.wait_call(queue);
  }

  /// `GET_VRING_BASE` of `queue`: the ring stops; its position.
  pub(crate) fn stop(&self, queue: u32) -> u32 {
    self.send(request::GET_VRING_BASE, 0, &Self::state(queue, 0), &[]);
    let reply = self.reply().unwrap();
    u32::from_le_bytes(reply[4..8].try_into().unwrap())
  }

  /// Stops every queue at its expected consumed position and checks that suspension keeps control connected.
  pub(crate) fn assert_suspended(&self, positions: &[u32]) {
    for (queue, position) in positions.iter().enumerate() {
      assert_eq!(self.stop(u32::try_from(queue).unwrap()), *position);
    }
    assert_eq!(
      self.get(request::GET_QUEUE_NUM),
      u64::try_from(positions.len()).unwrap()
    );
  }

  /// Restarts a stopped `queue` as QEMU's `vhost_virtqueue_start` does after the guest reset its driver: the ring's
  /// size, its base (QEMU's last available index, zero after a reset), the driver's new ring addresses, then its kick.
  /// The first three acknowledgements must report success (a stopped ring may be reconfigured); the kick's is
  /// returned.
  pub(crate) fn restart_after_reset(
    &self,
    queue: usize,
    layout: &slates_bridge_virtiofs::virtqueue::QueueLayout,
  ) -> u64 {
    let index = u32::try_from(queue).unwrap();
    let acks = [
      self.set(
        request::SET_VRING_NUM,
        &Self::state(index, u32::from(layout.size)),
        &[],
      ),
      self.set(request::SET_VRING_BASE, &Self::state(index, 0), &[]),
      self.set(request::SET_VRING_ADDR, &ring_addresses(index, layout), &[]),
    ];
    assert_eq!(
      acks, [0; 3],
      "queue {queue}'s stopped ring took its new layout"
    );
    self.set(
      request::SET_VRING_KICK,
      &u64::from(index).to_le_bytes(),
      &[self.kicks[queue].as_fd()],
    )
  }

  /// Readdresses a running `queue` to `layout` (a hostile front end: the protocol stops a ring before moving it);
  /// the acknowledgement's status.
  pub(crate) fn readdress(
    &self,
    queue: usize,
    layout: &slates_bridge_virtiofs::virtqueue::QueueLayout,
  ) -> u64 {
    let index = u32::try_from(queue).unwrap();
    self.set(request::SET_VRING_ADDR, &ring_addresses(index, layout), &[])
  }

  /// Restarts a stopped queue by handing its kick descriptor over again; no extra kick is required by the protocol.
  pub(crate) fn restart(&self, queue: usize) {
    assert_eq!(
      self.set(
        request::SET_VRING_KICK,
        &u64::try_from(queue).unwrap().to_le_bytes(),
        &[self.kicks[queue].as_fd()],
      ),
      0
    );
  }
}

/// The memory table: one region, the guest's RAM from guest address zero.
fn memory_table() -> Vec<u8> {
  memory_table_of(&[region_entry()])
}

/// A memory table of `entries`, in their order (each sent with its memory object's descriptor, in the same order).
pub(crate) fn memory_table_of(entries: &[RegionEntry]) -> Vec<u8> {
  let mut table = u32::try_from(entries.len()).unwrap().to_le_bytes().to_vec();
  table.extend_from_slice(&0u32.to_le_bytes());
  for entry in entries {
    for word in [entry.guest_phys, entry.size, entry.user_addr, entry.offset] {
      table.extend_from_slice(&word.to_le_bytes());
    }
  }
  table
}

/// The reply-ack's status from a reply payload.
pub(crate) fn status(reply: &[u8]) -> u64 {
  u64::from_le_bytes(reply.try_into().unwrap())
}

/// A vring-address payload for `layout` in the front end's address space.
fn ring_addresses(index: u32, layout: &slates_bridge_virtiofs::virtqueue::QueueLayout) -> Vec<u8> {
  let mut payload = index.to_le_bytes().to_vec();
  payload.extend_from_slice(&0u32.to_le_bytes());
  for at in [
    layout.descriptor_table.0,
    layout.used_ring.0,
    layout.available_ring.0,
    0,
  ] {
    let user = if at == 0 { 0 } else { USER_BASE + at };
    payload.extend_from_slice(&user.to_le_bytes());
  }
  payload
}
