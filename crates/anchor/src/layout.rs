//! The segment's geometry and its regions (§4.8's `Partition` fields that live "in the anchor
//! segment": the op log, the audit log, the landing manifests, plus the profile and the
//! catalog snapshots). Every offset is a page multiple so a region can be locked or advised on
//! its own, and every word two processes touch sits at an 8-byte offset.

/// Format: the segment's magic, `SLAN` in little-endian ASCII.
pub const MAGIC: u32 = 0x4E41_4C53;
/// Format: the layout version; bumped with any change to the header or the region shapes.
pub const LAYOUT_VERSION: u32 = 1;
/// Format: the header's size: magic (4), version (4), identity (32), generation (8), total
/// length (8), then the encoded geometry (64), padded to two cache lines.
pub const HEADER_BYTES: usize = 128;
/// Format: the magic's offset.
pub const AT_MAGIC: usize = 0;
/// Format: the layout version's offset.
pub const AT_VERSION: usize = 4;
/// Format: the identity hash's offset and width (BLAKE3).
pub const AT_IDENTITY: usize = 8;
/// Format: the identity hash's width.
pub const IDENTITY_BYTES: usize = 32;
/// Format: the header generation word's offset (odd while the creator writes the header).
pub const AT_GENERATION: usize = 40;
/// Format: the total length's offset.
pub const AT_TOTAL: usize = 48;
/// Format: the encoded geometry's offset.
pub const AT_GEOMETRY: usize = 56;
/// Format: the encoded geometry's width.
pub const GEOMETRY_BYTES: usize = 64;
/// Format: the encoded geometry's fields, by offset: partitions (u16).
const GEO_PARTITIONS: usize = 0;
/// Format: the page (u64).
const GEO_PAGE: usize = 8;
/// Format: the profile region's bytes (u64).
const GEO_PROFILE: usize = 16;
/// Format: one log ring's bytes (u64).
const GEO_LOG: usize = 24;
/// Format: one snapshot slot's bytes (u64).
const GEO_SNAPSHOT: usize = 32;
/// Format: the audit ring's bytes (u64).
const GEO_AUDIT: usize = 40;
/// Format: the landing slot count (u32).
const GEO_LANDING_SLOTS: usize = 48;
/// Format: one landing slot's bytes (u64).
const GEO_LANDING_SLOT_BYTES: usize = 56;

/// Format: the supervision block's words, by offset inside its region: the daemon's pid.
pub const SUP_PID: usize = 0;
/// Format: the daemon's heartbeat, monotonic nanoseconds of its clock.
pub const SUP_HEARTBEAT: usize = 8;
/// Format: the daemon generation, incremented at every start.
pub const SUP_GENERATION: usize = 16;
/// Format: restarts so far.
pub const SUP_RESTARTS: usize = 24;
/// Format: the supervision state word (`State`).
pub const SUP_STATE: usize = 32;
/// Format: when the current daemon was started, the anchor's monotonic nanoseconds.
pub const SUP_STARTED: usize = 40;

/// Format: a published payload's words (the profile, a snapshot slot, a landing slot): the
/// generation (odd while a writer is inside) then the length, then the bytes.
pub const PAYLOAD_GENERATION: usize = 0;
/// Format: the payload length's offset.
pub const PAYLOAD_LEN: usize = 8;
/// Format: the payload bytes' offset.
pub const PAYLOAD_BYTES: usize = 16;

/// Format: a ring region's words (a log, the audit): head, tail, capacity, sequence base, then
/// the byte ring at `RING_BYTES`.
pub const RING_HEAD: usize = 0;
/// Format: the tail word's offset.
pub const RING_TAIL: usize = 8;
/// Format: the capacity word's offset (the byte ring's length).
pub const RING_CAPACITY: usize = 16;
/// Format: the sequence base's offset (the sequence number of the record at the head).
pub const RING_SEQ_BASE: usize = 24;
/// Format: the byte ring's offset (one cache line of words before it).
pub const RING_BYTES: usize = 64;

/// The supervision state word's values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u64)]
pub enum State {
  /// No daemon.
  Stopped = 0,
  /// A daemon is running (its pid is in the block).
  Running = 1,
  /// Supervision stopped restarting: the daemon failed faster than it could recover.
  CrashLoop = 2,
}

impl State {
  /// The state a word names; an unknown word reads as `Stopped`.
  pub fn from_word(word: u64) -> State {
    match word {
      1 => State::Running,
      2 => State::CrashLoop,
      _ => State::Stopped,
    }
  }
}

/// The geometry the daemon derives at boot and the anchor persists in the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
  /// Partitions (one per shard).
  pub partitions: u16,
  /// Derived: the page every region is aligned to (the profile's base page).
  pub page: u64,
  /// Derived: the profile region's bytes (twice the measured payload, so a refresh with more
  /// cores fits).
  pub profile_bytes: u64,
  /// Derived: one partition's log ring bytes (recovery budget × measured replay throughput).
  pub log_bytes: u64,
  /// Derived: one snapshot slot's bytes (the measured partition state size × the growth
  /// headroom); two slots per partition.
  pub snapshot_bytes: u64,
  /// Derived: the audit ring's bytes (landing rate × audit horizon × record size).
  pub audit_bytes: u64,
  /// Derived: landing slots (landings in flight the host allows).
  pub landing_slots: u32,
  /// Derived: one landing slot's bytes (the measured manifest size p99).
  pub landing_slot_bytes: u64,
}

/// The kind of a region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionKind {
  /// The supervision block.
  Supervision,
  /// The profile payload.
  Profile,
  /// A partition's log ring.
  Log(u16),
  /// A partition's snapshot slot (two per partition).
  Snapshot(u16, u8),
  /// The audit ring.
  Audit,
  /// A landing slot.
  Landing(u32),
}

