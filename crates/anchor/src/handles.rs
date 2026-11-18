//! The Windows hold channel's formats (A-198; §4.6 "Windows", A-61's counterpart): the messages a daemon and the anchor
//! exchange to hold a daemon's handles across its death, and the environment variable that hands every held handle to
//! the next daemon. Windows has no `SCM_RIGHTS`, so a handle cannot travel with its message: a hold names the daemon's
//! handle values, the anchor duplicates each out of its child (it holds the child's process handle from the spawn) and
//! answers, and the daemon commits the record that names the hold only once the anchor has answered that it holds it.
//! A daemon that dies before the anchor took its handles leaves nothing held (the duplicate out of a dead process is
//! refused), and one that dies after leaves a hold whose record never committed, which the next daemon releases. So a
//! committed record never names a hold the anchor was not given, the guarantee `SCM_RIGHTS` gives the Unix channel
//! (`crate::held`).
//!
//! Three kinds of entry are held ([`admits`]): a WinFsp mount's handles (A-198); a Shards stream's resync end (A-423,
//! for A-418's Windows resync), whose record is the stream's session: its token, the owner view its guest named, its
//! kernel's form and the process that holds the stream; and a Shards stream's DAX section (A-448, A-365's object on
//! Windows), whose record names the stream's attachment and the file. A session moves while the stream lives (the guest
//! names its owner, its `INIT` is answered, a restore runs, a holder is named), so a daemon replaces a held entry's
//! record with [`HandleMessage::Record`], answered as a hold is, where the Unix channel sends a new session
//! (`crate::held`'s `Session`); the record names no handle, so nothing is duplicated for it.
//!
//! **Each kind its own bound** ([`HoldBounds`], A-448): the entries an attachment holds (a mount, a resync end) one per
//! live attachment, and the DAX sections one per range a shard's fill budget can count. A guest that maps many files
//! is refused [`Reason::Bound`] at its own bound and never crowds a mount or a stream's resync end out of the anchor.
//!
//! Every message is [`MESSAGE_BYTES`] long, checksum-verified and checked by kind, counts and lengths before anything is kept: the channel
//! crosses a process boundary, so its bytes are external input, and so is the environment a daemon parses. The
//! transport is `crate::held_windows`; this module is pure and cfg-free, so it is tested on every host.

/// Format: the environment variable carrying the hold channel's daemon end and every held entry to a spawned daemon:
/// `CHANNEL_HEX` then, for each entry, `;ID_HEX:WHAT_HEX:H1_HEX,H2_HEX,...:RECORD_HEX` (`-` for an empty record), then
/// `#CRC32C_HEX` over the preceding text. Handles are inherited across the spawn at the values named.
pub const ENV_HANDLES: &str = "SLATES_ANCHOR_HANDLES";

/// Format: what a hold holds, its `what` byte: a WinFsp mount's volume control handle and ledger (A-198), the volume
/// keeping its Mount Manager drive (A-369).
pub const WHAT_WINFSP_MOUNT: u8 = 1;

/// Format: the handles a WinFsp mount's hold names: its volume control handle and its ledger (A-369; A-198's held a
/// third, the drive's temporary link, which a Mount Manager drive does not have).
pub const WINFSP_MOUNT_HANDLES: usize = 2;

/// Format: what a hold holds, its `what` byte: a Shards stream's resync end (A-423; A-316's channel on Windows, A-418):
/// the daemon's end of the stream's message-mode resync pipe, its record the stream's session, which a successor reads
/// to hand the device a fresh stream.
pub const WHAT_SHARDS_RESYNC: u8 = 2;

/// Format: the handles a Shards stream's resync hold names: the daemon's end of its resync pipe.
pub const SHARDS_RESYNC_HANDLES: usize = 1;

/// Format: what a hold holds, its `what` byte: a Shards stream's DAX section (A-448; A-365's object on Windows): a
/// duplicate of the confined section one file of the stream's guest is mapped from, which maps read and write and may
/// ask its size, its record the stream's attachment and the file, which a successor adopts the section by. Its id is the
/// section's backing named apart from every attachment's id (the server's `dax_hold::section_id`), since a release and
/// an answer name an entry by id alone.
pub const WHAT_SHARDS_DAX: u8 = 3;

/// Format: the handles a Shards stream's DAX section hold names: the section.
pub const SHARDS_DAX_HANDLES: usize = 1;

/// Format: the room one hold's message has for handles: three, A-198's WinFsp mount, kept since A-369 so the message's
/// layout, which an anchor and a daemon built apart across an upgrade both read, does not move; a hold names one to
/// this many.
pub const MAX_HANDLES: usize = 3;

/// Format: a WinFsp mount's largest handoff record: `FSP_FSCTL_VOLUME_NAME_SIZEMAX` (64 + 192 UTF-16 units), its
/// two-unit drive name (`X:`), and one access-flags byte. The record is opaque to the anchor. It bounds every kind's
/// record: a Shards stream's session is far shorter (A-418).
pub const RECORD_CAP: usize = (64 + 192 + 2) * size_of::<u16>() + size_of::<u8>();

/// Whether the anchor admits a hold of `what` naming `count` handles: a WinFsp mount one to [`MAX_HANDLES`] (its
/// daemon names [`WINFSP_MOUNT_HANDLES`]; the room stays as A-198 left it, for a daemon built apart across an upgrade),
/// a Shards stream's resync end exactly [`SHARDS_RESYNC_HANDLES`], a DAX section exactly [`SHARDS_DAX_HANDLES`], and no
/// other kind at all.
pub fn admits(what: u8, count: usize) -> bool {
  match what {
    WHAT_WINFSP_MOUNT => (1..=MAX_HANDLES).contains(&count),
    WHAT_SHARDS_RESYNC => count == SHARDS_RESYNC_HANDLES,
    WHAT_SHARDS_DAX => count == SHARDS_DAX_HANDLES,
    _ => false,
  }
}

