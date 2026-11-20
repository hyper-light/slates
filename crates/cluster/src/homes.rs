//! The messages that home content by its identity among a region's members, and the rule a member accepts one by (A-724
//! piece 6, the region's half; §4.10 "Content homed by identity"; main's ruling of 2026-11-20).
//!
//! **Apart from the content plane's holds.** `crate::content`'s exchanges hold content for an object, authorized by the
//! object's owner, and every release there follows the owner's register (w-fed-faults' answer to Q3: a certified
//! takeover, a late copy under a newer head, a tombstone, a ledger's compaction). A window homed by its identity has no
//! owner whose record is its reason, so its messages are their own type on their own stream ([`HOME_STREAM`]) and never
//! reach `ContentHold`: a home keeps it in its home shard's content store, keyed by its identity.
//!
//! **The messages** ([`HomeMessage`]). A sender asks a member to keep a window ([`HomeMessage::Put`]: its identity and
//! bytes), to read one back ([`HomeMessage::Read`]), or to let one holder of it go ([`HomeMessage::LetGo`]); the member
//! answers each with [`HomeMessage::Answer`]: kept (with the image generation the window is first durable in, sent only
//! once that image is published), its bytes, let go, or refused by kind ([`HomeRefusal`]).
//!
//! **The rule at the door** ([`admits`]). A member keeps a window only if the configuration it knows ranks it among
//! the window's homes (`slates_db::identity_homes::homes`), and only bytes that hash to the identity they were sent
//! under: authority is the configuration alone, so no owner event can make a member hold or drop one.
//!
//! **The wire.** A kind byte, then the fields little-endian, byte strings length-prefixed by a `u32`: a window's bytes
//! are at most the archive format's chunk cap (`slates_archive::format::MAX_CHUNK_BYTES`), checked before they are
//! copied; truncation, an unknown kind or refusal, a length past the bytes and trailing bytes are each refused typed
//! ([`HomeWireError`]), never a panic.

use slates_archive::format::MAX_CHUNK_BYTES;
use slates_db::identity_homes::homes;
use slates_db::register::HostId;

/// The stream home exchanges ride on a member's record session: their own, beside the content plane's offer, chunk and
/// fetch streams (`crate::content::CONTENT_OFFER_STREAM` explains why each exchange kind has its own). Format: one
/// stream id a kind of exchange on a connection; a fixed label, not a tunable.
pub const HOME_STREAM: u64 = 30;

/// Format: the kind byte of a put.
const KIND_PUT: u8 = 1;
/// Format: the kind byte of a read.
const KIND_READ: u8 = 2;
/// Format: the kind byte of a let-go.
const KIND_LET_GO: u8 = 3;
/// Format: the kind byte of an answer.
const KIND_ANSWER: u8 = 4;
/// Format: the kind byte of a part of a put (appended: an object past one message, A-724 piece 8c).
const KIND_PUT_PART: u8 = 5;
/// Format: the kind byte of a read of a part (appended).
const KIND_READ_PART: u8 = 6;
/// Format: the outcome byte of an answer that kept the window.
const OUTCOME_KEPT: u8 = 1;
/// Format: the outcome byte of an answer carrying the window's bytes.
const OUTCOME_BYTES: u8 = 2;
/// Format: the outcome byte of an answer that let a holder go.
const OUTCOME_LET_GO: u8 = 3;
/// Format: the outcome byte of a refusal.
const OUTCOME_REFUSED: u8 = 4;
/// Format: the outcome byte of a put's part taken, the bytes received so far (appended).
const OUTCOME_TAKEN: u8 = 5;
/// Format: the outcome byte of a read's part: the object's length, then the part's bytes (appended).
const OUTCOME_PART: u8 = 6;

/// The most bytes one part carries ([`HomeMessage::PutPart`], [`HomeOutcome::Part`]): the chunk cap a message's byte
/// string is held to. Derived: `slates_archive::format::MAX_CHUNK_BYTES`.
pub const PART_BYTES: u64 = MAX_CHUNK_BYTES;
/// Format: the width of a BLAKE3 identity.
const IDENTITY_BYTES: usize = 32;