/// Where a region sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionSpec {
  /// The kind.
  pub kind: RegionKind,
  /// The offset from the segment's start.
  pub offset: u64,
  /// The bytes.
  pub len: u64,
}

impl Geometry {
  fn aligned(&self, bytes: u64) -> u64 {
    let page = self.page.max(1);
    bytes.max(page).next_multiple_of(page)
  }

  /// The regions, in layout order: the header page, supervision, profile, the logs, the
  /// snapshots, the audit, the landings.
  pub fn regions(&self) -> Vec<RegionSpec> {
    let mut out = Vec::new();
    let mut at = self.aligned(u64::try_from(HEADER_BYTES).unwrap_or(u64::MAX));
    let mut push = |kind: RegionKind, len: u64| {
      let len = self.aligned(len);
      out.push(RegionSpec {
        kind,
        offset: at,
        len,
      });
      at = at.saturating_add(len);
    };
    push(RegionKind::Supervision, self.page);
    push(RegionKind::Profile, self.profile_bytes);
    for p in 0..self.partitions {
      push(RegionKind::Log(p), self.log_bytes);
    }
    for p in 0..self.partitions {
      push(RegionKind::Snapshot(p, 0), self.snapshot_bytes);
      push(RegionKind::Snapshot(p, 1), self.snapshot_bytes);
    }
    push(RegionKind::Audit, self.audit_bytes);
    for slot in 0..self.landing_slots {
      push(RegionKind::Landing(slot), self.landing_slot_bytes);
    }
    out
  }

  /// The segment's total length.
  pub fn total_bytes(&self) -> u64 {
    self.regions().last().map_or(
      self.aligned(u64::try_from(HEADER_BYTES).unwrap_or(u64::MAX)),
      |r| r.offset.saturating_add(r.len),
    )
  }

  /// The region of a kind.
  pub fn region(&self, kind: RegionKind) -> Option<RegionSpec> {
    self.regions().into_iter().find(|r| r.kind == kind)
  }

  /// The fixed little-endian encoding kept in the header.
  pub fn encode(&self) -> [u8; GEOMETRY_BYTES] {
    let mut out = [0u8; GEOMETRY_BYTES];
    put(&mut out, GEO_PARTITIONS, &self.partitions.to_le_bytes());
    put(&mut out, GEO_PAGE, &self.page.to_le_bytes());
    put(&mut out, GEO_PROFILE, &self.profile_bytes.to_le_bytes());
    put(&mut out, GEO_LOG, &self.log_bytes.to_le_bytes());
    put(&mut out, GEO_SNAPSHOT, &self.snapshot_bytes.to_le_bytes());
    put(&mut out, GEO_AUDIT, &self.audit_bytes.to_le_bytes());
    put(
      &mut out,
      GEO_LANDING_SLOTS,
      &self.landing_slots.to_le_bytes(),
    );
    put(
      &mut out,
      GEO_LANDING_SLOT_BYTES,
      &self.landing_slot_bytes.to_le_bytes(),
    );
    out
  }

  /// The geometry an encoding names.
  pub fn decode(bytes: &[u8; GEOMETRY_BYTES]) -> Geometry {
    Geometry {
      partitions: u16::from_le_bytes([bytes[GEO_PARTITIONS], bytes[GEO_PARTITIONS + 1]]),
      page: word(bytes, GEO_PAGE),
      profile_bytes: word(bytes, GEO_PROFILE),
      log_bytes: word(bytes, GEO_LOG),
      snapshot_bytes: word(bytes, GEO_SNAPSHOT),
      audit_bytes: word(bytes, GEO_AUDIT),
      landing_slots: u32::from_le_bytes([
        bytes[GEO_LANDING_SLOTS],
        bytes[GEO_LANDING_SLOTS + 1],
        bytes[GEO_LANDING_SLOTS + 2],
        bytes[GEO_LANDING_SLOTS + 3],
      ]),
      landing_slot_bytes: word(bytes, GEO_LANDING_SLOT_BYTES),
    }
  }
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
  bytes[at..at + value.len()].copy_from_slice(value);
}

fn word(bytes: &[u8], at: usize) -> u64 {
  let mut w = [0u8; size_of::<u64>()];
  let n = w.len();
  w.copy_from_slice(&bytes[at..at + n]);
  u64::from_le_bytes(w)
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn regions_are_page_aligned_and_disjoint_and_the_geometry_round_trips() {
    let g = Geometry {
      partitions: 3,
      page: 4096,
      profile_bytes: 10_000,
      log_bytes: 1 << 20,
      snapshot_bytes: 8192,
      audit_bytes: 4096,
      landing_slots: 2,
      landing_slot_bytes: 100,
    };
    let regions = g.regions();
    let mut end = 4096u64;
    for r in &regions {
      assert_eq!(r.offset % 4096, 0, "{r:?}");
      assert_eq!(r.len % 4096, 0, "{r:?}");
      assert_eq!(r.offset, end, "{r:?} is contiguous");
      end = r.offset + r.len;
    }
    assert_eq!(g.total_bytes(), end);
    assert_eq!(
      regions
        .iter()
        .filter(|r| matches!(r.kind, RegionKind::Log(_)))
        .count(),
      3
    );
    assert_eq!(
      regions
        .iter()
        .filter(|r| matches!(r.kind, RegionKind::Snapshot(..)))
        .count(),
      6
    );
    assert_eq!(Geometry::decode(&g.encode()), g);
    assert_eq!(g.region(RegionKind::Landing(1)).unwrap().len, 4096);
  }
}
