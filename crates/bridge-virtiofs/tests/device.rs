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

use common::{SimDriver, context, layout_at, message, reply_error, store, vid, volume};
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
  // The captured barrier stood for a committed publication: it reserved the CREATE's inode number (A-150), as the
  // daemon's commit marks the volume published. Without it the number would stay unreserved and hold the next reply.
  drop(bridge);
  vol.mark_published(&store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
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

/// The DAX windows over per-file backings (§4.6; A-163, A-168, A-194, A-195), for both placers. Every test here runs
/// over each placer this host has: the fd-sharing cache region the device overlays itself (an in-process VMM's window;
/// every Unix), and the message placer with a stand-in front end over a socketpair (an out-of-process VMM's; Linux,
/// where vhost-user is). A window maps a range of the file's backing, which the volume's write boundary keeps equal to
/// the file, so nothing here revokes, pins or tears a window down under a guest that holds it.
#[cfg(unix)]
mod backed {
  use super::*;
  use common::{dax_init_body, dax_setup_body};
  use slates_vfs::ids::InodeNo;

  /// Shape: the guest's DAX window in these tests, in 2 MiB ranges: room for the windows any of them maps at once.
  const WINDOW_RANGES: u64 = 4;
  /// Shape: the fill budget of these tests, in ranges: more than any of them fills.
  const BUDGET_RANGES: u64 = 8;
  /// Shape: the passes a test drives before it judges the device stuck: far more than any fill or ingest here takes.
  const TURNS: usize = 4096;
  /// Shape: the pages a fill, ingest or refill slice copies in these tests, so a fill of several pages takes turns.
  const SLICE_PAGES: u64 = 2;
  /// Shape: the pages of the fixtures' content arena: room for every file these tests write.
  const ARENA_PAGES: usize = 64;
  /// Shape: the pages of the file whose write-back the arena cannot hold: twice the fixture's whole arena.
  const PAST_THE_ARENA_PAGES: usize = 2 * ARENA_PAGES;
  /// Format: Linux `EPERM`, `EISDIR` and `EINVAL`, as a guest reads them.
  const EPERM: i32 = 1;
  const EISDIR: i32 = 21;
  const EINVAL: i32 = 22;

  /// Which placer a fixture's device maps its windows through.
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  enum Placer {
    /// The fd-sharing cache region the device overlays itself.
    Local,
    /// The message placer, answered by the stand-in front end.
    #[cfg(target_os = "linux")]
    Message,
  }

  impl Placer {
    /// A letter for the placer in an object's name, which macOS caps at 31 bytes.
    fn letter(self) -> char {
      match self {
        Placer::Local => 'l',
        #[cfg(target_os = "linux")]
        Placer::Message => 'm',
      }
    }
  }

  /// The placers this host runs: the fd-sharing one on every Unix, and the message placer on Linux.
  fn placers() -> Vec<Placer> {
    [
      Placer::Local,
      #[cfg(target_os = "linux")]
      Placer::Message,
    ]
    .to_vec()
  }

  /// One SHMEM_MAP of a window as the stand-in front end received it: where in the backing's object it maps from, where
  /// in the guest's window it goes, and how long it is.
  #[cfg(target_os = "linux")]
  #[derive(Clone, Copy, Debug, PartialEq, Eq)]
  struct FrontPiece {
    fd_offset: u64,
    shm_offset: u64,
    len: u64,
  }

  /// A window as the stand-in front end mapped it: every SHMEM_MAP of it (one under the identity, two under the
  /// device's guarded layout, A-194) placed at its offset in one region, as crosvm maps each into the guest's window
  /// (`MAP_SHARED | MAP_FIXED`), writable when the maps asked so. The guest reads it as one contiguous range.
  #[cfg(target_os = "linux")]
  struct FrontMap {
    pieces: Vec<FrontPiece>,
    len: usize,
    writable: bool,
    at: *mut std::ffi::c_void,
  }

  #[cfg(target_os = "linux")]
  impl FrontMap {
    /// `len` bytes of the window from `at`, as the guest reads them through its map.
    fn read(&self, at: usize, len: usize) -> Vec<u8> {
      assert!(at + len <= self.len, "a read inside the window");
      // SAFETY: `self.at` is this value's live mapping of `self.len` bytes, and `[at, at + len)` lies inside it.
      unsafe { std::slice::from_raw_parts(self.at.cast::<u8>().add(at), len) }.to_vec()
    }

    /// Stores `bytes` at `at` of the window, as the guest stores through its map.
    fn store(&mut self, at: usize, bytes: &[u8]) {
      assert!(self.writable, "a store through a window mapped writable");
      assert!(at + bytes.len() <= self.len, "a store inside the window");
      // SAFETY: as `read`; the mapping is writable (`self.writable`), and nothing else borrows it during the copy.
      unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), self.at.cast::<u8>().add(at), bytes.len());
      }
    }
  }

  #[cfg(target_os = "linux")]
  impl Drop for FrontMap {
    fn drop(&mut self) {
      // SAFETY: unmapping exactly the region this value reserved and mapped its pieces into, with its own address and
      // length.
      unsafe { rustix::mm::munmap(self.at, self.len) }.unwrap();
    }
  }

  /// A window as its guest reaches it.
  enum GuestMap {
    /// The fd-sharing placer's: the guest reads and stores through the device's own window, from `moffset`.
    Local { moffset: u64, len: usize },
    /// The message placer's: the stand-in front end's map of what it was sent.
    #[cfg(target_os = "linux")]
    Message(FrontMap),
  }

  impl GuestMap {
    /// The window's length.
    fn len(&self) -> usize {
      match self {
        GuestMap::Local { len, .. } => *len,
        #[cfg(target_os = "linux")]
        GuestMap::Message(map) => map.len,
      }
    }

    /// Whether the placer mapped it writable, where a test can see so without a fault: the message placer's flags. The
    /// fd-sharing placer's protection is the device's own mapping's, which a store into a read-only one would fault.
    fn seen_writable(&self) -> Option<bool> {
      match self {
        GuestMap::Local { .. } => None,
        #[cfg(target_os = "linux")]
        GuestMap::Message(map) => Some(map.writable),
      }
    }
  }

  /// Takes one back-end request the way a vhost-user front end would, without waiting: its request code, its
  /// `VhostUserMMap` body and its descriptor, if it carried one. `None` when nothing is pending.
  #[cfg(target_os = "linux")]
  fn front_take(
    socket: &std::os::fd::OwnedFd,
  ) -> Option<(
    u32,
    [u8; slates_bridge_virtiofs::vhost_user::MMAP_LEN],
    Option<std::os::fd::OwnedFd>,
  )> {
    use slates_bridge_virtiofs::vhost_user::{HEADER_LEN, MAP_FDS, MMAP_LEN};
    let mut buf = [0u8; HEADER_LEN + MMAP_LEN];
    let mut space =
      [std::mem::MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(MAP_FDS))];
    let mut control = rustix::net::RecvAncillaryBuffer::new(&mut space);
    let got = match rustix::net::recvmsg(
      socket,
      &mut [std::io::IoSliceMut::new(&mut buf)],
      &mut control,
      rustix::net::RecvFlags::DONTWAIT,
    ) {
      Ok(got) => got,
      Err(rustix::io::Errno::AGAIN) => return None,
      Err(e) => panic!("the front end's socket: {e}"),
    };
    assert_eq!(got.bytes, buf.len(), "a whole back-end request");
    let mut fds: Vec<std::os::fd::OwnedFd> = Vec::new();
    for message in control.drain() {
      if let rustix::net::RecvAncillaryMessage::ScmRights(iter) = message {
        fds.extend(iter);
      }
    }
    assert!(
      fds.len() <= 1,
      "a back-end request carries at most one descriptor"
    );
    let code = u32::from_le_bytes(buf[0..4].try_into().unwrap());
    let body: [u8; MMAP_LEN] = buf[HEADER_LEN..].try_into().unwrap();
    Some((code, body, fds.into_iter().next()))
  }

  /// Maps a window's SHMEM_MAPs as the front end does: each piece's `len` bytes from its `fd_offset`, shared, at its
  /// offset in the window, into one region reserved for the whole window, writable when its flags say so.
  #[cfg(target_os = "linux")]
  fn front_map(
    received: Vec<(
      [u8; slates_bridge_virtiofs::vhost_user::MMAP_LEN],
      std::os::fd::OwnedFd,
    )>,
  ) -> FrontMap {
    use slates_bridge_virtiofs::vhost_user::MMAP_FLAG_MAP_RW;
    let field = |body: &[u8], at: usize| u64::from_le_bytes(body[at..at + 8].try_into().unwrap());
    let mut pieces: Vec<(FrontPiece, bool)> = received
      .iter()
      .map(|(body, _)| {
        (
          FrontPiece {
            fd_offset: field(body, 8),
            shm_offset: field(body, 16),
            len: field(body, 24),
          },
          field(body, 32) & MMAP_FLAG_MAP_RW != 0,
        )
      })
      .collect();
    let writable = pieces[0].1;
    assert!(
      pieces
        .iter()
        .all(|(_, piece_writable)| *piece_writable == writable),
      "every piece of a window is mapped alike"
    );
    let start = pieces
      .iter()
      .map(|(piece, _)| piece.shm_offset)
      .min()
      .unwrap();
    let end = pieces
      .iter()
      .map(|(piece, _)| piece.shm_offset + piece.len)
      .max()
      .unwrap();
    assert_eq!(
      pieces.iter().map(|(piece, _)| piece.len).sum::<u64>(),
      end - start,
      "the pieces cover the window once, with no gap"
    );
    let len = usize::try_from(end - start).unwrap();
    let prot = if writable {
      rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE
    } else {
      rustix::mm::ProtFlags::READ
    };
    // SAFETY: a fresh private reservation with no access at an address the kernel chooses, which nothing else uses;
    // each piece replaces its part below, and `FrontMap` unmaps the whole of it.
    let at = unsafe {
      rustix::mm::mmap_anonymous(
        std::ptr::null_mut(),
        len,
        rustix::mm::ProtFlags::empty(),
        rustix::mm::MapFlags::PRIVATE | rustix::mm::MapFlags::NORESERVE,
      )
    }
    .unwrap();
    for ((piece, _), (_, descriptor)) in pieces.iter().zip(&received) {
      let place = at as usize + usize::try_from(piece.shm_offset - start).unwrap();
      // SAFETY: `[place, place + piece.len)` lies inside the reservation just made; `MAP_FIXED` replaces that part of
      // it with the piece's bytes of a descriptor this test owns, from a page-aligned offset.
      let mapped = unsafe {
        rustix::mm::mmap(
          place as *mut std::ffi::c_void,
          usize::try_from(piece.len).unwrap(),
          prot,
          rustix::mm::MapFlags::SHARED | rustix::mm::MapFlags::FIXED,
          descriptor,
          piece.fd_offset,
        )
      }
      .expect("the front end maps what it was sent");
      assert_eq!(mapped as usize, place, "the piece lies where it was placed");
    }
    pieces.sort_by_key(|(piece, _)| piece.shm_offset);
    FrontMap {
      pieces: pieces.into_iter().map(|(piece, _)| piece).collect(),
      len,
      writable,
      at,
    }
  }

  /// Answers a back-end request with a reply-ack carrying `status` (zero for success), as a front end would.
  #[cfg(target_os = "linux")]
  fn front_send_ack(socket: &std::os::fd::OwnedFd, request: u32, status: u64) {
    use slates_bridge_virtiofs::vhost_user::{
      ACK_LEN, FLAG_REPLY, HEADER_AT_FLAGS, HEADER_AT_SIZE, HEADER_LEN, VERSION,
    };
    let mut ack = [0u8; HEADER_LEN + ACK_LEN];
    ack[0..4].copy_from_slice(&request.to_le_bytes());
    ack[HEADER_AT_FLAGS..HEADER_AT_FLAGS + 4]
      .copy_from_slice(&(VERSION | FLAG_REPLY).to_le_bytes());
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

  /// A store of `ARENA_PAGES` pages of the host's page, so a backing's ranges are whole pages of it.
  fn backed_store(page: usize) -> slates_vfs::volume::Store {
    let mut arena = slates_mem::arena::ChunkArena::new(page);
    arena
      .add_region(slates_mem::region::Region::map(page * ARENA_PAGES, page, false).unwrap())
      .unwrap();
    slates_vfs::volume::Store::new(
      &slates_vfs::volume::StoreConfig {
        page,
        cache_line: 128,
        max_dirs: 64,
        max_inodes: 256,
        max_chunks: ARENA_PAGES,
        max_dir_blocks: 64,
        dir_cutover: 16,
      },
      arena,
      0,
    )
  }

  /// The bridge one pass serves through, as the shard builds one per pass: the volume, the store, the open handles kept
  /// across passes, and the base host of an overlay.
  fn backed_bridge<'v>(
    vol: &'v mut slates_vfs::volume::Volume,
    store: &'v mut slates_vfs::volume::Store,
    handles: &'v mut slates_mem::Slab<u64>,
    host: &'v mut Option<slates_vfs::host::sim::SimHost>,
  ) -> VolumeBridge<'v> {
    VolumeBridge::attached(
      vid(),
      vol,
      store,
      handles,
      host
        .as_mut()
        .map(|host| host as &mut dyn slates_vfs::host::HostFs),
    )
  }

  /// A device serving one placer over a volume, its store (with a fill budget) and guest driver; INIT served. The
  /// passes run as the serve loop runs them.
  struct Fixture {
    placer: Placer,
    /// How the device lays its backings out, so a test knows how many pieces each window is.
    layout: slates_mem::DaxLayout,
    store: slates_vfs::volume::Store,
    vol: slates_vfs::volume::Volume,
    host: Option<slates_vfs::host::sim::SimHost>,
    handles: slates_mem::Slab<u64>,
    device: Device,
    driver: SimDriver,
    /// The stand-in front end's end of the back-end-request socketpair: the message placer's.
    #[cfg(target_os = "linux")]
    front: Option<std::os::fd::OwnedFd>,
    /// The VMM's cache window object the fd-sharing placer's region maps, kept as its VMM keeps it.
    _cache: Option<slates_mem::SharedObject>,
    /// The unmaps the stand-in front end has taken, all acknowledged.
    #[cfg(target_os = "linux")]
    front_unmaps: usize,
    cx: OpContext,
    page: usize,
    unique: u64,
    barriers: u32,
  }

  impl Fixture {
    /// Over a scratch volume.
    fn scratch(placer: Placer, tag: &str) -> Fixture {
      let page = rustix::param::page_size().max(1);
      let mut store = backed_store(page);
      let vol = volume(&mut store);
      Fixture::over(placer, tag, (store, vol, None), context())
    }

    /// Over an overlay of `host`'s root.
    fn overlay(placer: Placer, tag: &str, mut host: slates_vfs::host::sim::SimHost) -> Fixture {
      use slates_vfs::host::HostFs as _;
      let page = rustix::param::page_size().max(1);
      let mut store = backed_store(page);
      let root = host.root();
      let facts = host.facts(root).unwrap();
      let vol = slates_vfs::volume::Volume::create_overlay(
        &mut store,
        slates_vfs::volume::VolumeConfig {
          prefix: 1,
          names: slates_vfs::names::NameEquivalence::Exact,
          quota: slates_vfs::quota::Quota::Bounded { limit: 1 << 30 },
          journal_bytes: 1 << 16,
          clock: Box::new(slates_vfs::clock::StepClock::new(1_000_000, 1_000)),
        },
        slates_vfs::base::BaseConfig {
          root,
          facts,
          large_class_bytes: u64::try_from(page).unwrap(),
        },
      )
      .unwrap();
      Fixture::over(placer, tag, (store, vol, Some(host)), context())
    }

    /// Over a scratch volume, attached through `cx`.
    fn over(
      placer: Placer,
      tag: &str,
      (mut store, vol, host): (
        slates_vfs::volume::Store,
        slates_vfs::volume::Volume,
        Option<slates_vfs::host::sim::SimHost>,
      ),
      cx: OpContext,
    ) -> Fixture {
      let page = rustix::param::page_size().max(1);
      store
        .shards_backings
        .set_budget(BUDGET_RANGES * slates_mem::DAX_RANGE_BYTES);
      let (mut device, driver) = device_and_driver();
      let window = WINDOW_RANGES * slates_mem::DAX_RANGE_BYTES;
      #[cfg(target_os = "linux")]
      let mut front = None;
      let cache = match placer {
        Placer::Local => {
          let object = slates_mem::SharedObject::create(
            &format!("sl-w{}-{tag}-{}", placer.letter(), std::process::id()),
            usize::try_from(window).unwrap(),
            slates_mem::Words::new(),
          )
          .unwrap();
          device.install_dax_window(Some(
            slates_bridge_virtiofs::dax::DaxCacheRegion::new(object.as_fd(), window).unwrap(),
          ));
          Some(object)
        }
        #[cfg(target_os = "linux")]
        Placer::Message => {
          use slates_bridge_virtiofs::vhost_user::{BackendReqChannel, MessagePlacer};
          let (ours, back) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
          )
          .unwrap();
          device.install_dax_message_placer(MessagePlacer::new(
            BackendReqChannel::adopt(back),
            page,
            window,
          ));
          front = Some(ours);
          None
        }
      };
      let mut fixture = Fixture {
        placer,
        layout: slates_bridge_core::dax_backing::guarded_layout(),
        store,
        vol,
        host,
        handles: slates_bridge_core::volume_bridge::new_handle_store(),
        device,
        driver,
        #[cfg(target_os = "linux")]
        front,
        _cache: cache,
        #[cfg(target_os = "linux")]
        front_unmaps: 0,
        cx,
        page,
        unique: 0,
        barriers: 0,
      };
      let init = fixture.submit(FIRST_REQUEST_QUEUE, Opcode::Init, ROOT, &dax_init_body());
      assert_eq!(fixture.answer(FIRST_REQUEST_QUEUE, init), 0, "INIT served");
      fixture
    }

    /// A file named `name` in the root holding `bytes`, its number reserved as the daemon's verb barrier reserves it
    /// (A-150): its node id.
    fn file(&mut self, name: &str, bytes: &[u8]) -> u64 {
      let root = self.vol.root_inode(&self.store).unwrap();
      let no = self
        .vol
        .create_file_no(&mut self.store, root, name, 0o644)
        .unwrap();
      self.vol.write(&mut self.store, no, 0, bytes).unwrap();
      self.vol.mark_published(&self.store);
      no.0
    }

    /// Puts one request on `queue`: its descriptor head.
    fn submit(&mut self, queue: u16, opcode: Opcode, nodeid: u64, body: &[u8]) -> u16 {
      self.unique += 1;
      let request = message(opcode.to_wire(), self.unique, nodeid, body);
      let capacity = if queue == HIPRIO_QUEUE { 0 } else { 4096 };
      self
        .driver
        .submit(usize::from(queue), &request, capacity, 1)
    }

    /// One pass as the serve loop runs it (`serve.rs`, `one_pass`): the backed DAX's turn, then, unless it was busy, a
    /// service pass of each queue. A reply held for the owner's barrier is completed as an owner whose barrier
    /// captures, and counted.
    fn pass(&mut self) -> slates_bridge_virtiofs::device::DaxTurned {
      let slice = SLICE_PAGES * u64::try_from(self.page).unwrap();
      let mut bridge = backed_bridge(
        &mut self.vol,
        &mut self.store,
        &mut self.handles,
        &mut self.host,
      );
      let turned = self
        .device
        .dax_turn(
          &mut self.driver.memory,
          &mut bridge,
          &self.cx,
          slice,
          &mut Unlimited,
        )
        .unwrap();
      if turned.barrier_owed {
        self.barriers += 1;
        self
          .device
          .complete_awaiting(
            &mut self.driver.memory,
            &mut bridge,
            &self.cx,
            true,
            &mut Unlimited,
          )
          .unwrap();
      }
      if !turned.busy {
        for queue in [HIPRIO_QUEUE, FIRST_REQUEST_QUEUE] {
          let pass = self
            .device
            .service_queue(
              queue,
              &mut self.driver.memory,
              &mut bridge,
              &self.cx,
              BATCH,
              &mut Unlimited,
            )
            .unwrap();
          if pass.barrier_owed {
            self.barriers += 1;
            self
              .device
              .complete_awaiting(
                &mut self.driver.memory,
                &mut bridge,
                &self.cx,
                true,
                &mut Unlimited,
              )
              .unwrap();
          }
        }
      }
      turned
    }

    /// The errno of `head`'s reply on `queue` if its used element is published; `None` while it is held.
    fn reply_now(&mut self, queue: u16, head: u16) -> Option<i32> {
      let (id, len) = self.driver.reap(usize::from(queue))?;
      assert_eq!(id, head, "the used element is the request's");
      Some(reply_error(&self.driver.reply_of(
        usize::from(queue),
        head,
        len,
      )))
    }

    /// Holds that the placer sent its front end nothing since the last exchange (the message placer's; the fd-sharing
    /// placer has no front end to send to).
    fn assert_nothing_sent(&self, what: &str) {
      #[cfg(target_os = "linux")]
      if let Some(front) = &self.front {
        assert!(front_take(front).is_none(), "{what}");
      }
      let _ = what;
    }

    /// Passes until `head`'s reply on `queue` is published, with no back-end request meanwhile: its errno.
    fn answer(&mut self, queue: u16, head: u16) -> i32 {
      for _ in 0..TURNS {
        self.pass();
        if let Some(errno) = self.reply_now(queue, head) {
          return errno;
        }
        self.assert_nothing_sent("no back-end request while the request is answered");
      }
      panic!("the request was not answered within {TURNS} passes");
    }

    /// Maps `node`'s range `[foffset, foffset + len)` at window offset `moffset`, through the whole exchange: the
    /// window as its guest reaches it. Windows the placer must unmap first (any over the range) are unmapped as the
    /// placer does: by the device itself, or by the front end, which acknowledges each unmap it takes.
    fn mapped(
      &mut self,
      node: u64,
      (foffset, len): (u64, u64),
      writable: bool,
      moffset: u64,
    ) -> GuestMap {
      let head = self.submit(
        FIRST_REQUEST_QUEUE,
        Opcode::SetupMapping,
        node,
        &dax_setup_body(foffset, len, writable, moffset),
      );
      match self.placer {
        Placer::Local => {
          let (overlaid, _) = self.device.dax_overlays().unwrap();
          assert_eq!(
            self.answer(FIRST_REQUEST_QUEUE, head),
            0,
            "SETUPMAPPING answered once the device mapped the window"
          );
          assert_eq!(
            self.device.dax_overlays().unwrap().0 - overlaid,
            self.layout.pieces(foffset, len).count() as u64,
            "the device overlaid each piece of the window"
          );
          GuestMap::Local {
            moffset,
            len: usize::try_from(len).unwrap(),
          }
        }
        #[cfg(target_os = "linux")]
        Placer::Message => {
          let pieces = self.layout.pieces(foffset, len).count();
          let map = self.until_mapped(head, pieces);
          assert_eq!(
            self.ack(head, 0, pieces),
            0,
            "SETUPMAPPING answered once the front end mapped it"
          );
          GuestMap::Message(map)
        }
      }
    }

    /// Asks again, read-only, for the window at `moffset` over `node`'s range `[foffset, foffset + len)`, live as asked:
    /// answered success with no map, unmap or message, and counted kept.
    fn repeated(&mut self, node: u64, (foffset, len): (u64, u64), moffset: u64) {
      let kept = self.device.dax_backed_counts().maps_kept;
      let head = self.submit(
        FIRST_REQUEST_QUEUE,
        Opcode::SetupMapping,
        node,
        &dax_setup_body(foffset, len, false, moffset),
      );
      assert_eq!(
        self.answer(FIRST_REQUEST_QUEUE, head),
        0,
        "a window live as asked is answered with no map"
      );
      assert_eq!(self.device.dax_backed_counts().maps_kept, kept + 1);
    }

    /// Passes until the held SETUPMAPPING `head` sends its `pieces` maps, with no reply published before: the window,
    /// its pieces mapped as the front end maps them. An unmap taken first is acknowledged and counted.
    #[cfg(target_os = "linux")]
    fn until_mapped(&mut self, head: u16, pieces: usize) -> FrontMap {
      use slates_bridge_virtiofs::vhost_user::backend_request;
      let mut received = Vec::new();
      for _ in 0..TURNS {
        self.pass();
        let front = self.front.as_ref().unwrap();
        while let Some((code, body, descriptor)) = front_take(front) {
          if code == backend_request::SHMEM_UNMAP {
            assert!(
              received.is_empty() && descriptor.is_none(),
              "an unmap, carrying no descriptor, comes before the window's maps"
            );
            front_send_ack(front, backend_request::SHMEM_UNMAP, 0);
            self.front_unmaps += 1;
            continue;
          }
          assert_eq!(
            code,
            backend_request::SHMEM_MAP,
            "the back-end request is SHMEM_MAP"
          );
          received.push((
            body,
            descriptor.expect("SHMEM_MAP carries the backing's descriptor"),
          ));
        }
        if !received.is_empty() {
          assert_eq!(received.len(), pieces, "the window's pieces, sent together");
          return front_map(received);
        }
        assert!(
          self.reply_now(FIRST_REQUEST_QUEUE, head).is_none(),
          "no answer before the front end has mapped the window"
        );
      }
      panic!("the map was not sent within {TURNS} passes");
    }

    /// The front end acknowledges each of the window's `pieces` maps with `status`, and the device drains the acks:
    /// the SETUPMAPPING's errno, answered on the last.
    #[cfg(target_os = "linux")]
    fn ack(&mut self, head: u16, status: u64, pieces: usize) -> i32 {
      use slates_bridge_virtiofs::vhost_user::backend_request;
      for _ in 0..pieces {
        front_send_ack(
          self.front.as_ref().unwrap(),
          backend_request::SHMEM_MAP,
          status,
        );
      }
      self
        .device
        .complete_dax_awaiting(&mut self.driver.memory, &mut Unlimited)
        .unwrap();
      self
        .reply_now(FIRST_REQUEST_QUEUE, head)
        .expect("the SETUPMAPPING is answered on the last ack")
    }

    /// Removes the window at `moffset` as a guest does, through the whole exchange, `live` naming the file range of the
    /// window the device holds there, if any: the device answers the REMOVEMAPPING; the message placer sends one unmap
    /// a piece of a live window, carrying no descriptor, which the front end acknowledges, and nothing for a range it
    /// holds no window over.
    fn unmapped(&mut self, node: u64, moffset: u64, len: u64, live: Option<(u64, u64)>) {
      // `fuse_removemapping_in` (the count) then one `fuse_removemapping_one` (`include/uapi/linux/fuse.h`), unpadded.
      let mut remove = Vec::new();
      remove.extend_from_slice(&1u32.to_le_bytes());
      remove.extend_from_slice(&moffset.to_le_bytes());
      remove.extend_from_slice(&len.to_le_bytes());
      let restored = self.device.dax_overlays().map(|(_, unmapped)| unmapped);
      let head = self.submit(FIRST_REQUEST_QUEUE, Opcode::RemoveMapping, node, &remove);
      let mut answered = None;
      for _ in 0..TURNS {
        self.pass();
        answered = self.reply_now(FIRST_REQUEST_QUEUE, head);
        if answered.is_some() {
          break;
        }
      }
      assert_eq!(answered, Some(0), "REMOVEMAPPING answered");
      if let (Some(before), Some((_, after))) = (restored, self.device.dax_overlays()) {
        assert_eq!(
          after - before,
          live.map_or(0, |(foffset, len)| self.layout.pieces(foffset, len).count()
            as u64),
          "the device restored the reservation under each piece of the window it held there, and nothing else"
        );
      }
      #[cfg(target_os = "linux")]
      if self.placer == Placer::Message {
        use slates_bridge_virtiofs::vhost_user::backend_request;
        let front = self.front.as_ref().unwrap();
        if let Some((foffset, len)) = live {
          for _ in self.layout.pieces(foffset, len) {
            let (code, _, descriptor) = front_take(front).expect("an unmap sent for each piece");
            assert_eq!(code, backend_request::SHMEM_UNMAP);
            assert!(descriptor.is_none(), "an unmap carries no descriptor");
            front_send_ack(front, backend_request::SHMEM_UNMAP, 0);
            self.front_unmaps += 1;
          }
        }
        assert!(front_take(front).is_none(), "one unmap a piece, no more");
        self
          .device
          .complete_dax_awaiting(&mut self.driver.memory, &mut Unlimited)
          .unwrap();
      }
    }

    /// Forgets `node` as a guest does once it holds no window of it, and passes until the device has let its backing go.
    fn forgotten(&mut self, node: u64) {
      let _ = self.submit(HIPRIO_QUEUE, Opcode::Forget, node, &1u64.to_le_bytes());
      for _ in 0..TURNS {
        self.pass();
        if !self.backs(node) {
          return;
        }
      }
      panic!("the backing was not let go within {TURNS} passes");
    }

    /// Whether the store holds a backing of `node`.
    fn backs(&self, node: u64) -> bool {
      self
        .store
        .shards_backings
        .backs(self.vol.prefix(), InodeNo(node))
    }

    /// Holds that nothing of the guest's windows is left: no window recorded, no backing held or counted against the
    /// budget, no page placed.
    fn assert_nothing_held(&self, what: &str) {
      assert_eq!(
        (
          self.device.dax_window_count(),
          self.store.shards_backings.vmms_held(),
          self.store.shards_backings.placed_pages()
        ),
        (0, (0, 0), 0),
        "{what}: nothing is mapped, held, counted or placed"
      );
    }

    /// `len` bytes of `map` from `at`, as its guest reads them.
    fn read(&self, map: &GuestMap, at: usize, len: usize) -> Vec<u8> {
      match map {
        GuestMap::Local { moffset, .. } => {
          assert!(at + len <= map.len(), "a read inside the window");
          self
            .device
            .dax_window_bytes(
              moffset + u64::try_from(at).unwrap(),
              u64::try_from(len).unwrap(),
            )
            .expect("the window lies in the device's cache region")
        }
        #[cfg(target_os = "linux")]
        GuestMap::Message(front) => front.read(at, len),
      }
    }

    /// Stores `bytes` at `at` of `map`, as its guest stores through it.
    fn store(&mut self, map: &mut GuestMap, at: usize, bytes: &[u8]) {
      match map {
        GuestMap::Local { moffset, len } => {
          assert!(at + bytes.len() <= *len, "a store inside the window");
          assert!(
            self
              .device
              .dax_window_store(*moffset + u64::try_from(at).unwrap(), bytes),
            "the window lies in the device's cache region"
          );
        }
        #[cfg(target_os = "linux")]
        GuestMap::Message(front) => front.store(at, bytes),
      }
    }

    /// The descriptor of `node`'s backing's object as the store holds it: what every placer maps from.
    fn backing_object(&self, node: u64) -> std::os::fd::BorrowedFd<'_> {
      self
        .store
        .shards_backings
        .of_file(self.vol.prefix(), InodeNo(node))
        .next()
        .expect("the node is backed")
        .object()
        .descriptor()
    }

    /// `len` bytes of `node`'s backing's object from `offset` (a whole page), read through a mapping of the object the
    /// test makes for the read: where no piece maps, such as a page's guards.
    fn object_bytes(&self, node: u64, offset: u64, len: usize) -> Vec<u8> {
      let object = self.backing_object(node);
      // SAFETY: a fresh read-only shared mapping at an address the kernel chooses, of `len` bytes of a live
      // descriptor, from a page-aligned offset inside the object; copied out and unmapped below before anything else
      // runs.
      let at = unsafe {
        rustix::mm::mmap(
          std::ptr::null_mut(),
          len,
          rustix::mm::ProtFlags::READ,
          rustix::mm::MapFlags::SHARED,
          object,
          offset,
        )
      }
      .unwrap();
      // SAFETY: `at` is the mapping just made, `len` bytes long.
      let bytes = unsafe { std::slice::from_raw_parts(at.cast::<u8>(), len) }.to_vec();
      // SAFETY: unmapping exactly the mapping made above.
      unsafe { rustix::mm::munmap(at, len) }.unwrap();
      bytes
    }

    /// The bytes `node`'s backing's object holds in memory: its allocated blocks, as `fstat` counts them in 512-byte
    /// units (`st_blocks`, POSIX).
    fn resident(&self, node: u64) -> u64 {
      /// Format: the unit of `st_blocks` (POSIX `<sys/stat.h>`).
      const BLOCK_BYTES: u64 = 512;
      u64::try_from(
        rustix::fs::fstat(self.backing_object(node))
          .unwrap()
          .st_blocks,
      )
      .unwrap()
        * BLOCK_BYTES
    }

    /// The volume's bytes of `node` from `at`, `len` of them at most.
    fn volume_bytes(&mut self, node: u64, at: u64, len: usize) -> Vec<u8> {
      let mut bridge = backed_bridge(
        &mut self.vol,
        &mut self.store,
        &mut self.handles,
        &mut self.host,
      );
      let mut read = Vec::new();
      bridge
        .read(
          ObjectId::new(node, 0),
          &self.cx,
          at,
          u32::try_from(len).unwrap(),
          &mut read,
        )
        .unwrap();
      read
    }
  }

  /// §4.6 (goal G-vhost, step 1; `docs/bugs/2026-10-24-slates-own-dax-window-shows-stale-bytes-then-zeros-after-another-
  /// paths-write.md`), for both placers. A window maps the file's backing, which the write boundary keeps equal to the
  /// file, so every other path's change shows in the guest's map with no message and no guest request. Do: map a page's
  /// file through a 2 MiB window; then, through the volume, overwrite bytes in place, write past the end inside the
  /// window, and cut the file; then serve a GETATTR. Expect: the map is the backing's whole asked range (the message
  /// placer's pieces as the layout holds it, writable since the attachment may write); after each change the map shows
  /// the volume's bytes (the growth's hole and the cut's removal reading zero); and nothing is unmapped or sent after the
  /// map, while the backing took every change (non-vacuity).
  #[test]
  fn a_window_over_a_backing_shows_every_other_paths_change_with_no_message() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "coherent");
      let page = f.page;
      let range = slates_mem::DAX_RANGE_BYTES;
      let node = f.file("f", &vec![0x41; page]);
      let map = f.mapped(node, (0, range), false, 0);
      #[cfg(target_os = "linux")]
      if let GuestMap::Message(front) = &map {
        let expected: Vec<FrontPiece> = f
          .layout
          .pieces(0, range)
          .map(|piece| FrontPiece {
            fd_offset: piece.object_offset,
            shm_offset: piece.file_offset,
            len: piece.len,
          })
          .collect();
        assert_eq!(
          (front.pieces.clone(), front.writable),
          (expected, true),
          "the whole asked range of the file's backing, piece by piece as the layout holds it, writable at the front end"
        );
      }
      assert_eq!(map.len() as u64, range, "{placer:?}: the whole asked range");
      assert_eq!(
        f.read(&map, 0, page),
        vec![0x41; page],
        "{placer:?}: the window shows the file"
      );
      assert_eq!(f.device.dax_window_count(), 1, "the window is recorded");
      other_paths_change_the_file_and_the_map_shows_each(&mut f, node, &map);
      let getattr = f.submit(FIRST_REQUEST_QUEUE, Opcode::GetAttr, node, &[0u8; 16]);
      assert_eq!(f.answer(FIRST_REQUEST_QUEUE, getattr), 0, "GETATTR served");
      f.assert_nothing_sent("no SHMEM_UNMAP, nor any message: the window never left the guest");
      let counts = f.device.dax_backed_counts();
      assert_eq!(
        (
          counts.maps_sent,
          counts.unmaps_sent,
          f.device.dax_window_count()
        ),
        (1, 0, 1),
        "{placer:?}: one window mapped, none unmapped"
      );
      assert!(
        f.store.shards_backings.written() > 0,
        "non-vacuity: the changes went through the backing"
      );
    }
  }

  /// The other paths' changes of [`a_window_over_a_backing_shows_every_other_paths_change_with_no_message`]: an
  /// in-place write, a write past the old end inside the window, and a cut, each shown in the map at once.
  fn other_paths_change_the_file_and_the_map_shows_each(
    f: &mut Fixture,
    node: u64,
    map: &GuestMap,
  ) {
    let page = f.page;
    let no = InodeNo(node);
    f.vol.write(&mut f.store, no, 16, b"another path").unwrap();
    assert_eq!(
      f.read(map, 16, 12),
      b"another path".to_vec(),
      "an in-place write shows in the map with no guest request"
    );
    let grown = 3 * page;
    f.vol
      .write(&mut f.store, no, u64::try_from(grown).unwrap(), b"grown")
      .unwrap();
    assert_eq!(
      f.read(map, grown, 5),
      b"grown".to_vec(),
      "a write past the old end shows: the block moved in the volume, the backing did not"
    );
    assert_eq!(
      f.read(map, page, page),
      vec![0; page],
      "the hole the growth opened reads zero"
    );
    f.vol.truncate(&mut f.store, no, 8).unwrap();
    assert_eq!(f.read(map, 0, 8), vec![0x41; 8], "what the cut kept stays");
    assert_eq!(
      f.read(map, 8, page - 8),
      vec![0; page - 8],
      "what the cut removed reads zero"
    );
    assert_eq!(f.read(map, grown, 5), vec![0; 5]);
  }

  /// §4.6 (G-vhost), a guest's stores reach the volume at its sync points (A-163's ingest), for both placers. Do: map a
  /// file writable; store through the map; read the volume; send an FSYNC. Expect: the store is not in the volume before
  /// the FSYNC; the FSYNC is held, unanswered, while the store is written back; it is answered success once the store is
  /// in the volume, published first (an owner's barrier ran); the device counted the held request and the run it wrote.
  #[test]
  fn a_guests_store_through_a_writable_window_reaches_the_volume_before_its_fsync_is_answered() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "ingest");
      let page = f.page;
      let node = f.file("f", &vec![0x41; page]);
      let mut map = f.mapped(node, (0, slates_mem::DAX_RANGE_BYTES), true, 0);
      f.store(&mut map, 100, b"the guest's store");
      assert_eq!(
        f.volume_bytes(node, 100, 17),
        vec![0x41; 17],
        "{placer:?}: a store through the map is the guest's until it syncs"
      );
      let barriers = f.barriers;
      let fsync = f.submit(FIRST_REQUEST_QUEUE, Opcode::FSync, node, &[0u8; 16]);
      f.pass();
      assert!(
        f.reply_now(FIRST_REQUEST_QUEUE, fsync).is_none(),
        "the FSYNC is held while the store is written back"
      );
      assert_eq!(f.answer(FIRST_REQUEST_QUEUE, fsync), 0, "FSYNC answered");
      assert_eq!(
        f.volume_bytes(node, 100, 17),
        b"the guest's store".to_vec(),
        "{placer:?}: the store is in the volume by the FSYNC's answer"
      );
      assert!(
        f.barriers > barriers,
        "the write-back was published before the reply"
      );
      let counts = f.device.dax_backed_counts();
      assert_eq!(counts.ingests_held, 1);
      assert!(
        counts.session.ingested >= 1,
        "non-vacuity: a run was written"
      );
    }
  }

  /// §4.6 (G-vhost), a writable upgrade of a live window, for both placers. A Linux guest upgrades a range from
  /// read-only to writable with a second SETUPMAPPING over the same window (`fuse_upgrade_dax_mapping`), and the
  /// vhost-user protocol forbids a map over a live one. Do: map a file read-only; ask the same window writable; store
  /// through the map and FSYNC. Expect: the upgrade is answered success with no second map and no unmap (the placer maps
  /// the window writable already); the store reaches the volume, since the upgrade marked the range writable.
  #[test]
  fn a_writable_upgrade_of_a_live_window_is_answered_with_no_message() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "upgrade");
      let page = f.page;
      let range = slates_mem::DAX_RANGE_BYTES;
      let node = f.file("f", &vec![0x41; page]);
      let mut map = f.mapped(node, (0, range), false, 0);
      let upgrade = f.submit(
        FIRST_REQUEST_QUEUE,
        Opcode::SetupMapping,
        node,
        &dax_setup_body(0, range, true, 0),
      );
      assert_eq!(
        f.answer(FIRST_REQUEST_QUEUE, upgrade),
        0,
        "the upgrade is answered"
      );
      let counts = f.device.dax_backed_counts();
      assert_eq!(
        (counts.maps_sent, counts.maps_kept, counts.unmaps_sent),
        (1, 1, 0),
        "{placer:?}: one map, the upgrade kept, nothing unmapped"
      );
      assert_eq!(f.device.dax_window_count(), 1);
      f.store(&mut map, 0, b"upgraded");
      let fsync = f.submit(FIRST_REQUEST_QUEUE, Opcode::FSync, node, &[0u8; 16]);
      assert_eq!(f.answer(FIRST_REQUEST_QUEUE, fsync), 0);
      assert_eq!(
        f.volume_bytes(node, 0, 8),
        b"upgraded".to_vec(),
        "{placer:?}"
      );
    }
  }

  /// §4.6 (G-vhost), a write-back the volume refuses fails the request that asked for it, for both placers (A-163's
  /// rule; `docs/bugs/2026-10-24-a-refused-dax-write-back-answered-its-fsync-success.md`). Do: map a sparse file twice
  /// the arena's size writable, store into every page of it, and FSYNC; then write the same bytes through the volume.
  /// Expect: the FSYNC is answered exactly the errno the FUSE edge gives the volume's refusal of that write (the arena's
  /// exhaustion), never success; the refusal is counted.
  #[test]
  fn a_refused_write_back_fails_the_fsync_that_asked_for_it() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "refused");
      let page = f.page;
      let node = f.file("f", &[]);
      let no = InodeNo(node);
      let len = PAST_THE_ARENA_PAGES * page;
      f.vol
        .truncate(&mut f.store, no, u64::try_from(len).unwrap())
        .unwrap();
      let window = u64::try_from(len)
        .unwrap()
        .next_multiple_of(slates_mem::DAX_RANGE_BYTES);
      let mut map = f.mapped(node, (0, window), true, 0);
      f.store(&mut map, 0, &vec![0x5A; len]);
      let fsync = f.submit(FIRST_REQUEST_QUEUE, Opcode::FSync, node, &[0u8; 16]);
      let answered = f.answer(FIRST_REQUEST_QUEUE, fsync);
      let refusal = f
        .vol
        .write(&mut f.store, no, 0, &vec![0x5A; len])
        .expect_err("the arena still cannot hold the write-back");
      let named = slates_bridge_fuse::bridge::errno(refusal.clone());
      assert_ne!(named, 0);
      assert_eq!(
        answered, -named,
        "{placer:?}: the FSYNC carries the write-back's refusal as the FUSE edge names it ({refusal:?})"
      );
      assert!(f.device.dax_backed_counts().session.ingest_refused >= 1);
    }
  }

  /// §4.6 (G-vhost), the device's end, for both placers. Do: map a file writable, store through the map, then run the
  /// end's ingest as the serve loop runs it before the terminal step. Expect: the ingest wrote, and the store is in the
  /// volume.
  #[test]
  fn a_guests_unsynced_store_reaches_the_volume_when_the_device_ends() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "end");
      let page = f.page;
      let node = f.file("f", &vec![0x41; page]);
      let mut map = f.mapped(node, (0, slates_mem::DAX_RANGE_BYTES), true, 0);
      f.store(&mut map, 8, b"never synced");
      let slice = SLICE_PAGES * u64::try_from(page).unwrap();
      let mut bridge = backed_bridge(&mut f.vol, &mut f.store, &mut f.handles, &mut f.host);
      assert!(
        f.device.begin_end_ingest(&mut bridge),
        "an ingest is owed at the end"
      );
      let wrote = (0..TURNS)
        .find_map(|_| f.device.end_ingest_step(&mut bridge, &f.cx, slice))
        .expect("the end's ingest completes");
      assert!(wrote, "{placer:?}: the end's ingest wrote");
      drop(bridge);
      assert_eq!(
        f.volume_bytes(node, 8, 12),
        b"never synced".to_vec(),
        "{placer:?}"
      );
    }
  }

  /// §4.6 (G-vhost), what a window hands the VMM: one file's backing and nothing else, by construction, with no host
  /// device needed (`docs/bugs/2026-10-18-the-message-based-dax-map-hands-the-vmm-the-whole-arena.md`, closed by A-138
  /// with a udmabuf, now by the backing), for both placers. Do: make two files of different bytes; map the first through
  /// a 2 MiB window and read the whole map. Expect: the first file's bytes, then zeros, and not one byte of the second.
  #[test]
  fn a_window_hands_the_vmm_one_files_bytes_and_nothing_else() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "confined");
      let page = f.page;
      let first = f.file("first", &vec![0x41; page]);
      let _second = f.file("second", &vec![0x42; page]);
      let map = f.mapped(first, (0, slates_mem::DAX_RANGE_BYTES), false, 0);
      let whole = f.read(&map, 0, map.len());
      assert_eq!(
        &whole[..page],
        vec![0x41; page].as_slice(),
        "{placer:?}: the file's bytes"
      );
      assert!(
        whole[page..].iter().all(|byte| *byte == 0),
        "past the file the map reads zero"
      );
      assert!(
        !whole.contains(&0x42),
        "{placer:?}: nothing of another file is reachable through the map"
      );
    }
  }

  /// §4.6 (G-vhost), hostile SETUPMAPPINGs, for both placers. Do: ask a file range off a page, a window range off a page,
  /// a window past the guest's, an empty range, a file range past what the backing's object holds, and the root
  /// directory. Expect: each refused (`EINVAL`, `EISDIR` for the directory) with no window recorded, nothing sent, and no
  /// backing made.
  #[test]
  fn hostile_setupmappings_are_refused_with_no_window_and_no_message() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "hostile");
      let page = u64::try_from(f.page).unwrap();
      let range = slates_mem::DAX_RANGE_BYTES;
      let node = f.file("f", &vec![0x41; f.page]);
      let window = WINDOW_RANGES * range;
      let held = f.layout.file_bytes(slates_mem::DAX_OBJECT_BYTES);
      for (what, nodeid, body, want) in [
        (
          "a file range off a page",
          node,
          dax_setup_body(1, range, false, 0),
          EINVAL,
        ),
        (
          "a window range off a page",
          node,
          dax_setup_body(0, range, false, 1),
          EINVAL,
        ),
        (
          "a window past the guest's",
          node,
          dax_setup_body(0, range, false, window),
          EINVAL,
        ),
        (
          "an empty range",
          node,
          dax_setup_body(0, 0, false, 0),
          EINVAL,
        ),
        (
          "a file range past what the backing's object holds",
          node,
          dax_setup_body(held, range, false, 0),
          EINVAL,
        ),
        (
          "the root directory",
          ROOT,
          dax_setup_body(0, page, false, 0),
          EISDIR,
        ),
      ] {
        let head = f.submit(FIRST_REQUEST_QUEUE, Opcode::SetupMapping, nodeid, &body);
        assert_eq!(
          f.answer(FIRST_REQUEST_QUEUE, head),
          -want,
          "{placer:?}: {what}"
        );
      }
      f.assert_nothing_sent("nothing sent to the front end");
      f.assert_nothing_held("hostile SETUPMAPPINGs");
    }
  }

  /// §4.6 (A-195), a hostile front end, for both placers: what each ask is answered and that nothing is held after.
  /// Do, over two files: map a window over the first; map the second over the same window slot (an overlapping
  /// SETUPMAPPING) and then over half of it shifted by one granule; map a range past the second file's end inside its
  /// object; remove a window range never mapped; ask the first file writable through a read-only attachment; then remove
  /// every window and forget both files. Expect: each overlapping SETUPMAPPING answered, the windows under it unmapped
  /// first (one unmap a piece, for the message placer) and the window then showing the second file; the map past the end
  /// answered and reading zero; the never-mapped removal answered with no unmap and the windows unchanged; the writable
  /// ask refused `EPERM` with no window and no backing made; and at the end nothing mapped, held, counted or placed.
  #[test]
  fn a_hostile_front_ends_asks_are_answered_as_before_and_leave_nothing_held() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "frontend");
      let page = f.page;
      let granule = u64::try_from(page).unwrap();
      let range = slates_mem::DAX_RANGE_BYTES;
      let first = f.file("first", &vec![0x41; page]);
      let second = f.file("second", &vec![0x42; page]);
      let _over_first = f.mapped(first, (0, range), false, 0);
      let unmaps = f.device.dax_backed_counts().unmaps_sent;
      let over_second = f.mapped(second, (0, range), false, 0);
      assert_eq!(
        f.device.dax_backed_counts().unmaps_sent,
        unmaps + 1,
        "{placer:?}: the window under the overlapping SETUPMAPPING was unmapped first"
      );
      #[cfg(target_os = "linux")]
      if placer == Placer::Message {
        assert_eq!(
          f.front_unmaps,
          f.layout.pieces(0, range).count(),
          "one unmap a piece of the window under it reached the front end, before the new window's maps"
        );
      }
      assert_eq!(f.device.dax_window_count(), 1, "one window in the slot");
      assert_eq!(
        f.read(&over_second, 0, page),
        vec![0x42; page],
        "{placer:?}: the slot shows the second file"
      );
      let shifted = f.mapped(second, (0, range - granule), false, granule);
      assert_eq!(
        (f.device.dax_window_count(), f.read(&shifted, 0, page)),
        (1, vec![0x42; page]),
        "{placer:?}: a partly overlapping SETUPMAPPING replaced the window under it"
      );
      let past = f.mapped(second, (range, range), false, 2 * range);
      assert!(
        f.read(&past, 0, page).iter().all(|byte| *byte == 0),
        "{placer:?}: a window past the file's end reads zero"
      );
      let before = (
        f.device.dax_window_count(),
        f.device.dax_backed_counts().unmaps_sent,
      );
      f.unmapped(first, 3 * range, range, None);
      assert_eq!(
        (
          f.device.dax_window_count(),
          f.device.dax_backed_counts().unmaps_sent
        ),
        before,
        "{placer:?}: a removal of a range never mapped unmaps nothing"
      );
      f.unmapped(second, granule, range - granule, Some((0, range - granule)));
      f.unmapped(second, 2 * range, range, Some((range, range)));
      f.forgotten(first);
      f.forgotten(second);
      f.assert_nothing_held("the windows removed and the files forgotten");
      upgrade_without_write_rights_is_refused(placer);
    }
  }

  /// The read-only half of [`a_hostile_front_ends_asks_are_answered_as_before_and_leave_nothing_held`]: through a
  /// read-only attachment, a writable SETUPMAPPING is refused `EPERM` with no window, nothing sent and no backing made.
  fn upgrade_without_write_rights_is_refused(placer: Placer) {
    let mut f = read_only_fixture(placer, "upgrade-ro");
    let node = f.file("f", &vec![0x41; f.page]);
    let head = f.submit(
      FIRST_REQUEST_QUEUE,
      Opcode::SetupMapping,
      node,
      &dax_setup_body(0, slates_mem::DAX_RANGE_BYTES, true, 0),
    );
    assert_eq!(
      f.answer(FIRST_REQUEST_QUEUE, head),
      -EPERM,
      "{placer:?}: a writable ask through a read-only attachment is refused"
    );
    f.assert_nothing_sent("nothing sent for a refused ask");
    f.assert_nothing_held("a refused writable ask");
  }

  /// A fixture over a scratch volume attached read-only.
  fn read_only_fixture(placer: Placer, tag: &str) -> Fixture {
    let mut attachments = slates_bridge_core::Attachments::new();
    let attachment = attachments
      .attach(
        vid(),
        slates_bridge_core::View::Current,
        slates_db::catalog::Principal::Uid { uid: 0 },
        slates_bridge_core::Rights {
          read: true,
          write: false,
        },
      )
      .unwrap();
    let page = rustix::param::page_size().max(1);
    let mut store = backed_store(page);
    let vol = volume(&mut store);
    Fixture::over(
      placer,
      tag,
      (store, vol, None),
      attachments.context(attachment).unwrap(),
    )
  }

  /// §4.6 (G-vhost), a forgotten node's backing is let go, for both placers. Do: map a file, remove the mapping as a
  /// guest does before it forgets the inode, then FORGET it. Expect: the REMOVEMAPPING unmaps each piece of the window
  /// (two under the device's guarded layout, A-194) and leaves no window; once the FORGET is served the backing is gone
  /// from the store and counted released.
  #[test]
  fn a_forget_lets_the_nodes_backing_go_after_its_mapping_is_removed() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "forget");
      let page = f.page;
      let range = slates_mem::DAX_RANGE_BYTES;
      let node = f.file("f", &vec![0x41; page]);
      let _map = f.mapped(node, (0, range), false, 0);
      f.unmapped(node, 0, range, Some((0, range)));
      assert_eq!(f.device.dax_window_count(), 0, "{placer:?}");
      assert_eq!(f.device.dax_backed_counts().unmaps_sent, 1);
      assert!(
        f.backs(node),
        "the backing outlives its mapping until the node is forgotten"
      );
      let _forget = f.submit(HIPRIO_QUEUE, Opcode::Forget, node, &1u64.to_le_bytes());
      f.pass();
      assert!(!f.backs(node), "{placer:?}: the FORGET let the backing go");
      assert_eq!(f.device.dax_backed_counts().released, 1);
    }
  }

  /// §4.6 terminal teardown at the guest's unmount, for both placers. Do: map two files' windows, one writable with a
  /// store through it; send FUSE_DESTROY. Expect: the unmount is held while the store is written back, then answered;
  /// the store is in the volume; every window is unmapped (one unmap a piece, for the message placer) and every backing
  /// let go and counted, so nothing is mapped, held, counted or placed.
  #[test]
  fn fuse_destroy_unmaps_every_window_and_lets_every_backing_go() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "destroy");
      let page = f.page;
      let range = slates_mem::DAX_RANGE_BYTES;
      let first = f.file("first", &vec![0x41; page]);
      let second = f.file("second", &vec![0x42; page]);
      let _read_only = f.mapped(first, (0, range), false, 0);
      let mut writable = f.mapped(second, (0, range), true, range);
      f.store(&mut writable, 0, b"before the unmount");
      let destroy = f.submit(FIRST_REQUEST_QUEUE, Opcode::Destroy, 0, &[]);
      let mut answered = None;
      for _ in 0..TURNS {
        f.pass();
        answered = f.reply_now(FIRST_REQUEST_QUEUE, destroy);
        if answered.is_some() {
          break;
        }
      }
      assert_eq!(answered, Some(0), "{placer:?}: DESTROY answered");
      assert_eq!(
        f.volume_bytes(second, 0, 18),
        b"before the unmount".to_vec(),
        "{placer:?}: the guest's store reached the volume before the unmount let its backing go"
      );
      assert_unmapped_every_piece(&f, &[(0, range), (0, range)]);
      let counts = f.device.dax_backed_counts();
      assert_eq!(
        (counts.unmaps_sent, counts.released),
        (2, 2),
        "{placer:?}: both windows unmapped, both backings let go"
      );
      f.assert_nothing_held("the guest unmounted");
    }
  }

  /// §4.6 terminal teardown of a guest that never unmounted (a VMM that disconnected or crashed), for both placers. Do:
  /// map a window; run the terminal step's release as `AdmittedDevice::reclaim` does, with the attachment's bridge.
  /// Expect: the window unmapped (one unmap a piece, for the message placer) and its backing let go, so nothing is
  /// mapped, held, counted or placed; a second release finds nothing to do.
  #[test]
  fn a_disconnect_unmaps_every_window_and_lets_every_backing_go() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "disconnect");
      let page = f.page;
      let range = slates_mem::DAX_RANGE_BYTES;
      let node = f.file("f", &vec![0x41; page]);
      let _map = f.mapped(node, (0, range), false, 0);
      for _ in 0..2 {
        let mut bridge = backed_bridge(&mut f.vol, &mut f.store, &mut f.handles, &mut f.host);
        f.device.release_dax(&mut bridge, &f.cx);
        drop(bridge);
        f.device.drain_dax_terminal();
      }
      assert_unmapped_every_piece(&f, &[(0, range)]);
      assert_eq!(
        f.device.dax_backed_counts().unmaps_sent,
        1,
        "{placer:?}: the window unmapped once"
      );
      f.assert_nothing_held("the guest disconnected");
    }
  }

  /// Holds that the stand-in front end was sent one unmap for each piece of each of `windows` (file ranges), and nothing
  /// else (the message placer's; the fd-sharing placer unmaps its own window).
  fn assert_unmapped_every_piece(f: &Fixture, windows: &[(u64, u64)]) {
    #[cfg(target_os = "linux")]
    if f.placer == Placer::Message {
      use slates_bridge_virtiofs::vhost_user::backend_request;
      let front = f.front.as_ref().unwrap();
      for (foffset, len) in windows {
        for _ in f.layout.pieces(*foffset, *len) {
          let (code, _, descriptor) = front_take(front).expect("an unmap sent for each piece");
          assert_eq!(code, backend_request::SHMEM_UNMAP);
          assert!(descriptor.is_none(), "an unmap carries no descriptor");
        }
      }
      assert!(front_take(front).is_none(), "one unmap a piece, no more");
    }
    let _ = (f, windows);
  }

  /// Whether this host can place a DAX range in one huge page: a PMD page of exactly a range (§4.6, the backed route's
  /// L1; A-193). Where it cannot, a placement test skips loudly.
  fn ranges_can_be_huge(test: &str) -> bool {
    let huge = slates_machine::facts::huge_page_bytes();
    let can = huge == Some(slates_mem::DAX_RANGE_BYTES);
    if !can {
      eprintln!(
        "SKIP {test}: this host's PMD page is {huge:?}, not a 2 MiB DAX range, so no range is placed"
      );
    }
    can
  }

  /// Whether a page of file `node` is among `placements`.
  fn node_has_placed(placements: &[slates_vfs::shards_backing::PagePlacement], node: u64) -> bool {
    placements.iter().any(|placement| placement.inode.0 == node)
  }

  /// Holds the placements a fixture's device made to §4.6's backed L1 (A-193, A-194): every range `maps` maps placed or
  /// counted refused, at least one placed; the device's backings laid out guarded; each placed page one PMD page in
  /// slates' view; and, for the message placer, every piece the front end was sent inside one page's interior, none
  /// reaching a guard (the fd-sharing placer's pieces are its own overlays, held to the interiors by each window reading
  /// its file while the guards hold nothing). The placements it read; `None`, said loudly as a skip of `test`, where the
  /// host declined every placement, which proves nothing either way. One that placed none and refused none never asked,
  /// and fails.
  fn assert_maps_placed(
    f: &mut Fixture,
    maps: &[(u64, u64, GuestMap)],
    test: &str,
  ) -> Option<Vec<slates_vfs::shards_backing::PagePlacement>> {
    let range = slates_mem::DAX_RANGE_BYTES;
    let counts = f.device.dax_backed_counts().session;
    let placements = f.store.shards_backings.placements();
    assert_eq!(
      counts.placed + counts.placement_refused,
      maps.len() as u64,
      "every range mapped is placed or counted refused"
    );
    if counts.placed == 0 && counts.placement_refused > 0 {
      eprintln!(
        "SKIP {test} ({:?}): the kernel declined all {} placements on this host now, so no huge page is here to read",
        f.placer, counts.placement_refused
      );
      return None;
    }
    assert!(
      counts.placed > 0,
      "no range was placed or refused: placement never ran"
    );
    let slates_mem::DaxLayout::Guarded { guard } = f.layout else {
      panic!("a host that places ranges lays the device's backings out guarded");
    };
    for placement in &placements {
      assert_eq!(
        placement.pmd_bytes,
        Some(range),
        "slates' view of page {} of inode {} is one PMD page",
        placement.page,
        placement.inode.0
      );
    }
    #[cfg(target_os = "linux")]
    for (node, index, map) in maps {
      if let GuestMap::Message(front) = map {
        for piece in &front.pieces {
          let within = piece.fd_offset % range;
          assert!(
            within >= guard && within + piece.len <= range - guard,
            "range {index} of node {node}: {piece:?} reaches a guard"
          );
        }
      }
    }
    let _ = guard;
    Some(placements)
  }

  /// Holds that the guard slots of every placed page of file `node`, read through its backing's object, hold nothing: no
  /// byte of the file and no store of the guest's reaches them (§4.6, the backed route's L1; A-194).
  fn assert_guards_hold_nothing(
    f: &Fixture,
    placements: &[slates_vfs::shards_backing::PagePlacement],
    node: u64,
  ) {
    let range = slates_mem::DAX_RANGE_BYTES;
    for placement in placements
      .iter()
      .filter(|placement| placement.inode.0 == node)
    {
      let guard = placement.layout.guard();
      let page_start = placement.page * range;
      for start in [page_start, page_start + range - guard] {
        assert!(
          f.object_bytes(node, start, usize::try_from(guard).unwrap())
            .iter()
            .all(|byte| *byte == 0),
          "a guard of page {} of node {node} holds a byte",
          placement.page
        );
      }
    }
  }

  /// Holds what the placements cost against the fill budget (§4.6, the backed route's L1; A-193, A-194): each file's
  /// object holding no more than the backings count, and exactly its placed pages where no placement was refused; the
  /// backings counting within the budget. Prints each measurement.
  fn assert_placement_cost(
    f: &Fixture,
    nodes: &[u64],
    placements: &[slates_vfs::shards_backing::PagePlacement],
  ) {
    let range = slates_mem::DAX_RANGE_BYTES;
    let refused = f.device.dax_backed_counts().session.placement_refused;
    let charged = f.store.shards_backings.charged();
    let budget = f.store.shards_backings.budget();
    for node in nodes {
      let resident = f.resident(*node);
      let placed_here = placements
        .iter()
        .filter(|placement| placement.inode.0 == *node)
        .count() as u64;
      println!(
        "{:?} node {node}: {placed_here} pages placed, its object holds {resident} bytes; \
         the backings count {charged} of {budget}",
        f.placer
      );
      if refused == 0 {
        assert_eq!(
          resident,
          placed_here * range,
          "node {node}'s object holds its placed pages and nothing more"
        );
      }
      assert!(
        resident <= charged,
        "node {node}'s object holds no more than the backings count"
      );
    }
    assert!(charged <= budget, "the backings count within the budget");
  }

  /// The guest stores across the whole of `maps`' last window, past its file's end and into a page it shares with the
  /// window before it (`maps[1]`, the file's first page `page` bytes of `0x42`): no guard of any placed page takes a byte
  /// of it, and the window before keeps its own bytes.
  fn guest_stores_reach_no_guard(
    f: &mut Fixture,
    maps: &mut [(u64, u64, GuestMap)],
    placements: &[slates_vfs::shards_backing::PagePlacement],
    page: usize,
  ) {
    let whole = maps[2].2.len();
    f.store(&mut maps[2].2, 0, &vec![0xEE; whole]);
    for (node, _, _) in maps.iter() {
      assert_guards_hold_nothing(f, placements, *node);
    }
    assert_eq!(
      f.read(&maps[1].2, 0, page),
      vec![0x42; page],
      "the window before keeps the file's bytes"
    );
    assert!(
      f.read(&maps[1].2, page, maps[1].2.len() - page)
        .iter()
        .all(|byte| *byte == 0),
      "the window before keeps its zeros past the file's end"
    );
  }

  /// §4.6, the backed route's L1 (A-193, A-194), read-only and writable alike, for both placers: reading alone hammers
  /// (Kim et al., ISCA 2014), so every window a guest maps is an aggressor's reach. Do: map a read-only window over a
  /// four-page file's first range, and writable windows over another file's first range and over its second, wholly past
  /// its end; read where each page lies, the pieces the front end was sent, how many bytes each backing's object holds,
  /// and the counts the device reports; store across the whole of the last window; then store through the first writable
  /// window and FSYNC, and cut the read-only file to one page through the volume. Expect, where the host's PMD page is a
  /// range: every range mapped placed or counted refused, at least one placed; the backings guarded; each placed page one
  /// PMD page in slates' view; every message piece inside a page's interior; every guard slot holding nothing, even after
  /// the guest's stores across a whole window, and the window sharing that page keeping its bytes; each object holding
  /// exactly its placed pages where none was refused, within the budget; the counts under their status names; and the
  /// windows served as before (the file's bytes and zeros; the store in the volume; the cut reading zero), the cut keeping
  /// its pages one PMD page each, since a placed page is zeroed by stores, never a hole punch. Where the host declines
  /// every placement, it skips loudly.
  #[test]
  fn every_window_over_a_backing_is_one_huge_page_read_only_or_writable() {
    const TEST: &str = "every_window_over_a_backing_is_one_huge_page_read_only_or_writable";
    if !ranges_can_be_huge(TEST) {
      return;
    }
    for placer in placers() {
      placed_windows_hold_their_layer(placer, TEST);
    }
  }

  /// [`every_window_over_a_backing_is_one_huge_page_read_only_or_writable`] over one placer.
  fn placed_windows_hold_their_layer(placer: Placer, test: &str) {
    let range = slates_mem::DAX_RANGE_BYTES;
    let mut f = Fixture::scratch(placer, "placed");
    let page = f.page;
    let read_only = f.file("read-only", &vec![0x41; 4 * page]);
    let writable = f.file("writable", &vec![0x42; page]);
    let mut maps = [
      (read_only, 0, f.mapped(read_only, (0, range), false, 0)),
      (writable, 0, f.mapped(writable, (0, range), true, range)),
      (
        writable,
        1,
        f.mapped(writable, (range, range), true, 2 * range),
      ),
    ];
    let Some(placements) = assert_maps_placed(&mut f, &maps, test) else {
      return;
    };
    assert_placement_cost(&f, &[read_only, writable], &placements);
    let counts = f.device.dax_backed_counts();
    let named = counts.named();
    assert!(
      named.contains(&("virtiofs.dax_placed", counts.session.placed)),
      "the placements stand under their status name"
    );
    assert!(
      named.contains(&(
        "virtiofs.dax_placement_refused",
        counts.session.placement_refused
      )),
      "the refusals stand under their status name"
    );
    guest_stores_reach_no_guard(&mut f, &mut maps, &placements, page);
    let whole = f.read(&maps[0].2, 0, maps[0].2.len());
    assert!(
      whole[..4 * page].iter().all(|byte| *byte == 0x41)
        && whole[4 * page..].iter().all(|byte| *byte == 0),
      "{placer:?}: the read-only window holds the file's bytes and zeros, nothing of anyone else's"
    );
    f.store(&mut maps[1].2, 0, b"placed");
    let fsync = f.submit(FIRST_REQUEST_QUEUE, Opcode::FSync, writable, &[0u8; 16]);
    assert_eq!(f.answer(FIRST_REQUEST_QUEUE, fsync), 0);
    assert_eq!(f.volume_bytes(writable, 0, 6), b"placed".to_vec());
    let placed = node_has_placed(&placements, read_only);
    cut_keeps_the_placement(&mut f, read_only, &maps[0].2, placed);
  }

  /// Cuts four-page file `node`, which `map` maps from its start, to one page through the volume: the map reads the
  /// page and zeros after it, and where its pages were `placed`, they are still placed and still one PMD page each in
  /// slates' view, since a placed page is zeroed by stores, never a hole punch, which would split it.
  fn cut_keeps_the_placement(f: &mut Fixture, node: u64, map: &GuestMap, placed: bool) {
    let range = slates_mem::DAX_RANGE_BYTES;
    let page = f.page;
    f.vol
      .truncate(&mut f.store, InodeNo(node), u64::try_from(page).unwrap())
      .unwrap();
    assert_eq!(f.read(map, 0, page), vec![0x41; page]);
    assert!(
      f.read(map, page, 3 * page).iter().all(|byte| *byte == 0),
      "the cut reads zero"
    );
    if !placed {
      return;
    }
    let after = f.store.shards_backings.placements();
    assert!(
      node_has_placed(&after, node),
      "the cut kept its pages placed"
    );
    assert!(
      after
        .iter()
        .all(|placement| placement.pmd_bytes == Some(range)),
      "the cut kept slates' view of its pages one PMD page each: {after:?}"
    );
  }

  /// One cycle of a churning guest: map file `node`'s first range at window offset zero, ask it writable while it is
  /// live, and remove it, each answered as without placement (the map served the file's `page` of bytes, the upgrade
  /// answered with no map, the removal one unmap).
  fn churn_once(f: &mut Fixture, node: u64, page: usize) {
    let range = slates_mem::DAX_RANGE_BYTES;
    let map = f.mapped(node, (0, range), false, 0);
    assert_eq!(
      f.read(&map, 0, page),
      vec![0x41; page],
      "the window serves the file"
    );
    let upgrade = f.submit(
      FIRST_REQUEST_QUEUE,
      Opcode::SetupMapping,
      node,
      &dax_setup_body(0, range, true, 0),
    );
    assert_eq!(
      f.answer(FIRST_REQUEST_QUEUE, upgrade),
      0,
      "a writable ask over the live window is answered"
    );
    f.unmapped(node, 0, range, Some((0, range)));
  }

  /// §4.6, the backed route's L1 (A-193), a hostile guest, for both placers. Do: over a one-page file, map a window over
  /// its first range, ask it writable while it is live, and remove it, many times over; map a range far past the file's
  /// end; then remove that window and forget the node. Expect each ask served as without placement: every map made,
  /// every upgrade answered with no map, every removal one unmap, the window past the end reading zero; each range placed
  /// once however often it is mapped (placements and refusals together the two distinct ranges filled), each placed
  /// range one PMD page while its backing lives; and, the node forgotten, nothing placed, held or counted.
  #[test]
  fn a_hostile_guests_churn_over_placed_backings_is_served_as_before_and_leaks_nothing() {
    const TEST: &str =
      "a_hostile_guests_churn_over_placed_backings_is_served_as_before_and_leaks_nothing";
    /// Shape: the map, upgrade and removal cycles over one window: enough that a placement made per map, or one leaked
    /// per removal, would outnumber the two ranges filled many times over.
    const CYCLES: u64 = 16;
    if !ranges_can_be_huge(TEST) {
      return;
    }
    for placer in placers() {
      let range = slates_mem::DAX_RANGE_BYTES;
      let mut f = Fixture::scratch(placer, "churn");
      let page = f.page;
      let node = f.file("f", &vec![0x41; page]);
      for _ in 0..CYCLES {
        churn_once(&mut f, node, page);
      }
      let past = f.mapped(node, (3 * range, range), false, range);
      assert!(
        f.read(&past, 0, page).iter().all(|byte| *byte == 0),
        "a window past the file's end reads zero"
      );
      let counts = f.device.dax_backed_counts();
      println!(
        "{placer:?}: placed {} refused {} of 2 ranges over {} maps",
        counts.session.placed, counts.session.placement_refused, counts.maps_sent
      );
      assert_eq!(
        (counts.maps_sent, counts.maps_kept, counts.unmaps_sent),
        (CYCLES + 1, CYCLES, CYCLES),
        "{placer:?}: every map made, every upgrade kept, every removal an unmap"
      );
      assert_eq!(
        counts.session.placed + counts.session.placement_refused,
        2,
        "each range placed once however often it is mapped"
      );
      let placements = f.store.shards_backings.placements();
      if counts.session.placement_refused == 0 {
        assert_eq!(
          placements.len() as u64,
          f.layout.pages_holding([0, 3]),
          "the pages the two ranges lie in, each placed once"
        );
      }
      assert!(
        placements
          .iter()
          .all(|placement| placement.pmd_bytes == Some(range)),
        "every placed page is one PMD page while its backing lives: {placements:?}"
      );
      f.unmapped(node, range, range, Some((3 * range, range)));
      f.forgotten(node);
      f.assert_nothing_held("the node forgotten");
    }
  }

  /// §4.6, the backed route's L1 (A-193), through a read-only attachment, whose windows a guest can still hammer by
  /// reading, for both placers. Do: ask a window over a one-page file writable; then ask it read-only; then remove it and
  /// forget the node. Expect: the writable ask refused `EPERM` with no window recorded, nothing sent, and no backing made
  /// or placed; the read-only ask served (mapped read-only at the front end, for the message placer), its range placed
  /// (or counted refused) and, placed, one PMD page in slates' view; and, the node forgotten, nothing placed, held or
  /// counted.
  #[test]
  fn a_read_only_attachments_window_is_placed_and_its_writable_ask_refused() {
    const TEST: &str = "a_read_only_attachments_window_is_placed_and_its_writable_ask_refused";
    if !ranges_can_be_huge(TEST) {
      return;
    }
    for placer in placers() {
      let range = slates_mem::DAX_RANGE_BYTES;
      let mut f = read_only_fixture(placer, "placed-ro");
      let page = f.page;
      let node = f.file("f", &vec![0x41; page]);
      let head = f.submit(
        FIRST_REQUEST_QUEUE,
        Opcode::SetupMapping,
        node,
        &dax_setup_body(0, range, true, 0),
      );
      assert_eq!(
        f.answer(FIRST_REQUEST_QUEUE, head),
        -EPERM,
        "{placer:?}: a writable ask through a read-only attachment is refused"
      );
      f.assert_nothing_sent("nothing sent to the front end");
      f.assert_nothing_held("a refused ask");
      let maps = [(node, 0, f.mapped(node, (0, range), false, 0))];
      assert_ne!(
        maps[0].2.seen_writable(),
        Some(true),
        "a read-only attachment's window is mapped read-only"
      );
      assert_eq!(
        f.read(&maps[0].2, 0, page),
        vec![0x41; page],
        "the window serves the file"
      );
      if let Some(placements) = assert_maps_placed(&mut f, &maps, TEST) {
        assert_placement_cost(&f, &[node], &placements);
        assert_guards_hold_nothing(&f, &placements, node);
      }
      f.unmapped(node, 0, range, Some((0, range)));
      f.forgotten(node);
      f.assert_nothing_held("the node forgotten");
    }
  }

  /// §4.6 (G-vhost), a snapshot device's advance, for both placers. Do: map a file; tell the device its view moved, as
  /// an advance does. Expect: no window torn and nothing sent; the next pass is busy with a refill of the backing
  /// (counted), and the map still shows the file.
  #[test]
  fn an_advance_refills_the_backings_and_tears_no_window() {
    for placer in placers() {
      let mut f = Fixture::scratch(placer, "advance");
      let page = f.page;
      let node = f.file("f", &vec![0x41; page]);
      let map = f.mapped(node, (0, slates_mem::DAX_RANGE_BYTES), false, 0);
      {
        let mut bridge = backed_bridge(&mut f.vol, &mut f.store, &mut f.handles, &mut f.host);
        f.device.view_advanced(&mut bridge);
      }
      assert_eq!(f.device.dax_window_count(), 1, "{placer:?}: no window torn");
      assert!(f.pass().busy, "the refill runs before anything more");
      for _ in 0..TURNS {
        if !f.pass().busy {
          break;
        }
      }
      assert!(
        f.device.dax_backed_counts().session.refilled >= 1,
        "{placer:?}: the backing was refilled"
      );
      assert_eq!(f.read(&map, 0, page), vec![0x41; page]);
      f.assert_nothing_sent("nothing sent");
    }
  }

  /// An overlay over a simulated host holding `/big`, an untouched base file of four pages of 0x21, mapped read-only
  /// through a 2 MiB window; its number reserved by a publication when `published`, so a later request of it waits for
  /// no barrier. The fixture, the node and the map.
  fn overlay_with_mapped_big(
    placer: Placer,
    tag: &str,
    published: bool,
  ) -> (Fixture, u64, GuestMap) {
    let page = rustix::param::page_size().max(1);
    let mut host = slates_vfs::host::sim::SimHost::new();
    host.replace_file("/big", &vec![0x21; 4 * page]);
    let mut f = Fixture::overlay(placer, tag, host);
    let node = {
      let mut bridge = backed_bridge(&mut f.vol, &mut f.store, &mut f.handles, &mut f.host);
      let root = bridge.root(&f.cx).unwrap();
      bridge
        .lookup(ObjectId::new(root, 0), &f.cx, "big")
        .unwrap()
        .ino
    };
    if published {
      f.vol.mark_published(&f.store);
    }
    let map = f.mapped(node, (0, slates_mem::DAX_RANGE_BYTES), false, 0);
    assert_eq!(f.read(&map, 0, page), vec![0x21; page]);
    (f, node, map)
  }

  /// The disk's in-place edit of `/big` in [`overlay_with_mapped_big`]'s host, then a GETATTR of it through the
  /// device: its head, after the pass that served it.
  fn edit_big_and_getattr(f: &mut Fixture, node: u64) -> u16 {
    let page = f.page;
    f.host
      .as_mut()
      .unwrap()
      .write_in_place("/big", &vec![0x5C; 4 * page], 1_000);
    let getattr = f.submit(FIRST_REQUEST_QUEUE, Opcode::GetAttr, node, &[0u8; 16]);
    f.pass();
    getattr
  }

  /// §4.6 (G-vhost), beneath an overlay (A-163's refill), for both placers. An untouched base file follows the disk, so
  /// a change the disk makes reaches the guest's map when the base plane adopts it, and a reply that could reveal the
  /// change waits for the map to show it. Do: map an untouched base file read-only, its number published; edit it in
  /// place on the host; GETATTR it through the device. Expect: the GETATTR, which waits for no barrier, is held by the
  /// service pass that served it and adopted the edit; by the time it is published the map shows the disk's new bytes,
  /// and the refill was counted.
  #[test]
  fn a_reply_that_could_reveal_the_disks_change_waits_until_the_map_shows_it() {
    for placer in placers() {
      let (mut f, node, map) = overlay_with_mapped_big(placer, "refill", true);
      let barriers = f.barriers;
      let getattr = edit_big_and_getattr(&mut f, node);
      assert!(
        f.reply_now(FIRST_REQUEST_QUEUE, getattr).is_none(),
        "{placer:?}: the reply that could reveal the change is held while the map is refilled"
      );
      assert_eq!(
        f.answer(FIRST_REQUEST_QUEUE, getattr),
        0,
        "GETATTR answered"
      );
      assert_eq!(
        f.barriers, barriers,
        "the GETATTR waited for no barrier: the service pass held it"
      );
      assert_eq!(
        f.read(&map, 0, f.page),
        vec![0x5C; f.page],
        "{placer:?}: the map shows the disk's change by the reply"
      );
      let counts = f.device.dax_backed_counts();
      assert!(counts.replies_held >= 1 && counts.session.refilled >= 1);
    }
  }

  /// §4.6 (G-vhost), the same beneath an overlay, for a reply the owner's barrier holds first, for both placers. Do: as
  /// above, with the file's number not yet reserved by a publication, so every request of it waits for the barrier
  /// (A-150). Expect: the GETATTR's used element is held after the barrier completes, until the map shows the disk's new
  /// bytes.
  #[test]
  fn a_reply_held_for_the_owners_barrier_waits_for_the_refill_too() {
    for placer in placers() {
      let (mut f, node, map) = overlay_with_mapped_big(placer, "refillbar", false);
      let barriers = f.barriers;
      let getattr = edit_big_and_getattr(&mut f, node);
      assert!(
        f.barriers > barriers,
        "the GETATTR waited for the owner's barrier first"
      );
      assert!(
        f.reply_now(FIRST_REQUEST_QUEUE, getattr).is_none(),
        "{placer:?}: after the barrier, the reply is held while the map is refilled"
      );
      assert_eq!(
        f.answer(FIRST_REQUEST_QUEUE, getattr),
        0,
        "GETATTR answered"
      );
      assert_eq!(f.read(&map, 0, f.page), vec![0x5C; f.page], "{placer:?}");
      assert!(f.device.dax_backed_counts().replies_held >= 1);
    }
  }

  /// Shape: the files and window slots the property below plays over: few enough that a schedule revisits each.
  const PROPERTY_FILES: usize = 2;
  const PROPERTY_SLOTS: u64 = 3;
  /// Shape: the pages of each live window the property compares with the volume after every step: the file's pages and
  /// one past them, where a growth or a cut shows.
  const COMPARED_PAGES: usize = 4;
  /// Shape: the bytes one seal sweep may seal: more than every file here holds, so a sweep seals all it may.
  const SEAL_BUDGET: u64 = 1 << 20;

  /// One step of the property's schedule: what the guest or another path does.
  #[derive(Clone, Copy, Debug)]
  enum Step {
    /// The guest maps file `file`'s first range at window slot `slot`.
    Map { file: usize, slot: u64 },
    /// The guest removes the window range at slot `slot`, mapped or not.
    Remove { slot: u64 },
    /// Another path writes `len` bytes of `fill` at `page` of file `file`.
    Write {
      file: usize,
      page: usize,
      fill: u8,
      len: usize,
    },
    /// Another path cuts file `file` to `pages` whole pages.
    Cut { file: usize, pages: usize },
    /// The volume seals its idle open blocks, as the daemon's idle sweep does.
    Seal,
  }

  /// One step of the schedule, generated as a plain tuple so the strategy needs no `Arc`-backed `prop_oneof` (R2).
  fn step() -> impl Strategy<Value = Step> {
    (
      0u8..5,
      0..PROPERTY_FILES,
      0..PROPERTY_SLOTS,
      0..COMPARED_PAGES,
      any::<u8>(),
      1usize..64,
    )
      .prop_map(|(kind, file, slot, page, fill, len)| match kind {
        0 => Step::Map { file, slot },
        1 => Step::Remove { slot },
        2 => Step::Write {
          file,
          page,
          fill,
          len,
        },
        // A cut to fewer pages than the window shows, so what it removes is compared too.
        3 => Step::Cut {
          file,
          pages: page % (COMPARED_PAGES - 1),
        },
        _ => Step::Seal,
      })
  }

  /// Takes `step` over `f`, whose files are `files`, and keeps `live` (the file each slot's live window maps, and the
  /// guest's map of it) as the model the property compares with.
  fn take_step(
    f: &mut Fixture,
    files: &[u64],
    live: &mut std::collections::BTreeMap<u64, (u64, GuestMap)>,
    step: Step,
  ) {
    let range = slates_mem::DAX_RANGE_BYTES;
    let page = f.page;
    match step {
      Step::Map { file, slot } => {
        let node = files[file];
        if live.get(&slot).is_some_and(|(mapped, _)| *mapped == node) {
          // The window asked for is live as asked: answered with no map.
          f.repeated(node, (0, range), slot * range);
        } else {
          // Every window over the slot's range is unmapped first: the slot's own, if live.
          let _replaced = live.remove(&slot);
          let map = f.mapped(node, (0, range), false, slot * range);
          live.insert(slot, (node, map));
        }
      }
      Step::Remove { slot } => {
        let node = live.get(&slot).map_or(files[0], |(node, _)| *node);
        let was = live.remove(&slot).map(|_| (0, range));
        f.unmapped(node, slot * range, range, was);
      }
      Step::Write {
        file,
        page: at,
        fill,
        len,
      } => {
        let at = u64::try_from(at * page).unwrap();
        f.vol
          .write(&mut f.store, InodeNo(files[file]), at, &vec![fill; len])
          .unwrap();
      }
      Step::Cut { file, pages } => {
        f.vol
          .truncate(
            &mut f.store,
            InodeNo(files[file]),
            u64::try_from(pages * page).unwrap(),
          )
          .unwrap();
      }
      Step::Seal => {
        f.vol.age_writes();
        let _sealed = f.vol.seal_pending(&mut f.store, SEAL_BUDGET);
      }
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig {
      cases: slates_test_seeds::cases(24),
      ..ProptestConfig::default()
    }))]
    /// §4.6 (A-163, A-168, A-195), the backed route's coherence as a property, for both placers: whatever a guest maps
    /// and removes, and whatever other paths do to the files meanwhile, every live window shows exactly the volume's
    /// bytes of what it maps. Do: over two files and three window slots, take a random schedule of maps (a remap of a
    /// live slot among them), removals (of slots mapped or not), writes and cuts through the volume, and seal sweeps;
    /// after every step read the first pages of every live window; at the end remove every window and forget the files.
    /// Expect: each live window equal to the volume's bytes of its file (zeros past the end) after every step, every map
    /// and removal answered success, and at the end nothing mapped, held, counted or placed. Non-vacuous: every case maps
    /// at least once, since its schedule starts with a map.
    #[test]
    fn every_live_window_shows_the_volumes_bytes_whatever_the_schedule(
      first in (0..PROPERTY_FILES, 0..PROPERTY_SLOTS),
      schedule in prop::collection::vec(step(), 1..12),
    ) {
      for placer in placers() {
        let mut f = Fixture::scratch(placer, "property");
        let page = f.page;
        let files: Vec<u64> = (0..PROPERTY_FILES)
          .map(|index| f.file(&format!("f{index}"), &vec![0x30 + u8::try_from(index).unwrap(); page]))
          .collect();
        let mut live = std::collections::BTreeMap::new();
        let opening = Step::Map { file: first.0, slot: first.1 };
        for step in std::iter::once(opening).chain(schedule.iter().copied()) {
          take_step(&mut f, &files, &mut live, step);
          for (slot, (node, map)) in &live {
            let compared = COMPARED_PAGES * page;
            let mut expected = f.volume_bytes(*node, 0, compared);
            expected.resize(compared, 0);
            prop_assert_eq!(
              f.read(map, 0, compared),
              expected,
              "{:?}: the window at slot {} after {:?}",
              placer,
              slot,
              step
            );
          }
        }
        let range = slates_mem::DAX_RANGE_BYTES;
        let slots: Vec<u64> = live.keys().copied().collect();
        for slot in slots {
          let (node, _map) = live.remove(&slot).unwrap();
          f.unmapped(node, slot * range, range, Some((0, range)));
        }
        for node in &files {
          if f.backs(*node) {
            f.forgotten(*node);
          }
        }
        f.assert_nothing_held("every window removed and every file forgotten");
      }
    }
  }
}
