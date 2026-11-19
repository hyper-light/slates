//! The `phase-heads` and `check:phases` payloads (A-545; `internals.bundle2` [B]; `phases.py` `binaryencode` and
//! `binarydecode` [C]): entries of 24 bytes, a big-endian signed 32-bit phase then a node. `phase-heads` gives the heads
//! of each phase (a getbundle's part, and a push's toward a server with `phases=heads`); `check:phases` gives the public
//! heads and draft roots the pushing client saw. 7.2 reads 24 bytes at a time and refuses a short last entry ("bad
//! phase-heads stream"); a phase it does not define fails it with a `KeyError` in `binarydecode`, and is refused here
//! by kind.
//!
//! Nothing is allocated: the payload is checked whole, then its entries are read in place, so the payload itself is the
//! only cap.

use core::slice::ChunksExact;

use crate::Node;
use crate::read::{Cursor, to_u64};

/// Format: one entry: a phase as a big-endian signed 32-bit integer, then a node (`_fphasesentry = '>i20s'`).
pub const ENTRY_LEN: usize = 24;

/// Format: public's number (`phases.public`).
const PUBLIC: i32 = 0;

/// Format: draft's number (`phases.draft`).
const DRAFT: i32 = 1;

/// Format: secret's number (`phases.secret`).
const SECRET: i32 = 2;

/// Format: archived's number (`archived = 32  # non-continuous for compatibility`).
const ARCHIVED: i32 = 32;

/// Format: internal's number (`internal = 96  # non-continuous for compatibility`).
const INTERNAL: i32 = 96;

/// A phase 7.2 defines, in its order (`allphases`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Phase {
  /// Published: immutable and shared.
  Public,
  /// Shared and mutable.
  Draft,
  /// Never exchanged.
  Secret,
  /// Kept out of sight instead of stripped (local).
  Archived,
  /// Internal to an operation (local).
  Internal,
}

impl Phase {
  /// The phase a number names, if 7.2 defines it.
  pub fn from_number(number: i32) -> Option<Self> {
    match number {
      PUBLIC => Some(Self::Public),
      DRAFT => Some(Self::Draft),
      SECRET => Some(Self::Secret),
      ARCHIVED => Some(Self::Archived),
      INTERNAL => Some(Self::Internal),
      _ => None,
    }
  }

  /// The phase's number on the wire.
  pub fn number(self) -> i32 {
    match self {
      Self::Public => PUBLIC,
      Self::Draft => DRAFT,
      Self::Secret => SECRET,
      Self::Archived => ARCHIVED,
      Self::Internal => INTERNAL,
    }
  }

  /// The phase's name, as 7.2 prints it (`phasenames`).
  pub fn name(self) -> &'static str {
    match self {
      Self::Public => "public",
      Self::Draft => "draft",
      Self::Secret => "secret",
      Self::Archived => "archived",
      Self::Internal => "internal",
    }
  }
}

/// One entry: a phase and a node (a head of that phase, or for `check:phases` a draft root).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PhaseHead {
  /// The phase.
  pub phase: Phase,
  /// The node.
  pub node: Node,
}

/// Why a phases payload was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
  /// A payload that is not a whole number of entries (7.2: "bad phase-heads stream").
  PartialEntry {
    /// The payload's length.
    len: u64,
  },
  /// A phase 7.2 does not define.
  UnknownPhase {
    /// Its number.
    number: i32,
    /// The entry's offset in the payload.
    at: u64,
  },
}

/// The entries of a payload, in order.
#[derive(Clone)]
pub struct Entries<'a> {
  /// The payload's entries.
  entries: ChunksExact<'a, u8>,
}

