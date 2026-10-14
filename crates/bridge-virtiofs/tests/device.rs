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
  let object =
    SharedObject::create(&format!("sl-dp-{tag}-{}", std::process::id()), page * pages, Words::new()).unwrap();
  let mut arena = ChunkArena::new(page);
  // SAFETY: this test is the object's only mapper; it declares no words.
  arena
    .add_region(Region::shared(unsafe { ExclusiveObject::new(object) }, page))
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
    let no = vol.create_file_no(&mut store, root, &format!("f{i}"), 0o644).unwrap();
    vol.write(&mut store, no, 0, &vec![0x5A; page]).unwrap();
    files.push(no.0);
  }
  let (mut device, mut driver) = device_and_driver();
  let cache =
    SharedObject::create(&format!("sl-dpwin-{}", std::process::id()), page * n, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * n) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());

  let batch = 128usize;
  let (mut first, mut last) = (Duration::ZERO, Duration::ZERO);
  for (i, &nodeid) in files.iter().enumerate() {
    let moffset = (i * page) as u64;
    let unique = u64::try_from(i).unwrap_or(0).saturating_add(2);
    let at = Instant::now();
    let r = dax_exchange(
      &mut device, &mut driver, &mut bridge, &cx,
      (Opcode::SetupMapping, unique, nodeid), &dax_setup_body(0, page as u64, true, moffset),
    );
    let dt = at.elapsed();
    assert_eq!(reply_error(&r), 0, "map {i}");
    if i < batch {
      first = first.saturating_add(dt);
    }
    if i >= n.saturating_sub(batch) {
      last = last.saturating_add(dt);
    }
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
    if i < batch {
      tfirst = tfirst.saturating_add(dt);
    }
    if i >= n.saturating_sub(batch) {
      tlast = tlast.saturating_add(dt);
    }
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
  let object =
    SharedObject::create(&format!("sl-ds-{tag}-{}", std::process::id()), page * REGION_PAGES, Words::new())
      .unwrap();
  let mut arena = ChunkArena::new(page);
  // SAFETY: this test is the object's only mapper; it declares no words.
  arena
    .add_region(Region::shared(unsafe { ExclusiveObject::new(object) }, page))
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
  let cache = SharedObject::create(&format!("sl-daxwin-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  assert!(device.dax_offered(), "the device serves DAX now a window is installed");

  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);

  // FUSE_INIT brings the connection up (fuse_init_in: major, minor, max_readahead, flags = 4 u32, then flags2/unused).
  let mut init = Vec::new();
  init.extend_from_slice(&7u32.to_le_bytes()); // major
  init.extend_from_slice(&38u32.to_le_bytes()); // minor
  init.extend_from_slice(&0u32.to_le_bytes()); // max_readahead
  init.extend_from_slice(&0u32.to_le_bytes()); // flags
  init.resize(64, 0); // the kernel sends a 64-byte init_in; the codec reads what it needs
  let init_reply = dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &init);
  assert_eq!(reply_error(&init_reply), 0, "INIT served");

  // FUSE_SETUPMAPPING: fh, foffset=0, len=page, flags=WRITE, moffset=0.
  let mut setup = Vec::new();
  setup.extend_from_slice(&0u64.to_le_bytes()); // fh (slates maps by nodeid)
  setup.extend_from_slice(&0u64.to_le_bytes()); // foffset
  setup.extend_from_slice(&(page as u64).to_le_bytes()); // len
  setup.extend_from_slice(&slates_bridge_fuse::request::SETUPMAPPING_FLAG_WRITE.to_le_bytes()); // flags = write
  setup.extend_from_slice(&0u64.to_le_bytes()); // moffset
  let setup_reply =
    dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::SetupMapping, 2, nodeid), &setup);
  assert_eq!(reply_error(&setup_reply), 0, "SETUPMAPPING succeeded over the DAX window");
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
  let remove_reply =
    dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::RemoveMapping, 3, nodeid), &remove);
  assert_eq!(reply_error(&remove_reply), 0, "REMOVEMAPPING succeeded");
  assert_eq!(device.dax_window_count(), 0, "the window mapping was torn down");

  drop(cache);
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
  let cache = SharedObject::create(&format!("sl-daxA-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());

  let w = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, nodeid), &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&w), -1, "writable DAX refused EPERM without opt-in");
  assert_eq!(device.dax_window_count(), 0, "no window for the refused writable map");

  let r = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 3, nodeid), &dax_setup_body(0, page as u64, false, 0),
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
  let cache = SharedObject::create(&format!("sl-daxB-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());

  // moffset = the window's end: the whole requested range is outside it.
  let reply = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, nodeid), &dax_setup_body(0, page as u64, true, (page * 4) as u64),
  );
  assert_eq!(reply_error(&reply), -22, "a map past the window is refused EINVAL");
  assert_eq!(device.dax_window_count(), 0, "nothing mapped past the window");
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
  let cache =
    SharedObject::create(&format!("sl-daxremap-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());

  let ra = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, a_no), &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&ra), 0, "A mapped at window offset 0");
  assert_eq!(device.dax_window_count(), 1, "one window after A");

  let rb = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 3, b_no), &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&rb), 0, "B remapped over A's slot");
  assert_eq!(device.dax_window_count(), 1, "still one window after the remap — A's entry did not leak");
  assert_eq!(device.dax_window_bytes(0, 64).unwrap(), vec![0xBB; 64], "the window now shows B, not A");

  let mut remove = Vec::new();
  remove.extend_from_slice(&1u32.to_le_bytes());
  remove.extend_from_slice(&0u64.to_le_bytes());
  remove.extend_from_slice(&(page as u64).to_le_bytes());
  let rr = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx, (Opcode::RemoveMapping, 4, b_no), &remove,
  );
  assert_eq!(reply_error(&rr), 0, "REMOVEMAPPING of offset 0 served");
  assert_eq!(device.dax_window_count(), 0, "no window entry left");

  drop(bridge);
  assert_eq!(store.dax.len(), 0, "no live mapping in the registry — neither A nor B leaked");
  assert_eq!(store.dax.revoked(), 2, "both A (at the remap) and B (at REMOVEMAPPING) were revoked");
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
  let cache =
    SharedObject::create(&format!("sl-daxdestroy-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());
  let r = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, nodeid), &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&r), 0, "mapped");
  assert_eq!(device.dax_window_count(), 1, "one live window");

  let d = dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Destroy, 3, ROOT), &[]);
  assert_eq!(reply_error(&d), 0, "DESTROY served");
  assert_eq!(device.dax_window_count(), 0, "the live window was torn down at unmount");

  drop(bridge);
  assert_eq!(store.dax.len(), 0, "the registry holds no live mapping after unmount — the pin was not leaked");
  assert_eq!(store.dax.revoked(), 1, "the mapped block was revoked at teardown");
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
  let cache =
    SharedObject::create(&format!("sl-daxdisc-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());
  let r = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, nodeid), &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&r), 0, "mapped");
  assert_eq!(device.dax_window_count(), 1, "one live window");

  // The disconnect terminal step (no DESTROY): reclaim calls this with the attachment's bridge.
  device.release_dax(&mut bridge, &cx);
  assert_eq!(device.dax_window_count(), 0, "the live window was torn down on the disconnect");

  drop(bridge);
  assert_eq!(store.dax.len(), 0, "the registry holds no live mapping after the disconnect — no leaked pin");
  assert_eq!(store.dax.revoked(), 1, "the mapped block was revoked at the disconnect teardown");
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
  let cache =
    SharedObject::create(&format!("sl-daxhostile-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());

  // Hostile SETUPMAPPING bodies: a truncated body (the parser needs 5x u64), a colossal len (overflows the block/
  // window bound), an unaligned window offset (the map is refused and the block it briefly pinned is unpinned), and
  // an inline file (no mappable content block). Each a typed refusal, no window, the block a refused map pinned
  // released — a hostile guest establishes no mapping.
  let page_len = page as u64;
  for (unique, nodeid, body, why) in [
    (2u64, big_no, [0u8; 20].to_vec(), "a truncated SETUPMAPPING body"),
    (3, big_no, dax_setup_body(0, u64::MAX, true, 0), "len = u64::MAX"),
    (4, big_no, dax_setup_body(0, page_len, true, 1), "an unaligned moffset"),
    (5, tiny_no, dax_setup_body(0, page_len, true, 0), "an inline file"),
  ] {
    let r = dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::SetupMapping, unique, nodeid), &body);
    assert_ne!(reply_error(&r), 0, "{why} is refused");
    assert_eq!(device.dax_window_count(), 0, "{why} established no window");
  }

  // Hostile REMOVEMAPPING bodies: a truncated body (below the u32 count) and a foreign count (u32::MAX entries in a
  // short body). Each handled without faulting the device and establishing no window.
  for (unique, body) in [
    (6u64, [0u8; 3].to_vec()),
    (7, [u32::MAX.to_le_bytes().as_slice(), &0u64.to_le_bytes(), &page_len.to_le_bytes()].concat()),
  ] {
    let _ = dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::RemoveMapping, unique, big_no), &body);
    assert_eq!(device.dax_window_count(), 0, "a hostile REMOVEMAPPING established no window");
  }

  // The device was never faulted: a well-formed map still succeeds.
  let r = dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::SetupMapping, 8, big_no), &dax_setup_body(0, page as u64, true, 0));
  assert_eq!(reply_error(&r), 0, "a valid map still succeeds after the hostile requests");
  assert_eq!(device.dax_window_count(), 1, "exactly the one valid window is live");
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
  let cache =
    SharedObject::create(&format!("sl-daxdir-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());

  // SETUPMAPPING the root directory, read-only: the device maps its entries snapshot into the window.
  let reply = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, root_no), &dax_setup_body(0, page as u64, false, 0),
  );
  assert_eq!(reply_error(&reply), 0, "a directory SETUPMAPPING maps its entries");
  assert_eq!(device.dax_window_count(), 1, "one live directory window");

  let raw = device.dax_window_bytes(0, page as u64).unwrap();
  let got = dir_snapshot_names(&raw);
  for name in names {
    assert!(got.iter().any(|g| g == name), "entry {name} readable from the directory window: {got:?}");
  }

  let mut remove = Vec::new();
  remove.extend_from_slice(&1u32.to_le_bytes());
  remove.extend_from_slice(&0u64.to_le_bytes());
  remove.extend_from_slice(&(page as u64).to_le_bytes());
  let removed =
    dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::RemoveMapping, 3, root_no), &remove);
  assert_eq!(reply_error(&removed), 0, "REMOVEMAPPING tears the directory window down");
  assert_eq!(device.dax_window_count(), 0, "the directory window was torn down");
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
  let cache =
    SharedObject::create(&format!("sl-daxreq-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());
  let r = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, nodeid), &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&r), 0, "mapped");
  assert_eq!(device.dax_window_count(), 1, "one live window");

  // The daemon signals the device to revoke the inode's windows before it touches the block (step 5).
  let torn = device.request_revoke_inode(nodeid, &mut bridge, &cx);
  assert_eq!(torn, 1, "the daemon-initiated revoke tore down the inode's one window");
  assert_eq!(device.dax_window_count(), 0, "the window was munmapped");
  assert_eq!(
    device.request_revoke_inode(nodeid, &mut bridge, &cx),
    0,
    "idempotent: nothing left to revoke"
  );

  drop(bridge);
  assert_eq!(store.dax.len(), 0, "the registry mapping was revoked");
  assert_eq!(store.dax.revoked(), 1, "the block was unpinned through the two-phase (release after unmap)");
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
  let cache =
    SharedObject::create(&format!("sl-daxrw-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());
  // A writable window at window offset 0 and a read-only window at offset `page`, both of the same inode.
  let w = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 2, nodeid), &dax_setup_body(0, page as u64, true, 0),
  );
  assert_eq!(reply_error(&w), 0, "writable mapped");
  let r = dax_exchange(
    &mut device, &mut driver, &mut bridge, &cx,
    (Opcode::SetupMapping, 3, nodeid), &dax_setup_body(0, page as u64, false, page as u64),
  );
  assert_eq!(reply_error(&r), 0, "read-only mapped");
  assert_eq!(device.dax_window_count(), 2, "both windows live");

  // Snapshot's teardown: revoke only the inode's writable windows.
  let torn = device.request_revoke_writable_inode(nodeid, &mut bridge, &cx);
  assert_eq!(torn, 1, "exactly the writable window was torn");
  assert_eq!(device.dax_window_count(), 1, "the read-only window survives a writable revoke");
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
  let cache =
    SharedObject::create(&format!("sl-daxchan-{}", std::process::id()), page * 4, Words::new()).unwrap();
  device.install_dax_window(Some(
    slates_bridge_virtiofs::dax::DaxCacheRegion::new(cache.as_fd(), (page * 4) as u64).unwrap(),
  ));
  let cx = context();

  // Pass one: a transient bridge brings the connection up and maps the file writable — one live guest window.
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    dax_exchange(&mut device, &mut driver, &mut bridge, &cx, (Opcode::Init, 1, ROOT), &dax_init_body());
    let r = dax_exchange(
      &mut device, &mut driver, &mut bridge, &cx,
      (Opcode::SetupMapping, 2, nodeid), &dax_setup_body(0, page as u64, true, 0),
    );
    assert_eq!(reply_error(&r), 0, "mapped");
  }
  assert_eq!(device.dax_window_count(), 1, "one live window after the map pass");
  assert_eq!(store.dax.len(), 1, "the registry holds the live mapping");

  // Between passes a client on the shard writes the mapped file. The VFS write boundary revokes the inode's mappings
  // (phase one): the registry entry goes, the block stays pinned, and the inode is enqueued for the device.
  vol.write(&mut store, no, 0, &vec![0xA5; page]).unwrap();
  assert_eq!(store.dax.len(), 0, "the write revoked the mapping (phase one)");
  assert_eq!(store.dax.revoked(), 1, "phase one counted the revoke; the block is still pinned");

  // Pass two: a fresh transient bridge. The daemon drains its pending revokes and tears the guest window down.
  {
    let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
    let pending = bridge.take_pending_dax_revokes();
    assert_eq!(pending, vec![nodeid], "the write enqueued exactly the mutated inode for the device");
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
  assert_eq!(device.dax_window_count(), 0, "the guest window was unmapped");
  drop(cache);
}
