//! The anchor segment: create, attach, and the views the anchor and the daemon use.

use std::sync::atomic::{AtomicU64, Ordering};

use slates_machine::facts::Identity;
use slates_mem::{Handoff, SharedObject};

use crate::error::AnchorError;
use crate::layout::{
  AT_GENERATION, AT_GEOMETRY, AT_IDENTITY, AT_MAGIC, AT_TOTAL, AT_VERSION, GEOMETRY_BYTES,
  Geometry, HEADER_BYTES, IDENTITY_BYTES, LAYOUT_VERSION, MAGIC, PAYLOAD_BYTES, PAYLOAD_GENERATION,
  PAYLOAD_LEN, RING_CAPACITY, RING_HEAD, RING_SEQ_BASE, RING_TAIL, RegionKind, RegionSpec,
  SUP_GENERATION, SUP_HEARTBEAT, SUP_PID, SUP_RESTARTS, SUP_STARTED, SUP_STATE, State,
};

/// Format: the words at the head of a ring region: head, tail, capacity, sequence base.
pub const RING_WORDS: usize = 4;
/// Format: the environment variable carrying the handoff to the daemon (a descriptor number
/// on Linux, a name elsewhere).
pub const ENV_HANDOFF: &str = "SLATES_ANCHOR";
/// Format: the environment variable carrying the segment's length.
pub const ENV_LEN: &str = "SLATES_ANCHOR_LEN";
/// Format: the environment variable carrying the handoff to the content object — the anchor-owned
/// RAM that backs a shard's volume storage (§4.8), so an agent's writes survive a daemon restart
/// (the store's arena maps this object rather than a private mapping). A descriptor on Linux, a
/// name elsewhere, exactly like [`ENV_HANDOFF`].
pub const ENV_CONTENT: &str = "SLATES_ANCHOR_CONTENT";
/// Format: the environment variable carrying the content object's length.
pub const ENV_CONTENT_LEN: &str = "SLATES_ANCHOR_CONTENT_LEN";

/// The mapped anchor segment.
pub struct AnchorSegment {
  object: SharedObject,
  geometry: Geometry,
  /// The content object the supervisor creates and holds so a shard's volume storage lives in
  /// anchor-owned RAM (§4.8): `Some` on the process that created it (the supervisor keeps it alive
  /// across daemon restarts), `None` on a process that only attached the metadata object — that
  /// process opens the content object from the handoff env ([`AnchorSegment::open_content`]).
  content: Option<SharedObject>,
}

impl std::fmt::Debug for AnchorSegment {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("AnchorSegment")
      .field("geometry", &self.geometry)
      .field("len", &self.object.len())
      .finish()
  }
}

/// The supervision block, through atomic words.
pub struct Supervision<'a> {
  pid: &'a AtomicU64,
  heartbeat: &'a AtomicU64,
  generation: &'a AtomicU64,
  restarts: &'a AtomicU64,
  state: &'a AtomicU64,
  started: &'a AtomicU64,
}

