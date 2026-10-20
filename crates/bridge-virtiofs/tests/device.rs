//! The FUSE-over-virtio request cycle proven by use (§4.6 A-9; AC-4.11/T-4.13's guest leg,
//! AC-4.12/T-4.14): a simulated guest driver builds real virtqueues, submits FUSE requests through
//! them as descriptor chains, and reads the replies back from its buffers, against a real
//! `VolumeBridge` over a real scratch volume — create → write → getattr → read → readdir → release →
//! unlink → forget → statfs → destroy. The differential oracle: the same requests issued through the
//! FUSE codec's `dispatch` directly, over a second identical scratch volume with the same
//! deterministic clock, must produce byte-identical replies.
// Test harness code: an unwrap here is a failed test.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing
)]

mod common;

use common::{
  SimDriver, context, context_for, layout_at, message, other_vid, reply_error, store, vid, volume,
  volume_with_prefix,
};
use proptest::prelude::*;
use slates_bridge_core::{Bridge, ObjectId, OpContext, VolumeBridge};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode, flags};
use slates_bridge_fuse::bridge::dispatch;
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_virtiofs::credit::Unlimited;
use slates_bridge_virtiofs::device::{
  Device, DeviceConfig, DeviceError, FIRST_REQUEST_QUEUE, FsTag, HIPRIO_QUEUE, TAG_LEN,
};
use slates_bridge_virtiofs::memory::{GuestMemory, GuestMemoryError, GuestRange};
use slates_bridge_virtiofs::sim::SimGuestMemory;
use slates_bridge_virtiofs::virtqueue::{VIRTQ_DESC_F_WRITE, VirtqueueError};

/// Shape: the reply room posted for every request-queue request: 8 KiB holds any reply in the
/// script (a 2 000-byte read, a 4 KiB readdir, the headers).
const REPLY_CAP: u32 = 8192;
/// Shape: the chains a service pass takes at most in these tests — more than any script step
/// publishes at once.
const BATCH: u32 = 16;
/// Shape: the queue sizes: a 16-entry hiprio queue and a 64-entry request queue (a 3-way split of
/// request and reply needs six descriptors per request).
const QUEUE_SIZES: [u16; 2] = [16, 64];
/// Format: `FUSE_MAP_ALIGNMENT` (`include/uapi/linux/fuse.h`), the INIT flag a DAX-capable device
/// sets; a guest that supports DAX offers it.
const FUSE_MAP_ALIGNMENT: u64 = 1 << 26;
/// Format: `ENOENT`, `EIO`.
const ENOENT: i32 = 2;
const EIO: i32 = 5;
/// Linux's `EOPNOTSUPP`, the errno a map refuses with when the host cannot carve a confined descriptor
/// (`device.rs`'s `dax_errno::EOPNOTSUPP`, which is private to that crate). Gated like the one test that
/// reads it: an ungated constant used only from a `cfg(linux)` test is dead code on every other host.
#[cfg(target_os = "linux")]
const EOPNOTSUPP: i32 = 95;
/// Format: `O_RDWR`.
const O_RDWR: u32 = 2;
/// Format: the FUSE node id of the root directory.
const ROOT: u64 = 1;

fn u64_at(bytes: &[u8], at: usize) -> u64 {
  u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

/// `fuse_init_in`: major, minor, max_readahead, flags.
fn init_body(flags: u32) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&7u32.to_le_bytes());
  b.extend_from_slice(&36u32.to_le_bytes());
  b.extend_from_slice(&131_072u32.to_le_bytes());
  b.extend_from_slice(&flags.to_le_bytes());
  b
}

/// `fuse_create_in` (flags, mode, umask, open_flags) then the name.
fn create_body(name: &str) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&O_RDWR.to_le_bytes());
  b.extend_from_slice(&0o644u32.to_le_bytes());
  b.extend_from_slice(&0u32.to_le_bytes());
  b.extend_from_slice(&0u32.to_le_bytes());
  b.extend_from_slice(name.as_bytes());
  b.push(0);
  b
}

/// `fuse_write_in` (fh, offset, size, write_flags, lock_owner, flags, padding) then the data.
fn write_body(fh: u64, offset: u64, data: &[u8]) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&fh.to_le_bytes());
  b.extend_from_slice(&offset.to_le_bytes());
  b.extend_from_slice(&u32::try_from(data.len()).unwrap().to_le_bytes());
  b.extend_from_slice(&[0u8; 20]);
  b.extend_from_slice(data);
  b
}

/// `fuse_read_in`: fh, offset, size, read_flags, lock_owner, flags, padding (40 bytes).
fn read_body(fh: u64, offset: u64, size: u32) -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&fh.to_le_bytes());
  b.extend_from_slice(&offset.to_le_bytes());
  b.extend_from_slice(&size.to_le_bytes());
  b.extend_from_slice(&[0u8; 20]);
  b
}

/// `fuse_release_in`: fh, flags, release_flags, lock_owner (24 bytes).
fn release_body(fh: u64) -> Vec<u8> {
  let mut b = fh.to_le_bytes().to_vec();
  b.extend_from_slice(&[0u8; 16]);
  b
}

fn name_body(name: &str) -> Vec<u8> {
  let mut b = name.as_bytes().to_vec();
  b.push(0);
  b
}

/// One service pass of `queue` with no credit accounting (the codec-level tests), as an owner whose barrier
/// always captures runs it: a chain held for the barrier (AUD-29-82) is completed and counted as served.
fn serve(
  device: &mut Device,
  queue: u16,
  driver: &mut SimDriver,
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  batch: u32,
) -> Result<slates_bridge_virtiofs::device::Serviced, DeviceError> {
  let mut pass =
    device.service_queue(queue, &mut driver.memory, bridge, cx, batch, &mut Unlimited)?;
  if pass.barrier_owed {
    device.complete_awaiting(&mut driver.memory, bridge, cx, true, &mut Unlimited)?;
    pass.served += 1;
    pass.barrier_owed = false;
  }
  Ok(pass)
}

/// The two legs of the oracle: the device over one scratch volume and direct dispatch over another.
struct Legs<'a, 'v> {
  driver: &'a mut SimDriver,
  device: &'a mut Device,
  via_device: &'a mut VolumeBridge<'v>,
  direct: &'a mut VolumeBridge<'v>,
  cx: &'a OpContext,
  unique: u64,
}

impl Legs<'_, '_> {
  /// Sends one request through both legs and asserts the replies are byte-identical; returns them.
  fn exchange(
    &mut self,
    queue: u16,
    opcode: Opcode,
    nodeid: u64,
    body: &[u8],
    split: usize,
  ) -> Vec<u8> {
    self.unique += 1;
    let request = message(opcode.to_wire(), self.unique, nodeid, body);
    let capacity = if queue == HIPRIO_QUEUE { 0 } else { REPLY_CAP };
    let head = self
      .driver
      .submit(usize::from(queue), &request, capacity, split);
    let serviced = serve(
      self.device,
      queue,
      self.driver,
      self.via_device,
      self.cx,
      BATCH,
    )
    .unwrap();
    assert_eq!(serviced.served, 1, "{opcode:?}: one chain served");
    let (id, len) = self
      .driver
      .reap(usize::from(queue))
      .expect("a used element");
    assert_eq!(id, head, "{opcode:?}: the used element names the chain");
    let via_device = self.driver.reply_of(usize::from(queue), head, len);

    let mut out = vec![0u8; usize::try_from(REPLY_CAP).unwrap()];
    let n = dispatch(&request, self.direct, self.cx, &mut out);
    assert_eq!(
      via_device,
      &out[..n],
      "{opcode:?}: the reply through the device differs from direct dispatch"
    );
    via_device
  }
}

/// A configured device over a simulated driver with a hiprio and a request queue.
fn device_and_driver() -> (Device, SimDriver) {
  let driver = SimDriver::new(&QUEUE_SIZES);
  let mut device = Device::new(DeviceConfig::new(FsTag::new("slates").unwrap()));
  device.configure(&driver.layouts(), &driver.memory).unwrap();
  (device, driver)
}

/// Phase 1 of the cycle: INIT (with DAX offered and not advertised), a lookup miss, CREATE; returns
/// the created inode and its handle, read from the reply both legs agreed on.
fn phase_init_and_create(legs: &mut Legs<'_, '_>) -> (u64, u64) {
  let rq = FIRST_REQUEST_QUEUE;
  let init = legs.exchange(
    rq,
    Opcode::Init,
    0,
    &init_body(u32::try_from(flags::BIG_WRITES | FUSE_MAP_ALIGNMENT).unwrap()),
    1,
  );
  assert_eq!(reply_error(&init), 0);
  assert!(
    legs.device.negotiated().is_some(),
    "the device observed INIT"
  );
  let miss = legs.exchange(rq, Opcode::Lookup, ROOT, &name_body("hello"), 1);
  // A guest has no invalidation channel, so a miss has no lifetime to cache and is answered ENOENT, never a
  // negative entry (AUD-29-79; the bridge's negative entries are for mounts it can invalidate).
  assert_eq!(reply_error(&miss), -ENOENT);
  let created = legs.exchange(rq, Opcode::Create, ROOT, &create_body("hello"), 2);
  assert_eq!(reply_error(&created), 0);
  (
    u64_at(&created, OUT_HEADER_LEN),
    u64_at(&created, OUT_HEADER_LEN + EntryOut::LEN),
  )
}

/// Phase 2: a 3 000-byte write gathered from three descriptors, getattr, a 2 000-byte read
/// scattered across three.
fn phase_write_and_read(legs: &mut Legs<'_, '_>, ino: u64, fh: u64) {
  let rq = FIRST_REQUEST_QUEUE;
  let payload: Vec<u8> = (0..3000u32)
    .map(|i| u8::try_from(i % 251).unwrap())
    .collect();
  let written = legs.exchange(rq, Opcode::Write, ino, &write_body(fh, 0, &payload), 3);
  assert_eq!(reply_error(&written), 0);
  assert_eq!(
    u32::from_le_bytes(
      written[OUT_HEADER_LEN..OUT_HEADER_LEN + 4]
        .try_into()
        .unwrap()
    ),
    3000,
    "the write landed whole"
  );
  let attr = legs.exchange(rq, Opcode::GetAttr, ino, &[0u8; 16], 1);
  // fuse_attr_out: attr_valid (8), attr_valid_nsec (4), dummy (4), then fuse_attr: ino (8), size (8).
  assert_eq!(
    u64_at(&attr, OUT_HEADER_LEN + 16 + 8),
    3000,
    "getattr reports the size"
  );
  let read = legs.exchange(rq, Opcode::Read, ino, &read_body(fh, 100, 2000), 3);
  assert_eq!(reply_error(&read), 0);
  assert_eq!(
    &read[OUT_HEADER_LEN..],
    &payload[100..2100],
    "the read scattered the right bytes"
  );
}

/// Phase 3: opendir, readdir (the file is listed), releasedir.
fn phase_directory(legs: &mut Legs<'_, '_>) {
  let rq = FIRST_REQUEST_QUEUE;
  let opened = legs.exchange(rq, Opcode::OpenDir, ROOT, &[0u8; 8], 1);
  let dfh = u64_at(&opened, OUT_HEADER_LEN);
  let listing = legs.exchange(rq, Opcode::ReadDir, ROOT, &read_body(dfh, 0, 4096), 2);
  assert!(
    listing[OUT_HEADER_LEN..].windows(5).any(|w| w == b"hello"),
    "the listing names the file"
  );
  assert_eq!(
    reply_error(&legs.exchange(rq, Opcode::ReleaseDir, ROOT, &release_body(dfh), 1)),
    0
  );
}

/// Phase 4: release, unlink, the lookup reference dropped by a hiprio FORGET (no reply), the inode
/// reclaimed, statfs, DESTROY.
fn phase_teardown(legs: &mut Legs<'_, '_>, ino: u64, fh: u64) {
  let rq = FIRST_REQUEST_QUEUE;
  assert_eq!(
    reply_error(&legs.exchange(rq, Opcode::Release, ino, &release_body(fh), 1)),
    0
  );
  assert_eq!(
    reply_error(&legs.exchange(rq, Opcode::Unlink, ROOT, &name_body("hello"), 1)),
    0
  );
  let forgotten = legs.exchange(HIPRIO_QUEUE, Opcode::Forget, ino, &1u64.to_le_bytes(), 1);
  assert!(forgotten.is_empty(), "FORGET has no reply");
  let gone = legs.exchange(rq, Opcode::GetAttr, ino, &[0u8; 16], 1);
  assert_eq!(
    reply_error(&gone),
    -ENOENT,
    "unlinked, released and forgotten: reclaimed"
  );
  assert_eq!(
    reply_error(&legs.exchange(rq, Opcode::StatFs, ROOT, &[], 1)),
    0
  );
  assert_eq!(
    reply_error(&legs.exchange(rq, Opcode::Destroy, 0, &[], 1)),
    0
  );
}

/// AC-4.11/T-4.13 (the guest leg): the whole FUSE cycle through real virtqueues — INIT, a lookup
/// miss, create, a write split across three descriptors, getattr, a read scattered across three,
/// opendir/readdir/releasedir, release, unlink, a hiprio FORGET, the reclaimed inode, statfs,
/// DESTROY — byte-identical with the same requests dispatched directly, and every counter moved.
#[test]
fn a_guest_driver_round_trips_the_fuse_cycle_byte_identical_with_direct_dispatch() {
  let (mut device, mut driver) = device_and_driver();
  let (mut store_a, mut store_b) = (store(), store());
  let (mut vol_a, mut vol_b) = (volume(&mut store_a), volume(&mut store_b));
  let mut via_device = VolumeBridge::new(vid(), &mut vol_a, &mut store_a);
  let mut direct = VolumeBridge::new(vid(), &mut vol_b, &mut store_b);
  let cx = context();
  let mut legs = Legs {
    driver: &mut driver,
    device: &mut device,
    via_device: &mut via_device,
    direct: &mut direct,
    cx: &cx,
    unique: 0,
  };
  let (ino, fh) = phase_init_and_create(&mut legs);
  phase_write_and_read(&mut legs, ino, fh);
  phase_directory(&mut legs);
  phase_teardown(&mut legs, ino, fh);

  let counters = legs.device.counters();
  assert_eq!(
    counters.requests_served, 14,
    "every request-queue chain was served"
  );
  assert_eq!(counters.hiprio_served, 1);
  assert_eq!(counters.no_reply, 1, "the FORGET");
  assert_eq!(counters.init_seen, 1);
  assert_eq!(counters.destroy_seen, 1);
  assert_eq!(counters.replies_truncated, 0);
  assert!(counters.bytes_gathered > 3000 && counters.bytes_scattered > 2000);
  assert_eq!(
    legs
      .device
      .queue_counters(FIRST_REQUEST_QUEUE)
      .unwrap()
      .used_published,
    14
  );
}

/// AC-4.12 "Do not advertise DAX without this gate": a guest that offers `FUSE_MAP_ALIGNMENT` (the
/// DAX flag) gets an INIT reply without it, so it never sets up a mapping.
#[test]
fn init_does_not_advertise_dax_map_alignment() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let offered = u32::try_from(flags::BIG_WRITES | flags::DONT_MASK | FUSE_MAP_ALIGNMENT).unwrap();
  let request = message(Opcode::Init.to_wire(), 1, 0, &init_body(offered));
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let head = driver.submit(rq, &request, REPLY_CAP, 1);
  serve(
    &mut device,
    FIRST_REQUEST_QUEUE,
    &mut driver,
    &mut bridge,
    &cx,
    BATCH,
  )
  .unwrap();
  let (_, len) = driver.reap(rq).unwrap();
  let reply = driver.reply_of(rq, head, len);
  // fuse_init_out: major (4), minor (4), max_readahead (4), flags (4).
  let negotiated = u32::from_le_bytes(
    reply[OUT_HEADER_LEN + 12..OUT_HEADER_LEN + 16]
      .try_into()
      .unwrap(),
  );
  assert_eq!(
    negotiated & u32::try_from(FUSE_MAP_ALIGNMENT).unwrap(),
    0,
    "DAX is not advertised"
  );
  assert_ne!(
    negotiated & u32::try_from(flags::BIG_WRITES).unwrap(),
    0,
    "a wanted flag is kept (non-vacuous)"
  );
  assert_eq!(device.negotiated().unwrap().flags & FUSE_MAP_ALIGNMENT, 0);
}

/// §4.3 "bounded work everywhere": a service pass takes at most its batch and reports more pending;
/// the next pass takes the rest.
#[test]
fn a_service_pass_is_bounded_by_its_batch() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  for unique in 1..=5u64 {
    driver.submit(
      rq,
      &message(Opcode::GetAttr.to_wire(), unique, 1, &[0u8; 16]),
      REPLY_CAP,
      1,
    );
  }
  let first = serve(
    &mut device,
    FIRST_REQUEST_QUEUE,
    &mut driver,
    &mut bridge,
    &cx,
    2,
  )
  .unwrap();
  assert_eq!((first.served, first.more_pending), (2, true));
  let second = serve(
    &mut device,
    FIRST_REQUEST_QUEUE,
    &mut driver,
    &mut bridge,
    &cx,
    10,
  )
  .unwrap();
  assert_eq!((second.served, second.more_pending), (3, false));
  assert!(
    second.interrupt_wanted,
    "the driver did not suppress interrupts"
  );
  assert_eq!((0..5).filter_map(|_| driver.reap(rq)).count(), 5);
}