/// Why a member refused a home exchange (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeRefusal {
  /// The configuration the member knows does not rank it among the window's homes.
  NotAHome,
  /// The bytes do not hash to the identity they were sent under.
  Misnamed,
  /// The member holds no such window.
  NotHeld,
  /// The member's home shard has no room for it.
  Room,
  /// The member's home shard did not answer in time, or its image was not published within the wait.
  Unanswered,
}

impl HomeRefusal {
  fn byte(self) -> u8 {
    match self {
      HomeRefusal::NotAHome => 1,
      HomeRefusal::Misnamed => 2,
      // Format: a refusal's wire byte, one a kind, 0 for none.
      HomeRefusal::NotHeld => 3,
      HomeRefusal::Room => 4,
      HomeRefusal::Unanswered => 5,
    }
  }

  fn of(byte: u8) -> Option<HomeRefusal> {
    match byte {
      1 => Some(HomeRefusal::NotAHome),
      2 => Some(HomeRefusal::Misnamed),
      // Format: a refusal's wire byte, as `byte` writes it.
      3 => Some(HomeRefusal::NotHeld),
      4 => Some(HomeRefusal::Room),
      5 => Some(HomeRefusal::Unanswered),
      _ => None,
    }
  }
}

/// What a member answered (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeOutcome {
  /// The window is kept, durable from this image generation of the member's home shard on (answered only once that
  /// image is published).
  Kept {
    /// The image generation.
    generation: u64,
  },
  /// The window's bytes.
  Bytes(Vec<u8>),
  /// One holder of the window was let go.
  LetGo,
  /// Refused, by kind.
  Refused(HomeRefusal),
  /// A put's part was taken: the bytes received of the object so far (appended, A-724 piece 8c).
  Taken {
    /// The bytes received so far.
    received: u64,
  },
  /// A read's part: the object's whole length, and the part's bytes (appended).
  Part {
    /// The object's length.
    total: u64,
    /// The part's bytes.
    bytes: Vec<u8>,
  },
}

/// A home exchange's message (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeMessage {
  /// Keep the window `bytes`, named `identity`.
  Put {
    /// The window's identity: the BLAKE3 of its bytes.
    identity: [u8; 32],
    /// Its bytes.
    bytes: Vec<u8>,
  },
  /// Read the window named `identity` back.
  Read {
    /// Its identity.
    identity: [u8; 32],
  },
  /// Let one holder of the window named `identity` go.
  LetGo {
    /// Its identity.
    identity: [u8; 32],
  },
  /// Keep part of the object named `identity`, `total` bytes long, its bytes from `offset` (appended, A-724 piece 8c: an
  /// object past one message, sent in order a part at a time, each at most [`PART_BYTES`]).
  PutPart {
    /// The object's identity: the BLAKE3 of its whole bytes.
    identity: [u8; 32],
    /// The object's length.
    total: u64,
    /// Where this part begins.
    offset: u64,
    /// This part's bytes.
    bytes: Vec<u8>,
  },
  /// Read back the part of the object named `identity` from `offset` (appended).
  ReadPart {
    /// Its identity.
    identity: [u8; 32],
    /// Where the part begins.
    offset: u64,
  },
  /// A member's answer about the window named `identity`.
  Answer {
    /// Its identity.
    identity: [u8; 32],
    /// What became of it.
    outcome: HomeOutcome,
  },
}

/// A home message's bytes refused (module docs, "The wire").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HomeWireError {
  /// The bytes end before a field they declare.
  Truncated,
  /// A kind, outcome or refusal byte names nothing.
  Unknown {
    /// The byte found.
    byte: u8,
  },
  /// A length is past the bytes or the chunk cap, or bytes trail the message.
  BadLength,
}