impl Supervision<'_> {
  /// The daemon's pid, zero when none.
  pub fn pid(&self) -> u64 {
    self.pid.load(Ordering::Acquire)
  }

  /// The daemon's last heartbeat, its monotonic nanoseconds.
  pub fn heartbeat_ns(&self) -> u64 {
    self.heartbeat.load(Ordering::Acquire)
  }

  /// The daemon writes its heartbeat here every loop tick.
  pub fn beat(&self, now_ns: u64) {
    self.heartbeat.store(now_ns, Ordering::Release);
  }

  /// The daemon generation (starts so far).
  pub fn generation(&self) -> u64 {
    self.generation.load(Ordering::Acquire)
  }

  /// Restarts so far.
  pub fn restarts(&self) -> u64 {
    self.restarts.load(Ordering::Acquire)
  }

  /// The state.
  pub fn state(&self) -> State {
    State::from_word(self.state.load(Ordering::Acquire))
  }

  /// When the current daemon started, the anchor's monotonic nanoseconds.
  pub fn started_ns(&self) -> u64 {
    self.started.load(Ordering::Acquire)
  }

  /// The anchor records a start.
  pub fn record_start(&self, pid: u64, now_ns: u64, restart: bool) {
    self.pid.store(pid, Ordering::Release);
    self.started.store(now_ns, Ordering::Release);
    self.heartbeat.store(0, Ordering::Release);
    self.generation.fetch_add(1, Ordering::AcqRel);
    if restart {
      self.restarts.fetch_add(1, Ordering::AcqRel);
    }
    self.state.store(State::Running as u64, Ordering::Release);
  }

  /// The anchor records an exit.
  pub fn record_exit(&self, crash_loop: bool) {
    self.pid.store(0, Ordering::Release);
    let state = if crash_loop {
      State::CrashLoop
    } else {
      State::Stopped
    };
    self.state.store(state as u64, Ordering::Release);
  }

  /// The health signal `daemon.alive` as the anchor observes it: running, and the heartbeat
  /// no older than `budget_ns` by the daemon's clock reading `daemon_now_ns`; the freshness is
  /// how old the heartbeat is.
  pub fn alive(&self, daemon_now_ns: u64, budget_ns: u64) -> (bool, u64) {
    let beat = self.heartbeat_ns();
    let age = daemon_now_ns.saturating_sub(beat);
    (
      self.state() == State::Running && beat > 0 && age <= budget_ns,
      age,
    )
  }
}

impl AnchorSegment {
  /// Creates the segment for `identity` with `geometry`, the header written under the seqlock
  /// rule, the supervision block zeroed.
  pub fn create(
    name: &str,
    identity: &Identity,
    geometry: Geometry,
  ) -> Result<AnchorSegment, AnchorError> {
    let total = usize::try_from(geometry.total_bytes()).map_err(|_| AnchorError::Geometry {
      reason: "total length overflows",
    })?;
    let mut object = SharedObject::create(name, total)?;
    let bytes = object.bytes_mut();
    put(bytes, AT_GENERATION, &1u64.to_le_bytes());
    put(bytes, AT_MAGIC, &MAGIC.to_le_bytes());
    put(bytes, AT_VERSION, &LAYOUT_VERSION.to_le_bytes());
    put(bytes, AT_IDENTITY, &identity.hash());
    put(bytes, AT_TOTAL, &geometry.total_bytes().to_le_bytes());
    put(bytes, AT_GEOMETRY, &geometry.encode());
    put(bytes, AT_GENERATION, &2u64.to_le_bytes());
    let segment = AnchorSegment {
      object,
      geometry,
      content: None,
    };
    segment.init_rings();
    Ok(segment)
  }

  /// Adds the content object that backs a shard's volume storage in anchor-owned RAM (§4.8): a
  /// shared memory object of `content_bytes` named `content_name`. The process that creates it (the
  /// supervisor) holds it, so it — and an agent's writes in it — survive a daemon restart; a
  /// restarted daemon opens it from [`AnchorSegment::handoff_env`] through
  /// [`AnchorSegment::open_content`] and maps it as its store's content arena. This is the fix for
  /// a restart recreating scratch content empty (BUG-11): the bytes no longer live in a private
  /// mapping the exiting process takes with it.
  pub fn with_content(
    mut self,
    content_name: &str,
    content_bytes: usize,
  ) -> Result<AnchorSegment, AnchorError> {
    self.content = Some(SharedObject::create(content_name, content_bytes.max(1))?);
    Ok(self)
  }

  /// Opens the content object the anchor handed off, read from the daemon's `env`, or `None` when
  /// none was provided (a build or config without anchor-backed storage). The parse mirrors the
  /// metadata handoff: a descriptor on Linux, a name elsewhere.
  pub fn open_content(env: &[(String, String)]) -> Option<Result<SharedObject, AnchorError>> {
    let raw = env.iter().find(|(k, _)| k == ENV_CONTENT).map(|(_, v)| v)?;
    let len: usize = env
      .iter()
      .find(|(k, _)| k == ENV_CONTENT_LEN)
      .and_then(|(_, v)| v.parse().ok())?;
    let handoff = match raw.parse::<i32>() {
      Ok(fd) if cfg!(target_os = "linux") => Handoff::Descriptor(fd),
      _ => Handoff::Name(raw.clone()),
    };
    Some(SharedObject::open(&handoff, len).map_err(AnchorError::from))
  }