/// The anchor's bounds on the entries it holds for a daemon on Windows (A-198, A-448), one for each class of entry,
/// each derived by the daemon's configuration and refused [`Reason::Bound`] at its own: so a guest that maps many files
/// never crowds a mount or a stream's resync end out of the anchor, and no class grows past what its daemon can make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HoldBounds {
  /// The entries an attachment holds, a WinFsp mount's or a Shards stream's resync end: one per live attachment the
  /// daemon's shards can hold (the server's `held_device_bound`).
  pub attachments: usize,
  /// The Shards streams' DAX sections: one per range the daemon's shards' fill budgets can count, since each section
  /// held counts at least one range against its shard's (the server's `held_dax_bound`, A-365's derivation).
  pub sections: usize,
}

impl HoldBounds {
  /// The bound entries of `what` are held against: a DAX section's own, and every other kind's the attachments'.
  pub fn of(&self, what: u8) -> usize {
    if what == WHAT_SHARDS_DAX {
      self.sections
    } else {
      self.attachments
    }
  }

  /// Whether `what` is held against the same bound as `other`.
  pub fn shared(what: u8, other: u8) -> bool {
    (what == WHAT_SHARDS_DAX) == (other == WHAT_SHARDS_DAX)
  }

  /// Every entry the anchor may hold at once, of every class: the most a handoff names, which a daemon parses at most
  /// ([`parse_env`]'s bound), and the most messages one drain of the channel may meet.
  pub fn total(&self) -> usize {
    self.attachments.saturating_add(self.sections)
  }

  /// Whether one more entry of `what` fits, `held` of its class being held already.
  pub fn admits_another(&self, what: u8, held: usize) -> bool {
    held < self.of(what)
  }
}

/// Format: the channel's magic, distinguishing these messages from another inherited protocol.
const MAGIC: &[u8; 4] = b"SLHD";
/// Format: the message kind follows the magic.
const AT_KIND: usize = MAGIC.len();

/// Format: a message's kind byte at `AT_KIND`: a daemon's hold.
const KIND_HOLD: u8 = 1;
/// Format: see [`KIND_HOLD`]: a daemon's release of an entry it ended.
const KIND_RELEASE: u8 = 2;
/// Format: see [`KIND_HOLD`]: the anchor's answer that it holds an entry.
const KIND_HELD: u8 = 3;
/// Format: see [`KIND_HOLD`]: the anchor's answer that it refused an entry, with the reason's byte.
const KIND_REFUSED: u8 = 4;
/// Format: the anchor's acknowledgement after closing every handle of an entry (A-198).
const KIND_RELEASED: u8 = 5;
/// Format: a daemon's replacement of a held entry's record (A-423), laid out as a hold whose kind, count and handle
/// slots are zero.
const KIND_RECORD: u8 = 6;

/// Format: where the entry's id starts (after the kind byte).
const AT_ID: usize = AT_KIND + 1;
/// Format: the id's width.
const ID_BYTES: usize = size_of::<u64>();
/// Format: where the `what` byte (or a refusal's reason byte) sits.
const AT_WHAT: usize = AT_ID + ID_BYTES;
/// Format: where the handle count sits.
const AT_COUNT: usize = AT_WHAT + 1;
/// Format: where the handle values start, each a little-endian `u64`.
const AT_HANDLES: usize = AT_COUNT + 1;
/// Format: one handle value's width.
const HANDLE_BYTES: usize = size_of::<u64>();
/// Format: where the record's length sits, a little-endian `u16`, after room for [`MAX_HANDLES`] values.
const AT_RECORD_LEN: usize = AT_HANDLES + MAX_HANDLES * HANDLE_BYTES;
/// Format: the record length's width.
const RECORD_LEN_BYTES: usize = size_of::<u16>();
/// Format: where the record's bytes start.
const AT_RECORD: usize = AT_RECORD_LEN + RECORD_LEN_BYTES;
/// Format: CRC32C follows the whole record area, including its zero padding.
const AT_CHECKSUM: usize = AT_RECORD + RECORD_CAP;
/// Format: every message's length, including its CRC32C. Unused fields are zero.
pub const MESSAGE_BYTES: usize = AT_CHECKSUM + size_of::<u32>();
/// Format: hexadecimal digits in a u64 field.
const U64_HEX_DIGITS: usize = size_of::<u64>() * 2;
/// Format: hexadecimal digits in a CRC32C.
const CHECKSUM_HEX_DIGITS: usize = size_of::<u32>() * 2;
/// Format: every textual numeric field uses hexadecimal.
const HEX_RADIX: u32 = 16;
/// Derived: one textual entry's largest width: id and handles in hex, record bytes in hex, a what byte in hex,
/// and its semicolon, colons and handle separators.
const ENTRY_TEXT_CAP: usize =
  (MAX_HANDLES + 1) * U64_HEX_DIGITS + RECORD_CAP * 2 + size_of::<u8>() * 2 + MAX_HANDLES + 4;

/// Format: a refusal's reason byte: the anchor holds its bound of entries.
const REASON_BOUND: u8 = 1;
/// Format: see [`REASON_BOUND`]: a handle could not be duplicated out of the daemon.
const REASON_DUPLICATE: u8 = 2;
/// Format: see [`REASON_BOUND`]: the hold was not one of the defined shapes, or its id is held already.
const REASON_MALFORMED: u8 = 3;

/// Why the anchor refused a hold, as it answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
  /// The anchor holds its bound of entries.
  Bound,
  /// A handle could not be duplicated out of the daemon (the daemon gone, or the value not a handle of it).
  Duplicate,
  /// The hold was malformed, or named an id already held.
  Malformed,
}

impl Reason {
  fn byte(self) -> u8 {
    match self {
      Reason::Bound => REASON_BOUND,
      Reason::Duplicate => REASON_DUPLICATE,
      Reason::Malformed => REASON_MALFORMED,
    }
  }

  fn of(byte: u8) -> Option<Reason> {
    match byte {
      REASON_BOUND => Some(Reason::Bound),
      REASON_DUPLICATE => Some(Reason::Duplicate),
      REASON_MALFORMED => Some(Reason::Malformed),
      _ => None,
    }
  }
}