/// A request shorter than the FUSE header, and a request-queue chain without room for a reply
/// header, each fault the device typed: the refusal is returned now and on every later pass.
#[test]
fn a_malformed_request_faults_the_device() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let rq = FIRST_REQUEST_QUEUE;
  driver.submit(usize::from(rq), &[1u8; 10], REPLY_CAP, 1);
  let refused = serve(&mut device, rq, &mut driver, &mut bridge, &cx, BATCH).unwrap_err();
  assert_eq!(
    refused,
    DeviceError::RequestTooShort {
      queue: rq,
      readable: 10,
      need: u64::try_from(IN_HEADER_LEN).unwrap()
    }
  );
  assert_eq!(
    serve(&mut device, rq, &mut driver, &mut bridge, &cx, BATCH).unwrap_err(),
    refused,
    "the fault persists"
  );

  let (mut device, mut driver) = device_and_driver();
  driver.submit(
    usize::from(rq),
    &message(Opcode::GetAttr.to_wire(), 1, 1, &[0u8; 16]),
    8,
    1,
  );
  assert_eq!(
    serve(&mut device, rq, &mut driver, &mut bridge, &cx, BATCH).unwrap_err(),
    DeviceError::ReplyBufferTooSmall {
      queue: rq,
      writable: 8,
      need: u64::try_from(OUT_HEADER_LEN).unwrap()
    }
  );
}

/// A reply the guest's writable buffers cannot hold is answered `EIO` in the room there is, never
/// left waiting; the truncation is counted.
#[test]
fn a_reply_larger_than_the_posted_buffer_is_answered_eio() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let head = driver.submit(
    rq,
    &message(Opcode::GetAttr.to_wire(), 1, 1, &[0u8; 16]),
    40,
    1,
  );
  serve(
    &mut device,
    FIRST_REQUEST_QUEUE,
    &mut driver,
    &mut bridge,
    &cx,
    BATCH,
  )
  .unwrap();
  let (_, len) = driver.reap(rq).unwrap();
  let reply = driver.reply_of(rq, head, len);
  assert_eq!(len, u32::try_from(OUT_HEADER_LEN).unwrap());
  assert_eq!(reply_error(&reply), -EIO);
  assert_eq!(device.counters().replies_truncated, 1);
}

/// Before the driver configures the queues nothing is served; a queue index past the configured
/// count is refused; a wrong queue count is refused at configuration; a malformed ring is refused
/// typed through the device.
#[test]
fn configuration_is_validated_before_any_queue_exists() {
  let mut driver = SimDriver::new(&QUEUE_SIZES);
  let mut device = Device::new(DeviceConfig::new(FsTag::new("slates").unwrap()));
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  assert_eq!(
    serve(
      &mut device,
      FIRST_REQUEST_QUEUE,
      &mut driver,
      &mut bridge,
      &cx,
      BATCH
    )
    .unwrap_err(),
    DeviceError::NotConfigured
  );
  assert_eq!(
    device
      .configure(&driver.layouts()[..1], &driver.memory)
      .unwrap_err(),
    DeviceError::QueueCountMismatch {
      offered: 1,
      required: 2
    }
  );
  let mut bad = driver.layouts();
  bad[1].size = 3;
  assert_eq!(
    device.configure(&bad, &driver.memory).unwrap_err(),
    DeviceError::Virtqueue(VirtqueueError::QueueSizeInvalid { size: 3 })
  );
  device.configure(&driver.layouts(), &driver.memory).unwrap();
  assert_eq!(
    serve(&mut device, 2, &mut driver, &mut bridge, &cx, BATCH).unwrap_err(),
    DeviceError::QueueIndexOutOfRange {
      queue: 2,
      queues: 2
    }
  );
  assert_eq!(device.config().num_request_queues.get(), 1);
}

/// The tag is validated for the configuration field (§5.11.4): empty, over 36 bytes and a NUL are
/// refused; a 36-byte tag fills the field unterminated; a shorter one is NUL-padded.
#[test]
fn a_tag_is_validated_and_laid_out_as_the_configuration_field() {
  assert_eq!(FsTag::new("").unwrap_err(), DeviceError::TagEmpty);
  assert_eq!(
    FsTag::new(&"x".repeat(TAG_LEN + 1)).unwrap_err(),
    DeviceError::TagTooLong {
      bytes: TAG_LEN + 1,
      max: TAG_LEN
    }
  );
  assert_eq!(FsTag::new("a\0b").unwrap_err(), DeviceError::TagHasNul);
  let full = FsTag::new(&"y".repeat(TAG_LEN)).unwrap();
  assert!(
    full.config_bytes().iter().all(|b| *b == b'y'),
    "unterminated when full"
  );
  let short = FsTag::new("slates").unwrap();
  let bytes = short.config_bytes();
  assert_eq!(&bytes[..6], b"slates");
  assert!(bytes[6..].iter().all(|b| *b == 0), "NUL-padded");
  assert_eq!(short.as_str(), "slates");
}

/// AUD-29-85. Do: post a CREATE with reply room for the header alone (the §7.6 witness's 16 bytes); then
/// post it again with room. Expect: the first answers `EIO` with no effect — the file is absent (before, it
/// was created with an open handle no reply named); the retry creates it.
#[test]
fn an_undersized_create_has_no_effect_and_its_retry_succeeds() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = context();
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let header_only = u32::try_from(OUT_HEADER_LEN).unwrap();
  let create = message(Opcode::Create.to_wire(), 1, ROOT, &create_body("made"));
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    let head = driver.submit(rq, &create, header_only, 1);
    serve(
      &mut device,
      FIRST_REQUEST_QUEUE,
      &mut driver,
      &mut bridge,
      &cx,
      BATCH,
    )
    .unwrap();
    let (_, len) = driver.reap(rq).unwrap();
    assert_eq!(reply_error(&driver.reply_of(rq, head, len)), -EIO);
    let root = ObjectId::new(bridge.root(&cx).unwrap(), 0);
    assert!(
      bridge.lookup(root, &cx, "made").is_err(),
      "no file was made"
    );
    let head = driver.submit(rq, &create, REPLY_CAP, 1);
    serve(
      &mut device,
      FIRST_REQUEST_QUEUE,
      &mut driver,
      &mut bridge,
      &cx,
      BATCH,
    )
    .unwrap();
    let (_, len) = driver.reap(rq).unwrap();
    assert_eq!(
      reply_error(&driver.reply_of(rq, head, len)),
      0,
      "the retry succeeded"
    );
  }
  let root = vol.root_inode(&store).unwrap();
  assert!(
    vol.lookup_no(&store, root, "made").is_ok(),
    "the retry made the file"
  );
  assert_eq!(
    device.counters().replies_truncated,
    1,
    "the short room was refused once"
  );
  assert_eq!(
    device.counters().reclaimed,
    0,
    "nothing was granted and lost"
  );
}

/// Guest memory whose region under `withdrawn` fails every write: a VMM that withdrew the reply buffer's
/// memory after the device validated the chain, so the scatter fails after the request's effect.
struct WithdrawnAfterValidation<'m> {
  memory: &'m mut SimGuestMemory,
  withdrawn: GuestRange,
}

impl GuestMemory for WithdrawnAfterValidation<'_> {
  fn check(&self, range: GuestRange) -> Result<(), GuestMemoryError> {
    self.memory.check(range)
  }

  fn read(&self, range: GuestRange, out: &mut [u8]) -> Result<(), GuestMemoryError> {
    self.memory.read(range, out)
  }

  fn write(&mut self, range: GuestRange, bytes: &[u8]) -> Result<(), GuestMemoryError> {
    if range.overlaps(&self.withdrawn) {
      return Err(GuestMemoryError::OutsideGuestMemory {
        start: range.start().0,
        len: range.len(),
      });
    }
    self.memory.write(range, bytes)
  }
  fn order(&self, edge: slates_bridge_virtiofs::memory::Edge) {
    self.memory.order(edge);
  }
}

/// AUD-29-85. Do: post a CREATE whose reply buffer's memory fails the write after the chain was validated, so
/// the reply cannot be scattered after the file is made; then unlink the file. Expect: the device faults typed
/// (a guest-memory refusal), and what the lost reply granted is given back — its lookup reference and its open
/// handle (the counter names two) — so once unlinked, the inode is reclaimed (before, the handle and reference
/// no reply named held it until teardown).
#[test]
fn a_reply_the_guest_cannot_take_gives_back_what_it_granted() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let cx = context();
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    let head = driver.submit(
      rq,
      &message(Opcode::Create.to_wire(), 1, ROOT, &create_body("lost")),
      REPLY_CAP,
      1,
    );
    let withdrawn = driver.writable_of(rq, head)[0];
    let mut memory = WithdrawnAfterValidation {
      memory: &mut driver.memory,
      withdrawn,
    };
    let refused = device
      .service_queue(
        FIRST_REQUEST_QUEUE,
        &mut memory,
        &mut bridge,
        &cx,
        BATCH,
        &mut Unlimited,
      )
      .unwrap_err();
    assert!(matches!(refused, DeviceError::Memory(_)), "{refused:?}");
  }
  assert_eq!(
    device.counters().reclaimed,
    2,
    "one reference and one handle"
  );
  let root = vol.root_inode(&store).unwrap();
  let lost = vol.lookup_no(&store, root, "lost").unwrap().inode;
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    let root = ObjectId::new(bridge.root(&cx).unwrap(), 0);
    bridge.unlink(root, &cx, "lost").unwrap();
  }
  assert!(
    vol.stat(&store, lost).is_err(),
    "the unlinked inode was reclaimed: nothing references it"
  );
}

/// AUD-29-82 (§4.8, D-18). Do: post a CREATE, serve it, and look for its used element before the owner's
/// barrier; complete it with the barrier refused; then post a WRITE on a captured file and serve it. Expect:
/// the CREATE's pass reports the barrier owed and publishes no used element (the guest cannot see the reply);
/// the refused completion answers `EIO` in its place and gives back the reply's reference and handle; a WRITE
/// (made stable by the flush that follows, as an NFS unstable write) never waits for a barrier.
#[test]
fn a_mutations_used_element_waits_for_the_owners_barrier() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let head = driver.submit(
    rq,
    &message(Opcode::Create.to_wire(), 1, ROOT, &create_body("held")),
    REPLY_CAP,
    1,
  );
  let pass = device
    .service_queue(
      FIRST_REQUEST_QUEUE,
      &mut driver.memory,
      &mut bridge,
      &cx,
      BATCH,
      &mut Unlimited,
    )
    .unwrap();
  assert!(pass.barrier_owed);
  assert!(
    driver.reap(rq).is_none(),
    "no used element before the barrier"
  );
  let completed = device
    .complete_awaiting(&mut driver.memory, &mut bridge, &cx, false, &mut Unlimited)
    .unwrap()
    .unwrap();
  assert!(completed.refused);
  let (id, len) = driver.reap(rq).unwrap();
  assert_eq!(id, head);
  assert_eq!(reply_error(&driver.reply_of(rq, head, len)), -EIO);
  let counters = device.counters();
  assert_eq!(
    (
      counters.barriers_awaited,
      counters.barriers_refused,
      counters.reclaimed
    ),
    (1, 1, 2)
  );

  let created = {
    let head = driver.submit(
      rq,
      &message(Opcode::Create.to_wire(), 2, ROOT, &create_body("kept")),
      REPLY_CAP,
      1,
    );
    serve(
      &mut device,
      FIRST_REQUEST_QUEUE,
      &mut driver,
      &mut bridge,
      &cx,
      BATCH,
    )
    .unwrap();
    let (_, len) = driver.reap(rq).unwrap();
    driver.reply_of(rq, head, len)
  };
  let ino = u64::from_le_bytes(
    created[OUT_HEADER_LEN..OUT_HEADER_LEN + 8]
      .try_into()
      .unwrap(),
  );
  let fh = u64::from_le_bytes(
    created[OUT_HEADER_LEN + EntryOut::LEN..OUT_HEADER_LEN + EntryOut::LEN + 8]
      .try_into()
      .unwrap(),
  );
  driver.submit(
    rq,
    &message(
      Opcode::Write.to_wire(),
      3,
      ino,
      &write_body(fh, 0, b"bytes"),
    ),
    REPLY_CAP,
    1,
  );
  let pass = device
    .service_queue(
      FIRST_REQUEST_QUEUE,
      &mut driver.memory,
      &mut bridge,
      &cx,
      BATCH,
      &mut Unlimited,
    )
    .unwrap();
  assert!(!pass.barrier_owed, "a write waits for no barrier");
  assert_eq!(pass.served, 1);
  assert_eq!(
    device.counters().barriers_awaited,
    2,
    "only the two CREATEs waited"
  );
}

/// Format: the descriptor table's alignment (virtio 1.2 §2.7, 16 bytes).
const DESCRIPTOR_TABLE_ALIGN: u64 = 16;

/// AUD-29-71. Do: configure a device whose driver gives both queues the same layout, then one whose request
/// queue's descriptor table starts inside the high-priority queue's used ring. Expect: each refused
/// `QueuesOverlap` naming the two queues, before any queue is served. Before, each queue was validated alone
/// and both layouts were accepted.
#[test]
fn queues_sharing_ring_bytes_are_refused_at_configuration() {
  let driver = SimDriver::new(&QUEUE_SIZES);
  let first = driver.layout(0);
  let mut device = Device::new(DeviceConfig::new(FsTag::new("slates").unwrap()));
  let same = [first, layout_at(first.descriptor_table.0, QUEUE_SIZES[1])];
  assert_eq!(
    device.configure(&same, &driver.memory).unwrap_err(),
    DeviceError::QueuesOverlap {
      first: 0,
      second: 1
    }
  );
  // A descriptor table must be 16-byte aligned (§2.7); rounded down, the start still lies inside the first
  // queue's rings.
  let inside_used = layout_at(
    first.used_ring.0 & !(DESCRIPTOR_TABLE_ALIGN - 1),
    QUEUE_SIZES[1],
  );
  assert_eq!(
    device
      .configure(&[first, inside_used], &driver.memory)
      .unwrap_err(),
    DeviceError::QueuesOverlap {
      first: 0,
      second: 1
    }
  );
}

/// Shape: the reply room of the aliasing descriptor — any length that reaches into the other queue's table.
const ALIASED_REPLY: u32 = 64;

/// AUD-29-71. Do: post a GETATTR on the request queue whose writable descriptor points at the high-priority
/// queue's descriptor table. Expect: the device refuses the chain before any access — the buffer aliases
/// another queue's ring — and the high-priority queue's table is unchanged. Before, the reply was scattered
/// over it.
#[test]
fn a_buffer_aliasing_another_queues_ring_is_refused_before_access() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let other_table = driver.layout(0).descriptor_table.0;
  let before = driver.read_u32(other_table);
  driver.submit(
    rq,
    &message(Opcode::GetAttr.to_wire(), 1, 1, &[0u8; 16]),
    ALIASED_REPLY,
    1,
  );
  // A fresh queue hands out descriptor 0 for the request and 1 for its reply room.
  driver.descriptor(rq, 1, other_table, ALIASED_REPLY, VIRTQ_DESC_F_WRITE, 0);
  let refused = serve(
    &mut device,
    FIRST_REQUEST_QUEUE,
    &mut driver,
    &mut bridge,
    &cx,
    BATCH,
  )
  .unwrap_err();
  assert_eq!(
    refused,
    DeviceError::Virtqueue(VirtqueueError::BufferOverlapsOtherQueue { at: 1 })
  );
  assert_eq!(
    driver.read_u32(other_table),
    before,
    "the other queue's table is untouched"
  );
}

/// Format: offsets inside the rings (virtio 1.2 §2.7.6, §2.7.8): the available ring's `idx` after its
/// `flags`, and the used ring's `idx` and first element after its `flags`.
const AVAIL_IDX_AT: u64 = 2;
const USED_IDX_AT: u64 = 2;
const USED_RING_AT: u64 = 4;