/// Whether member `me` keeps a window named `identity` whose bytes are `bytes`, among `members` with `f` (module docs,
/// "The rule at the door"): a refusal by kind when not.
pub fn admits(
  identity: &[u8; 32],
  bytes: &[u8],
  (me, members, f): (HostId, &[HostId], usize),
) -> Result<(), HomeRefusal> {
  if !homes(identity, members, f).contains(&me) {
    return Err(HomeRefusal::NotAHome);
  }
  if blake3::hash(bytes).as_bytes() != identity {
    return Err(HomeRefusal::Misnamed);
  }
  Ok(())
}

impl HomeMessage {
  /// The message's canonical bytes (module docs, "The wire").
  pub fn encode(&self) -> Vec<u8> {
    let mut out = Vec::new();
    match self {
      HomeMessage::Put { identity, bytes } => {
        out.push(KIND_PUT);
        out.extend_from_slice(identity);
        put_blob(&mut out, bytes);
      }
      HomeMessage::Read { identity } => {
        out.push(KIND_READ);
        out.extend_from_slice(identity);
      }
      HomeMessage::LetGo { identity } => {
        out.push(KIND_LET_GO);
        out.extend_from_slice(identity);
      }
      HomeMessage::PutPart {
        identity,
        total,
        offset,
        bytes,
      } => {
        out.push(KIND_PUT_PART);
        out.extend_from_slice(identity);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        put_blob(&mut out, bytes);
      }
      HomeMessage::ReadPart { identity, offset } => {
        out.push(KIND_READ_PART);
        out.extend_from_slice(identity);
        out.extend_from_slice(&offset.to_le_bytes());
      }
      HomeMessage::Answer { identity, outcome } => {
        out.push(KIND_ANSWER);
        out.extend_from_slice(identity);
        match outcome {
          HomeOutcome::Kept { generation } => {
            out.push(OUTCOME_KEPT);
            out.extend_from_slice(&generation.to_le_bytes());
          }
          HomeOutcome::Bytes(bytes) => {
            out.push(OUTCOME_BYTES);
            put_blob(&mut out, bytes);
          }
          HomeOutcome::LetGo => out.push(OUTCOME_LET_GO),
          HomeOutcome::Refused(refusal) => {
            out.push(OUTCOME_REFUSED);
            out.push(refusal.byte());
          }
          HomeOutcome::Taken { received } => {
            out.push(OUTCOME_TAKEN);
            out.extend_from_slice(&received.to_le_bytes());
          }
          HomeOutcome::Part { total, bytes } => {
            out.push(OUTCOME_PART);
            out.extend_from_slice(&total.to_le_bytes());
            put_blob(&mut out, bytes);
          }
        }
      }
    }
    out
  }

  /// Parses a message (module docs, "The wire").
  pub fn decode(bytes: &[u8]) -> Result<HomeMessage, HomeWireError> {
    let mut reader = Reader { rest: bytes };
    let kind = reader.byte()?;
    let identity = reader.identity()?;
    let message = match kind {
      KIND_PUT => HomeMessage::Put {
        identity,
        bytes: reader.blob()?,
      },
      KIND_READ => HomeMessage::Read { identity },
      KIND_LET_GO => HomeMessage::LetGo { identity },
      KIND_PUT_PART => HomeMessage::PutPart {
        identity,
        total: reader.long()?,
        offset: reader.long()?,
        bytes: reader.blob()?,
      },
      KIND_READ_PART => HomeMessage::ReadPart {
        identity,
        offset: reader.long()?,
      },
      KIND_ANSWER => {
        let outcome = match reader.byte()? {
          OUTCOME_KEPT => HomeOutcome::Kept {
            generation: reader.long()?,
          },
          OUTCOME_BYTES => HomeOutcome::Bytes(reader.blob()?),
          OUTCOME_LET_GO => HomeOutcome::LetGo,
          OUTCOME_REFUSED => {
            let byte = reader.byte()?;
            HomeOutcome::Refused(HomeRefusal::of(byte).ok_or(HomeWireError::Unknown { byte })?)
          }
          OUTCOME_TAKEN => HomeOutcome::Taken {
            received: reader.long()?,
          },
          OUTCOME_PART => HomeOutcome::Part {
            total: reader.long()?,
            bytes: reader.blob()?,
          },
          byte => return Err(HomeWireError::Unknown { byte }),
        };
        HomeMessage::Answer { identity, outcome }
      }
      byte => return Err(HomeWireError::Unknown { byte }),
    };
    if !reader.rest.is_empty() {
      return Err(HomeWireError::BadLength);
    }
    Ok(message)
  }
}