  /// Attaches to a segment another process created, from its handoff and length, checking
  /// the header against this machine.
  pub fn attach(
    handoff: &Handoff,
    len: usize,
    identity: &Identity,
  ) -> Result<AnchorSegment, AnchorError> {
    let object = SharedObject::open(handoff, len)?;
    let bytes = object.bytes();
    if bytes.len() < HEADER_BYTES {
      return Err(AnchorError::Layout {
        reason: "shorter than its header",
      });
    }
    if read_u32(bytes, AT_MAGIC) != MAGIC {
      return Err(AnchorError::Layout {
        reason: "wrong magic",
      });
    }
    if read_u32(bytes, AT_VERSION) != LAYOUT_VERSION {
      return Err(AnchorError::Layout {
        reason: "wrong layout version",
      });
    }
    let generation = object.atomic_u64(AT_GENERATION)?.load(Ordering::Acquire);
    if !generation.is_multiple_of(2) {
      return Err(AnchorError::Layout {
        reason: "the creator is still writing the header",
      });
    }
    let cached = &bytes[AT_IDENTITY..AT_IDENTITY + IDENTITY_BYTES];
    if cached != identity.hash() {
      return Err(AnchorError::Identity {
        cached: hex(cached),
        current: hex(&identity.hash()),
      });
    }
    let mut encoded = [0u8; GEOMETRY_BYTES];
    encoded.copy_from_slice(&bytes[AT_GEOMETRY..AT_GEOMETRY + GEOMETRY_BYTES]);
    let geometry = Geometry::decode(&encoded);
    let total = read_u64(bytes, AT_TOTAL);
    if total != geometry.total_bytes() || usize::try_from(total).ok() != Some(len) {
      return Err(AnchorError::Geometry {
        reason: "the mapped length is not the geometry's",
      });
    }
    Ok(AnchorSegment {
      object,
      geometry,
      content: None,
    })
  }

  /// Attaches from the environment the anchor gave the daemon.
  pub fn attach_from_env(identity: &Identity) -> Result<AnchorSegment, AnchorError> {
    let handoff = std::env::var(ENV_HANDOFF).map_err(|_| AnchorError::Layout {
      reason: "no handoff in the environment",
    })?;
    let len: usize = std::env::var(ENV_LEN)
      .ok()
      .and_then(|l| l.parse().ok())
      .ok_or(AnchorError::Layout {
        reason: "no length in the environment",
      })?;
    let handoff = match handoff.parse::<i32>() {
      Ok(fd) if cfg!(target_os = "linux") => Handoff::Descriptor(fd),
      _ => Handoff::Name(handoff),
    };
    Self::attach(&handoff, len, identity)
  }

  /// The handoff and the length an attach in this process (or a child) needs.
  pub fn handoff(&self) -> Result<(Handoff, usize), AnchorError> {
    Ok((self.object.handoff()?, self.object.len()))
  }

  /// The environment a child needs to attach.
  pub fn handoff_env(&self) -> Result<Vec<(String, String)>, AnchorError> {
    let handoff = match self.object.handoff()? {
      Handoff::Descriptor(fd) => fd.to_string(),
      Handoff::Name(name) => name,
    };
    let mut env = vec![
      (ENV_HANDOFF.to_owned(), handoff),
      (ENV_LEN.to_owned(), self.object.len().to_string()),
    ];
    // Hand off the content object too, so a restarted daemon re-maps the same anchor-owned RAM and
    // its volume content is still there (§4.8). Absent when this anchor has no content object.
    if let Some(content) = &self.content {
      let content_handoff = match content.handoff()? {
        Handoff::Descriptor(fd) => fd.to_string(),
        Handoff::Name(name) => name,
      };
      env.push((ENV_CONTENT.to_owned(), content_handoff));
      env.push((ENV_CONTENT_LEN.to_owned(), content.len().to_string()));
    }
    Ok(env)
  }

