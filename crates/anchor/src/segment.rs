//! The anchor segment: create, attach, and the views the anchor and the daemon use.

use std::sync::atomic::{AtomicU64, Ordering};

use slates_machine::facts::Identity;
use slates_mem::{Handoff, SparseObject, Width, WordRun, Words};

use crate::error::AnchorError;
use crate::layout::{
  AT_GENERATION, AT_GEOMETRY, AT_IDENTITY, AT_MAGIC, AT_TOTAL, AT_VERSION, GEOMETRY_BYTES,
  Geometry, HEADER_BYTES, IDENTITY_BYTES, ISSUER_SECRET_BYTES, LAYOUT_VERSION, MAGIC,
  PAYLOAD_BYTES, PAYLOAD_GENERATION, PAYLOAD_LEN, RING_CAPACITY, RING_HEAD, RING_SEQ_BASE,
  RING_TAIL, RegionKind, RegionSpec, SUP_GENERATION, SUP_HEARTBEAT, SUP_ISSUER, SUP_PID,
  SUP_RESTARTS, SUP_STARTED, SUP_STATE, SUP_STOP, SUP_STOP_BY, State,
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

/// Format: the supervision block's consecutive 64-bit words, `SUP_PID` through `SUP_STOP_BY`.
const SUPERVISION_WORDS: usize = 8;

/// The segment's declared layout (AUD-29-09), derived from its geometry: the header's seqlock word; the
/// supervision block's words and its issuer secret (racy: the daemon rewrites it while a `slates grant`
/// command may copy it); each payload region's generation word and, after it, its length and body as a
/// racy span (the seqlock rule: a reader copies while a writer may be rewriting, then checks the
/// generation); each ring region's four words, its data plain (written by the daemon, read by a later one).
/// The header's other fields and a ring's data are plain. Until 2026-09-30 the header's generation word
/// was written as plain bytes and read as an atomic, and every region handed out byte slices.
fn segment_words(geometry: &Geometry) -> Words {
  let mut words = Words::new().with(WordRun::one(AT_GENERATION, Width::U64));
  for region in geometry.regions() {
    let at = usize::try_from(region.offset).unwrap_or(usize::MAX);
    let len = usize::try_from(region.len).unwrap_or(0);
    words = match region.kind {
      RegionKind::Supervision => words
        .with(WordRun::strided(
          at.saturating_add(SUP_PID),
          size_of::<u64>(),
          SUPERVISION_WORDS,
          Width::U64,
        ))
        .with(WordRun::racy(
          at.saturating_add(SUP_ISSUER),
          ISSUER_SECRET_BYTES,
        )),
      RegionKind::Log(_) | RegionKind::Audit => words.with(WordRun::strided(
        at.saturating_add(RING_HEAD),
        size_of::<u64>(),
        RING_WORDS,
        Width::U64,
      )),
      RegionKind::Profile
      | RegionKind::Snapshot(..)
      | RegionKind::Consensus(_)
      | RegionKind::Landing(_) => words
        .with(WordRun::one(
          at.saturating_add(PAYLOAD_GENERATION),
          Width::U64,
        ))
        .with(WordRun::racy(
          at.saturating_add(PAYLOAD_LEN),
          len.saturating_sub(PAYLOAD_LEN),
        )),
    };
  }
  words
}

/// The mapped anchor segment: a sparse object (`slates_mem::SparseObject`), backed only where its
/// regions are touched — a log ring's written span, a published slot — so a layout sized by the design's
/// bounds costs RAM only for what it holds, on Windows as on Linux and macOS.
pub struct AnchorSegment {
  object: SparseObject,
  geometry: Geometry,
  /// The content object the supervisor creates and holds so a shard's volume storage lives in
  /// anchor-owned RAM (§4.8): `Some` on the process that created it (the supervisor keeps it alive
  /// across daemon restarts), `None` on a process that only attached the metadata object — that
  /// process opens the content object from the handoff env ([`AnchorSegment::open_content`]).
  content: Option<SparseObject>,
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
  stop: &'a AtomicU64,
  stop_by: &'a AtomicU64,
}

impl Supervision<'_> {
  /// The daemon's pid, zero when none.
  pub fn pid(&self) -> u64 {
    self.pid.load(Ordering::Acquire)
  }

  /// The daemon's last heartbeat in the host's common monotonic clock domain.
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

  /// When the current daemon started, in the same host clock domain as its heartbeat.
  pub fn started_ns(&self) -> u64 {
    self.started.load(Ordering::Acquire)
  }

  /// The anchor asks the daemon to stop gracefully (`layout::SUP_STOP`): `now_ns` in the heartbeat's host
  /// domain, never zero (a zero reading is taken as one nanosecond), so the request always reads as made.
  pub fn request_stop(&self, now_ns: u64) {
    self.stop.store(now_ns.max(1), Ordering::Release);
  }

  /// When the anchor asked the daemon to stop, if it has since this daemon's start.
  pub fn stop_requested_at(&self) -> Option<u64> {
    Some(self.stop.load(Ordering::Acquire)).filter(|at| *at != 0)
  }

  /// The daemon acknowledges a stop request with the deadline it will have exited by
  /// (`layout::SUP_STOP_BY`), in the heartbeat's host domain; never zero.
  pub fn declare_stop_by(&self, deadline_ns: u64) {
    self.stop_by.store(deadline_ns.max(1), Ordering::Release);
  }

  /// The deadline the daemon declared for its stop, once it has acknowledged one.
  pub fn stop_by(&self) -> Option<u64> {
    Some(self.stop_by.load(Ordering::Acquire)).filter(|at| *at != 0)
  }

  /// The anchor records a start. A new daemon starts with no stop requested or declared.
  pub fn record_start(&self, pid: u64, now_ns: u64, restart: bool) {
    self.stop.store(0, Ordering::Release);
    self.stop_by.store(0, Ordering::Release);
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
  /// no older than `budget_ns` by the observer's host clock reading `now_ns`; the freshness is
  /// how old the heartbeat is.
  pub fn alive(&self, now_ns: u64, budget_ns: u64) -> (bool, u64) {
    let beat = self.heartbeat_ns();
    let age = now_ns.saturating_sub(beat);
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
    let mut object = SparseObject::create(name, total, segment_words(&geometry))?;
    // The header under its seqlock: odd while written, even once whole.
    let generation = object.atomic_u64(AT_GENERATION)?;
    generation.store(1, Ordering::Release);
    object.write(AT_MAGIC, &MAGIC.to_le_bytes())?;
    object.write(AT_VERSION, &LAYOUT_VERSION.to_le_bytes())?;
    object.write(AT_IDENTITY, &identity.hash())?;
    object.write(AT_TOTAL, &geometry.total_bytes().to_le_bytes())?;
    object.write(AT_GEOMETRY, &geometry.encode())?;
    object
      .atomic_u64(AT_GENERATION)?
      .store(2, Ordering::Release);
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
    // The content object has one accessor at a time (the running daemon): no concurrent words.
    self.content = Some(SparseObject::create(
      content_name,
      content_bytes.max(1),
      Words::new(),
    )?);
    Ok(self)
  }

  /// Opens the content object the anchor handed off, read from the daemon's `env`, or `None` when
  /// none was provided (a build or config without anchor-backed storage). The parse mirrors the
  /// metadata handoff: a descriptor on Linux, a name elsewhere.
  pub fn open_content(env: &[(String, String)]) -> Option<Result<SparseObject, AnchorError>> {
    let (handoff, len) = Self::content_handoff_in(env)?;
    Some(SparseObject::open(&handoff, len, Words::new()).map_err(AnchorError::from))
  }

  /// The content object's handoff and length as the anchor gave them in the daemon's `env`, or `None` when
  /// none was provided: what [`AnchorSegment::open_content`] opens, and what a shard maps its arena range of
  /// (A-64).
  pub fn content_handoff_in(env: &[(String, String)]) -> Option<(Handoff, usize)> {
    let raw = env.iter().find(|(k, _)| k == ENV_CONTENT).map(|(_, v)| v)?;
    let len: usize = env
      .iter()
      .find(|(k, _)| k == ENV_CONTENT_LEN)
      .and_then(|(_, v)| v.parse().ok())?;
    let handoff = match raw.parse::<i32>() {
      Ok(fd) if cfg!(target_os = "linux") => Handoff::Descriptor(fd),
      _ => Handoff::Name(raw.clone()),
    };
    Some((handoff, len))
  }

  /// Attaches to a segment another process created, from its handoff and length, checking
  /// the header against this machine.
  pub fn attach(
    handoff: &Handoff,
    len: usize,
    identity: &Identity,
  ) -> Result<AnchorSegment, AnchorError> {
    if len < HEADER_BYTES {
      return Err(AnchorError::Layout {
        reason: "shorter than its header",
      });
    }
    // The header first, with only its seqlock word declared: the geometry that lays out the rest is in it.
    let header_words = Words::new().with(WordRun::one(AT_GENERATION, Width::U64));
    let object = SparseObject::open(handoff, len, header_words)?;
    let generation = object.atomic_u64(AT_GENERATION)?.load(Ordering::Acquire);
    if generation == 0 || !generation.is_multiple_of(2) {
      return Err(AnchorError::Layout {
        reason: "the creator is still writing the header",
      });
    }
    let mut header = [0u8; HEADER_BYTES];
    object.read(0, header.get_mut(..AT_GENERATION).unwrap_or_default())?;
    let after = AT_GENERATION.saturating_add(size_of::<u64>());
    object.read(after, header.get_mut(after..).unwrap_or_default())?;
    let bytes = &header[..];
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
    let cached = bytes
      .get(AT_IDENTITY..AT_IDENTITY.saturating_add(IDENTITY_BYTES))
      .unwrap_or_default();
    if cached != identity.hash() {
      return Err(AnchorError::Identity {
        cached: hex(cached),
        current: hex(&identity.hash()),
      });
    }
    let encoded = bytes
      .get(AT_GEOMETRY..AT_GEOMETRY.saturating_add(GEOMETRY_BYTES))
      .and_then(|field| <[u8; GEOMETRY_BYTES]>::try_from(field).ok())
      .ok_or(AnchorError::Layout {
        reason: "shorter than its header",
      })?;
    let geometry = Geometry::decode(&encoded);
    let total = read_u64(bytes, AT_TOTAL);
    if total != geometry.total_bytes() || usize::try_from(total).ok() != Some(len) {
      return Err(AnchorError::Geometry {
        reason: "the mapped length is not the geometry's",
      });
    }
    // Reopened with the geometry's full layout: a header whose geometry overlaps words is refused here.
    let object = SparseObject::open(handoff, len, segment_words(&geometry))?;
    Ok(AnchorSegment {
      object,
      geometry,
      content: None,
    })
  }

  /// Attaches the metadata and content objects from the environment the anchor gave the daemon.
  /// Keeping both on the segment preserves the content handoff to its shard children (§4.8).
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
    let mut segment = Self::attach(&handoff, len, identity)?;
    let content_env: Vec<(String, String)> = [ENV_CONTENT, ENV_CONTENT_LEN]
      .into_iter()
      .filter_map(|key| std::env::var(key).ok().map(|value| (key.to_owned(), value)))
      .collect();
    if !content_env.is_empty() {
      segment.adopt_content(Self::open_content(&content_env).ok_or(AnchorError::Layout {
        reason: "incomplete content handoff in the environment",
      })??);
    }
    Ok(segment)
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
  pub fn adopt_content(&mut self, object: SparseObject) {
    self.content = Some(object);
  }

  /// The content object, if any (a shard reads and publishes its recovery image through it).
  pub fn content(&self) -> Option<&SparseObject> {
    self.content.as_ref()
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
    if offset.saturating_add(size_of::<u64>()) > range.end {
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
      stop: self.word_at(spec, SUP_STOP)?,
      stop_by: self.word_at(spec, SUP_STOP_BY)?,
    })
  }

  /// Publishes the daemon's grant-issuer secret into the supervision block (§4.13; `SUP_ISSUER`): the
  /// daemon mints it at every start and writes it here, so a `slates grant` command running as the
  /// anchor's own user — the only other mapper of this page besides the supervisor — can prove issuer
  /// authority with a keyed hash the daemon recomputes. Written in one pass after the supervision
  /// words; readers copy it out whole ([`issuer_secret`](AnchorSegment::issuer_secret)).
  pub fn publish_issuer_secret(
    &mut self,
    secret: &[u8; ISSUER_SECRET_BYTES],
  ) -> Result<(), AnchorError> {
    let at = self.region_span(RegionKind::Supervision, SUP_ISSUER, ISSUER_SECRET_BYTES)?;
    Ok(self.object.write_racy(at, secret)?)
  }

  /// The daemon's published grant-issuer secret, copied out of the supervision block; all zero until a
  /// daemon has started under this anchor (a fresh segment holds no authority, so no proof verifies).
  pub fn issuer_secret(&self) -> Result<[u8; ISSUER_SECRET_BYTES], AnchorError> {
    let mut out = [0u8; ISSUER_SECRET_BYTES];
    let at = self.region_span(RegionKind::Supervision, SUP_ISSUER, ISSUER_SECRET_BYTES)?;
    self.object.read_racy(at, &mut out)?;
    Ok(out)
  }

  /// A region's length in bytes.
  pub fn region_len(&self, kind: RegionKind) -> Result<usize, AnchorError> {
    let range = self.range(self.spec(kind)?)?;
    Ok(range.end.saturating_sub(range.start))
  }

  /// Copies the `into.len()` plain bytes at `at` within a region out, for the region's single owner (a
  /// ring's data). Only these bytes are backed (the segment is sparse); a span past the region is refused,
  /// never read from its neighbour, and a span over a word or a racy payload is refused by the layout.
  pub fn region_read(
    &self,
    kind: RegionKind,
    at: usize,
    into: &mut [u8],
  ) -> Result<(), AnchorError> {
    let start = self.region_span(kind, at, into.len())?;
    Ok(self.object.read(start, into)?)
  }

  /// Copies `from` into a region's plain bytes at `at`, for the region's single owner.
  pub fn region_write(
    &mut self,
    kind: RegionKind,
    at: usize,
    from: &[u8],
  ) -> Result<(), AnchorError> {
    let start = self.region_span(kind, at, from.len())?;
    Ok(self.object.write(start, from)?)
  }

  /// Copies a payload region's racy bytes at `at` out (its length and body, as the seqlock reader does).
  pub fn region_read_racy(
    &self,
    kind: RegionKind,
    at: usize,
    into: &mut [u8],
  ) -> Result<(), AnchorError> {
    let start = self.region_span(kind, at, into.len())?;
    Ok(self.object.read_racy(start, into)?)
  }

  /// Copies `from` into a payload region's racy bytes at `at` (as the seqlock writer does).
  pub fn region_write_racy(
    &mut self,
    kind: RegionKind,
    at: usize,
    from: &[u8],
  ) -> Result<(), AnchorError> {
    let start = self.region_span(kind, at, from.len())?;
    Ok(self.object.write_racy(start, from)?)
  }

  /// A payload region's seqlock generation word.
  pub fn payload_generation(&self, kind: RegionKind) -> Result<&AtomicU64, AnchorError> {
    self.word_at(self.spec(kind)?, PAYLOAD_GENERATION)
  }

  /// The segment offset of `[at, at + len)` inside a region, refused when it leaves the region.
  fn region_span(&self, kind: RegionKind, at: usize, len: usize) -> Result<usize, AnchorError> {
    let range = self.range(self.spec(kind)?)?;
    let past_region = AnchorError::Geometry {
      reason: "a span past its region",
    };
    let start = range.start.checked_add(at).ok_or(past_region.clone())?;
    let end = start.checked_add(len).ok_or(past_region.clone())?;
    if end > range.end {
      return Err(past_region);
    }
    Ok(start)
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
    self.region_write_racy(
      kind,
      PAYLOAD_LEN,
      &u64::try_from(payload.len())
        .unwrap_or(u64::MAX)
        .to_le_bytes(),
    )?;
    self.region_write_racy(kind, PAYLOAD_BYTES, payload)?;
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
      return Err(AnchorError::PublicationInProgress);
    }
    let mut len_word = [0u8; size_of::<u64>()];
    self.region_read_racy(kind, PAYLOAD_LEN, &mut len_word)?;
    let len = usize::try_from(u64::from_le_bytes(len_word)).map_err(|_| AnchorError::Layout {
      reason: "payload length overflows",
    })?;
    let end = PAYLOAD_BYTES.checked_add(len).ok_or(AnchorError::Layout {
      reason: "payload length overflows",
    })?;
    if end > self.region_len(kind)? {
      return Err(AnchorError::Layout {
        reason: "payload length exceeds its region",
      });
    }
    let mut payload = vec![0u8; len];
    self.region_read_racy(kind, PAYLOAD_BYTES, &mut payload)?;
    if generation.load(Ordering::Acquire) != before {
      return Err(AnchorError::Layout {
        reason: "the payload changed while it was read",
      });
    }
    Ok(Some(payload))
  }
}

/// The little-endian `u32` at `at`, or zero where the header does not reach (every caller names a fixed
/// field inside a header it has checked the length of; a zero magic or version is refused as wrong).
fn read_u32(bytes: &[u8], at: usize) -> u32 {
  bytes
    .get(at..at.saturating_add(size_of::<u32>()))
    .and_then(|field| <[u8; size_of::<u32>()]>::try_from(field).ok())
    .map_or(0, u32::from_le_bytes)
}

/// The little-endian `u64` at `at`, or zero where the header does not reach (a zero total or length is
/// refused by the checks that read it).
fn read_u64(bytes: &[u8], at: usize) -> u64 {
  bytes
    .get(at..at.saturating_add(size_of::<u64>()))
    .and_then(|field| <[u8; size_of::<u64>()]>::try_from(field).ok())
    .map_or(0, u64::from_le_bytes)
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|b| format!("{b:02x}")).collect()
}
