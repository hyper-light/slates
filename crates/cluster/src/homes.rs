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
//! **Owners (A-727 (d1a), main 01a20182's rulings of 2026-11-23).** A hold is for its owners ([`HomeOwner`]): the attempt
//! in flight that homed it (its repository, the node and process that run it, its number there), then the records that name
//! it once the attempt's entry commits ([`HomeMessage::Moved`]). A keep is idempotent per (identity, owner); a let-go
//! releases one owner ([`HomeMessage::LetGo`]), or every one where a holder is no home; the bytes go once none is left.
//! An attempt a crash left unresolved is asked of the repository's current owner ([`HomeMessage::Resolve`]).
//!
//! **The wire.** A version byte ([`VERSION`]), then a kind byte, then the fields little-endian, byte strings length-prefixed by a `u32`: a window's bytes
//! are at most the archive format's chunk cap (`slates_archive::format::MAX_CHUNK_BYTES`), checked before they are
//! copied; truncation, an unknown kind or refusal, a length past the bytes and trailing bytes are each refused typed
//! ([`HomeWireError`]), never a panic. A message of another version (version 1 had none: its first byte was its kind) is
//! refused typed ([`HomeWireError::Version`]): a region whose members speak different versions homes nothing, so its
//! commits are refused until they match; there is no mixed-version homing.

use slates_archive::format::MAX_CHUNK_BYTES;
use slates_db::identity_homes::homes;
use slates_db::register::HostId;

/// The stream home exchanges ride on a member's record session: their own, beside the content plane's offer, chunk and
/// fetch streams (`crate::content::CONTENT_OFFER_STREAM` explains why each exchange kind has its own). Format: one
/// stream id a kind of exchange on a connection; a fixed label, not a tunable.
pub const HOME_STREAM: u64 = 30;

/// Format: the version byte every message begins with (A-727 (d1a): owners on the wire). Version 1 had none, its first
/// byte a kind of 1 to 6, so neither version reads the other's messages as its own.
pub const VERSION: u8 = 0x82;
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
/// Format: the kind byte of an owner's move (A-727 (d1a)).
const KIND_MOVED: u8 = 7;
/// Format: the kind byte of an attempt's resolution asked of its repository's owner (A-727 (d1a)).
const KIND_RESOLVE: u8 = 8;
/// Format: an owner's tag byte: an attempt in flight.
const OWNER_ATTEMPT: u8 = 1;
/// Format: an owner's tag byte: the records naming it.
const OWNER_RECORD: u8 = 2;
/// Format: a let-go's owner byte: one owner follows.
const LET_GO_ONE: u8 = 1;
/// Format: a let-go's owner byte: every owner.
const LET_GO_ALL: u8 = 0;
/// Format: the width of a repository id.
const REPOSITORY_BYTES: usize = 16;
/// Format: the fewest bytes an owner takes on the wire (a record's: its tag and repository), by which a count of owners
/// is checked against the bytes left before anything is reserved.
const OWNER_MIN_BYTES: usize = 1 + REPOSITORY_BYTES;
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
/// Format: the outcome byte of an owner moved (A-727 (d1a)).
const OUTCOME_MOVED: u8 = 7;
/// Format: the outcome byte of a resolution: whether a record names the identity, a byte 0 or 1 (A-727 (d1a)).
const OUTCOME_NAMED: u8 = 8;

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
  /// The message was of another version (module docs, "The wire"): answered at once, so its sender refuses its homing
  /// at once rather than waiting out its deadline (A-727 (d1a)).
  Version,
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
      HomeRefusal::Version => 6,
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
      6 => Some(HomeRefusal::Version),
      _ => None,
    }
  }
}

/// Who a hold is for (module docs, "Owners").
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HomeOwner {
  /// An attempt in flight: its repository, the member and process running it (the process's nonce, fresh at every start
  /// of its daemon, since a member's SWIM incarnation starts over at each start and a warm restart keeps its process nonce),
  /// and its number in that process.
  Attempt {
    /// The repository's id.
    repository: [u8; 16],
    /// The member running it.
    node: HostId,
    /// That member's process.
    process: u64,
    /// Its number in that process.
    attempt: u64,
  },
  /// The records of a repository, which name it.
  Record {
    /// The repository's id.
    repository: [u8; 16],
  },
}

impl HomeOwner {
  /// The owner's repository.
  pub fn repository(&self) -> [u8; 16] {
    match self {
      HomeOwner::Attempt { repository, .. } | HomeOwner::Record { repository } => *repository,
    }
  }