  /// The geometry.
  pub fn geometry(&self) -> Geometry {
    self.geometry
  }

  /// The mapped length.
  pub fn len(&self) -> usize {
    self.object.len()
  }

  /// Whether the map is empty (never, for a created segment).
  pub fn is_empty(&self) -> bool {
    self.object.is_empty()
  }

  /// The content object's handoff and length, for a caller (a test playing the anchor, or a
  /// supervisor) that must hand it to a restarted daemon alongside the segment's; `None` when there
  /// is no content object.
  pub fn content_handoff(&self) -> Result<Option<(Handoff, usize)>, AnchorError> {
    match &self.content {
      Some(object) => Ok(Some((object.handoff()?, object.len()))),
      None => Ok(None),
    }
  }

  /// Adopts a content object a daemon opened from its handoff after attaching the segment, so the
  /// segment carries it into [`AnchorSegment::handoff_env`] for the daemon's shard children (§4.8).
  pub fn adopt_content(&mut self, object: SharedObject) {
    self.content = Some(object);
  }

  /// The content object, if any (a shard reads and publishes its recovery image through it).
  pub fn content(&self) -> Option<&SharedObject> {
    self.content.as_ref()
  }

  /// Locks the segment into RAM.
  pub fn lock(&mut self) -> Result<(), AnchorError> {
    Ok(self.object.lock()?)
  }

  fn spec(&self, kind: RegionKind) -> Result<RegionSpec, AnchorError> {
    self.geometry.region(kind).ok_or(AnchorError::Geometry {
      reason: "no such region",
    })
  }

  fn range(&self, spec: RegionSpec) -> Result<std::ops::Range<usize>, AnchorError> {
    let start = usize::try_from(spec.offset).map_err(|_| AnchorError::Geometry {
      reason: "region offset overflows",
    })?;
    let end = start
      .checked_add(
        usize::try_from(spec.len).map_err(|_| AnchorError::Geometry {
          reason: "region length overflows",
        })?,
      )
      .ok_or(AnchorError::Geometry {
        reason: "region end overflows",
      })?;
    if end > self.object.len() {
      return Err(AnchorError::Geometry {
        reason: "region past the segment",
      });
    }
    Ok(start..end)
  }

  fn word_at(&self, spec: RegionSpec, at: usize) -> Result<&AtomicU64, AnchorError> {
    let range = self.range(spec)?;
    let offset = range.start.checked_add(at).ok_or(AnchorError::Geometry {
      reason: "word offset overflows",
    })?;
    if offset + size_of::<u64>() > range.end {
      return Err(AnchorError::Geometry {
        reason: "word past its region",
      });
    }
    Ok(self.object.atomic_u64(offset)?)
  }