/// Why a message, a hold or the environment's handoff was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleRefusal {
  /// Bytes that are not one of the defined shapes.
  Malformed {
    /// What was wrong.
    reason: &'static str,
  },
  /// The anchor already holds its bound of entries.
  Bound {
    /// The bound.
    bound: usize,
  },
  /// The anchor answered that it refused the hold.
  Refused {
    /// Why.
    reason: Reason,
  },
  /// An operating-system call refused.
  Os {
    /// The call.
    call: &'static str,
    /// Its error code (`GetLastError`).
    code: u32,
  },
  /// A transfer did not finish within the caller's budget.
  TimedOut,
  /// The other process closed its channel end.
  Closed,
  /// The underlying IPC operation refused, with its original error intact.
  #[cfg(windows)]
  Ipc(slates_ipc::IpcError),
}

impl std::fmt::Display for HandleRefusal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      HandleRefusal::Malformed { reason } => write!(f, "malformed hold message: {reason}"),
      HandleRefusal::Bound { bound } => write!(f, "the anchor already holds its {bound} entries"),
      HandleRefusal::Refused { reason } => write!(f, "the anchor refused the hold: {reason:?}"),
      HandleRefusal::Os { call, code } => write!(f, "{call} refused: error {code}"),
      HandleRefusal::TimedOut => f.write_str("the hold transfer exceeded its caller's budget"),
      HandleRefusal::Closed => f.write_str("the hold channel's peer closed"),
      #[cfg(windows)]
      HandleRefusal::Ipc(error) => write!(f, "hold transport: {error}"),
    }
  }
}

fn malformed(reason: &'static str) -> HandleRefusal {
  HandleRefusal::Malformed { reason }
}

/// Shape: how many distinct refusals an anchor's log remembers. This module and the Windows channel name about 40
/// reasons, the Unix device channel (`crate::held`) about 25, and an OS refusal differs by its code as well, so the
/// table is above every kind a channel meets in practice; past it a new kind is counted and not logged, so the log
/// still has a bound whatever a daemon or the OS does.
const REFUSALS_REMEMBERED: usize = 64;

/// Which refusals the anchor has already logged: each distinct refusal once over the anchor's life, in a bounded table.
/// Logging on a change of kind would not be enough, since a daemon alternating two bad messages changes kind at every
/// message and the log would grow without bound. Every refusal is still counted by its caller. One table serves each
/// hold channel's refusals: the Windows hold's [`HandleRefusal`] and the Unix device hold's `held::HoldRefusal`
/// (A-213).
#[derive(Debug)]
pub struct RefusalLog<Refusal> {
  logged: Vec<Refusal>,
}

impl<Refusal> Default for RefusalLog<Refusal> {
  fn default() -> Self {
    RefusalLog { logged: Vec::new() }
  }
}

impl<Refusal: PartialEq + Clone> RefusalLog<Refusal> {
  /// Whether `refusal` should be logged now: true the first time it is met, while the table has room.
  pub fn first_sighting(&mut self, refusal: &Refusal) -> bool {
    if self.logged.len() >= REFUSALS_REMEMBERED || self.logged.contains(refusal) {
      return false;
    }
    self.logged.push(refusal.clone());
    true
  }
}

/// A message on the hold channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandleMessage {
  /// A daemon asks the anchor to hold `values`, handles in the daemon's table, as entry `id`.
  Hold {
    /// The entry's id (an attachment's).
    id: u64,
    /// What the handles are ([`WHAT_WINFSP_MOUNT`]).
    what: u8,
    /// The daemon's handle values, at least one and at most [`MAX_HANDLES`].
    values: Vec<u64>,
    /// The record, at most [`RECORD_CAP`] bytes.
    record: Vec<u8>,
  },
  /// A daemon ended entry `id`; the anchor closes its copies.
  Release {
    /// The entry's id.
    id: u64,
  },
  /// A daemon replaces held entry `id`'s record with `record` (A-423: a Shards stream's session as it moves), answered
  /// [`HandleMessage::Held`] once the anchor keeps it, or [`HandleMessage::Refused`] for an entry it does not hold.
  Record {
    /// The entry's id.
    id: u64,
    /// The new record, at most [`RECORD_CAP`] bytes.
    record: Vec<u8>,
  },
  /// The anchor holds entry `id`.
  Held {
    /// The entry's id.
    id: u64,
  },
  /// The anchor closed entry `id` (or already held none), so normal teardown may delete the volume.
  Released {
    /// The entry's id.
    id: u64,
  },
  /// The anchor refused entry `id`.
  Refused {
    /// The entry's id.
    id: u64,
    /// Why.
    reason: Reason,
  },
}