/// A length-prefixed byte string.
fn put_blob(out: &mut Vec<u8>, blob: &[u8]) {
  out.extend_from_slice(&u32::try_from(blob.len()).unwrap_or(u32::MAX).to_le_bytes());
  out.extend_from_slice(blob);
}

/// The bytes a message has left, read from the front.
struct Reader<'a> {
  rest: &'a [u8],
}

impl<'a> Reader<'a> {
  fn take(&mut self, len: usize) -> Result<&'a [u8], HomeWireError> {
    let (taken, rest) = self
      .rest
      .split_at_checked(len)
      .ok_or(HomeWireError::Truncated)?;
    self.rest = rest;
    Ok(taken)
  }

  fn byte(&mut self) -> Result<u8, HomeWireError> {
    self
      .take(1)?
      .first()
      .copied()
      .ok_or(HomeWireError::Truncated)
  }

  fn long(&mut self) -> Result<u64, HomeWireError> {
    let bytes = <[u8; size_of::<u64>()]>::try_from(self.take(size_of::<u64>())?)
      .map_err(|_| HomeWireError::Truncated)?;
    Ok(u64::from_le_bytes(bytes))
  }

  fn identity(&mut self) -> Result<[u8; 32], HomeWireError> {
    <[u8; IDENTITY_BYTES]>::try_from(self.take(IDENTITY_BYTES)?)
      .map_err(|_| HomeWireError::Truncated)
  }

  /// A length-prefixed byte string, its length checked against the chunk cap and the bytes left before it is copied.
  fn blob(&mut self) -> Result<Vec<u8>, HomeWireError> {
    let bytes = <[u8; 4]>::try_from(self.take(4)?).map_err(|_| HomeWireError::Truncated)?;
    let len = usize::try_from(u32::from_le_bytes(bytes)).map_err(|_| HomeWireError::BadLength)?;
    let cap = usize::try_from(MAX_CHUNK_BYTES).unwrap_or(usize::MAX);
    if len > cap || len > self.rest.len() {
      return Err(HomeWireError::BadLength);
    }
    Ok(self.take(len)?.to_vec())
  }
}

#[cfg(test)]
mod tests {
  use super::{HomeMessage, HomeOutcome, HomeRefusal, HomeWireError, admits};
  use proptest::prelude::*;
  use slates_db::identity_homes::homes;
  use slates_db::register::HostId;