/// The entries of a `phase-heads` or `check:phases` payload, the payload checked whole first.
pub fn decode(payload: &[u8]) -> Result<Entries<'_>, Refusal> {
  let entries = payload.chunks_exact(ENTRY_LEN);
  if !entries.remainder().is_empty() {
    return Err(Refusal::PartialEntry {
      len: to_u64(payload.len()),
    });
  }
  for (index, entry) in entries.clone().enumerate() {
    let at = to_u64(index.saturating_mul(ENTRY_LEN));
    match split(entry) {
      Some((number, _)) if Phase::from_number(number).is_none() => {
        return Err(Refusal::UnknownPhase { number, at });
      }
      Some(_) => {}
      None => {
        return Err(Refusal::PartialEntry {
          len: to_u64(payload.len()),
        });
      }
    }
  }
  Ok(Entries { entries })
}

/// One entry's phase number and node.
fn split(entry: &[u8]) -> Option<(i32, Node)> {
  let mut cursor = Cursor::new(entry);
  let number = cursor.i32("phase").ok()?;
  let node = cursor.node("node").ok()?;
  Some((number, node))
}

impl Iterator for Entries<'_> {
  type Item = PhaseHead;

  fn next(&mut self) -> Option<PhaseHead> {
    let (number, node) = split(self.entries.next()?)?;
    Some(PhaseHead {
      phase: Phase::from_number(number)?,
      node,
    })
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    self.entries.size_hint()
  }
}

/// Appends the payload of `entries`, in order.
pub fn encode(entries: &[PhaseHead], out: &mut Vec<u8>) {
  for entry in entries {
    out.extend_from_slice(&entry.phase.number().to_be_bytes());
    out.extend_from_slice(&entry.node);
  }
}

#[cfg(test)]
mod tests {
  use super::{Phase, PhaseHead, Refusal, decode, encode};
  use proptest::prelude::*;

  /// A-545: do encode one entry of every phase and decode it. Expect each phase back with its node, 7.2's numbers
  /// (0, 1, 2, 32, 96) on the wire, and its names.
  #[test]
  fn every_phase_mercurial_defines_round_trips() {
    let all = [
      Phase::Public,
      Phase::Draft,
      Phase::Secret,
      Phase::Archived,
      Phase::Internal,
    ];
    let entries: Vec<PhaseHead> = all
      .iter()
      .enumerate()
      .map(|(i, phase)| PhaseHead {
        phase: *phase,
        node: [u8::try_from(i).unwrap(); 20],
      })
      .collect();
    let mut payload = Vec::new();
    encode(&entries, &mut payload);
    assert_eq!(decode(&payload).unwrap().collect::<Vec<_>>(), entries);
    assert_eq!(all.map(Phase::number), [0, 1, 2, 32, 96]);
    assert_eq!(
      all.map(Phase::name),
      ["public", "draft", "secret", "archived", "internal"]
    );
  }

  /// AC-HG.9 (hostile input), A-545: do give a payload cut inside an entry, and entries whose phase 7.2 does not
  /// define (3, and `u32::MAX`, which is −1 read signed). Expect `PartialEntry`, and `UnknownPhase` at its entry's
  /// offset.
  #[test]
  fn partial_entries_and_unknown_phases_are_refused() {
    let mut payload = Vec::new();
    encode(
      &[PhaseHead {
        phase: Phase::Draft,
        node: [1; 20],
      }],
      &mut payload,
    );
    assert_eq!(
      decode(&payload[..23]).err(),
      Some(Refusal::PartialEntry { len: 23 })
    );
    for number in [3_i32, -1] {
      let mut bad = payload.clone();
      bad.extend_from_slice(&number.to_be_bytes());
      bad.extend_from_slice(&[2; 20]);
      assert_eq!(
        decode(&bad).err(),
        Some(Refusal::UnknownPhase { number, at: 24 })
      );
    }
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

    /// AC-HG.9, A-545: do read arbitrary bytes as phase entries. Expect entries that encode back exactly, or a
    /// refusal; never a panic.
    #[test]
    fn arbitrary_payloads_read_exactly_or_are_refused(bytes in proptest::collection::vec(any::<u8>(), 0..128)) {
      if let Ok(entries) = decode(&bytes) {
        let entries: Vec<_> = entries.collect();
        let mut out = Vec::new();
        encode(&entries, &mut out);
        prop_assert_eq!(out, bytes);
      }
    }
  }
}