/// Encodes `message` into its fixed-length bytes; a hold outside the shapes (no handle, too many, a record past the
/// cap) is refused, never truncated.
pub fn encode(message: &HandleMessage) -> Result<[u8; MESSAGE_BYTES], HandleRefusal> {
  let mut bytes = [0u8; MESSAGE_BYTES];
  let (kind, id) = match message {
    HandleMessage::Hold { id, .. } => (KIND_HOLD, *id),
    HandleMessage::Record { id, .. } => (KIND_RECORD, *id),
    HandleMessage::Release { id } => (KIND_RELEASE, *id),
    HandleMessage::Held { id } => (KIND_HELD, *id),
    HandleMessage::Released { id } => (KIND_RELEASED, *id),
    HandleMessage::Refused { id, .. } => (KIND_REFUSED, *id),
  };
  put(&mut bytes, 0, MAGIC)?;
  put(&mut bytes, AT_KIND, &[kind])?;
  put(&mut bytes, AT_ID, &id.to_le_bytes())?;
  match message {
    HandleMessage::Hold {
      what,
      values,
      record,
      ..
    } => {
      if values.is_empty() || values.len() > MAX_HANDLES {
        return Err(malformed("a hold names one to MAX_HANDLES handles"));
      }
      if record.len() > RECORD_CAP {
        return Err(malformed("a hold's record is past RECORD_CAP"));
      }
      let count = u8::try_from(values.len()).map_err(|_| malformed("handle count"))?;
      let record_len = u16::try_from(record.len()).map_err(|_| malformed("record length"))?;
      put(&mut bytes, AT_WHAT, &[*what, count])?;
      for (slot, value) in values.iter().enumerate() {
        let at = slot
          .checked_mul(HANDLE_BYTES)
          .and_then(|offset| offset.checked_add(AT_HANDLES))
          .ok_or_else(|| malformed("handle offset"))?;
        put(&mut bytes, at, &value.to_le_bytes())?;
      }
      put(&mut bytes, AT_RECORD_LEN, &record_len.to_le_bytes())?;
      put(&mut bytes, AT_RECORD, record)?;
    }
    // A record's replacement names no kind, count or handle: those bytes stay zero.
    HandleMessage::Record { record, .. } => {
      if record.len() > RECORD_CAP {
        return Err(malformed("a record past RECORD_CAP"));
      }
      let record_len = u16::try_from(record.len()).map_err(|_| malformed("record length"))?;
      put(&mut bytes, AT_RECORD_LEN, &record_len.to_le_bytes())?;
      put(&mut bytes, AT_RECORD, record)?;
    }
    HandleMessage::Refused { reason, .. } => put(&mut bytes, AT_WHAT, &[reason.byte()])?,
    HandleMessage::Release { .. } | HandleMessage::Held { .. } | HandleMessage::Released { .. } => {
    }
  }
  let checksum = slates_wire::crc32c::crc32c(field(&bytes, 0, AT_CHECKSUM)?);
  put(&mut bytes, AT_CHECKSUM, &checksum.to_le_bytes())?;
  Ok(bytes)
}

/// Writes `value` into `bytes` at `at`, refused if it would not fit.
fn put(bytes: &mut [u8], at: usize, value: &[u8]) -> Result<(), HandleRefusal> {
  let end = at
    .checked_add(value.len())
    .ok_or_else(|| malformed("field offset"))?;
  bytes
    .get_mut(at..end)
    .ok_or_else(|| malformed("field past the message"))?
    .copy_from_slice(value);
  Ok(())
}

/// The `width` bytes of `bytes` at `at`.
fn field(bytes: &[u8], at: usize, width: usize) -> Result<&[u8], HandleRefusal> {
  let end = at
    .checked_add(width)
    .ok_or_else(|| malformed("field offset"))?;
  bytes
    .get(at..end)
    .ok_or_else(|| malformed("field past the message"))
}

fn u64_at(bytes: &[u8], at: usize) -> Result<u64, HandleRefusal> {
  let raw: [u8; size_of::<u64>()] = field(bytes, at, size_of::<u64>())?
    .try_into()
    .map_err(|_| malformed("u64 field"))?;
  Ok(u64::from_le_bytes(raw))
}

fn byte_at(bytes: &[u8], at: usize) -> Result<u8, HandleRefusal> {
  field(bytes, at, 1)?
    .first()
    .copied()
    .ok_or_else(|| malformed("byte field"))
}

/// Decodes one message, refusing any that is not exactly [`MESSAGE_BYTES`] long, of a known kind, with counts and
/// lengths inside their caps, and zero past what it uses (so a message has one encoding).
pub fn decode(bytes: &[u8]) -> Result<HandleMessage, HandleRefusal> {
  if bytes.len() != MESSAGE_BYTES {
    return Err(malformed("a message is MESSAGE_BYTES long"));
  }
  let raw: [u8; size_of::<u32>()] = field(bytes, AT_CHECKSUM, size_of::<u32>())?
    .try_into()
    .map_err(|_| malformed("checksum field"))?;
  if u32::from_le_bytes(raw) != slates_wire::crc32c::crc32c(field(bytes, 0, AT_CHECKSUM)?) {
    return Err(malformed("checksum mismatch"));
  }
  if field(bytes, 0, MAGIC.len())? != MAGIC {
    return Err(malformed("foreign channel magic"));
  }
  let kind = byte_at(bytes, AT_KIND)?;
  let id = u64_at(bytes, AT_ID)?;
  let (message, used) = match kind {
    KIND_HOLD => decode_hold(bytes, id)?,
    KIND_RECORD => decode_record(bytes, id)?,
    KIND_RELEASE => (HandleMessage::Release { id }, AT_WHAT),
    KIND_RELEASED => (HandleMessage::Released { id }, AT_WHAT),
    KIND_HELD => (HandleMessage::Held { id }, AT_WHAT),
    KIND_REFUSED => {
      let reason =
        Reason::of(byte_at(bytes, AT_WHAT)?).ok_or_else(|| malformed("refusal reason"))?;
      (HandleMessage::Refused { id, reason }, AT_COUNT)
    }
    _ => return Err(malformed("unknown kind")),
  };
  if bytes
    .get(used..AT_CHECKSUM)
    .is_some_and(|rest| rest.iter().any(|byte| *byte != 0))
  {
    return Err(malformed("nonzero bytes past the message"));
  }
  Ok(message)
}

/// A hold's fields, and how many of the message's bytes it uses (the handle slots it does not fill must be zero too,
/// which the caller's check past `used` covers for the record and this one for the slots).
fn decode_hold(bytes: &[u8], id: u64) -> Result<(HandleMessage, usize), HandleRefusal> {
  let what = byte_at(bytes, AT_WHAT)?;
  let count = usize::from(byte_at(bytes, AT_COUNT)?);
  if count == 0 || count > MAX_HANDLES {
    return Err(malformed("a hold names one to MAX_HANDLES handles"));
  }
  let mut values = Vec::with_capacity(count);
  for slot in 0..MAX_HANDLES {
    let at = slot
      .checked_mul(HANDLE_BYTES)
      .and_then(|offset| offset.checked_add(AT_HANDLES))
      .ok_or_else(|| malformed("handle offset"))?;
    let value = u64_at(bytes, at)?;
    if slot < count {
      values.push(value);
    } else if value != 0 {
      return Err(malformed("a handle slot past the count"));
    }
  }
  let (record, used) = decode_record_field(bytes)?;
  Ok((
    HandleMessage::Hold {
      id,
      what,
      values,
      record,
    },
    used,
  ))
}