/// AUD-29-72 (virtio 1.2 §2.7.8.2, §2.7.10, §2.7.13.3). Do: record the guest-memory accesses and ordering
/// edges while the device serves one GETATTR. Expect: an acquire edge right after the available index is read
/// and before any descriptor or request byte; a release edge after the reply and the used element are written
/// and right before the used index is; a full edge after the used index and before the driver's notification
/// flags are read. Before, the device asked the memory for no ordering at all.
#[test]
fn the_ring_protocol_asks_for_its_ordering_edges_where_virtio_puts_them() {
  use slates_bridge_virtiofs::memory::Edge;
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let layout = driver.layout(rq);
  driver.submit(
    rq,
    &message(Opcode::GetAttr.to_wire(), 1, 1, &[0u8; 16]),
    REPLY_CAP,
    1,
  );
  driver.memory.record_accesses(true);
  serve(
    &mut device,
    FIRST_REQUEST_QUEUE,
    &mut driver,
    &mut bridge,
    &cx,
    BATCH,
  )
  .unwrap();
  assert_eq!(
    driver.memory.accesses_dropped(),
    0,
    "the access log holds every access this test reasons about"
  );
  let accesses = driver.memory.accesses();
  let edges = driver.memory.edges();
  let at = |start: u64, write: bool| {
    accesses
      .iter()
      .position(|a| a.range.start().0 == start && a.write == write)
      .unwrap_or_else(|| panic!("an access at {start:#x}"))
  };
  let edge = |kind: Edge| {
    edges
      .iter()
      .find(|(_, e)| *e == kind)
      .map(|(position, _)| *position)
      .unwrap_or_else(|| panic!("a {kind:?} edge"))
  };
  let avail_idx = at(layout.available_ring.0 + AVAIL_IDX_AT, false);
  let first_descriptor = at(layout.descriptor_table.0, false);
  let used_element = at(layout.used_ring.0 + USED_RING_AT, true);
  let used_idx = at(layout.used_ring.0 + USED_IDX_AT, true);
  let flags = accesses
    .iter()
    .rposition(|a| a.range.start().0 == layout.available_ring.0 && !a.write)
    .unwrap();
  let acquire = edge(Edge::Acquire);
  let release = edge(Edge::Release);
  let full = edge(Edge::Full);
  assert!(
    avail_idx < acquire && acquire <= first_descriptor,
    "acquire after the index, before the descriptors"
  );
  assert!(
    used_element < release && release == used_idx,
    "release after the element, right before the index"
  );
  assert!(
    used_idx < full && full <= flags,
    "full after the index, before the flags"
  );
}

/// One request through the device with room for any reply; the reply.
fn exchange_once(
  device: &mut Device,
  driver: &mut SimDriver,
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  request: &[u8],
) -> Vec<u8> {
  let rq = usize::from(FIRST_REQUEST_QUEUE);
  let head = driver.submit(rq, request, REPLY_CAP, 1);
  serve(device, FIRST_REQUEST_QUEUE, driver, bridge, cx, BATCH).unwrap();
  let (_, len) = driver.reap(rq).unwrap();
  driver.reply_of(rq, head, len)
}

/// The little-endian `u64` at `at` of a reply's body.
fn body_u64(reply: &[u8], at: usize) -> u64 {
  u64::from_le_bytes(
    reply[OUT_HEADER_LEN + at..OUT_HEADER_LEN + at + 8]
      .try_into()
      .unwrap(),
  )
}

/// Format: offsets inside reply bodies — `fuse_init_out`'s flags word after major, minor and max_readahead;
/// `fuse_entry_out`'s `entry_valid` after the node id and generation; `fuse_attr_out`'s `attr_valid` first.
const INIT_FLAGS_AT: usize = 12;
const ENTRY_VALID_AT: usize = 16;
const ATTR_VALID_AT: usize = 0;

/// AUD-29-79. Do: a guest kernel offers explicit data invalidation and automatic data invalidation at INIT,
/// then creates a file and asks its attributes and its name. Expect: the device's INIT answer does not
/// negotiate explicit invalidation (the device has no queue to deliver one) and asks for automatic data
/// invalidation; the attribute and the entry lifetimes are zero, so the guest revalidates on every use and
/// converges with any other attachment's change; the guest capability reports no explicit invalidation.
/// Before, explicit invalidation was negotiated and both lifetimes were u64::MAX seconds, a promise no
/// delivery kept.
#[test]
fn a_guest_without_invalidation_delivery_is_promised_no_cache() {
  let (mut device, mut driver) = device_and_driver();
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let offered = u32::try_from(flags::EXPLICIT_INVAL_DATA | flags::AUTO_INVAL_DATA).unwrap();
  let init = exchange_once(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    &message(Opcode::Init.to_wire(), 1, 0, &init_body(offered)),
  );
  let answered = u64::from(u32::from_le_bytes(
    init[OUT_HEADER_LEN + INIT_FLAGS_AT..OUT_HEADER_LEN + INIT_FLAGS_AT + 4]
      .try_into()
      .unwrap(),
  ));
  assert_eq!(
    answered & flags::EXPLICIT_INVAL_DATA,
    0,
    "explicit invalidation is not promised"
  );
  assert_ne!(
    answered & flags::AUTO_INVAL_DATA,
    0,
    "pages are dropped when a revalidation shows a change"
  );
  let created = exchange_once(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    &message(
      Opcode::Create.to_wire(),
      2,
      ROOT,
      &create_body("revalidated"),
    ),
  );
  assert_eq!(
    body_u64(&created, ENTRY_VALID_AT),
    0,
    "a created entry is not cached"
  );
  let ino = body_u64(&created, 0);
  let attrs = exchange_once(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    &message(Opcode::GetAttr.to_wire(), 3, ino, &[0u8; 16]),
  );
  assert_eq!(
    body_u64(&attrs, ATTR_VALID_AT),
    0,
    "attributes are not cached"
  );
  let mut name = b"revalidated".to_vec();
  name.push(0);
  let looked = exchange_once(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    &message(Opcode::Lookup.to_wire(), 4, ROOT, &name),
  );
  assert_eq!(
    body_u64(&looked, ENTRY_VALID_AT),
    0,
    "a looked-up entry is not cached"
  );
  let negotiated = device.negotiated().expect("INIT was served");
  assert_eq!(negotiated.flags & flags::EXPLICIT_INVAL_DATA, 0);
}

/// Shape: the large transfer of the retained-copy test — far past the small requests' size, so the device's
/// kept buffers are dominated by it.
const BIG: usize = 32 << 10;
/// Shape: the reply room the retained-copy test posts for a request whose reply is a header and a small body.
const SMALL_REPLY: u32 = 256;
/// Shape: the retained-copy test's byte credit: one large chain (its `BIG` bytes and a few hundred of
/// headers and small reply room) fits, but a large request buffer and a large reply buffer kept together do
/// not.
const ONE_LARGE_CHAIN: u64 = (BIG as u64) + 2048;

/// One request through the device under `ledger`, its barrier completed as captured; the reply.
fn exchange_charged(
  device: &mut Device,
  driver: &mut SimDriver,
  bridge: &mut VolumeBridge<'_>,
  ledger: &mut slates_bridge_virtiofs::credit::CreditLedger,
  (opcode, unique, nodeid): (Opcode, u64, u64),
  body: &[u8],
  reply_room: u32,
) -> Vec<u8> {
  let cx = context();
  let request = message(opcode.to_wire(), unique, nodeid, body);
  let queue = FIRST_REQUEST_QUEUE;
  let head = driver.submit(usize::from(queue), &request, reply_room, 1);
  let pass = device
    .service_queue(queue, &mut driver.memory, bridge, &cx, BATCH, ledger)
    .unwrap_or_else(|e| panic!("{opcode:?} served: {e}"));
  if pass.barrier_owed {
    device
      .complete_awaiting(&mut driver.memory, bridge, &cx, true, ledger)
      .unwrap();
  }
  let (id, len) = driver.reap(usize::from(queue)).expect("a used element");
  assert_eq!(id, head);
  driver.reply_of(usize::from(queue), head, len)
}

/// AUD-29-77 (a retained copy charged exactly once). Do: under a ledger whose byte credit fits one large chain,
/// serve INIT, CREATE, a `BIG`-byte WRITE, a `BIG`-byte READ and a GETATTR; after each, compare the ledger's
/// copy bytes in flight with the bytes the device's copy buffers keep. Expect: equal after every chain — the
/// buffers the device keeps between chains stay charged to the attachment (before, the ledger read zero
/// while the device kept the write's request buffer); and the READ is served though the write's buffer and
/// the read's reply room together exceed the credit, because the device gives its kept buffers back before
/// charging a chain they cannot both fit beside.
#[test]
fn the_copy_buffers_a_device_keeps_stay_charged_to_its_attachment() {
  let (mut device, mut driver) = device_and_driver();
  let mut store_a = store();
  let mut vol_a = volume(&mut store_a);
  let mut bridge = VolumeBridge::new(vid(), &mut vol_a, &mut store_a);
  let mut ledger = slates_bridge_virtiofs::credit::CreditLedger::new(
    slates_bridge_virtiofs::credit::AttachmentCredits {
      requests: slates_machine::derived!(BATCH, "the test's request credit", ["test"]),
      bytes: slates_machine::derived!(ONE_LARGE_CHAIN, "one large chain", ["BIG"]),
    },
  );
  let charged_as_kept =
    |device: &Device, ledger: &slates_bridge_virtiofs::credit::CreditLedger, step: &str| {
      assert_eq!(
        ledger.in_flight(),
        (0, device.retained_copy_bytes()),
        "after {step}"
      );
    };
  let init = exchange_charged(
    &mut device,
    &mut driver,
    &mut bridge,
    &mut ledger,
    (Opcode::Init, 1, 0),
    &init_body(u32::try_from(flags::BIG_WRITES).unwrap()),
    SMALL_REPLY,
  );
  assert_eq!(reply_error(&init), 0);
  charged_as_kept(&device, &ledger, "INIT");
  let created = exchange_charged(
    &mut device,
    &mut driver,
    &mut bridge,
    &mut ledger,
    (Opcode::Create, 2, ROOT),
    &create_body("big"),
    SMALL_REPLY,
  );
  assert_eq!(reply_error(&created), 0);
  let (ino, fh) = (
    u64_at(&created, OUT_HEADER_LEN),
    u64_at(&created, OUT_HEADER_LEN + EntryOut::LEN),
  );
  charged_as_kept(&device, &ledger, "CREATE");
  let payload = vec![7u8; BIG];
  let written = exchange_charged(
    &mut device,
    &mut driver,
    &mut bridge,
    &mut ledger,
    (Opcode::Write, 3, ino),
    &write_body(fh, 0, &payload),
    SMALL_REPLY,
  );
  assert_eq!(reply_error(&written), 0);
  charged_as_kept(&device, &ledger, "WRITE");
  assert!(
    device.retained_copy_bytes() >= u64::try_from(BIG).unwrap(),
    "the write's request buffer is kept"
  );
  let read = exchange_charged(
    &mut device,
    &mut driver,
    &mut bridge,
    &mut ledger,
    (Opcode::Read, 4, ino),
    &read_body(fh, 0, u32::try_from(BIG).unwrap()),
    u32::try_from(BIG + OUT_HEADER_LEN).unwrap(),
  );
  assert_eq!(reply_error(&read), 0);
  assert_eq!(&read[OUT_HEADER_LEN..], &payload[..]);
  charged_as_kept(&device, &ledger, "READ");
  let attr = exchange_charged(
    &mut device,
    &mut driver,
    &mut bridge,
    &mut ledger,
    (Opcode::GetAttr, 5, ino),
    &[0u8; 16],
    SMALL_REPLY,
  );
  assert_eq!(reply_error(&attr), 0);
  charged_as_kept(&device, &ledger, "GETATTR");
}

// ───────────────────────────────────────────────── the DAX device window, end to end over a simulated guest (§4.6)

/// A store whose content arena is a shared-object (anchor-backed) region, so a mapped block has a descriptor the DAX
/// window can map (a scratch/anonymous store has none). `page` is the host page so blocks are page-aligned for `mmap`.
/// A shared-backed store sized for `pages` mappable blocks — for the DAX map-path profiling at scale.
#[cfg(unix)]
fn dax_profiling_store(page: usize, pages: usize, tag: &str) -> slates_vfs::volume::Store {
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;
  use slates_mem::{ExclusiveObject, SharedObject, Words};
  let object = SharedObject::create(
    &format!("sl-dp-{tag}-{}", std::process::id()),
    page * pages,
    Words::new(),
  )
  .unwrap();
  let mut arena = ChunkArena::new(page);
  // SAFETY: this test is the object's only mapper; it declares no words.
  arena
    .add_region(Region::shared(
      unsafe { ExclusiveObject::new(object) },
      page,
    ))
    .unwrap();
  slates_vfs::volume::Store::new(
    &slates_vfs::volume::StoreConfig {
      page,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: pages + 64,
      max_chunks: pages,
      max_dir_blocks: 256,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

/// Adds `dt` to `first` while `i` is in the leading `batch`, and to `last` while `i` is in the trailing `batch`
/// (of `n`). Factored out of the profiling loops so each stays under the cognitive-complexity lint.
#[cfg(unix)]
fn bucket(
  i: usize,
  n: usize,
  batch: usize,
  dt: std::time::Duration,
  first: &mut std::time::Duration,
  last: &mut std::time::Duration,
) {
  if i < batch {
    *first = first.saturating_add(dt);
  }
  if i >= n.saturating_sub(batch) {
    *last = last.saturating_add(dt);
  }
}

/// PROFILING (§4.6, the device DAX map path; run with `--ignored --nocapture`). Map N files' blocks into N distinct
/// window slots and report the per-map latency of the first and last batch. Each map pays the pin (copy-up for a
/// writable map) + the `MAP_FIXED` syscall + the remap overlap scan over the live windows (O(live), bounded by the
/// registry cap). If the scan were the bottleneck the last batch would be far slower than the first; a flat ratio
/// means the path is syscall-bound and the scan is negligible within the bound (so an index over the windows would
/// be a premature optimization of a control-plane op — Ada's optimization hierarchy: measure first).
#[cfg(unix)]
#[test]
#[ignore = "profiling: run explicitly with --ignored --nocapture"]
fn t_dax_device_map_path_latency_profile() {
  use slates_mem::{SharedObject, Words};
  use std::time::{Duration, Instant};
  let page = rustix::param::page_size().max(1);
  // Bounded by the sim driver's guest memory and the arena's chunk budget; large enough that an O(live) overlap
  // scan would show as per-map growth (last batch vs first) if it were the bottleneck.
  let n = 300usize;
  let mut store = dax_profiling_store(page, n + 256, "prof");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let mut files = Vec::with_capacity(n);
  for i in 0..n {
    let no = vol
      .create_file_no(&mut store, root, &format!("f{i}"), 0o644)
      .unwrap();
    vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
    files.push(no.0);
  }
  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-dpwin-{}", std::process::id()),
    page * n,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * n) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );

  let batch = 128usize;
  let (mut first, mut last) = (Duration::ZERO, Duration::ZERO);
  for (i, &nodeid) in files.iter().enumerate() {
    let moffset = (i * page) as u64;
    let unique = u64::try_from(i).unwrap_or(0).saturating_add(2);
    let at = Instant::now();
    let r = dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::SetupMapping, unique, nodeid),
      &dax_setup_body(0, page as u64, true, moffset),
    );
    let dt = at.elapsed();
    assert_eq!(reply_error(&r), 0, "map {i}");
    bucket(i, n, batch, dt, &mut first, &mut last);
  }
  assert_eq!(device.dax_window_count(), n, "all N mapped");
  let first_ns = first.as_nanos() / (batch as u128);
  let last_ns = last.as_nanos() / (batch as u128);
  let ratio = (last_ns as f64) / (first_ns.max(1) as f64);
  eprintln!(
    "dax device map path: n={n}, first-{batch} {first_ns} ns/map, last-{batch} {last_ns} ns/map, ratio {ratio:.2} \
     (flat => syscall-bound, the O(live) overlap scan negligible within the registry cap)"
  );

  // The teardown path (the step-5 revoke channel's work): revoke each inode's one window and time it. Each
  // `request_revoke_inode` scans the live windows for the inode (O(live), shrinking by one per revoke via
  // swap_remove) then munmaps + unpins. The first batch scans ~n windows, the last ~1. MEASURED (M5 Max, release,
  // n=300): first ~781 ns/revoke, last ~477 ns/revoke, first/last ~1.64 — so, unlike the map path (whose ~4.5 µs
  // `MAP_FIXED` dwarfs the scan, ratio 0.94), the revoke's base (~477 ns munmap+unpin) is cheap enough that the
  // O(live) scan is visible (~1.8 ns/window). A *bulk* drain of P pending inodes is therefore O(P × live) — fine at
  // these sizes, but it would dominate at a large registry cap. The fix (an inode→window index making a revoke
  // O(windows-of-that-inode)) is deliberately NOT taken yet: it is a control-plane path that is empty in production
  // until DAX is advertised, and maintaining positional indices through `swap_remove` is fiddly to get right — per
  // the optimization hierarchy, measure a real advertised guest's drain pattern before adding it. Recorded in GAPS.
  let (mut tfirst, mut tlast) = (Duration::ZERO, Duration::ZERO);
  for (i, &nodeid) in files.iter().enumerate() {
    let at = Instant::now();
    let torn = device.request_revoke_inode(nodeid, &mut bridge, &cx);
    let dt = at.elapsed();
    assert_eq!(torn, 1, "revoke {i} tore its one window");
    bucket(i, n, batch, dt, &mut tfirst, &mut tlast);
  }
  assert_eq!(device.dax_window_count(), 0, "all N revoked");
  let tfirst_ns = tfirst.as_nanos() / (batch as u128);
  let tlast_ns = tlast.as_nanos() / (batch as u128);
  let tratio = (tfirst_ns as f64) / (tlast_ns.max(1) as f64);
  eprintln!(
    "dax device revoke path: n={n}, first-{batch} {tfirst_ns} ns/revoke (scans ~n), last-{batch} {tlast_ns} \
     ns/revoke (scans ~1), first/last {tratio:.2} (>1 => the O(live) scan is visible over the ~{tlast_ns} ns \
     munmap base; a bulk drain is O(P×live), an inode→window index deferred until advertised — see GAPS)"
  );
  drop(bridge);
  drop(cache);
}

