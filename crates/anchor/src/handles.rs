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
//! Every message is [`MESSAGE_BYTES`] long, checksum-verified and checked by kind, counts and lengths before anything is kept: the channel
//! crosses a process boundary, so its bytes are external input, and so is the environment a daemon parses. The
//! transport is `crate::held_windows`; this module is pure and cfg-free, so it is tested on every host.

/// Format: the environment variable carrying the hold channel's daemon end and every held entry to a spawned daemon:
/// `CHANNEL_HEX` then, for each entry, `;ID_HEX:WHAT_HEX:H1_HEX,H2_HEX,...:RECORD_HEX` (`-` for an empty record). The
/// handles are inherited across the spawn at the values named.
pub const ENV_HANDLES: &str = "SLATES_ANCHOR_HANDLES";

/// Format: what a hold holds, its `what` byte: a WinFsp mount's volume control handle, drive link and ledger (A-198).
pub const WHAT_WINFSP_MOUNT: u8 = 1;

/// Format: the most handles one hold names: a WinFsp mount's three (A-198).
pub const MAX_HANDLES: usize = 3;

/// Format: a WinFsp mount's largest handoff record: `FSP_FSCTL_VOLUME_NAME_SIZEMAX` (64 + 192 UTF-16 units), its
/// two-unit drive name (`X:`), and one access-flags byte. The record is opaque to the anchor.
pub const RECORD_CAP: usize = (64 + 192 + 2) * size_of::<u16>() + size_of::<u8>();

/// Format: the channel's magic, distinguishing these messages from another inherited protocol.
const MAGIC: &[u8; 4] = b"SLHD";
/// Format: the message kind follows the magic.
const AT_KIND: usize = MAGIC.len();

/// Format: a message's kind, its first byte: a daemon's hold.
const KIND_HOLD: u8 = 1;
/// Format: see [`KIND_HOLD`]: a daemon's release of an entry it ended.
const KIND_RELEASE: u8 = 2;
/// Format: see [`KIND_HOLD`]: the anchor's answer that it holds an entry.
const KIND_HELD: u8 = 3;
/// Format: see [`KIND_HOLD`]: the anchor's answer that it refused an entry, with the reason's byte.
const KIND_REFUSED: u8 = 4;

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
  /// The anchor holds entry `id`.
  Held {
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
    HandleMessage::Release { id } => (KIND_RELEASE, *id),
    HandleMessage::Held { id } => (KIND_HELD, *id),
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
    HandleMessage::Refused { reason, .. } => put(&mut bytes, AT_WHAT, &[reason.byte()])?,
    HandleMessage::Release { .. } | HandleMessage::Held { .. } => {}
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
  let raw: [u8; 8] = field(bytes, at, 8)?
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
    KIND_RELEASE => (HandleMessage::Release { id }, AT_WHAT),
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
    || u32::from_str_radix(checksum, 16).ok() != Some(slates_wire::crc32c::crc32c(value.as_bytes()))
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
  let what = u8::from_str_radix(what, 16).map_err(|_| malformed("an entry's what"))?;
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
  u64::from_str_radix(text, 16).ok()
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
      u8::from_str_radix(digits, 16).ok()
    })
    .collect()
}

#[cfg(test)]
mod tests {
  use super::*;

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
    unknown[0] = 0xEE;
    cases.push(unknown.to_vec());
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
    padded[MESSAGE_BYTES - 1] = 1;
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
    for case in cases {
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
}