/// A record's replacement (A-423), and how many of the message's bytes it uses: its kind, count and handle slots must
/// be zero, since it names no handle, and its record is read as a hold's.
fn decode_record(bytes: &[u8], id: u64) -> Result<(HandleMessage, usize), HandleRefusal> {
  let unnamed = AT_RECORD_LEN
    .checked_sub(AT_WHAT)
    .ok_or_else(|| malformed("record offsets"))?;
  if field(bytes, AT_WHAT, unnamed)?
    .iter()
    .any(|byte| *byte != 0)
  {
    return Err(malformed(
      "a record's replacement names a kind, a count or a handle",
    ));
  }
  let (record, used) = decode_record_field(bytes)?;
  Ok((HandleMessage::Record { id, record }, used))
}

/// The record a hold or a record's replacement carries, at most [`RECORD_CAP`] bytes, and the end of the bytes it uses.
fn decode_record_field(bytes: &[u8]) -> Result<(Vec<u8>, usize), HandleRefusal> {
  let raw: [u8; 2] = field(bytes, AT_RECORD_LEN, RECORD_LEN_BYTES)?
    .try_into()
    .map_err(|_| malformed("record length"))?;
  let record_len = usize::from(u16::from_le_bytes(raw));
  if record_len > RECORD_CAP {
    return Err(malformed("a hold's record is past RECORD_CAP"));
  }
  let record = field(bytes, AT_RECORD, record_len)?.to_vec();
  let used = AT_RECORD
    .checked_add(record_len)
    .ok_or_else(|| malformed("record end"))?;
  Ok((record, used))
}

/// One held entry as the environment hands it to a spawned daemon: its id, what it is, the handle values the daemon
/// inherits, and its record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandedEntry {
  /// The entry's id.
  pub id: u64,
  /// What its handles are.
  pub what: u8,
  /// The inherited handle values, at least one and at most [`MAX_HANDLES`].
  pub values: Vec<u64>,
  /// The record, at most [`RECORD_CAP`] bytes.
  pub record: Vec<u8>,
}

/// The [`ENV_HANDLES`] value handing `channel` (the daemon's end, inherited) and `entries` to a spawned daemon.
pub fn encode_env(channel: u64, entries: &[HandedEntry]) -> String {
  let mut value = format!("{channel:x}");
  for entry in entries {
    let handles: Vec<String> = entry.values.iter().map(|v| format!("{v:x}")).collect();
    let record = if entry.record.is_empty() {
      "-".to_owned()
    } else {
      entry.record.iter().map(|b| format!("{b:02x}")).collect()
    };
    value.push_str(&format!(
      ";{:x}:{:x}:{}:{record}",
      entry.id,
      entry.what,
      handles.join(",")
    ));
  }
  checked_env(&value)
}

/// The textual handoff's checksum covers the channel and every entry together.
fn checked_env(value: &str) -> String {
  format!(
    "{value}#{:08x}",
    slates_wire::crc32c::crc32c(value.as_bytes())
  )
}

/// Parses an [`ENV_HANDLES`] value: the channel's value and at most `bound` entries, each checked against the caps
/// before anything is kept. A value that is not exactly the shape [`encode_env`] writes is refused whole.
pub fn parse_env(value: &str, bound: usize) -> Result<(u64, Vec<HandedEntry>), HandleRefusal> {
  // At most a channel and `bound` entries, each with bounded hexadecimal fields, plus the checksum suffix. Check
  // before hashing or allocating anything from the input.
  let cap = bound
    .checked_mul(ENTRY_TEXT_CAP)
    .and_then(|entries| entries.checked_add(U64_HEX_DIGITS + CHECKSUM_HEX_DIGITS + 1))
    .ok_or_else(|| malformed("environment capacity"))?;
  if value.len() > cap {
    return Err(malformed("handoff exceeds its entry-derived capacity"));
  }
  let (value, checksum) = value
    .rsplit_once('#')
    .ok_or_else(|| malformed("handoff checksum absent"))?;
  if checksum.len() != CHECKSUM_HEX_DIGITS
    || u32::from_str_radix(checksum, HEX_RADIX).ok()
      != Some(slates_wire::crc32c::crc32c(value.as_bytes()))
  {
    return Err(malformed("handoff checksum mismatch"));
  }
  let mut parts = value.split(';');
  let channel = parts
    .next()
    .and_then(hex_u64)
    .ok_or_else(|| malformed("the channel's value"))?;
  let mut entries = Vec::new();
  let mut ids = std::collections::BTreeSet::new();
  let mut handles = std::collections::BTreeSet::from([channel]);
  for part in parts {
    if entries.len() >= bound {
      return Err(HandleRefusal::Bound { bound });
    }
    let entry = parse_entry(part)?;
    if !ids.insert(entry.id) || entry.values.iter().any(|handle| !handles.insert(*handle)) {
      return Err(malformed("an id or handle has two owners"));
    }
    entries.push(entry);
  }
  Ok((channel, entries))
}

fn parse_entry(part: &str) -> Result<HandedEntry, HandleRefusal> {
  let mut fields = part.split(':');
  let (Some(id), Some(what), Some(handles), Some(record), None) = (
    fields.next(),
    fields.next(),
    fields.next(),
    fields.next(),
    fields.next(),
  ) else {
    return Err(malformed("an entry is ID:WHAT:HANDLES:RECORD"));
  };
  let id = hex_u64(id).ok_or_else(|| malformed("an entry's id"))?;
  let what = u8::from_str_radix(what, HEX_RADIX).map_err(|_| malformed("an entry's what"))?;
  let mut values = Vec::new();
  for handle in handles.split(',') {
    if values.len() >= MAX_HANDLES {
      return Err(malformed("an entry names more than MAX_HANDLES handles"));
    }
    values.push(hex_u64(handle).ok_or_else(|| malformed("a handle's value"))?);
  }
  let record = if record == "-" {
    Vec::new()
  } else {
    hex_bytes(record).ok_or_else(|| malformed("an entry's record"))?
  };
  Ok(HandedEntry {
    id,
    what,
    values,
    record,
  })
}