#[cfg(unix)]
fn dax_shared_store(page: usize, tag: &str) -> slates_vfs::volume::Store {
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;
  use slates_mem::{ExclusiveObject, SharedObject, Words};
  const REGION_PAGES: usize = 64;
  // The name is unique per test (tag), so parallel tests never collide on one `shm` object.
  let object = SharedObject::create(
    &format!("sl-ds-{tag}-{}", std::process::id()),
    page * REGION_PAGES,
    Words::new(),
  )
  .unwrap();
  let mut arena = ChunkArena::new(page);
  // SAFETY: this test is the object's only mapper; it declares no words.
  arena
    .add_region(Region::shared(
      unsafe { ExclusiveObject::new(object) },
      page,
    ))
    .unwrap();
  slates_vfs::volume::Store::new(
    &slates_vfs::volume::StoreConfig {
      page,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: REGION_PAGES,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

/// [`dax_shared_store`] with an object big enough to hold a §4.6 L1 sandbox, so a test can drive a *shift*
/// through the device. A sandbox is one of the host's transparent huge pages and the arena needs room for it
/// beside the content, so the object is several of them rather than the 64 pages the plain fixture takes.
///
/// Where the host offers no huge page the object falls back to the plain fixture's size — nothing can reserve a
/// sandbox there anyway, and the test skips on the product's refusal rather than on the fixture.
/// Brings the connection up over one transient bridge, as the shard builds one per pass.
#[cfg(unix)]
fn dax_init_pass(
  device: &mut Device,
  driver: &mut SimDriver,
  vol: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
  cx: &OpContext,
) {
  let mut bridge = VolumeBridge::new(vid(), vol, store);
  dax_exchange(
    device,
    driver,
    &mut bridge,
    cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
}

/// One SETUPMAPPING pass over its own transient bridge: `nodeid`'s first page, writable, at window offset zero,
/// answering with the guest's reply errno.
#[cfg(unix)]
fn dax_map_pass(
  device: &mut Device,
  driver: &mut SimDriver,
  vol: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
  cx: &OpContext,
  (unique, nodeid): (u64, u64),
  page: usize,
) -> i32 {
  let mut bridge = VolumeBridge::new(vid(), vol, store);
  let reply = dax_exchange(
    device,
    driver,
    &mut bridge,
    cx,
    (Opcode::SetupMapping, unique, nodeid),
    &dax_setup_body(0, page as u64, true, 0),
  );
  reply_error(&reply)
}

/// The volume's one live writable mapping — the window a shift is about to move. Panics with what it found
/// instead, so a fixture that drifted says so rather than the test reading a mapping it did not mean.
#[cfg(unix)]
fn only_writable_mapping(
  store: &slates_vfs::volume::Store,
  prefix: u16,
) -> slates_vfs::dax::Mapping {
  let mapped = store.dax.writable_of_prefix(prefix);
  match mapped.as_slice() {
    [one] => *one,
    other => panic!("one writable mapping after the map pass, found {other:?}"),
  }
}

/// The §4.6 L2 shift under test, or `None` with the skip printed where this host backs no transparent huge page
/// for §4.6's L1 sandbox, so there is no physically different row for the content to land on.
#[cfg(unix)]
fn dax_shift_or_skip(
  vol: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
) -> Option<(slates_mem::arena::Extent, slates_vfs::dax::Mapping)> {
  let live = only_writable_mapping(store, vol.prefix());
  match vol.dax_migrate(store, &live) {
    Ok(moved) => {
      assert_ne!(
        moved.block, live.block,
        "the content moved to a different row"
      );
      Some((live.block, moved))
    }
    Err(slates_vfs::error::VfsError::NoPhysicalIsolation) => {
      println!(
        "SKIP a_shift_tells_the_device_so_the_guests_window_follows_the_content: this host backed no \
         transparent huge page, so §4.6's L1 sandbox cannot be reserved"
      );
      None
    }
    Err(other) => panic!("a shift is performed or refused for want of a sandbox: {other:?}"),
  }
}

/// Phase one: the guest maps the file writable, leaving one live window over the row the content sits on now.
#[cfg(unix)]
fn one_writable_window_is_mapped(
  device: &mut Device,
  driver: &mut SimDriver,
  vol: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
  cx: &OpContext,
  nodeid: u64,
  page: usize,
) {
  dax_init_pass(device, driver, vol, store, cx);
  let mapped = dax_map_pass(device, driver, vol, store, cx, (2, nodeid), page);
  assert_eq!(mapped, 0, "mapped");
  assert_eq!(device.dax_window_count(), 1, "one live guest window");
}

/// Phase three: a service pass drains what the shift enqueued and the device tears the window that named the row
/// the content left, so the guest faults instead of reading a page the registry no longer authorizes.
#[cfg(unix)]
fn the_drain_tears_the_stale_window(
  device: &mut Device,
  vol: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
  cx: &OpContext,
  nodeid: u64,
) {
  let (pending, torn) = {
    let mut bridge = VolumeBridge::new(vid(), vol, store);
    let pending = bridge.take_pending_dax_revokes();
    let torn = device.request_revoke_inode(nodeid, &mut bridge, cx);
    (pending, torn)
  };
  assert_eq!(
    pending,
    vec![(nodeid, true)],
    "the shift enqueued this inode writable-only, so a read-only window of it would survive"
  );
  assert_eq!(
    torn, 1,
    "the device tore down the window that named the row the content left"
  );
  assert_eq!(
    device.dax_window_count(),
    0,
    "the stale window is gone, so the guest faults rather than read an unauthorized page"
  );
}

/// Phase four: the guest re-faults and its fresh SETUPMAPPING reaches the row the content moved to, with its
/// bytes — which is the whole point of telling the device, and what makes a periodic shift safe to run.
#[cfg(unix)]
fn the_refault_reaches_the_moved_row(
  device: &mut Device,
  driver: &mut SimDriver,
  vol: &mut slates_vfs::volume::Volume,
  store: &mut slates_vfs::volume::Store,
  cx: &OpContext,
  (nodeid, page, fill): (u64, usize, u8),
  moved: &slates_vfs::dax::Mapping,
) {
  let refaulted = dax_map_pass(device, driver, vol, store, cx, (3, nodeid), page);
  assert_eq!(refaulted, 0, "the re-fault maps again");
  assert_eq!(
    device.dax_window_count(),
    1,
    "the guest has its window back"
  );
  let names_moved_row = store
    .dax
    .writable_of_prefix(vol.prefix())
    .iter()
    .any(|mapping| mapping.block == moved.block);
  assert!(
    names_moved_row,
    "the window the guest re-mapped names the row the content moved to, not the one it left"
  );
  assert_eq!(
    store.dax_read_raw(moved, 0, 8),
    vec![fill; 8],
    "the content survived the shift"
  );
}

/// A sandbox-capable DAX store holding one writable-DAX volume with a one-page file of `fill`: the store, the
/// volume and the file's nodeid, which is all the shift test needs before it touches the device.
#[cfg(unix)]
fn dax_sandbox_fixture(
  page: usize,
  tag: &str,
  fill: u8,
) -> (slates_vfs::volume::Store, slates_vfs::volume::Volume, u64) {
  let mut store = dax_sandbox_store(page, tag);
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![fill; page]).unwrap();
  (store, vol, no.0)
}

/// §4.6 **L2's caller contract, through the real device** (A-9, AC-4.12). The periodic shift is only safe if the
/// guest's window follows it: a shift that moved the content but left the device's window naming the revoked
/// mapping would leave a guest reading a page the registry no longer authorizes, with nothing to tell it.
///
/// Do: map a file writable through the device (one live guest window), have the daemon shift it (L2), then drain
/// the pending revokes the way a service pass does and let the guest re-fault. Expect: the shift enqueued exactly
/// this inode with `writable_only` — so a read-only window of the same inode would survive, which is what makes
/// this queue the right one; the device tears the one window down; and the re-fault reaches the row the content
/// moved to with its bytes intact. Skips loudly where the host backs no sandbox for the shift to land in.
#[cfg(unix)]
#[test]
fn a_shift_tells_the_device_so_the_guests_window_follows_the_content() {
  use slates_mem::{SharedObject, Words};
  const FILL: u8 = 0x5A;
  let page = rustix::param::page_size().max(1);
  let (mut store, mut vol, nodeid) = dax_sandbox_fixture(page, "shift", FILL);
  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxshift-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();

  one_writable_window_is_mapped(
    &mut device,
    &mut driver,
    &mut vol,
    &mut store,
    &cx,
    nodeid,
    page,
  );
  // The daemon shifts it (L2's moving target), as the periodic defence would.
  let Some((_left, moved)) = dax_shift_or_skip(&mut vol, &mut store) else {
    return;
  };
  the_drain_tears_the_stale_window(&mut device, &mut vol, &mut store, &cx, nodeid);
  the_refault_reaches_the_moved_row(
    &mut device,
    &mut driver,
    &mut vol,
    &mut store,
    &cx,
    (nodeid, page, FILL),
    &moved,
  );
  drop(cache);
}

#[cfg(unix)]
fn dax_sandbox_store(page: usize, tag: &str) -> slates_vfs::volume::Store {
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;
  use slates_mem::{ExclusiveObject, SharedObject, Words};
  // Derived: one huge page for the sandbox and the rest for the content and the arena's rounding, as
  // `crates/vfs/tests/common/mod.rs::store_sandboxed` sizes its region and for the same reason — the fixture must
  // not be what decides the result.
  const SANDBOX_HUGE_PAGES: usize = 8;
  const PLAIN_PAGES: usize = 64;
  let bytes = slates_machine::facts::huge_page_bytes()
    .and_then(|huge| usize::try_from(huge).ok())
    .map_or(page * PLAIN_PAGES, |huge| huge * SANDBOX_HUGE_PAGES);
  let object = SharedObject::create(
    &format!("sl-dsb-{tag}-{}", std::process::id()),
    bytes,
    Words::new(),
  )
  .unwrap();
  let mut arena = ChunkArena::new(page);
  // SAFETY: this test is the object's only mapper; it declares no words.
  arena
    .add_region(Region::shared(
      unsafe { ExclusiveObject::new(object) },
      page,
    ))
    .unwrap();
  slates_vfs::volume::Store::new(
    &slates_vfs::volume::StoreConfig {
      page,
      cache_line: 128,
      max_dirs: 64,
      max_inodes: 256,
      max_chunks: 1 << 12,
      max_dir_blocks: 64,
      dir_cutover: 16,
    },
    arena,
    0,
  )
}

/// One request straight through the device (no differential leg — the DAX reply is the device's, not dispatch's).
#[cfg(unix)]
fn dax_exchange(
  device: &mut Device,
  driver: &mut SimDriver,
  bridge: &mut VolumeBridge<'_>,
  cx: &OpContext,
  (opcode, unique, nodeid): (Opcode, u64, u64),
  body: &[u8],
) -> Vec<u8> {
  let request = message(opcode.to_wire(), unique, nodeid, body);
  let queue = FIRST_REQUEST_QUEUE;
  let head = driver.submit(usize::from(queue), &request, 4096, 1);
  serve(device, queue, driver, bridge, cx, BATCH).unwrap();
  let (id, len) = driver.reap(usize::from(queue)).expect("a used element");
  assert_eq!(id, head);
  driver.reply_of(usize::from(queue), head, len)
}

/// §4.6 (AC-4.12, the device DAX window end to end). Do: on a shared-backed volume with a file of known bytes and
/// writable DAX opted in, offer the device a cache region, serve FUSE_INIT, then a guest FUSE_SETUPMAPPING for the
/// file writable; read the window; write through it; then FUSE_REMOVEMAPPING. Expect: setup succeeds, the window shows
/// the file's bytes (a real shared mapping — what a guest reaches through its own page table), a write through the
/// window reaches the file's block, and removemapping tears the window down (no live window left). No VMM, no KVM.
#[cfg(unix)]
#[test]
fn a_guest_setupmapping_maps_the_file_into_the_dax_window_and_removemapping_tears_it_down() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "e2e");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  // The VMM's cache window: a shared object the device maps block pages into.
  let cache = SharedObject::create(
    &format!("sl-daxwin-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  assert!(
    device.dax_offered(),
    "the device serves DAX now a window is installed"
  );

  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);

  // FUSE_INIT brings the connection up (fuse_init_in: major, minor, max_readahead, flags = 4 u32, then flags2/unused).
  let mut init = Vec::new();
  init.extend_from_slice(&7u32.to_le_bytes()); // major
  init.extend_from_slice(&38u32.to_le_bytes()); // minor
  init.extend_from_slice(&0u32.to_le_bytes()); // max_readahead
  init.extend_from_slice(&0u32.to_le_bytes()); // flags
  init.resize(64, 0); // the kernel sends a 64-byte init_in; the codec reads what it needs
  let init_reply = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &init,
  );
  assert_eq!(reply_error(&init_reply), 0, "INIT served");

  // FUSE_SETUPMAPPING: fh, foffset=0, len=page, flags=WRITE, moffset=0.
  let mut setup = Vec::new();
  setup.extend_from_slice(&0u64.to_le_bytes()); // fh (slates maps by nodeid)
  setup.extend_from_slice(&0u64.to_le_bytes()); // foffset
  setup.extend_from_slice(&(page as u64).to_le_bytes()); // len
  setup.extend_from_slice(&slates_bridge_fuse::request::SETUPMAPPING_FLAG_WRITE.to_le_bytes()); // flags = write
  setup.extend_from_slice(&0u64.to_le_bytes()); // moffset
  let setup_reply = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &setup,
  );
  assert_eq!(
    reply_error(&setup_reply),
    0,
    "SETUPMAPPING succeeded over the DAX window"
  );
  assert_eq!(device.dax_window_count(), 1, "one live window mapping");

  // The window shows the file's bytes — the guest would reach these through its own page table, no daemon call.
  assert_eq!(
    device.dax_window_bytes(0, 64).unwrap(),
    vec![0x5A; 64],
    "the DAX window reaches the file's content block"
  );

  // FUSE_REMOVEMAPPING: count=1, (moffset=0, len=page).
  let mut remove = Vec::new();
  remove.extend_from_slice(&1u32.to_le_bytes()); // count
  remove.extend_from_slice(&0u64.to_le_bytes()); // moffset
  remove.extend_from_slice(&(page as u64).to_le_bytes()); // len
  let remove_reply = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::RemoveMapping, 3, nodeid),
    &remove,
  );
  assert_eq!(reply_error(&remove_reply), 0, "REMOVEMAPPING succeeded");
  assert_eq!(
    device.dax_window_count(),
    0,
    "the window mapping was torn down"
  );

  drop(cache);
}

/// One tenant of the cross-tenant test: its own volume in the shared store, writable DAX opted in, and one file
/// filled with `byte`. Returns the volume and its file's nodeid.
#[cfg(unix)]
fn tenant_with_file(
  store: &mut slates_vfs::volume::Store,
  (prefix, name, byte): (u16, &str, u8),
  page: usize,
) -> (slates_vfs::volume::Volume, u64) {
  let mut volume = volume_with_prefix(store, prefix);
  volume.enable_writable_dax();
  let root = volume.root_inode(&*store).unwrap();
  let no = volume.create_file_no(store, root, name, 0o644).unwrap();
  volume.write(store, no, 0, &vec![byte; page]).unwrap();
  (volume, no.0)
}

/// A tenant's cache region: the window a VMM would hand its device.
#[cfg(unix)]
fn tenant_cache(tag: &str, page: usize) -> slates_mem::SharedObject {
  slates_mem::SharedObject::create(
    &format!("sl-tenant-{tag}-{}", std::process::id()),
    page * 4,
    slates_mem::Words::new(),
  )
  .unwrap()
}

