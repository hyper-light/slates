//! The FUSE-over-virtio request cycle proven by use (§4.6 A-9; AC-4.11/T-4.13's guest leg,
//! AC-4.12/T-4.14): a simulated guest driver builds real virtqueues, submits FUSE requests through
//! them as descriptor chains, and reads the replies back from its buffers, against a real
//! `VolumeBridge` over a real scratch volume — create → write → getattr → read → readdir → release →
//! unlink → forget → statfs → destroy. The differential oracle: the same requests issued through the
//! FUSE codec's `dispatch` directly, over a second identical scratch volume with the same
//! deterministic clock, must produce byte-identical replies.
// Test harness code: an unwrap here is a failed test.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::SimDriver;
use slates_bridge_core::{Attachments, OpContext, Rights, View, VolumeBridge};
use slates_bridge_fuse::abi::{IN_HEADER_LEN, OUT_HEADER_LEN, Opcode, flags};
use slates_bridge_fuse::bridge::dispatch;
use slates_bridge_fuse::reply::EntryOut;
use slates_bridge_virtiofs::device::{
  Device, DeviceConfig, DeviceError, FIRST_REQUEST_QUEUE, FsTag, HIPRIO_QUEUE, TAG_LEN,
};
use slates_bridge_virtiofs::virtqueue::VirtqueueError;
use slates_db::catalog::{Principal, VolumeId};
use slates_mem::arena::ChunkArena;
use slates_mem::region::Region;
use slates_vfs::clock::StepClock;
use slates_vfs::names::NameEquivalence;
use slates_vfs::quota::Quota;
use slates_vfs::volume::{Store, StoreConfig, Volume, VolumeConfig};

/// Shape: the page and a small arena for the test volumes.
const PAGE: usize = 4096;
const REGION_PAGES: usize = 4096;
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

fn store() -> Store {
  let mut arena = ChunkArena::new(PAGE);
  arena
    .add_region(Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
    .unwrap();
  Store::new(
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
  )
}

/// A scratch volume on a deterministic clock, so two volumes driven identically agree byte for byte.
fn volume(store: &mut Store) -> Volume {
  Volume::create(
    store,
    VolumeConfig {
      prefix: 1,
      names: NameEquivalence::Exact,
      quota: Quota::Bounded { limit: 1 << 30 },
      journal_bytes: 1 << 16,
      clock: Box::new(StepClock::new(1_000_000_000, 1)),
    },
  )
  .unwrap()
}

fn vid() -> VolumeId {
  VolumeId { bytes: [7; 16] }
}

/// A read-write current-view context, minted through the attachment registry.
fn context() -> OpContext {
  let mut attachments = Attachments::new();
  let id = attachments
    .attach(
      vid(),
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

/// A FUSE request: the header then the body, `len` set to the total.
fn message(opcode: u32, unique: u64, nodeid: u64, body: &[u8]) -> Vec<u8> {
  let total = IN_HEADER_LEN + body.len();
  let mut m = vec![0u8; total];
  m[0..4].copy_from_slice(&u32::try_from(total).unwrap().to_le_bytes());
  m[4..8].copy_from_slice(&opcode.to_le_bytes());
  m[8..16].copy_from_slice(&unique.to_le_bytes());
  m[16..24].copy_from_slice(&nodeid.to_le_bytes());
  m[IN_HEADER_LEN..].copy_from_slice(body);
  m
}

fn reply_error(reply: &[u8]) -> i32 {
  i32::from_le_bytes(reply[4..8].try_into().unwrap())
}

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
    let serviced = self
      .device
      .service_queue(
        queue,
        &mut self.driver.memory,
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

/// Format: the FUSE node id of the root directory.
const ROOT: u64 = 1;

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
  let head = driver.submit(usize::from(FIRST_REQUEST_QUEUE), &request, REPLY_CAP, 1);
  device
    .service_queue(
      FIRST_REQUEST_QUEUE,
      &mut driver.memory,
      &mut bridge,
      &cx,
      BATCH,
    )
    .unwrap();
  let (_, len) = driver.reap(usize::from(FIRST_REQUEST_QUEUE)).unwrap();
  let reply = driver.reply_of(usize::from(FIRST_REQUEST_QUEUE), head, len);
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
  let n = device.negotiated().unwrap();
  assert_eq!(n.flags & FUSE_MAP_ALIGNMENT, 0);
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
  let first = device
    .service_queue(FIRST_REQUEST_QUEUE, &mut driver.memory, &mut bridge, &cx, 2)
    .unwrap();
  assert_eq!((first.served, first.more_pending), (2, true));
  let second = device
    .service_queue(
      FIRST_REQUEST_QUEUE,
      &mut driver.memory,
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
  let refused = device
    .service_queue(rq, &mut driver.memory, &mut bridge, &cx, BATCH)
    .unwrap_err();
  assert_eq!(
    refused,
    DeviceError::RequestTooShort {
      queue: rq,
      readable: 10,
      need: u64::try_from(IN_HEADER_LEN).unwrap()
    }
  );
  assert_eq!(
    device
      .service_queue(rq, &mut driver.memory, &mut bridge, &cx, BATCH)
      .unwrap_err(),
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
    device
      .service_queue(rq, &mut driver.memory, &mut bridge, &cx, BATCH)
      .unwrap_err(),
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
  device
    .service_queue(
      FIRST_REQUEST_QUEUE,
      &mut driver.memory,
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
  let driver = SimDriver::new(&QUEUE_SIZES);
  let mut device = Device::new(DeviceConfig::new(FsTag::new("slates").unwrap()));
  let mut store = store();
  let mut vol = volume(&mut store);
  let mut bridge = VolumeBridge::new(vid(), &mut vol, &mut store);
  let cx = context();
  let mut memory = SimDriver::new(&QUEUE_SIZES).memory;
  assert_eq!(
    device
      .service_queue(FIRST_REQUEST_QUEUE, &mut memory, &mut bridge, &cx, BATCH)
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
    device
      .service_queue(2, &mut memory, &mut bridge, &cx, BATCH)
      .unwrap_err(),
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