fn hex_u64(text: &str) -> Option<u64> {
  if text.is_empty() || text.len() > U64_HEX_DIGITS {
    return None;
  }
  u64::from_str_radix(text, HEX_RADIX).ok()
}

fn hex_bytes(text: &str) -> Option<Vec<u8>> {
  if text.is_empty() || !text.len().is_multiple_of(2) || text.len() > RECORD_CAP.checked_mul(2)? {
    return None;
  }
  text
    .as_bytes()
    .chunks(2)
    .map(|pair| {
      let digits = std::str::from_utf8(pair).ok()?;
      u8::from_str_radix(digits, HEX_RADIX).ok()
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

  /// A-198. Do: meet one refusal the OS keeps giving, then two kinds alternating, then more distinct kinds than the
  /// table holds. Expect: each distinct refusal is logged once, a repeat or an alternation never again, and the lines
  /// stop at the table's bound.
  #[test]
  fn a_refusal_is_logged_once_and_the_lines_stay_bounded() {
    let mut log = RefusalLog::default();
    let persistent = HandleRefusal::Os {
      call: "ReadFile of a hold message",
      code: 5,
    };
    let lines = (0..1_000)
      .filter(|_| log.first_sighting(&persistent))
      .count();
    assert_eq!(lines, 1, "the same refusal on every supervision step");

    let mut log = RefusalLog::default();
    let (short, corrupted) = (malformed("short"), malformed("corrupted"));
    let lines = (0..1_000)
      .filter(|turn| log.first_sighting(if turn % 2 == 0 { &short } else { &corrupted }))
      .count();
    assert_eq!(
      lines, 2,
      "two kinds alternating change kind at every message"
    );

    let mut log = RefusalLog::default();
    let lines = (0..u32::try_from(REFUSALS_REMEMBERED * 2).unwrap())
      .filter(|code| {
        log.first_sighting(&HandleRefusal::Os {
          call: "ReadFile of a hold message",
          code: *code,
        })
      })
      .count();
    assert_eq!(lines, REFUSALS_REMEMBERED, "every kind distinct");
  }

  fn hold() -> HandleMessage {
    HandleMessage::Hold {
      id: 0x0102_0304_0506_0708,
      what: WHAT_WINFSP_MOUNT,
      values: vec![0x1a4, 0x2b8, 0x3cc],
      record: b"S:".to_vec(),
    }
  }

  /// A-198. Do: encode then decode every kind of message. Expect: each comes back whole.
  #[test]
  fn every_message_round_trips() {
    for message in [
      hold(),
      HandleMessage::Release { id: 7 },
      HandleMessage::Released { id: 7 },
      HandleMessage::Held { id: u64::MAX },
      HandleMessage::Refused {
        id: 9,
        reason: Reason::Bound,
      },
      HandleMessage::Refused {
        id: 9,
        reason: Reason::Duplicate,
      },
      HandleMessage::Refused {
        id: 9,
        reason: Reason::Malformed,
      },
    ] {
      let bytes = encode(&message).unwrap();
      assert_eq!(decode(&bytes).unwrap(), message);
    }
  }

  /// A-198: golden vector. Do: encode a known hold. Expect: these bytes, so the format cannot drift unseen between an
  /// anchor and a daemon of one build.
  #[test]
  fn a_hold_encodes_to_its_golden_bytes() {
    let bytes = encode(&hold()).unwrap();
    let mut expected = MAGIC.to_vec();
    expected.push(KIND_HOLD);
    expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    expected.extend_from_slice(&[WHAT_WINFSP_MOUNT, 3]);
    for value in [0x1a4u64, 0x2b8, 0x3cc] {
      expected.extend_from_slice(&value.to_le_bytes());
    }
    expected.extend_from_slice(&2u16.to_le_bytes());
    expected.extend_from_slice(b"S:");
    expected.resize(AT_CHECKSUM, 0);
    let checksum = slates_wire::crc32c::crc32c(&expected);
    expected.extend_from_slice(&checksum.to_le_bytes());
    assert_eq!(bytes.to_vec(), expected);
  }

  /// A-198: hostile input. Do: decode truncated, overlong, unknown-kind, overcounted, overlong-record, padded-nonzero,
  /// slot-past-count and bad-reason messages. Expect: each refused as malformed, none accepted.
  #[test]
  fn hostile_messages_are_refused() {
    let good = encode(&hold()).unwrap();
    let mut cases: Vec<Vec<u8>> = vec![
      good.get(..MESSAGE_BYTES - 1).unwrap().to_vec(),
      [good.to_vec(), vec![0]].concat(),
      vec![],
    ];
    let mut unknown = good;
    unknown[AT_KIND] = 0xEE;
    cases.push(unknown.to_vec());
    let mut foreign = good;
    foreign[0] ^= 1;
    cases.push(foreign.to_vec());
    let mut overcount = good;
    overcount[AT_COUNT] = u8::try_from(MAX_HANDLES + 1).unwrap();
    cases.push(overcount.to_vec());
    let mut zero_count = good;
    zero_count[AT_COUNT] = 0;
    cases.push(zero_count.to_vec());
    let mut long_record = good;
    long_record[AT_RECORD_LEN..AT_RECORD].copy_from_slice(&u16::MAX.to_le_bytes());
    cases.push(long_record.to_vec());
    let mut padded = good;
    padded[AT_CHECKSUM - 1] = 1;
    cases.push(padded.to_vec());
    let mut slot = encode(&HandleMessage::Hold {
      id: 1,
      what: WHAT_WINFSP_MOUNT,
      values: vec![5],
      record: vec![],
    })
    .unwrap();
    slot[AT_HANDLES + HANDLE_BYTES] = 1;
    cases.push(slot.to_vec());
    let mut reason = encode(&HandleMessage::Refused {
      id: 1,
      reason: Reason::Bound,
    })
    .unwrap();
    reason[AT_WHAT] = 0;
    cases.push(reason.to_vec());
    let mut release_tail = encode(&HandleMessage::Release { id: 1 }).unwrap();
    release_tail[AT_WHAT] = 1;
    cases.push(release_tail.to_vec());
    for mut case in cases {
      // Re-sign full-sized malformed payloads so their shape checks are exercised after checksum verification.
      if case.len() == MESSAGE_BYTES {
        let checksum = slates_wire::crc32c::crc32c(&case[..AT_CHECKSUM]);
        case[AT_CHECKSUM..].copy_from_slice(&checksum.to_le_bytes());
      }
      assert!(
        matches!(decode(&case), Err(HandleRefusal::Malformed { .. })),
        "accepted {} bytes beginning {:?}",
        case.len(),
        case.get(..12)
      );
    }
  }

  /// A-198. Do: encode holds outside the shapes. Expect: refused, never truncated.
  #[test]
  fn a_hold_outside_the_shapes_is_refused_at_encoding() {
    for (values, record) in [
      (vec![], vec![]),
      (vec![1, 2, 3, 4], vec![]),
      (vec![1], vec![0; RECORD_CAP + 1]),
    ] {
      let message = HandleMessage::Hold {
        id: 1,
        what: WHAT_WINFSP_MOUNT,
        values,
        record,
      };
      assert!(matches!(
        encode(&message),
        Err(HandleRefusal::Malformed { .. })
      ));
    }
  }

  /// A-198. Do: encode an environment with entries, one with an empty record, and parse it back. Expect: the same
  /// channel and entries.
  #[test]
  fn the_environment_round_trips() {
    let entries = vec![
      HandedEntry {
        id: 0xabc,
        what: WHAT_WINFSP_MOUNT,
        values: vec![0x10, 0x20, 0x30],
        record: vec![0, 0xff, 7],
      },
      HandedEntry {
        id: u64::MAX,
        what: WHAT_WINFSP_MOUNT,
        values: vec![u64::MAX],
        record: vec![],
      },
    ];
    let value = encode_env(0x4c, &entries);
    assert_eq!(parse_env(&value, 8).unwrap(), (0x4c, entries));
    assert_eq!(
      parse_env(&encode_env(0x4c, &[]), 8).unwrap(),
      (0x4c, vec![])
    );
  }

  /// A-198: hostile input. Do: parse environments with a bad channel, a missing field, an extra field, bad hex, too
  /// many handles, an odd or overlong record, and more entries than the bound. Expect: each refused whole.
  #[test]
  fn hostile_environments_are_refused() {
    let too_long_record = format!("1;2:1:3:{}", "00".repeat(RECORD_CAP + 1));
    let over_bound = "1;2:1:3:-;4:1:5:-;6:1:7:-";
    for value in [
      "",
      "zz",
      "1;2:1:3",
      "1;2:1:3:-:extra",
      "1;x:1:3:-",
      "1;2:1:3,4,5,6:-",
      "1;2:1::-",
      "1;2:1:3:abc",
      "1;2:1:3:",
      "1;12345678901234567:1:3:-",
      too_long_record.as_str(),
    ] {
      assert!(
        parse_env(&checked_env(value), 8).is_err(),
        "accepted {value:?}"
      );
    }
    assert_eq!(
      parse_env(&checked_env(over_bound), 2),
      Err(HandleRefusal::Bound { bound: 2 })
    );
  }

  /// A-198: hostile input. Do: change one bit anywhere in a valid hold message. Expect: each damaged message is
  /// refused before its id, handle values or record can select a kernel object.
  #[test]
  fn a_changed_message_is_refused_before_a_handle_is_selected() {
    let original = encode(&hold()).unwrap();
    for offset in 0..original.len() {
      let mut changed = original;
      changed[offset] ^= 1;
      assert!(
        decode(&changed).is_err(),
        "accepted a change at byte {offset}"
      );
    }
  }

  /// A-198: hostile handoff. Do: name one inherited handle as both the channel and an entry, or twice among entries.
  /// Expect: refusal of the whole handoff, so adopting it can never create two owners that close the same handle.
  #[test]
  fn one_inherited_handle_cannot_have_two_owners() {
    let entry = HandedEntry {
      id: 1,
      what: WHAT_WINFSP_MOUNT,
      values: vec![0x10],
      record: vec![],
    };
    let alias = encode_env(0x10, std::slice::from_ref(&entry));
    assert!(
      parse_env(&alias, 2).is_err(),
      "the channel cannot also belong to an entry"
    );
    let duplicate = encode_env(0x20, &[entry.clone(), HandedEntry { id: 2, ..entry }]);
    assert!(
      parse_env(&duplicate, 2).is_err(),
      "entries cannot share a handle value"
    );
  }

  /// The session a Shards stream's resync hold carries in the tests: opaque to the anchor, the length of a Windows
  /// stream's session (A-418: the token, the owner view, the form, the holder's id and start time).
  fn session(fill: u8) -> Vec<u8> {
    vec![fill; 52]
  }

  /// A-423: golden vector. Do: encode a record's replacement for a held entry. Expect: the magic, the record kind, the
  /// id, zero where a hold names its kind, count and handles, then the record's length and bytes as a hold's, zero to
  /// the checksum and the CRC32C; and the message back whole.
  #[test]
  fn a_record_encodes_to_its_golden_bytes_and_back() {
    let message = HandleMessage::Record {
      id: 0x0102_0304_0506_0708,
      record: session(0x5a),
    };
    let mut expected = MAGIC.to_vec();
    expected.push(6);
    expected.extend_from_slice(&0x0102_0304_0506_0708u64.to_le_bytes());
    expected.extend_from_slice(&[0, 0]);
    expected.extend_from_slice(&[0; MAX_HANDLES * size_of::<u64>()]);
    expected.extend_from_slice(&52u16.to_le_bytes());
    expected.extend_from_slice(&session(0x5a));
    expected.resize(AT_CHECKSUM, 0);
    let checksum = slates_wire::crc32c::crc32c(&expected);
    expected.extend_from_slice(&checksum.to_le_bytes());
    let bytes = encode(&message).unwrap();
    assert_eq!(bytes.to_vec(), expected);
    assert_eq!(decode(&bytes).unwrap(), message);
  }

  /// A-423. Do: encode then decode a Shards stream's resync hold (one handle, a session as its record), a record's
  /// replacement, and one with an empty record. Expect: each back whole, the kind's byte kept.
  #[test]
  fn a_resync_hold_and_a_record_round_trip() {
    for message in [
      HandleMessage::Hold {
        id: 11,
        what: WHAT_SHARDS_RESYNC,
        values: vec![0x2c4],
        record: session(1),
      },
      HandleMessage::Record {
        id: 11,
        record: session(2),
      },
      HandleMessage::Record {
        id: u64::MAX,
        record: Vec::new(),
      },
    ] {
      let bytes = encode(&message).unwrap();
      assert_eq!(decode(&bytes).unwrap(), message);
    }
  }

  /// A-423: hostile input. Do: decode a record's replacement naming a kind, a count or a handle, one whose record runs
  /// past the cap, and one with a bit changed anywhere; and encode one whose record is past the cap. Expect: every one
  /// refused, none accepted, and the long record refused at encoding rather than cut.
  #[test]
  fn a_record_names_no_handle_and_stays_within_the_cap() {
    let good = encode(&HandleMessage::Record {
      id: 3,
      record: session(7),
    })
    .unwrap();
    let mut cases = Vec::new();
    for at in [AT_WHAT, AT_COUNT, AT_HANDLES, AT_RECORD_LEN - 1] {
      let mut named = good;
      named[at] = 1;
      cases.push(named);
    }
    let mut long = good;
    long[AT_RECORD_LEN..AT_RECORD].copy_from_slice(&u16::MAX.to_le_bytes());
    cases.push(long);
    for mut case in cases {
      // Re-signed, so each is refused by its shape and not by its checksum.
      let checksum = slates_wire::crc32c::crc32c(&case[..AT_CHECKSUM]);
      case[AT_CHECKSUM..].copy_from_slice(&checksum.to_le_bytes());
      assert!(
        matches!(decode(&case), Err(HandleRefusal::Malformed { .. })),
        "accepted a record beginning {:?}",
        case.get(..AT_RECORD)
      );
    }
    for offset in 0..good.len() {
      let mut changed = good;
      changed[offset] ^= 1;
      assert!(
        decode(&changed).is_err(),
        "accepted a change at byte {offset}"
      );
    }
    assert!(matches!(
      encode(&HandleMessage::Record {
        id: 3,
        record: vec![0; RECORD_CAP + 1],
      }),
      Err(HandleRefusal::Malformed { .. })
    ));
  }

  /// A-423, A-448. Do: ask whether the anchor admits a hold of each kind naming each count of handles from none to one
  /// past the message's room, and of kinds no daemon sends. Expect: a WinFsp mount one to the room (its daemon names
  /// two, the room kept since A-369), a Shards stream's resync end exactly one, a DAX section exactly one, and nothing of
  /// any other kind.
  #[test]
  fn the_anchor_admits_each_kind_at_its_handle_counts_alone() {
    for count in 0..=MAX_HANDLES + 1 {
      assert_eq!(
        admits(WHAT_WINFSP_MOUNT, count),
        (1..=MAX_HANDLES).contains(&count),
        "a WinFsp mount naming {count}"
      );
      assert_eq!(
        admits(WHAT_SHARDS_RESYNC, count),
        count == SHARDS_RESYNC_HANDLES,
        "a resync end naming {count}"
      );
      assert_eq!(
        admits(WHAT_SHARDS_DAX, count),
        count == SHARDS_DAX_HANDLES,
        "a DAX section naming {count}"
      );
      for what in [0, 4, u8::MAX] {
        assert!(!admits(what, count), "kind {what} naming {count}");
      }
    }
  }

  /// A-448. Do: fill each class of a hold's bounds in turn, asking after each entry whether one more of each kind fits.
  /// Expect: DAX sections refused at their own bound while a mount and a resync end still fit, those refused at theirs
  /// whatever the sections hold, nothing fitting bounds of none, and the handoff bound the two classes together.
  #[test]
  fn each_class_of_entry_is_refused_at_its_own_bound() {
    let bounds = HoldBounds {
      attachments: 2,
      sections: 3,
    };
    assert_eq!(bounds.total(), 5);
    for held in 0..3 {
      assert!(
        bounds.admits_another(WHAT_SHARDS_DAX, held),
        "section {held}"
      );
    }
    assert!(
      !bounds.admits_another(WHAT_SHARDS_DAX, 3),
      "past the sections' bound"
    );
    for what in [WHAT_WINFSP_MOUNT, WHAT_SHARDS_RESYNC] {
      assert!(
        bounds.admits_another(what, 0),
        "kind {what} beside full sections"
      );
      assert!(
        bounds.admits_another(what, 1),
        "kind {what} beside full sections"
      );
      assert!(
        !bounds.admits_another(what, 2),
        "kind {what} past its bound"
      );
    }
    assert!(HoldBounds::shared(WHAT_WINFSP_MOUNT, WHAT_SHARDS_RESYNC));
    assert!(!HoldBounds::shared(WHAT_SHARDS_DAX, WHAT_SHARDS_RESYNC));
    assert!(HoldBounds::shared(WHAT_SHARDS_DAX, WHAT_SHARDS_DAX));
    let none = HoldBounds {
      attachments: 0,
      sections: 0,
    };
    for what in [WHAT_WINFSP_MOUNT, WHAT_SHARDS_RESYNC, WHAT_SHARDS_DAX] {
      assert!(!none.admits_another(what, 0), "kind {what} with no room");
    }
  }
}