/// FUSE_INIT over one tenant's device and bridge; the reply's error.
#[cfg(unix)]
fn tenant_connect(
  (device, driver): (&mut Device, &mut SimDriver),
  (volume_id, volume, store): (
    slates_db::catalog::VolumeId,
    &mut slates_vfs::volume::Volume,
    &mut slates_vfs::volume::Store,
  ),
  cx: &OpContext,
) -> i32 {
  let mut bridge = VolumeBridge::new(volume_id, volume, store);
  let reply = dax_exchange(
    device,
    driver,
    &mut bridge,
    cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
  reply_error(&reply)
}

/// One writable FUSE_SETUPMAPPING over a tenant's device and bridge; the reply's error.
#[cfg(unix)]
fn tenant_dax_map(
  (device, driver): (&mut Device, &mut SimDriver),
  (volume_id, volume, store): (
    slates_db::catalog::VolumeId,
    &mut slates_vfs::volume::Volume,
    &mut slates_vfs::volume::Store,
  ),
  cx: &OpContext,
  (unique, nodeid): (u64, u64),
  (moffset, len): (u64, u64),
) -> i32 {
  let mut bridge = VolumeBridge::new(volume_id, volume, store);
  let reply = dax_exchange(
    device,
    driver,
    &mut bridge,
    cx,
    (Opcode::SetupMapping, unique, nodeid),
    &dax_setup_body(0, len, true, moffset),
  );
  reply_error(&reply)
}

/// A tenant with one file, its device, cache region and context, brought up and ready to map — the fixture the
/// two pin-ledger tests share, so each of them does one thing.
#[cfg(unix)]
struct PinLedgerFixture {
  store: slates_vfs::volume::Store,
  tenant: slates_vfs::volume::Volume,
  device: Device,
  driver: SimDriver,
  cx: OpContext,
  no: u64,
  page_len: u64,
  quiet: usize,
  _cache: slates_mem::SharedObject,
}

#[cfg(unix)]
fn pin_ledger_fixture(tag: &str) -> PinLedgerFixture {
  const BYTE: u8 = 0xC7;
  let page = rustix::param::page_size().max(1);
  let window = (page * 4) as u64;
  let mut store = dax_shared_store(page, tag);
  let (mut tenant, no) = tenant_with_file(&mut store, (1, "p", BYTE), page);
  let (mut device, mut driver) = device_and_driver();
  let cache = tenant_cache(tag, page);
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), window).unwrap(),
  ));
  let cx = context_for(vid());
  tenant_connect(
    (&mut device, &mut driver),
    (vid(), &mut tenant, &mut store),
    &cx,
  );
  let quiet = store.content.arena().pinned_blocks();
  PinLedgerFixture {
    store,
    tenant,
    device,
    driver,
    cx,
    no,
    page_len: page as u64,
    quiet,
    _cache: cache,
  }
}

#[cfg(unix)]
impl PinLedgerFixture {
  fn map(&mut self, unique: u64, moffset: u64) -> i32 {
    tenant_dax_map(
      (&mut self.device, &mut self.driver),
      (vid(), &mut self.tenant, &mut self.store),
      &self.cx,
      (unique, self.no),
      (moffset, self.page_len),
    )
  }

  fn unmap(&mut self, unique: u64, moffset: u64) -> i32 {
    tenant_dax_unmap(
      (&mut self.device, &mut self.driver),
      (vid(), &mut self.tenant, &mut self.store),
      &self.cx,
      (unique, self.no),
      (moffset, self.page_len),
    )
  }

  fn pinned(&self) -> usize {
    self.store.content.arena().pinned_blocks()
  }
}

/// §4.6 — a window a device maps gives its pin back when it unmaps. A pin left behind holds a chunk out of the
/// arena for good. Do: map one window over a real cache region, then unmap it. Expect: mapping pins a block
/// (non-vacuity) and the unmap returns the arena to exactly where it started.
#[cfg(unix)]
#[test]
fn a_device_window_gives_its_pin_back_when_it_unmaps() {
  let mut f = pin_ledger_fixture("pinledger-one");
  assert_eq!(f.map(2, 0), 0, "the window maps");
  let with_one = f.pinned();
  assert!(
    with_one > f.quiet,
    "non-vacuity: mapping a window pins a block ({} -> {with_one})",
    f.quiet
  );
  assert_eq!(f.unmap(3, 0), 0, "the window unmaps");
  assert_eq!(
    (f.pinned(), f.store.release_refusals),
    (f.quiet, 0),
    "its pin came back, and the arena refused no release on the way"
  );
}

/// §4.6, gate checklist item 6 — the **destroy sequence** leaves no block pinned. A destroy tears the volume's
/// guest devices down first (AUD-29-68) and only then runs the volume teardown, so by the time
/// `Store::dax_revoke_volume` unpins anything, every window is already munmapped. Do: bring a window up over a
/// real device and cache region, then run those two steps in that order. Expect: nothing pinned afterwards, and
/// no release the arena refused.
///
/// Why this exists beside the single-window test: that one proves a window gives its pin back when it *unmaps*.
/// This proves the two teardowns **compose** in the order the server uses them — the ordering is what keeps
/// `dax_revoke_volume` safe being synchronous, and the thing that would break it is a path that unpins before
/// the windows are gone. `AdmittedDevice::abandon` is exactly such a path (it has no bridge, so it cannot
/// `release_dax`); the destroy verb cannot reach it with live windows today, and its doc now says so.
///
/// **Red-checked, and the result is why `release_refusals` is asserted here at all.** Running the two steps in
/// the wrong order gives `(pinned, release_refusals) = (0, 1)` against `(0, 0)`: the block count comes back to
/// zero *either way*, because `dax_revoke_volume` unpins it itself — so the pinned count alone cannot tell the
/// orders apart, and the only signal is the release the arena then refuses when `release_dax` tries to unpin a
/// block that is already free. A counter nobody reads is a signal that cannot fire; this is the case it fires
/// on.
#[cfg(unix)]
#[test]
fn the_destroy_sequence_leaves_no_block_pinned() {
  let mut f = pin_ledger_fixture("pinledger-destroy");
  assert_eq!(f.map(2, 0), 0, "the window maps");
  assert!(
    f.pinned() > f.quiet,
    "non-vacuity: the window pinned a block before the destroy ({} -> {})",
    f.quiet,
    f.pinned()
  );
  let prefix = f.tenant.prefix();

  // Step one, as the destroy does it: the device's terminal teardown, through a still-live bridge.
  {
    let mut bridge = VolumeBridge::new(vid(), &mut f.tenant, &mut f.store);
    f.device.release_dax(&mut bridge, &f.cx);
  }
  // Step two: the volume teardown, which unpins synchronously.
  f.store.dax_revoke_volume(prefix);

  assert_eq!(
    (f.pinned(), f.store.release_refusals),
    (f.quiet, 0),
    "after the destroy sequence no block is left pinned, and the arena refused no release"
  );
}

/// §4.6 — two guest windows over **one** block each hold its pin, and only the last one out releases it. A block
/// released while a window still maps it is reusable by another allocation under a live guest page table, which
/// is the escape the two-phase revoke exists to prevent.
///
/// Do: map the same file range at two different guest offsets, drop one window, then the other. Expect: after
/// the first, the block is **still pinned**; after the second, nothing is.
///
/// Why this test exists, and it is a red-checked guard rather than a hopeful one. No device-level test looked at
/// pins at all, and that gap let an unsafe change through on 2026-10-18: deduplicating windows in the registry
/// made two guest windows share one refcounted pin, and it was green on macOS aarch64, x86_64 Linux and aarch64
/// Linux. With that change re-applied this test fails exactly here — `left: 0, right: 1`, the block released
/// while the second window is live. The two halves of the pin live in different crates (the arena counts holders
/// per mapping; the device keeps one window entry per SETUPMAPPING and releases each), so neither crate's own
/// tests could hold them to each other. Record:
/// `docs/bugs/2026-10-18-one-volumes-dax-map-storm-denies-every-other-volume.md`.
#[cfg(unix)]
#[test]
fn two_windows_over_one_block_each_hold_its_pin() {
  let mut f = pin_ledger_fixture("pinledger-two");
  let second_at = f.page_len;
  assert_eq!(f.map(2, 0), 0, "the first window maps");
  assert_eq!(
    f.map(3, second_at),
    0,
    "the second window maps the same range"
  );
  let with_two = f.pinned();
  assert!(
    with_two > f.quiet,
    "non-vacuity: two windows over one range hold the block ({} -> {with_two})",
    f.quiet
  );

  assert_eq!(f.unmap(4, 0), 0, "the first window unmaps");
  assert_eq!(
    f.pinned(),
    with_two,
    "the block is STILL pinned: the second window over it is live, and a block released here would be reusable \
     under a live guest page table"
  );

  assert_eq!(f.unmap(5, second_at), 0, "the second window unmaps");
  assert_eq!(
    (f.pinned(), f.store.release_refusals),
    (f.quiet, 0),
    "with both windows gone the ledger balances: no block left pinned, and no release the arena refused"
  );
}

/// §4.6 / AC-4.12, the cross-tenant question asked where a VMM actually reaches. The VFS proves a guest cannot
/// read across a volume boundary (`vfs/tests/dax.rs a_guest_cannot_read_across_a_volume_boundary`); this proves it
/// at the device seam, the half a guest's own page table is wired to. Do: put two tenants in ONE store, so they
/// share an arena and a leak would be a real one; give each a file of its own byte; map each through its own
/// device, its own cache region and its own context. Then try the two crossings a hostile guest has: present
/// tenant B's context to tenant A's bridge, and present tenant B's inode number on tenant A's own device.
///
/// Expect: both windows live at once, each showing only its own tenant's bytes; the foreign *context* is refused
/// outright (`VolumeBridge::authorize_read` — the context's volume must be the one the bridge serves); and the
/// foreign *inode number* never yields B's bytes. Deliberately NOT asserting the foreign number is refused:
/// slates confines by counter, not by refusing a foreign prefix, so such a number resolves to A's own
/// same-counter inode or misses. Asserting "refused" there would encode the wrong rule — an earlier cross-volume
/// test did exactly that and a run disproved it. What must hold, and all that must hold, is that B's content is
/// unreachable from A.
#[cfg(unix)]
#[test]
fn two_tenants_dax_windows_each_show_only_their_own_bytes() {
  const A_BYTE: u8 = 0xAA;
  const B_BYTE: u8 = 0xBB;
  let page = rustix::param::page_size().max(1);
  let window = (page * 4) as u64;
  let mut store = dax_shared_store(page, "tenants");
  let (mut tenant_a, a_no) = tenant_with_file(&mut store, (1, "a", A_BYTE), page);
  let (mut tenant_b, b_no) = tenant_with_file(&mut store, (2, "b", B_BYTE), page);

  let (mut device_a, mut driver_a) = device_and_driver();
  let (mut device_b, mut driver_b) = device_and_driver();
  let cache_a = tenant_cache("a", page);
  let cache_b = tenant_cache("b", page);
  device_a.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache_a.as_fd(), window).unwrap(),
  ));
  device_b.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache_b.as_fd(), window).unwrap(),
  ));
  let cx_a = context_for(vid());
  let cx_b = context_for(other_vid());
  let page_len = page as u64;

  // Each tenant brings its connection up and maps its own file, through its own device and context.
  tenant_connect(
    (&mut device_a, &mut driver_a),
    (vid(), &mut tenant_a, &mut store),
    &cx_a,
  );
  tenant_connect(
    (&mut device_b, &mut driver_b),
    (other_vid(), &mut tenant_b, &mut store),
    &cx_b,
  );
  assert_eq!(
    tenant_dax_map(
      (&mut device_a, &mut driver_a),
      (vid(), &mut tenant_a, &mut store),
      &cx_a,
      (2, a_no),
      (0, page_len)
    ),
    0,
    "tenant A maps its own file"
  );
  assert_eq!(
    tenant_dax_map(
      (&mut device_b, &mut driver_b),
      (other_vid(), &mut tenant_b, &mut store),
      &cx_b,
      (2, b_no),
      (0, page_len)
    ),
    0,
    "tenant B maps its own file"
  );

  // Both live at once: each window reaches its own tenant's block and nothing else.
  assert_eq!(
    device_a.dax_window_bytes(0, 64).unwrap(),
    vec![A_BYTE; 64],
    "tenant A's window reaches tenant A's bytes"
  );
  assert_eq!(
    device_b.dax_window_bytes(0, 64).unwrap(),
    vec![B_BYTE; 64],
    "tenant B's window reaches tenant B's bytes"
  );

  // Crossing 1: tenant B's context on tenant A's bridge — refused before any mapping, by the object-side check
  // that the context binds the volume this bridge serves.
  assert_ne!(
    tenant_dax_map(
      (&mut device_a, &mut driver_a),
      (vid(), &mut tenant_a, &mut store),
      &cx_b,
      (3, a_no),
      (page_len, page_len)
    ),
    0,
    "a context binding another tenant's volume is refused"
  );

  // Crossing 2: tenant B's inode number, on tenant A's own device with A's own context.
  let answered = tenant_dax_map(
    (&mut device_a, &mut driver_a),
    (vid(), &mut tenant_a, &mut store),
    &cx_a,
    (4, b_no),
    (page_len * 2, page_len),
  );
  // Scanned over the WHOLE of A's cache region, not just the slot: a leak anywhere in it is a leak. The
  // emptiness check is what keeps this honest — an empty vector would satisfy "contains no B byte" and prove
  // nothing.
  let reached = device_a.dax_window_bytes(0, window).unwrap_or_default();
  assert!(
    !reached.is_empty(),
    "A's region is readable, so the scan means something"
  );
  assert!(
    !reached.contains(&B_BYTE),
    "tenant B's bytes reached tenant A's window (setupmapping answered {answered})"
  );
  assert!(
    reached.iter().all(|byte| *byte == 0 || *byte == A_BYTE),
    "A's region holds only its own bytes and untouched zeroes"
  );
  assert_eq!(
    device_b.dax_window_bytes(0, 64).unwrap(),
    vec![B_BYTE; 64],
    "tenant B's own window is untouched by A's attempts"
  );

  drop(cache_a);
  drop(cache_b);
}

/// §4.6, the leak question asked at the device seam rather than the mediated path. The VFS proves a truncate's
/// leftover bytes are not reachable through `dax_read` (`vfs/tests/dax.rs
/// a_mapping_does_not_expose_the_bytes_past_the_files_length`), but that path bounds to `Mapping::page_span`,
/// which counts `DAX_PAGE` units. A **device** grants whole HOST pages — `file_len` rounded up to the placer's
/// granule, capped at the block — so where the host page is larger than `DAX_PAGE` (16 KiB against 4 KiB on an
/// Apple-Silicon host) the guest's window is strictly larger than the mediated path's, and the mediated test says
/// nothing about the extra bytes. Do: fill a whole page with a secret, truncate the file to 100 bytes, map it
/// through the device, and scan the ENTIRE granted window. Expect: the file's own 100 bytes, then zeroes — the
/// map scrubs the whole block past `file_len`, and the cap at the block keeps a coarse granule from reaching the
/// next one. Runs on both granules: 16 KiB on this host, 4 KiB on x86_64 Linux, where the two bounds coincide.
#[cfg(unix)]
#[test]
fn a_device_window_past_the_files_length_shows_zeroes_not_the_truncated_secret() {
  const SECRET: u8 = 0xEE;
  const KEPT: usize = 100;
  let page = rustix::param::page_size().max(1);
  let window = (page * 4) as u64;
  let mut store = dax_shared_store(page, "tailleak");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![SECRET; page]).unwrap();
  vol.truncate(&mut store, no, KEPT as u64).unwrap();

  let (mut device, mut driver) = device_and_driver();
  let cache = tenant_cache("tail", page);
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), window).unwrap(),
  ));
  let cx = context_for(vid());
  tenant_connect(
    (&mut device, &mut driver),
    (vid(), &mut vol, &mut store),
    &cx,
  );
  assert_eq!(
    tenant_dax_map(
      (&mut device, &mut driver),
      (vid(), &mut vol, &mut store),
      &cx,
      (2, no.0),
      (0, page as u64)
    ),
    0,
    "the truncated file maps"
  );

  // The whole window the device granted, not one DAX_PAGE of it.
  let granted = device.dax_window_bytes(0, page as u64).unwrap_or_default();
  assert_eq!(
    granted.len(),
    page,
    "the granted window is readable in full"
  );
  assert_eq!(
    granted.get(..KEPT),
    Some(vec![SECRET; KEPT].as_slice()),
    "the file's own surviving bytes are there"
  );
  assert!(
    granted
      .get(KEPT..)
      .is_some_and(|tail| tail.iter().all(|byte| *byte == 0)),
    "every byte past the file's length is zero, not the truncated-away secret"
  );

  drop(cache);
}

/// The fills the chaos schedule writes. A byte in the guest's window that is none of these and not zero came from
/// somewhere the guest has no business reaching — another volume, arena metadata, or memory never initialised.
#[cfg(unix)]
const CHAOS_FILLS: [u8; 3] = [0x11, 0x22, 0x33];