  /// Any message, each kind alike. The union is built as `prop_oneof!` builds one of more than ten arms, from boxed
  /// strategies: its shorter forms hold their arms in `Arc`s (R2).
  fn message() -> impl Strategy<Value = HomeMessage> {
    let identity = proptest::array::uniform32(any::<u8>());
    let bytes = proptest::collection::vec(any::<u8>(), 0..2048);
    proptest::strategy::Union::new([
      (identity, bytes.clone())
        .prop_map(|(identity, bytes)| HomeMessage::Put { identity, bytes })
        .boxed(),
      identity
        .prop_map(|identity| HomeMessage::Read { identity })
        .boxed(),
      identity
        .prop_map(|identity| HomeMessage::LetGo { identity })
        .boxed(),
      (identity, any::<u64>(), any::<u64>(), bytes.clone())
        .prop_map(|(identity, total, offset, bytes)| HomeMessage::PutPart {
          identity,
          total,
          offset,
          bytes,
        })
        .boxed(),
      (identity, any::<u64>())
        .prop_map(|(identity, offset)| HomeMessage::ReadPart { identity, offset })
        .boxed(),
      (identity, any::<u64>())
        .prop_map(|(identity, received)| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::Taken { received },
        })
        .boxed(),
      (identity, any::<u64>(), bytes.clone())
        .prop_map(|(identity, total, bytes)| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::Part { total, bytes },
        })
        .boxed(),
      (identity, any::<u64>())
        .prop_map(|(identity, generation)| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::Kept { generation },
        })
        .boxed(),
      (identity, bytes)
        .prop_map(|(identity, bytes)| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::Bytes(bytes),
        })
        .boxed(),
      identity
        .prop_map(|identity| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::LetGo,
        })
        .boxed(),
      identity
        .prop_map(|identity| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::Refused(HomeRefusal::NotAHome),
        })
        .boxed(),
    ])
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(300)))]

    /// A-724 piece 6 (the region's half's wire). Do: encode any home message, decode it; then cut it short at every
    /// length and flip one byte. Expect: the same message back; every cut refused (never a panic); a flipped message
    /// either refused or decoded to some message, never a panic.
    #[test]
    fn a_home_message_round_trips_and_hostile_bytes_are_refused_typed(message in message(), flip in any::<usize>()) {
      let bytes = message.encode();
      prop_assert_eq!(HomeMessage::decode(&bytes), Ok(message));
      for cut in 0..bytes.len() {
        prop_assert!(HomeMessage::decode(&bytes[..cut]).is_err());
      }
      let mut flipped = bytes.clone();
      let at = flip % flipped.len();
      flipped[at] ^= 0xff;
      let _ = HomeMessage::decode(&flipped);
    }
  }

  /// A-724 piece 6, the rule at the door. Do: offer a window to each member of a nine-member region, f = 1, then the
  /// same bytes under another identity. Expect: exactly its two homes keep it, every other member refuses `NotAHome`;
  /// a home refuses bytes under another identity `Misnamed`.
  #[test]
  fn only_a_windows_homes_keep_it_and_only_under_its_own_name() {
    let members: Vec<HostId> = (1..=9u64).map(|id| HostId(id * 7919)).collect();
    let bytes = b"a cold window of a monorepo".to_vec();
    let identity = *blake3::hash(&bytes).as_bytes();
    let placed = homes(&identity, &members, 1);
    let kept: Vec<HostId> = members
      .iter()
      .copied()
      .filter(|me| admits(&identity, &bytes, (*me, &members, 1)).is_ok())
      .collect();
    assert_eq!(kept.len(), 2);
    assert!(kept.iter().all(|me| placed.contains(me)));
    let outsider = members
      .iter()
      .copied()
      .find(|me| !placed.contains(me))
      .unwrap();
    assert_eq!(
      admits(&identity, &bytes, (outsider, &members, 1)),
      Err(HomeRefusal::NotAHome)
    );
    assert_eq!(
      admits(
        &identity,
        b"other bytes under its name",
        (placed[0], &members, 1)
      ),
      Err(HomeRefusal::Misnamed)
    );
  }

  /// A-724 piece 6, hostile lengths. Do: a put whose length prefix passes the bytes, one past the chunk cap, an unknown
  /// kind, an unknown refusal and trailing bytes. Expect: each refused by its kind.
  #[test]
  fn hostile_home_messages_are_refused_by_kind() {
    let mut long = vec![1u8];
    long.extend_from_slice(&[0; 32]);
    long.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(HomeMessage::decode(&long), Err(HomeWireError::BadLength));
    let mut unknown = vec![9u8];
    unknown.extend_from_slice(&[0; 32]);
    assert_eq!(
      HomeMessage::decode(&unknown),
      Err(HomeWireError::Unknown { byte: 9 })
    );
    let mut refusal = vec![4u8];
    refusal.extend_from_slice(&[0; 32]);
    refusal.extend_from_slice(&[4, 77]);
    assert_eq!(
      HomeMessage::decode(&refusal),
      Err(HomeWireError::Unknown { byte: 77 })
    );
    let mut trailing = HomeMessage::Read { identity: [3; 32] }.encode();
    trailing.push(0);
    assert_eq!(
      HomeMessage::decode(&trailing),
      Err(HomeWireError::BadLength)
    );
  }
}