  /// The supervision block.
  pub fn supervision(&self) -> Result<Supervision<'_>, AnchorError> {
    let spec = self.spec(RegionKind::Supervision)?;
    Ok(Supervision {
      pid: self.word_at(spec, SUP_PID)?,
      heartbeat: self.word_at(spec, SUP_HEARTBEAT)?,
      generation: self.word_at(spec, SUP_GENERATION)?,
      restarts: self.word_at(spec, SUP_RESTARTS)?,
      state: self.word_at(spec, SUP_STATE)?,
      started: self.word_at(spec, SUP_STARTED)?,
    })
  }

  /// The bytes of a region, for its single owner.
  pub fn region_bytes(&self, kind: RegionKind) -> Result<&[u8], AnchorError> {
    let range = self.range(self.spec(kind)?)?;
    Ok(&self.object.bytes()[range])
  }

  /// The bytes of a region, for its single owner.
  pub fn region_bytes_mut(&mut self, kind: RegionKind) -> Result<&mut [u8], AnchorError> {
    let range = self.range(self.spec(kind)?)?;
    Ok(&mut self.object.bytes_mut()[range])
  }

  /// A ring region's head, tail, capacity and sequence-base words (a log or the audit).
  pub fn ring_words(&self, kind: RegionKind) -> Result<[&AtomicU64; RING_WORDS], AnchorError> {
    let spec = self.spec(kind)?;
    Ok([
      self.word_at(spec, RING_HEAD)?,
      self.word_at(spec, RING_TAIL)?,
      self.word_at(spec, RING_CAPACITY)?,
      self.word_at(spec, RING_SEQ_BASE)?,
    ])
  }

  /// Every ring's capacity word set to its byte ring's length (once, at create).
  fn init_rings(&self) {
    let kinds: Vec<RegionKind> = self
      .geometry
      .regions()
      .iter()
      .map(|r| r.kind)
      .filter(|k| matches!(k, RegionKind::Log(_) | RegionKind::Audit))
      .collect();
    for kind in kinds {
      if let (Ok(spec), Ok(words)) = (self.spec(kind), self.ring_words(kind)) {
        let capacity = spec
          .len
          .saturating_sub(u64::try_from(crate::layout::RING_BYTES).unwrap_or(u64::MAX));
        words[2].store(capacity, Ordering::Release);
      }
    }
  }

  /// Publishes a payload into a payload region (the profile, a snapshot slot, a landing
  /// slot) under the seqlock rule: the generation goes odd, the bytes and length land, the
  /// generation goes even.
  pub fn publish(&mut self, kind: RegionKind, payload: &[u8]) -> Result<(), AnchorError> {
    let spec = self.spec(kind)?;
    let capacity = usize::try_from(spec.len)
      .unwrap_or(usize::MAX)
      .saturating_sub(PAYLOAD_BYTES);
    if payload.len() > capacity {
      return Err(AnchorError::ProfileTooLarge {
        offered: payload.len(),
        capacity,
      });
    }
    let generation = self.word_at(spec, PAYLOAD_GENERATION)?;
    let start = generation.load(Ordering::Acquire);
    generation.store(start | 1, Ordering::Release);
    let bytes = self.region_bytes_mut(kind)?;
    put(
      bytes,
      PAYLOAD_LEN,
      &u64::try_from(payload.len())
        .unwrap_or(u64::MAX)
        .to_le_bytes(),
    );
    put(bytes, PAYLOAD_BYTES, payload);
    let generation = self.word_at(spec, PAYLOAD_GENERATION)?;
    generation.store((start | 1).wrapping_add(1), Ordering::Release);
    Ok(())
  }

  /// Reads a published payload back, refusing a torn one (the seqlock rule); `None` when
  /// nothing was published yet.
  pub fn read_published(&self, kind: RegionKind) -> Result<Option<Vec<u8>>, AnchorError> {
    let spec = self.spec(kind)?;
    let generation = self.word_at(spec, PAYLOAD_GENERATION)?;
    let before = generation.load(Ordering::Acquire);
    if before == 0 {
      return Ok(None);
    }
    if !before.is_multiple_of(2) {
      return Err(AnchorError::Layout {
        reason: "a writer is inside the payload",
      });
    }
    let bytes = self.region_bytes(kind)?;
    let len = usize::try_from(read_u64(bytes, PAYLOAD_LEN)).map_err(|_| AnchorError::Layout {
      reason: "payload length overflows",
    })?;
    let end = PAYLOAD_BYTES.checked_add(len).ok_or(AnchorError::Layout {
      reason: "payload length overflows",
    })?;
    if end > bytes.len() {
      return Err(AnchorError::Layout {
        reason: "payload length exceeds its region",
      });
    }
    let payload = bytes[PAYLOAD_BYTES..end].to_vec();
    if generation.load(Ordering::Acquire) != before {
      return Err(AnchorError::Layout {
        reason: "the payload changed while it was read",
      });
    }
    Ok(Some(payload))
  }
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
  bytes[at..at + value.len()].copy_from_slice(value);
}

fn read_u32(bytes: &[u8], at: usize) -> u32 {
  let mut word = [0u8; size_of::<u32>()];
  let n = word.len();
  word.copy_from_slice(&bytes[at..at + n]);
  u32::from_le_bytes(word)
}

fn read_u64(bytes: &[u8], at: usize) -> u64 {
  let mut word = [0u8; size_of::<u64>()];
  let n = word.len();
  word.copy_from_slice(&bytes[at..at + n]);
  u64::from_le_bytes(word)
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}