/// One FUSE_REMOVEMAPPING of `len` at `moffset`; the reply's error.
#[cfg(unix)]
fn tenant_dax_unmap(
  (device, driver): (&mut Device, &mut SimDriver),
  (volume_id, volume, store): (
    slates_db::catalog::VolumeId,
    &mut slates_vfs::volume::Volume,
    &mut slates_vfs::volume::Store,
  ),
  cx: &OpContext,
  (unique, nodeid): (u64, u64),
  (moffset, len): (u64, u64),
) -> i32 {
  let mut body = Vec::new();
  body.extend_from_slice(&1u32.to_le_bytes());
  body.extend_from_slice(&moffset.to_le_bytes());
  body.extend_from_slice(&len.to_le_bytes());
  let mut bridge = VolumeBridge::new(volume_id, volume, store);
  let reply = dax_exchange(
    device,
    driver,
    &mut bridge,
    cx,
    (Opcode::RemoveMapping, unique, nodeid),
    &body,
  );
  reply_error(&reply)
}

/// One step of the device chaos schedule. Every operation may be refused — a slot may be taken, a file may have
/// been truncated away, a map may collide — and a refusal is a valid outcome; the invariant is about what the
/// guest-visible region SHOWS afterwards.
#[cfg(unix)]
#[expect(
  clippy::too_many_arguments,
  reason = "one step of a schedule, taken as tuples"
)]
fn device_chaos_step(
  (device, driver): (&mut Device, &mut SimDriver),
  (volume, store): (
    &mut slates_vfs::volume::Volume,
    &mut slates_vfs::volume::Store,
  ),
  cx: &OpContext,
  files: &[u64],
  page: usize,
  slots: u64,
  unique: u64,
  (op, which, size): (u8, u8, u16),
) -> ChaosEffect {
  let nodeid = files[usize::from(which) % files.len()];
  let slot = (u64::from(which) % slots) * page as u64;
  let no = slates_vfs::ids::InodeNo(nodeid);
  match op % 5 {
    0 => {
      let answered = tenant_dax_map(
        (device, driver),
        (vid(), volume, store),
        cx,
        (unique, nodeid),
        (slot, page as u64),
      );
      if answered == 0 {
        return ChaosEffect::Mapped(slot, nodeid);
      }
      ChaosEffect::None
    }
    1 => {
      tenant_dax_unmap(
        (device, driver),
        (vid(), volume, store),
        cx,
        (unique, nodeid),
        (slot, page as u64),
      );
      ChaosEffect::TornDown
    }
    2 => {
      let fill = CHAOS_FILLS[usize::from(op) % CHAOS_FILLS.len()];
      let len = (usize::from(size) % page).max(1);
      let _ = volume.write(store, no, 0, &vec![fill; len]);
      ChaosEffect::TornDown
    }
    3 => {
      let _ = volume.truncate(store, no, u64::from(size) % (page as u64));
      ChaosEffect::TornDown
    }
    _ => {
      volume.age_writes();
      let _ = volume.seal_pending(store, 1 << 20);
      ChaosEffect::TornDown
    }
  }
}

/// What a chaos step did to the window's slots, as far as the schedule can know: it mapped one, or it may have
/// torn some down. Conservative by design — `unmap`, a write, a truncate and the sweep can each leave a slot
/// `PROT_NONE`, and reading one of those faults, so after any of them the schedule trusts no slot until a map
/// re-establishes it.
#[cfg(unix)]
#[derive(Clone, Copy)]
enum ChaosEffect {
  /// A SETUPMAPPING succeeded at this window offset, for this inode.
  Mapped(u64, u64),
  /// Nothing was established.
  None,
  /// Something that can revoke ran; no slot is known-live afterwards.
  TornDown,
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(ProptestConfig { cases: slates_test_seeds::cases(80), .. ProptestConfig::default() }))]

  /// §4.6 / AC-4.12 under chaos, at the device seam. `vfs/tests/dax.rs` plays this schedule against the gate;
  /// this plays it against the half that owns the guest-visible window — SETUPMAPPING and REMOVEMAPPING over a
  /// shared cache region, interleaved with the writes, truncates and idle sweeps that revoke underneath them.
  /// Do: a random schedule over three files and several window slots. Expect, after **every** step: every byte
  /// of the whole cache region is either zero or one of the fills this volume wrote. A byte that is neither came
  /// from somewhere the guest has no business reaching — another volume's block, arena metadata, or memory never
  /// initialised — which is exactly the class of leak a shared window makes possible and a per-access check
  /// cannot catch, since the guest reads these pages through its own page table.
  ///
  /// **What this schedule does NOT catch, measured rather than assumed:** a revoke that forgets to unmap.
  /// Deleting the `teardown_dax_window` call leaves this test green while
  /// `request_revoke_inode_tears_down_the_daemons_mapping` goes red — so that class is covered, but by that
  /// test and not this one. The cause is the conservative slot tracking below, which this schedule needs to
  /// stay fault-safe: a torn-down slot is `PROT_NONE` and reading it faults, so the schedule stops trusting
  /// every slot after any revoking op, and a slot it never reads is one whose staleness it cannot see.
  /// Fault-safety and missed-unmap detection pull against each other here. Anyone extending this should reach
  /// for `DaxCacheRegion::unmapped()` — a counter, observable without touching the address space — rather than
  /// trying to read a slot that may legitimately fault.
  #[cfg(unix)]
  #[test]
  fn a_random_window_schedule_never_shows_the_guest_a_foreign_byte(
    schedule in prop::collection::vec((any::<u8>(), any::<u8>(), any::<u16>()), 1..40)
  ) {
    const SLOTS: usize = 4;
    let page = rustix::param::page_size().max(1);
    let region = page * SLOTS;
    let slots = SLOTS as u64;
    let mut store = dax_shared_store(page, "devchaos");
    let mut volume = volume(&mut store);
    volume.enable_writable_dax();
    let root = volume.root_inode(&store).unwrap();
    let mut files = Vec::new();
    for (i, fill) in CHAOS_FILLS.iter().enumerate() {
      let no = volume.create_file_no(&mut store, root, &format!("c{i}"), 0o644).unwrap();
      volume.write(&mut store, no, 0, &vec![*fill; page]).unwrap();
      files.push(no.0);
    }

    let (mut device, mut driver) = device_and_driver();
    let cache = tenant_cache("chaos", page);
    device.install_dax_window(Some(
      slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), region as u64).unwrap(),
    ));
    let cx = context_for(vid());
    tenant_connect((&mut device, &mut driver), (vid(), &mut volume, &mut store), &cx);

    let mut live: Vec<(u64, u64)> = Vec::new();
    let mut scanned = 0usize;
    for (step, triple) in schedule.into_iter().enumerate() {
      let unique = (step as u64).saturating_add(2);
      let effect = device_chaos_step(
        (&mut device, &mut driver),
        (&mut volume, &mut store),
        &cx,
        &files,
        page,
        slots,
        unique,
        triple,
      );
      match effect {
        ChaosEffect::Mapped(slot, nodeid) => {
          live.retain(|(at, _)| *at != slot);
          live.push((slot, nodeid));
        }
        ChaosEffect::None => {}
        // Any revoking op can leave a slot PROT_NONE, and reading one of those faults by design.
        ChaosEffect::TornDown => live.clear(),
      }
      for (slot, nodeid) in &live {
        let seen = device.dax_window_bytes(*slot, page as u64).unwrap_or_default();
        prop_assert!(!seen.is_empty(), "a live window reads, so the scan means something");
        scanned = scanned.saturating_add(1);
        let foreign = seen
          .iter()
          .find(|byte| **byte != 0 && !CHAOS_FILLS.contains(byte));
        prop_assert!(
          foreign.is_none(),
          "a byte the guest can see belongs to neither this volume nor zero: {foreign:?}"
        );
        // The stronger half: the window is not merely plausible, it is this file's current bytes. A stale
        // window would still pass the provenance check above, since every file here writes the same fills.
        let no = slates_vfs::ids::InodeNo(*nodeid);
        let len = usize::try_from(volume.stat(&store, no).map(|s| s.size).unwrap_or(0)).unwrap_or(0);
        let mut truth = vec![0u8; len];
        prop_assert!(volume.read(&store, no, 0, &mut truth).is_ok(), "the file reads");
        prop_assert_eq!(
          seen.get(..len),
          Some(truth.as_slice()),
          "the guest's window is this file's current bytes"
        );
        prop_assert!(
          seen.get(len..).is_some_and(|tail| tail.iter().all(|byte| *byte == 0)),
          "and zeroes past its length"
        );
      }
    }
    prop_assume!(scanned > 0);

    // Non-vacuity, where the schedule left a window live: pins must be held before the release can mean
    // anything. A schedule that ended with no window says nothing either way, and asserts nothing.
    if device.dax_window_count() > 0 {
      prop_assert!(
        store.content.arena().pinned_blocks() > 0,
        "windows are live, so blocks must be pinned before the release below"
      );
    }

    // Whatever the schedule did, the pin ledger must balance once the device lets go. `release_dax` is the
    // terminal teardown a crashed or disconnected VMM gets, and after it no block of this store may still be
    // pinned: a pin left behind holds a chunk out of the arena for good, and the only thing that would hide
    // one is nobody looking. Nothing looked, before 2026-10-18 — see
    // `two_windows_over_one_block_each_hold_its_pin`.
    {
      let mut bridge = VolumeBridge::new(vid(), &mut volume, &mut store);
      device.release_dax(&mut bridge, &cx);
    }
    prop_assert_eq!(
      store.content.arena().pinned_blocks(),
      0,
      "the schedule left blocks pinned after the device released everything"
    );
    // `release_refusals` counts an unpin or a deferred free the arena refused — a double release, or a pin the
    // ledger had already lost. It exists to signal exactly the imbalance a block count can hide (an extra
    // release that errors leaves the count right and the ledger wrong), and until now nothing read it.
    prop_assert_eq!(
      store.release_refusals,
      0,
      "the schedule made the arena refuse a release"
    );

    drop(cache);
  }
}

/// A 64-byte FUSE_INIT body (major 7, minor 38): the codec reads what it needs.
#[cfg(unix)]
fn dax_init_body() -> Vec<u8> {
  let mut b = Vec::new();
  b.extend_from_slice(&7u32.to_le_bytes());
  b.extend_from_slice(&38u32.to_le_bytes());
  b.extend_from_slice(&0u32.to_le_bytes());
  b.extend_from_slice(&0u32.to_le_bytes());
  b.resize(64, 0);
  b
}

/// A FUSE_SETUPMAPPING body: fh=0, `foffset`, `len`, write flag per `writable`, `moffset`.
#[cfg(unix)]
fn dax_setup_body(foffset: u64, len: u64, writable: bool, moffset: u64) -> Vec<u8> {
  let flags = if writable {
    slates_bridge_fuse::request::SETUPMAPPING_FLAG_WRITE
  } else {
    0
  };
  let mut b = Vec::new();
  for field in [0u64, foffset, len, flags, moffset] {
    b.extend_from_slice(&field.to_le_bytes());
  }
  b
}

/// §4.6 L0, through the device (AC-4.12). Do: on a shared-backed volume that has *not* opted in to writable DAX, a
/// guest asks SETUPMAPPING writable, then read-only. Expect: the writable one is refused `EPERM` with no window (L0
/// gates it through the whole device path, not just the gate), the read-only one is served (immutable DAX needs no
/// opt-in).
#[cfg(unix)]
#[test]
fn a_writable_dax_setupmapping_without_opt_in_is_refused() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "noopt");
  let mut vol = volume(&mut store); // NOT opted in to writable DAX
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x33; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxA-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );

  let w = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(
    reply_error(&w),
    -1,
    "writable DAX refused EPERM without opt-in"
  );
  assert_eq!(
    device.dax_window_count(),
    0,
    "no window for the refused writable map"
  );

  let r = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 3, nodeid),
    &dax_setup_body(0, page as u64, false, 0),
  );
  assert_eq!(reply_error(&r), 0, "read-only DAX served without opt-in");
  assert_eq!(device.dax_window_count(), 1, "the read-only window is live");
  drop(cache);
}

/// §4.6 (AC-4.12, bounds). Do: a guest asks SETUPMAPPING at a window offset past the end of the cache region. Expect:
/// refused `EINVAL` with no window — a hostile or confused guest cannot map beyond the window the VMM gave it.
#[cfg(unix)]
#[test]
fn a_setupmapping_past_the_window_is_refused() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "bounds");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x44; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxB-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );

  // moffset = the window's end: the whole requested range is outside it.
  let reply = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &dax_setup_body(0, page as u64, true, (page * 4) as u64),
  );
  assert_eq!(
    reply_error(&reply),
    -22,
    "a map past the window is refused EINVAL"
  );
  assert_eq!(
    device.dax_window_count(),
    0,
    "nothing mapped past the window"
  );
  drop(cache);
}

/// §4.6 multi-tenancy (adversarial: a guest maps the arena block's padding past its own file). The buddy allocator
/// rounds a content block up to a power of two, so a file of two pages + 1 byte lives in a four-page block; the file
/// authorizes only three pages (`Mapping::file_len` rounded up to the window granule), and the fourth page is padding
/// the file never reaches. Do: opt in to writable DAX, write such a file, then SETUPMAPPING the whole four-page block.
/// Expect: the map is CAPPED to the file's three authorized pages (it succeeds, mapping only those — the real guest
/// maps DAX in window chunks that reach past a small file, and refusing the whole chunk outright SIGBUS'd a live
/// crosvm guest), while a map STARTING entirely past the authorized pages is refused. So the guest's shared-memory
/// window never covers a page lying entirely past its file — the invariant holds by the cap, not by refusing the
/// over-reaching request — keeping the surface the host shares with the guest minimal.
#[cfg(unix)]
#[test]
fn a_setupmapping_reaching_past_the_file_is_capped_to_its_authorized_pages() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "pad");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  // Two pages + 1 byte: the buddy block rounds to four pages, the file authorizes three (rounded up to the granule).
  let file_len = page * 2 + 1;
  vol.write(&mut store, no, 0, &vec![0x44; file_len]).unwrap();
  let nodeid = no.0;
  let authorized = page * 3; // file_len rounded up to the page granule
  let block = page * 4; // the next power of two of (two pages + 1 byte)

  let (mut device, mut driver) = device_and_driver();
  let region = (page * 4) as u64;
  let cache = SharedObject::create(
    &format!("sl-daxpad-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), region).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );

  // The whole four-page block — its fourth page lies entirely past the file. The map is CAPPED to the file's three
  // authorized pages: it succeeds, mapping only those; the fourth page is never mapped.
  let capped = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &dax_setup_body(0, block as u64, true, 0),
  );
  assert_eq!(
    reply_error(&capped),
    0,
    "a request reaching past the file is capped to the authorized pages, not refused"
  );
  assert_eq!(
    device.dax_window_count(),
    1,
    "the cap mapped the file's authorized pages"
  );

  // A map STARTING entirely past the file's authorized pages backs nothing: refused, at a distinct window slot so
  // it does not remap the capped window above. The invariant: the guest never maps a page lying entirely past its
  // file.
  let entirely_past = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 3, nodeid),
    &dax_setup_body(authorized as u64, page as u64, true, (page * 3) as u64),
  );
  assert_eq!(
    reply_error(&entirely_past),
    -22,
    "a map starting entirely past the file's pages is refused EINVAL"
  );
  assert_eq!(
    device.dax_window_count(),
    1,
    "nothing new is mapped past the file; the capped window still stands"
  );

  // Exactly the file's authorized pages, remapping the capped slot. Succeeds.
  let ok = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 4, nodeid),
    &dax_setup_body(0, authorized as u64, true, 0),
  );
  assert_eq!(
    reply_error(&ok),
    0,
    "a map of exactly the file's authorized pages succeeds"
  );
  assert_eq!(
    device.dax_window_count(),
    1,
    "the authorized window is live"
  );
  drop(cache);
}