  /// Its bytes on the wire and in a home's image.
  pub fn encode_into(&self, out: &mut Vec<u8>) {
    match self {
      HomeOwner::Attempt {
        repository,
        node,
        process,
        attempt,
      } => {
        out.push(OWNER_ATTEMPT);
        out.extend_from_slice(repository);
        out.extend_from_slice(&node.0.to_le_bytes());
        out.extend_from_slice(&process.to_le_bytes());
        out.extend_from_slice(&attempt.to_le_bytes());
      }
      HomeOwner::Record { repository } => {
        out.push(OWNER_RECORD);
        out.extend_from_slice(repository);
      }
    }
  }

  /// One read from the front of `bytes`, with what is left; refused typed where they are short or name no owner.
  pub fn decode_from(bytes: &[u8]) -> Result<(HomeOwner, &[u8]), HomeWireError> {
    let mut reader = Reader { rest: bytes };
    let owner = reader.owner()?;
    Ok((owner, reader.rest))
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
  /// The owner was moved (A-727 (d1a)).
  Moved,
  /// A resolution's answer: whether a record of the attempt's repository names the identity (A-727 (d1a)).
  Named {
    /// Whether one does.
    named: bool,
  },
}

/// A home exchange's message (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HomeMessage {
  /// Keep the window `bytes`, named `identity`, for `owners`.
  Put {
    /// The window's identity: the BLAKE3 of its bytes.
    identity: [u8; 32],
    /// Who it is kept for (at least one).
    owners: Vec<HomeOwner>,
    /// Its bytes.
    bytes: Vec<u8>,
  },
  /// Read the window named `identity` back.
  Read {
    /// Its identity.
    identity: [u8; 32],
  },
  /// Let `owner`'s hold of the window named `identity` go, or every owner's (`None`: a holder that is no home).
  LetGo {
    /// Its identity.
    identity: [u8; 32],
    /// The owner whose hold goes, or none for every one.
    owner: Option<HomeOwner>,
  },
  /// Keep part of the object named `identity`, `total` bytes long, its bytes from `offset` (appended, A-724 piece 8c: an
  /// object past one message, sent in order a part at a time, each at most [`PART_BYTES`]).
  PutPart {
    /// The object's identity: the BLAKE3 of its whole bytes.
    identity: [u8; 32],
    /// Who it is kept for, once whole (at least one).
    owners: Vec<HomeOwner>,
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
  /// Move the hold of the object named `identity` from owner `from` to owner `to` (A-727 (d1a): an attempt's entry
  /// committed, so its records now name it).
  Moved {
    /// Its identity.
    identity: [u8; 32],
    /// The owner it was held for.
    from: HomeOwner,
    /// The owner it is held for now.
    to: HomeOwner,
  },
  /// Whether a record of `attempt`'s repository names the object `identity` (A-727 (d1a)): asked of the repository's
  /// current owner by a home holding it for an attempt whose process has ended.
  Resolve {
    /// Its identity.
    identity: [u8; 32],
    /// The attempt it is held for.
    attempt: HomeOwner,
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
  /// The message is of another version (module docs, "The wire").
  Version {
    /// The first byte found.
    byte: u8,
  },
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
    let mut out = vec![VERSION];
    match self {
      HomeMessage::Put {
        identity,
        owners,
        bytes,
      } => {
        out.push(KIND_PUT);
        out.extend_from_slice(identity);
        put_owners(&mut out, owners);
        put_blob(&mut out, bytes);
      }
      HomeMessage::Read { identity } => {
        out.push(KIND_READ);
        out.extend_from_slice(identity);
      }
      HomeMessage::LetGo { identity, owner } => {
        out.push(KIND_LET_GO);
        out.extend_from_slice(identity);
        match owner {
          Some(owner) => {
            out.push(LET_GO_ONE);
            owner.encode_into(&mut out);
          }
          None => out.push(LET_GO_ALL),
        }
      }
      HomeMessage::PutPart {
        identity,
        owners,
        total,
        offset,
        bytes,
      } => {
        out.push(KIND_PUT_PART);
        out.extend_from_slice(identity);
        put_owners(&mut out, owners);
        out.extend_from_slice(&total.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        put_blob(&mut out, bytes);
      }
      HomeMessage::ReadPart { identity, offset } => {
        out.push(KIND_READ_PART);
        out.extend_from_slice(identity);
        out.extend_from_slice(&offset.to_le_bytes());
      }
      HomeMessage::Moved { identity, from, to } => {
        out.push(KIND_MOVED);
        out.extend_from_slice(identity);
        from.encode_into(&mut out);
        to.encode_into(&mut out);
      }
      HomeMessage::Resolve { identity, attempt } => {
        out.push(KIND_RESOLVE);
        out.extend_from_slice(identity);
        attempt.encode_into(&mut out);
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
          HomeOutcome::Moved => out.push(OUTCOME_MOVED),
          HomeOutcome::Named { named } => {
            out.push(OUTCOME_NAMED);
            out.push(u8::from(*named));
          }
        }
      }
    }
    out
  }

  /// Parses a message (module docs, "The wire"): refused [`HomeWireError::Version`] for one of another version.
  pub fn decode(bytes: &[u8]) -> Result<HomeMessage, HomeWireError> {
    let mut reader = Reader { rest: bytes };
    let version = reader.byte()?;
    if version != VERSION {
      return Err(HomeWireError::Version { byte: version });
    }
    let kind = reader.byte()?;
    let identity = reader.identity()?;
    let message = match kind {
      KIND_PUT => HomeMessage::Put {
        identity,
        owners: reader.owners()?,
        bytes: reader.blob()?,
      },
      KIND_READ => HomeMessage::Read { identity },
      KIND_LET_GO => HomeMessage::LetGo {
        identity,
        owner: match reader.byte()? {
          LET_GO_ONE => Some(reader.owner()?),
          LET_GO_ALL => None,
          byte => return Err(HomeWireError::Unknown { byte }),
        },
      },
      KIND_PUT_PART => HomeMessage::PutPart {
        identity,
        owners: reader.owners()?,
        total: reader.long()?,
        offset: reader.long()?,
        bytes: reader.blob()?,
      },
      KIND_READ_PART => HomeMessage::ReadPart {
        identity,
        offset: reader.long()?,
      },
      KIND_MOVED => HomeMessage::Moved {
        identity,
        from: reader.owner()?,
        to: reader.owner()?,
      },
      KIND_RESOLVE => HomeMessage::Resolve {
        identity,
        attempt: reader.owner()?,
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
          OUTCOME_MOVED => HomeOutcome::Moved,
          OUTCOME_NAMED => HomeOutcome::Named {
            named: match reader.byte()? {
              0 => false,
              1 => true,
              byte => return Err(HomeWireError::Unknown { byte }),
            },
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

/// A count of owners, then each.
fn put_owners(out: &mut Vec<u8>, owners: &[HomeOwner]) {
  out.extend_from_slice(
    &u32::try_from(owners.len())
      .unwrap_or(u32::MAX)
      .to_le_bytes(),
  );
  for owner in owners {
    owner.encode_into(out);
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

  /// An owner ([`HomeOwner::encode_into`]).
  fn owner(&mut self) -> Result<HomeOwner, HomeWireError> {
    let tag = self.byte()?;
    let repository = <[u8; REPOSITORY_BYTES]>::try_from(self.take(REPOSITORY_BYTES)?)
      .map_err(|_| HomeWireError::Truncated)?;
    match tag {
      OWNER_ATTEMPT => Ok(HomeOwner::Attempt {
        repository,
        node: HostId(self.long()?),
        process: self.long()?,
        attempt: self.long()?,
      }),
      OWNER_RECORD => Ok(HomeOwner::Record { repository }),
      byte => Err(HomeWireError::Unknown { byte }),
    }
  }

  /// A count of owners, then each ([`put_owners`]): the count checked against the bytes left before anything is reserved,
  /// and at least one.
  fn owners(&mut self) -> Result<Vec<HomeOwner>, HomeWireError> {
    let bytes = <[u8; 4]>::try_from(self.take(4)?).map_err(|_| HomeWireError::Truncated)?;
    let count = usize::try_from(u32::from_le_bytes(bytes)).map_err(|_| HomeWireError::BadLength)?;
    if count == 0 || count > self.rest.len() / OWNER_MIN_BYTES {
      return Err(HomeWireError::BadLength);
    }
    let mut owners = Vec::with_capacity(count);
    for _ in 0..count {
      owners.push(self.owner()?);
    }
    Ok(owners)
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
  use super::{HomeMessage, HomeOutcome, HomeOwner, HomeRefusal, HomeWireError, VERSION, admits};
  use proptest::prelude::*;
  use slates_db::identity_homes::homes;
  use slates_db::register::HostId;

  /// Any message, each kind alike. The union is built as `prop_oneof!` builds one of more than ten arms, from boxed
  /// strategies: its shorter forms hold their arms in `Arc`s (R2).
  /// Any owner (A-727 (d1a)).
  fn owner() -> impl Strategy<Value = HomeOwner> {
    let repository = proptest::array::uniform16(any::<u8>());
    prop_oneof![
      (repository, any::<u64>(), any::<u64>(), any::<u64>()).prop_map(
        |(repository, node, process, attempt)| HomeOwner::Attempt {
          repository,
          node: HostId(node),
          process,
          attempt,
        }
      ),
      repository.prop_map(|repository| HomeOwner::Record { repository }),
    ]
  }

  fn message() -> impl Strategy<Value = HomeMessage> {
    let identity = proptest::array::uniform32(any::<u8>());
    let bytes = proptest::collection::vec(any::<u8>(), 0..2048);
    let owners = || proptest::collection::vec(owner(), 1..4);
    proptest::strategy::Union::new([
      (identity, owners(), bytes.clone())
        .prop_map(|(identity, owners, bytes)| HomeMessage::Put {
          identity,
          owners,
          bytes,
        })
        .boxed(),
      identity
        .prop_map(|identity| HomeMessage::Read { identity })
        .boxed(),
      (identity, proptest::option::of(owner()))
        .prop_map(|(identity, owner)| HomeMessage::LetGo { identity, owner })
        .boxed(),
      (
        identity,
        owners(),
        any::<u64>(),
        any::<u64>(),
        bytes.clone(),
      )
        .prop_map(
          |(identity, owners, total, offset, bytes)| HomeMessage::PutPart {
            identity,
            owners,
            total,
            offset,
            bytes,
          },
        )
        .boxed(),
      (identity, owner(), owner())
        .prop_map(|(identity, from, to)| HomeMessage::Moved { identity, from, to })
        .boxed(),
      (identity, owner())
        .prop_map(|(identity, attempt)| HomeMessage::Resolve { identity, attempt })
        .boxed(),
      (identity, any::<bool>())
        .prop_map(|(identity, named)| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::Named { named },
        })
        .boxed(),
      identity
        .prop_map(|identity| HomeMessage::Answer {
          identity,
          outcome: HomeOutcome::Moved,
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
    let mut long = vec![VERSION, 1u8];
    long.extend_from_slice(&[0; 32]);
    long.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(HomeMessage::decode(&long), Err(HomeWireError::BadLength));
    let mut unknown = vec![VERSION, 9u8];
    unknown.extend_from_slice(&[0; 32]);
    assert_eq!(
      HomeMessage::decode(&unknown),
      Err(HomeWireError::Unknown { byte: 9 })
    );
    let mut refusal = vec![VERSION, 4u8];
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

  /// A-727 (d1a), no mixed-version homing (main 01a20182's ruling of 2026-11-23). Do: decode a version-1 put (its kind
  /// byte first, no owner) and a put of the version after this one; decode a put with no owner and one whose owner
  /// count passes its bytes. Expect: both versions refused `Version` with the byte found, never read as an ownerless
  /// put; the owners refused `BadLength`.
  #[test]
  fn a_home_message_of_another_version_or_without_an_owner_is_refused_typed() {
    let mut first = vec![1u8];
    first.extend_from_slice(&[5; 32]);
    first.extend_from_slice(&3u32.to_le_bytes());
    first.extend_from_slice(b"abc");
    assert_eq!(
      HomeMessage::decode(&first),
      Err(HomeWireError::Version { byte: 1 })
    );
    let mut next = HomeMessage::Read { identity: [5; 32] }.encode();
    next[0] = VERSION.wrapping_add(1);
    assert_eq!(
      HomeMessage::decode(&next),
      Err(HomeWireError::Version {
        byte: VERSION.wrapping_add(1)
      })
    );
    let mut ownerless = vec![VERSION, 1u8];
    ownerless.extend_from_slice(&[5; 32]);
    ownerless.extend_from_slice(&0u32.to_le_bytes());
    ownerless.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(
      HomeMessage::decode(&ownerless),
      Err(HomeWireError::BadLength)
    );
    let mut counted = vec![VERSION, 1u8];
    counted.extend_from_slice(&[5; 32]);
    counted.extend_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(HomeMessage::decode(&counted), Err(HomeWireError::BadLength));
  }
}