/// §4.6 (adversarial: a guest remaps a window slot). Do: opt in to writable DAX, map file A's block into window
/// offset 0, then SETUPMAPPING file B's block into the SAME window offset 0 (a remap). Expect: the remap replaces A
/// — one live window entry (not two), the window shows B's bytes, and after a later REMOVEMAPPING of offset 0 the
/// registry holds no live mapping and both A (revoked at the remap) and B (revoked at REMOVEMAPPING) were released.
/// Without the remap teardown A's entry and pin leak behind B's — `position` at REMOVEMAPPING finds one, the other
/// stays pinned forever.
#[cfg(unix)]
#[test]
fn a_remap_over_an_occupied_window_slot_revokes_the_old_mapping() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "remap");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let a = vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  vol.write(&mut store, a, 0, &vec![0xAA; page]).unwrap();
  let b = vol.create_file_no(&mut store, root, "b", 0o644).unwrap();
  vol.write(&mut store, b, 0, &vec![0xBB; page]).unwrap();
  let (a_no, b_no) = (a.0, b.0);

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxremap-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );

  let ra = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, a_no),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&ra), 0, "A mapped at window offset 0");
  assert_eq!(device.dax_window_count(), 1, "one window after A");

  let rb = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 3, b_no),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&rb), 0, "B remapped over A's slot");
  assert_eq!(
    device.dax_window_count(),
    1,
    "still one window after the remap — A's entry did not leak"
  );
  assert_eq!(
    device.dax_window_bytes(0, 64).unwrap(),
    vec![0xBB; 64],
    "the window now shows B, not A"
  );

  let mut remove = Vec::new();
  remove.extend_from_slice(&1u32.to_le_bytes());
  remove.extend_from_slice(&0u64.to_le_bytes());
  remove.extend_from_slice(&(page as u64).to_le_bytes());
  let rr = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::RemoveMapping, 4, b_no),
    &remove,
  );
  assert_eq!(reply_error(&rr), 0, "REMOVEMAPPING of offset 0 served");
  assert_eq!(device.dax_window_count(), 0, "no window entry left");

  drop(bridge);
  assert_eq!(
    store.dax.len(),
    0,
    "no live mapping in the registry — neither A nor B leaked"
  );
  assert_eq!(
    store.dax.revoked(),
    2,
    "both A (at the remap) and B (at REMOVEMAPPING) were revoked"
  );
  drop(cache);
}

/// §4.6 (terminal teardown). Do: opt in, map a file's block, then send FUSE_DESTROY (the guest unmounting) with no
/// prior REMOVEMAPPING. Expect: DESTROY tears the live window down — no window entry left, the registry holds no
/// live mapping, and the block's pin was released (`store.dax.revoked()` moved). Without the teardown drain an
/// unmount leaks the mapped block's pin for the attachment's life (the deferred free could never reclaim it).
#[cfg(unix)]
#[test]
fn fuse_destroy_tears_down_live_dax_windows() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "destroy");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxdestroy-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
  let r = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&r), 0, "mapped");
  assert_eq!(device.dax_window_count(), 1, "one live window");

  let d = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Destroy, 3, ROOT),
    &[],
  );
  assert_eq!(reply_error(&d), 0, "DESTROY served");
  assert_eq!(
    device.dax_window_count(),
    0,
    "the live window was torn down at unmount"
  );

  drop(bridge);
  assert_eq!(
    store.dax.len(),
    0,
    "the registry holds no live mapping after unmount — the pin was not leaked"
  );
  assert_eq!(
    store.dax.revoked(),
    1,
    "the mapped block was revoked at teardown"
  );
  drop(cache);
}

/// §4.6 (terminal teardown on a disconnect, no FUSE_DESTROY). Do: opt in, map a file's block, then call the
/// device's terminal `release_dax` directly — what `AdmittedDevice::reclaim` runs when the VMM disconnects or
/// crashes without unmounting (the guest never sent FUSE_DESTROY). Expect: the live window is torn down — no entry
/// left, the registry holds no live mapping, and the block's pin was released. This is the leak path FUSE_DESTROY
/// cannot cover.
#[cfg(unix)]
#[test]
fn release_dax_tears_down_live_windows_on_a_disconnect() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "disc");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxdisc-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
  let r = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&r), 0, "mapped");
  assert_eq!(device.dax_window_count(), 1, "one live window");

  // The disconnect terminal step (no DESTROY): reclaim calls this with the attachment's bridge.
  device.release_dax(&mut bridge, &cx);
  assert_eq!(
    device.dax_window_count(),
    0,
    "the live window was torn down on the disconnect"
  );

  drop(bridge);
  assert_eq!(
    store.dax.len(),
    0,
    "the registry holds no live mapping after the disconnect — no leaked pin"
  );
  assert_eq!(
    store.dax.revoked(),
    1,
    "the mapped block was revoked at the disconnect teardown"
  );
  drop(cache);
}

/// §4.6 (adversarial / hostile input on the device's SETUPMAPPING + REMOVEMAPPING parsers of guest bytes;
/// CLAUDE.md hostile-input taxonomy: truncation, overflow, foreign counts). With a window offered and writable DAX
/// opted in, a hostile guest sends a truncated SETUPMAPPING body, a `len = u64::MAX`, an unaligned window offset, a
/// SETUPMAPPING of a non-mappable (inline) file, a truncated REMOVEMAPPING body, and a REMOVEMAPPING claiming
/// `u32::MAX` entries in a short body. Expect: each a typed refusal or a no-op — no window established, the block a
/// refused map briefly pinned is unpinned (no leak), no panic, and a valid map still works afterward (the device
/// was never faulted).
#[cfg(unix)]
#[test]
fn hostile_dax_requests_are_refused_and_establish_no_window() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "hostile");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let big = vol.create_file_no(&mut store, root, "big", 0o644).unwrap();
  vol.write(&mut store, big, 0, &vec![0x5A; page]).unwrap();
  let tiny = vol.create_file_no(&mut store, root, "tiny", 0o644).unwrap();
  vol.write(&mut store, tiny, 0, b"x").unwrap(); // inline: not one mappable content block
  let (big_no, tiny_no) = (big.0, tiny.0);

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxhostile-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );

  // Hostile SETUPMAPPING bodies: a truncated body (the parser needs 5x u64), a map starting entirely past the file's
  // pages (backs nothing), an unaligned window offset (the map is refused and the block it briefly pinned is
  // unpinned), and an inline file (no mappable content block). Each a typed refusal, no window, the block a refused
  // map pinned released — a hostile guest establishes no mapping. (A colossal len that reaches past the file is
  // capped to the file's pages, not a refusal — covered by the cap test above.)
  let page_len = page as u64;
  for (unique, nodeid, body, why) in [
    (
      2u64,
      big_no,
      [0u8; 20].to_vec(),
      "a truncated SETUPMAPPING body",
    ),
    (
      3,
      big_no,
      dax_setup_body(page_len, page_len, true, 0),
      "a map starting entirely past the file's pages",
    ),
    (
      4,
      big_no,
      dax_setup_body(0, page_len, true, 1),
      "an unaligned moffset",
    ),
    (
      5,
      tiny_no,
      dax_setup_body(0, page_len, true, 0),
      "an inline file",
    ),
  ] {
    let r = dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::SetupMapping, unique, nodeid),
      &body,
    );
    assert_ne!(reply_error(&r), 0, "{why} is refused");
    assert_eq!(device.dax_window_count(), 0, "{why} established no window");
  }

  // Hostile REMOVEMAPPING bodies: a truncated body (below the u32 count) and a foreign count (u32::MAX entries in a
  // short body). Each handled without faulting the device and establishing no window.
  for (unique, body) in [
    (6u64, [0u8; 3].to_vec()),
    (
      7,
      [
        u32::MAX.to_le_bytes().as_slice(),
        &0u64.to_le_bytes(),
        &page_len.to_le_bytes(),
      ]
      .concat(),
    ),
  ] {
    let _ = dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::RemoveMapping, unique, big_no),
      &body,
    );
    assert_eq!(
      device.dax_window_count(),
      0,
      "a hostile REMOVEMAPPING established no window"
    );
  }

  // The device was never faulted: a well-formed map still succeeds.
  let r = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 8, big_no),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(
    reply_error(&r),
    0,
    "a valid map still succeeds after the hostile requests"
  );
  assert_eq!(
    device.dax_window_count(),
    1,
    "exactly the one valid window is live"
  );

  // The hostile sequence must have leaked nothing: with the one valid window released, no block of the store is
  // still pinned. A refused map that kept its pin would hold a chunk out of the arena for good, and a hostile
  // guest could repeat it — the leak is the attack. Asserted over the whole arena rather than one block,
  // because the interesting leak is the one from a path nobody thought to name.
  //
  // The bridge is dropped first so the store can be read, and the pins are checked **before** the release as
  // well as after: the one live window really did pin something, so the zero below is the ledger balancing and
  // not a counter that never moves.
  drop(bridge);
  assert!(
    store.content.arena().pinned_blocks() > 0,
    "the live window pins a block, so the zero asserted below means something"
  );
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  device.release_dax(&mut bridge, &cx);
  drop(bridge);
  assert_eq!(
    (
      store.content.arena().pinned_blocks(),
      store.release_refusals
    ),
    (0, 0),
    "a hostile SETUPMAPPING/REMOVEMAPPING sequence left a block pinned, or made the arena refuse a release"
  );
  drop(cache);
}

/// The names in a flat READDIRPLUS directory snapshot (`Volume::dax_map_dir`'s layout): a u32 count, then per entry
/// a u64 inode, a kind byte, the attributes (u64 size, i64 mtime, u32 mode/uid/gid/nlink = 32 bytes), a u8 name
/// length, and the name's bytes — so the fixed part before the name is 8 + 1 + 32 + 1 = 42 bytes.
#[cfg(unix)]
fn dir_snapshot_names(raw: &[u8]) -> Vec<String> {
  let mut names = Vec::new();
  let count = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
  let mut off = 4usize;
  for _ in 0..count {
    let name_len = usize::from(raw[off + 41]);
    names.push(String::from_utf8_lossy(&raw[off + 42..off + 42 + name_len]).into_owned());
    off += 42 + name_len;
  }
  names
}

/// §4.6 metadata DAX through the device (READDIR). Do: create three named files in the root, offer a cache window,
/// serve FUSE_INIT, then a guest FUSE_SETUPMAPPING of the ROOT **directory** read-only; read the window as a guest
/// would, then REMOVEMAPPING. Expect: the window is the directory's entries as a flat snapshot (count + records),
/// so a guest lists the directory through its own page table with no READDIR round-trip; the teardown frees the
/// throwaway snapshot block. No VMM, no KVM — a simulated guest, as the data-path DAX test is.
#[cfg(unix)]
#[test]
fn a_guest_setupmapping_of_a_directory_maps_its_entries() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "dirmap");
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let names = ["alpha", "beta", "gamma"];
  for name in names {
    vol.create_file_no(&mut store, root, name, 0o644).unwrap();
  }
  let root_no = root.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxdir-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );

  // SETUPMAPPING the root directory, read-only: the device maps its entries snapshot into the window.
  let reply = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, root_no),
    &dax_setup_body(0, page as u64, false, 0),
  );
  assert_eq!(
    reply_error(&reply),
    0,
    "a directory SETUPMAPPING maps its entries"
  );
  assert_eq!(device.dax_window_count(), 1, "one live directory window");

  let raw = device.dax_window_bytes(0, page as u64).unwrap();
  let got = dir_snapshot_names(&raw);
  for name in names {
    assert!(
      got.iter().any(|g| g == name),
      "entry {name} readable from the directory window: {got:?}"
    );
  }

  let mut remove = Vec::new();
  remove.extend_from_slice(&1u32.to_le_bytes());
  remove.extend_from_slice(&0u64.to_le_bytes());
  remove.extend_from_slice(&(page as u64).to_le_bytes());
  let removed = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::RemoveMapping, 3, root_no),
    &remove,
  );
  assert_eq!(
    reply_error(&removed),
    0,
    "REMOVEMAPPING tears the directory window down"
  );
  assert_eq!(
    device.dax_window_count(),
    0,
    "the directory window was torn down"
  );
  drop(cache);
}

/// §4.6 step 5 (daemon-initiated revoke → the device munmaps the window BEFORE the daemon unpins). Do: opt in, map
/// a file writable through the device, then call `request_revoke_inode` for that inode — what the daemon signals
/// before it moves, writes, shares, lands or takes over the block. Expect: the window is torn down (the two-phase
/// revoke — unregister, unmap, unpin), the registry mapping is revoked and the block released, so a reused block is
/// never reachable through a stale TLB; a second call tears nothing down (idempotent). This is the device half of
/// coherence for both data and metadata DAX — the daemon-initiated counterpart of a guest's REMOVEMAPPING.
#[cfg(unix)]
#[test]
fn request_revoke_inode_tears_down_the_daemons_mapping() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "reqrevoke");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxreq-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
  let r = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&r), 0, "mapped");
  assert_eq!(device.dax_window_count(), 1, "one live window");

  // The daemon signals the device to revoke the inode's windows before it touches the block (step 5).
  let torn = device.request_revoke_inode(nodeid, &mut bridge, &cx);
  assert_eq!(
    torn, 1,
    "the daemon-initiated revoke tore down the inode's one window"
  );
  assert_eq!(device.dax_window_count(), 0, "the window was munmapped");
  assert_eq!(
    device.request_revoke_inode(nodeid, &mut bridge, &cx),
    0,
    "idempotent: nothing left to revoke"
  );

  drop(bridge);
  assert_eq!(store.dax.len(), 0, "the registry mapping was revoked");
  assert_eq!(
    store.dax.revoked(),
    1,
    "the block was unpinned through the two-phase (release after unmap)"
  );
  drop(cache);
}

/// §4.6 — the device half of the snapshot/clone teardown (`Device::request_revoke_writable_inode`). A snapshot shares
/// the volume's current blocks with the frozen version, so a **writable** DAX window of one must be torn down (its
/// next write would reach the frozen version), while a **read-only** window survives (immutable shared content is the
/// allowed DAX case, and a read-only window cannot corrupt a snapshot). Do: map one inode both writable (one window
/// slot) and read-only (another slot), then revoke the inode's *writable* windows. Expect: the writable window is
/// torn (1), the read-only window survives, a second writable revoke tears nothing, and the all-windows revoke then
/// tears the read-only one — so the writable filter keeps exactly the windows a snapshot must keep. This is the leaf
/// primitive; the synchronous server-side recall that drives it at `snapshot` is the owed cross-task integration.
#[cfg(unix)]
#[test]
fn request_revoke_writable_inode_keeps_the_read_only_window() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "reqrevwr");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxrw-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
  // A writable window at window offset 0 and a read-only window at offset `page`, both of the same inode.
  let w = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 2, nodeid),
    &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&w), 0, "writable mapped");
  let r = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::SetupMapping, 3, nodeid),
    &dax_setup_body(0, page as u64, false, page as u64),
  );
  assert_eq!(reply_error(&r), 0, "read-only mapped");
  assert_eq!(device.dax_window_count(), 2, "both windows live");

  // Snapshot's teardown: revoke only the inode's writable windows.
  let torn = device.request_revoke_writable_inode(nodeid, &mut bridge, &cx);
  assert_eq!(torn, 1, "exactly the writable window was torn");
  assert_eq!(
    device.dax_window_count(),
    1,
    "the read-only window survives a writable revoke"
  );
  assert_eq!(
    device.request_revoke_writable_inode(nodeid, &mut bridge, &cx),
    0,
    "no writable window remains to revoke"
  );
  // The all-windows revoke (the write/truncate path) then tears the surviving read-only one.
  assert_eq!(
    device.request_revoke_inode(nodeid, &mut bridge, &cx),
    1,
    "the read-only window is torn by the all-windows revoke"
  );
  assert_eq!(device.dax_window_count(), 0, "no window remains");
  drop(bridge);
  drop(cache);
}

/// §4.6 step 5 — the daemon→device coherence channel, end to end through a real window. A client on the shard that
/// writes a DAX-mapped file enqueues a step-5 revoke in the VFS; the device's next service pass drains that queue
/// and tears the guest's now-stale window down, so the guest re-faults onto the new block. The daemon builds a
/// *transient* `VolumeBridge` per pass over the shard's state (serve.rs), so dropping and rebuilding the bridge
/// between the map, the mutation and the drain models exactly what the shard does between the device loop's passes.
/// Do: map a file writable through the device, drop the bridge, write the file (a client mutation on the shard),
/// rebuild the bridge, drain its pending revokes and feed each to `request_revoke_inode`. Expect: the write
/// enqueued the inode and revoked its registry mapping (phase one, block still pinned); the drain yields exactly
/// that inode; the device tears the one window down; and the block is unpinned (phase two) — a reused block is
/// never reachable through the guest's stale TLB.
#[cfg(unix)]
#[test]
fn a_write_of_a_mapped_file_enqueues_a_revoke_the_next_service_pass_drains() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "channel");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxchan-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();

  // Pass one: a transient bridge brings the connection up and maps the file writable — one live guest window.
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::Init, 1, ROOT),
      &dax_init_body(),
    );
    let r = dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::SetupMapping, 2, nodeid),
      &dax_setup_body(0, page as u64, true, 0),
    );
    assert_eq!(reply_error(&r), 0, "mapped");
  }
  assert_eq!(
    device.dax_window_count(),
    1,
    "one live window after the map pass"
  );
  assert_eq!(store.dax.len(), 1, "the registry holds the live mapping");

  // Between passes a client on the shard writes the mapped file. The VFS write boundary revokes the inode's mappings
  // (phase one): the registry entry goes, the block stays pinned, and the inode is enqueued for the device.
  vol.write(&mut store, no, 0, &vec![0xA5; page]).unwrap();
  assert_eq!(
    store.dax.len(),
    0,
    "the write revoked the mapping (phase one)"
  );
  assert_eq!(
    store.dax.revoked(),
    1,
    "phase one counted the revoke; the block is still pinned"
  );

  // Pass two: a fresh transient bridge. The daemon drains its pending revokes and tears the guest window down.
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    let pending = bridge.take_pending_dax_revokes();
    assert_eq!(
      pending,
      vec![(nodeid, false)],
      "the write enqueued exactly the mutated inode, writable_only=false (every window stale)"
    );
    assert_eq!(
      device.request_revoke_inode(nodeid, &mut bridge, &cx),
      1,
      "the device tore down the inode's one window"
    );
    assert!(
      bridge.take_pending_dax_revokes().is_empty(),
      "the queue is drained — a later pass does no redundant work"
    );
  }
  assert_eq!(
    device.dax_window_count(),
    0,
    "the guest window was unmapped"
  );
  drop(cache);
}

/// §4.6 "revoke on advance": the device re-validates its live DAX windows against its current view and tears any the
/// view no longer backs, keeping those still current. A snapshot reader's `advance` to a newer snapshot moves some
/// inodes' content to new blocks without enqueuing a per-inode revoke (which would tear *every* reader's windows,
/// not just the one moving), so the advancing device re-validates its own windows here and tears exactly the stale
/// ones — the guest re-maps against the new view. Do: map two files' windows; move one file's content so its block
/// and length no longer match its window (the change an advance presents), leaving the other untouched; re-validate.
/// Expect: the stale window is torn (1), the current window survives, and a second re-validate tears nothing. The
/// transient per-pass bridge models the shard between the device loop's passes (as the channel test above).
#[cfg(unix)]
#[test]
fn an_advance_revalidate_tears_the_stale_window_and_keeps_the_current_one() {
  use slates_mem::{SharedObject, Words};
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "revalidate");
  let mut vol = volume(&mut store);
  vol.enable_writable_dax();
  let root = vol.root_inode(&store).unwrap();
  let a = vol.create_file_no(&mut store, root, "a", 0o644).unwrap();
  let b = vol.create_file_no(&mut store, root, "b", 0o644).unwrap();
  vol.write(&mut store, a, 0, &vec![0xA1; page]).unwrap();
  vol.write(&mut store, b, 0, &vec![0xB2; page]).unwrap();

  let (mut device, mut driver) = device_and_driver();
  let cache = SharedObject::create(
    &format!("sl-daxrev-{}", std::process::id()),
    page * 4,
    Words::new(),
  )
  .unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();

  // Map one read-only window of each file (a snapshot reader's windows).
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::Init, 1, ROOT),
      &dax_init_body(),
    );
    let wa = dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::SetupMapping, 2, a.0),
      &dax_setup_body(0, page as u64, false, 0),
    );
    assert_eq!(reply_error(&wa), 0, "a mapped");
    let wb = dax_exchange(
      &mut device,
      &mut driver,
      &mut bridge,
      &cx,
      (Opcode::SetupMapping, 3, b.0),
      &dax_setup_body(0, page as u64, false, page as u64),
    );
    assert_eq!(reply_error(&wb), 0, "b mapped");
  }
  assert_eq!(device.dax_window_count(), 2, "both windows live");

  // `a`'s content moves to a new block and length (the change an advance to a newer snapshot presents); `b` is
  // untouched, so its window still names the current block.
  vol.write(&mut store, a, 0, &vec![0xA2; page * 2]).unwrap();

  // The advance kick: the device re-validates its windows against the current view.
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    let torn = device.revalidate_windows(&mut bridge, &cx);
    assert_eq!(torn, 1, "exactly the stale window (a) was torn");
    assert_eq!(device.dax_window_count(), 1, "b's current window survives");
    assert_eq!(
      device.revalidate_windows(&mut bridge, &cx),
      0,
      "a second re-validate finds every remaining window current and tears nothing"
    );
  }
  drop(cache);
}

/// Receives one `SHMEM_MAP` back-end request and its single descriptor the way a vhost-user front end would,
/// asserting the request code; returns the descriptor so the caller can read the mapped block's bytes.
#[cfg(target_os = "linux")]
fn front_recv_shmem_map(socket: &std::os::fd::OwnedFd) -> std::os::fd::OwnedFd {
  use slates_bridge_virtiofs::vhost_user::{HEADER_LEN, MAP_FDS, MMAP_LEN, backend_request};
  let mut buf = [0u8; HEADER_LEN + MMAP_LEN];
  let mut space = [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAP_FDS))];
  let mut control = rustix::net::RecvAncillaryBuffer::new(&mut space);
  let got = rustix::net::recvmsg(
    socket,
    &mut [std::io::IoSliceMut::new(&mut buf)],
    &mut control,
    rustix::net::RecvFlags::empty(),
  )
  .unwrap();
  assert_eq!(got.bytes, buf.len(), "a whole SHMEM_MAP message");
  assert_eq!(
    u32::from_le_bytes(buf[0..4].try_into().unwrap()),
    backend_request::SHMEM_MAP,
    "the back-end request is SHMEM_MAP"
  );
  let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
  for message in control.drain() {
    if let rustix::net::RecvAncillaryMessage::ScmRights(iter) = message {
      fds.extend(iter);
    }
  }
  assert_eq!(fds.len(), 1, "SHMEM_MAP carries exactly one descriptor");
  fds.into_iter().next().unwrap()
}

/// Answers a back-end request with a reply-ack carrying `status` (zero for success), as a front end would.
#[cfg(target_os = "linux")]
fn front_send_ack(socket: &std::os::fd::OwnedFd, request: u32, status: u64) {
  use slates_bridge_virtiofs::vhost_user::{
    ACK_LEN, FLAG_REPLY, HEADER_AT_FLAGS, HEADER_AT_SIZE, HEADER_LEN, VERSION,
  };
  let mut ack = [0u8; HEADER_LEN + ACK_LEN];
  ack[0..4].copy_from_slice(&request.to_le_bytes());
  ack[HEADER_AT_FLAGS..HEADER_AT_FLAGS + 4].copy_from_slice(&(VERSION | FLAG_REPLY).to_le_bytes());
  ack[HEADER_AT_SIZE..HEADER_AT_SIZE + 4]
    .copy_from_slice(&u32::try_from(ACK_LEN).unwrap().to_le_bytes());
  ack[HEADER_LEN..].copy_from_slice(&status.to_le_bytes());
  rustix::net::sendmsg(
    socket,
    &[std::io::IoSlice::new(&ack)],
    &mut rustix::net::SendAncillaryBuffer::default(),
    rustix::net::SendFlags::NOSIGNAL,
  )
  .unwrap();
}

/// §4.6 — the other half of the message-based SETUPMAPPING proof above, and the half that runs on every host
/// this project can reach. `udmabuf::confine` carves a descriptor covering **only** the guest's window; a host
/// that cannot carve one must **refuse** the map, because sending the wide descriptor instead is the escape
/// itself (`docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`) and CLAUDE.md #7
/// leaves no room for a fallback.
///
/// Do: on a host with no `/dev/udmabuf` — this laptop, Docker's linuxkit VM, and any runner without the
/// module — install a message placer and ask for a SETUPMAPPING. Expect: the pass does **not** park waiting
/// for a front-end acknowledgement, which is what a descriptor actually sent would do; the guest is answered
/// `EOPNOTSUPP`; and no window is recorded.
///
/// Until this existed the refusal was asserted nowhere. `MessagePlacer` appeared only in the test above, which
/// skips exactly when this condition holds, so a regression that sent the unconfined descriptor would have
/// passed on every machine reachable from here. The two tests partition the hosts: one proves confinement
/// where `/dev/udmabuf` exists, this one proves refusal where it does not, and each skips loudly on the other's
/// host.
#[cfg(target_os = "linux")]
#[test]
fn a_message_based_setupmapping_is_refused_where_the_window_cannot_be_confined() {
  use slates_bridge_virtiofs::device::DaxWindowEntry;
  use slates_bridge_virtiofs::vhost_user::{BackendReqChannel, MessagePlacer};

  if rustix::fs::open(
    "/dev/udmabuf",
    rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
    rustix::fs::Mode::empty(),
  )
  .is_ok()
  {
    eprintln!(
      "skipping the refusal proof: this host has /dev/udmabuf, so the placer confines the window instead — \
       which is what a_message_based_setupmapping_parks_until_the_front_end_acks_then_answers_the_guest proves"
    );
    return;
  }

  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "msgrefuse");
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  vol.write(&mut store, no, 0, &vec![0x5Au8; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let (_front, back) = rustix::net::socketpair(
    rustix::net::AddressFamily::UNIX,
    rustix::net::SocketType::STREAM,
    rustix::net::SocketFlags::CLOEXEC,
    None,
  )
  .unwrap();
  let placer: MessagePlacer<DaxWindowEntry> =
    MessagePlacer::new(BackendReqChannel::adopt(back), page, (page * 16) as u64);
  device.install_dax_message_placer(placer);
  assert!(device.dax_offered(), "the device serves message-based DAX");

  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let init_reply = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
  assert_eq!(reply_error(&init_reply), 0, "INIT served");

  let setup = dax_setup_body(0, (2 * 1024 * 1024) as u64, false, 0);
  let request = message(Opcode::SetupMapping.to_wire(), 2, nodeid, &setup);
  let queue = FIRST_REQUEST_QUEUE;
  let head = driver.submit(usize::from(queue), &request, 4096, 1);
  let pass = serve(&mut device, queue, &mut driver, &mut bridge, &cx, BATCH).unwrap();

  assert!(
    !pass.more_pending,
    "the map was refused outright, not parked waiting for an acknowledgement of a descriptor it sent"
  );
  let (_, len) = driver
    .reap(usize::from(queue))
    .expect("the guest is answered rather than left waiting");
  assert_eq!(
    reply_error(&driver.reply_of(usize::from(queue), head, len)),
    -EOPNOTSUPP,
    "a window that cannot be confined is refused EOPNOTSUPP, never served wide"
  );
  assert_eq!(
    device.dax_window_count(),
    0,
    "and no window is recorded for a map that never happened"
  );
}

/// §4.6 / A-9 / A-135 (message-based DAX, Piece C.2b, by use with no VMM): a guest FUSE_SETUPMAPPING over a
/// message-based placer parks — its reply is withheld until the front end acknowledges the map — then completes.
/// Do: on a real volume with a single-block file of known bytes, install a message placer over a socketpair,
/// serve FUSE_INIT, then a guest SETUPMAPPING for a 2 MiB window chunk of the 1-page file. Expect: the service
/// pass parks (no used element published, no window recorded yet); the placer sent a SHMEM_MAP carrying a
/// descriptor **confined to the window** — its bytes read back by mapping it, and one byte past the window not
/// mappable at all, which is the escape of 2026-10-18 closed; after the front end acknowledges, complete_dax_awaiting
/// answers the guest SETUPMAPPING OK, records the one window, and releases the chain's charge — the async map-ack
/// park, mirroring the owner's barrier, proven in-tree.
#[cfg(target_os = "linux")]
#[allow(clippy::cognitive_complexity)]
#[test]
fn a_message_based_setupmapping_parks_until_the_front_end_acks_then_answers_the_guest() {
  use slates_bridge_virtiofs::device::DaxWindowEntry;
  use slates_bridge_virtiofs::vhost_user::{BackendReqChannel, MessagePlacer, backend_request};

  // The message-based placer confines the descriptor it sends with a `udmabuf` (§4.6), so this proof needs
  // `/dev/udmabuf`. Where the kernel or the container has none, the placer refuses the map by design — a
  // different property, and not this test's — so it skips loudly rather than failing, as the suite's other
  // environment gates do. Docker's linuxkit VM has no such device; a Fedora host does.
  if rustix::fs::open(
    "/dev/udmabuf",
    rustix::fs::OFlags::RDWR | rustix::fs::OFlags::CLOEXEC,
    rustix::fs::Mode::empty(),
  )
  .is_err()
  {
    eprintln!(
      "skipping the message-based SETUPMAPPING proof: /dev/udmabuf is unavailable, so the placer refuses \
       the map rather than confining it"
    );
    return;
  }
  let page = rustix::param::page_size().max(1);
  let mut store = dax_shared_store(page, "msgmap");
  let mut vol = volume(&mut store);
  let root = vol.root_inode(&store).unwrap();
  let no = vol.create_file_no(&mut store, root, "f", 0o644).unwrap();
  // One page of known bytes: above inline, within one chunk -> a single unsealed block the DAX gate maps.
  vol.write(&mut store, no, 0, &vec![0x5Au8; page]).unwrap();
  let nodeid = no.0;

  let (mut device, mut driver) = device_and_driver();
  let (front, back) = rustix::net::socketpair(
    rustix::net::AddressFamily::UNIX,
    rustix::net::SocketType::STREAM,
    rustix::net::SocketFlags::CLOEXEC,
    None,
  )
  .unwrap();
  let placer: MessagePlacer<DaxWindowEntry> =
    MessagePlacer::new(BackendReqChannel::adopt(back), page, (page * 16) as u64);
  device.install_dax_message_placer(placer);
  assert!(device.dax_offered(), "the device serves message-based DAX");

  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);

  let init_reply = dax_exchange(
    &mut device,
    &mut driver,
    &mut bridge,
    &cx,
    (Opcode::Init, 1, ROOT),
    &dax_init_body(),
  );
  assert_eq!(reply_error(&init_reply), 0, "INIT served");

  // SETUPMAPPING for a 2 MiB window chunk of the 1-page file (read): the placer caps the map to the file's page.
  let setup = dax_setup_body(0, (2 * 1024 * 1024) as u64, false, 0);
  let request = message(Opcode::SetupMapping.to_wire(), 2, nodeid, &setup);
  let queue = FIRST_REQUEST_QUEUE;
  let head = driver.submit(usize::from(queue), &request, 4096, 1);
  let pass = serve(&mut device, queue, &mut driver, &mut bridge, &cx, BATCH).unwrap();
  assert!(
    pass.more_pending,
    "the pass parked the map and asks to come back"
  );
  assert!(
    driver.reap(usize::from(queue)).is_none(),
    "no used element is published while the map is parked"
  );
  assert_eq!(
    device.dax_window_count(),
    0,
    "the window is not recorded until the front end acknowledges"
  );

  // The front end receives the SHMEM_MAP and its descriptor. Since 2026-10-19 that descriptor is a `udmabuf`
  // confined to the window (§4.6), so it is read by mapping it rather than with `pread` — a dmabuf has no read
  // method — and the length it maps to is the point: the arena's object is far larger, and an unconfined
  // descriptor let the front end map all of it, a neighbour's blocks included
  // (`docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`).
  let got_fd = front_recv_shmem_map(&front);
  // SAFETY: a fresh mapping at an address the kernel chooses, read-only and shared, of `page` bytes of a
  // descriptor this test owns; it is unmapped below, with the same length, before the descriptor is dropped.
  let window = unsafe {
    rustix::mm::mmap(
      std::ptr::null_mut(),
      page,
      rustix::mm::ProtFlags::READ,
      rustix::mm::MapFlags::SHARED,
      &got_fd,
      0,
    )
  }
  .expect("the front end maps the window it was sent");
  // SAFETY: `window` is the mapping just made, of `page` bytes, and nothing else aliases it for this borrow.
  let bytes = unsafe { std::slice::from_raw_parts(window.cast::<u8>(), page) };
  assert_eq!(
    &bytes[..64],
    &[0x5Au8; 64],
    "the SHMEM_MAP carried the file's content block"
  );
  // The confinement itself: one byte past the authorized window is not reachable through what was sent.
  // SAFETY: the same call, expected to be refused; the result is never dereferenced, and is unmapped if the
  // kernel surprises us by allowing it.
  let past = unsafe {
    rustix::mm::mmap(
      std::ptr::null_mut(),
      page.saturating_mul(2),
      rustix::mm::ProtFlags::READ,
      rustix::mm::MapFlags::SHARED,
      &got_fd,
      0,
    )
  };
  assert!(
    past.is_err(),
    "the descriptor the front end holds covers the window and nothing past it"
  );
  // SAFETY: unmapping exactly the mapping made above, with its own address and length.
  unsafe { rustix::mm::munmap(window, page) }.expect("the window unmaps");
  front_send_ack(&front, backend_request::SHMEM_MAP, 0);

  // The device drains the ack, answers the guest, records the window, and releases the chain's charge.
  let completed = device
    .complete_dax_awaiting(&mut driver.memory, &mut bridge, &cx, &mut Unlimited)
    .unwrap();
  assert!(
    completed.is_some(),
    "the parked SETUPMAPPING completed on the ack"
  );
  let (id, len) = driver
    .reap(usize::from(queue))
    .expect("the used element is published after the ack");
  assert_eq!(id, head);
  assert_eq!(
    reply_error(&driver.reply_of(usize::from(queue), head, len)),
    0,
    "SETUPMAPPING is answered OK once the front end has mapped the window"
  );
  assert_eq!(device.dax_window_count(), 1, "the one window is recorded");

  drop(front);
}
