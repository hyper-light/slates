//! A Shards stream's tables (§4.6 the Shards form; A-303): what a stream's guest holds of its share, handed to Shards'
//! device when it saves a VM and taken back by a fresh stream when the VM is restored (Shards' D176 part 5).
//!
//! **What they hold.** The volume the stream served and the snapshot its SAVE took of it at the cut; the directory the
//! stream presented (its scope's inode, none for the whole volume); the owner view its guest's run named (A-281); and
//! every inode the guest's kernel holds, with its references and how many of them are open handles, exactly as the
//! volume keeps them for the stream's attachment ([`slates_vfs::recover::InodeReferences`], the recovery image's own
//! record). Node ids are inode numbers and a clone shares its origin's numbers (A-166), so they name the same files on
//! the volume unchanged since the cut and on a clone of its snapshot. No handle table is kept: a request names its
//! node, and a handle the restored stream never issued gives its open back at its release, as one from before a
//! takeover does (A-152).
//!
//! **Format.** A format word, slates' canonical wire encoding of [`StreamTables`], then the CRC-32C of both. Shards
//! keeps the bytes in the VM's snapshot on disk and hands them back whole, opaque; the check refuses tables a disk or a
//! copy damaged, rather than giving a guest references to whatever inode a flipped bit names.
//!
//! **Hostile input.** The bytes come back from outside the daemon. They are refused before anything is allocated when
//! longer than any tables this daemon's shard could make ([`most_bytes`]: its inode slab's capacity, an entry each),
//! then by their check, their format and a canonical decode that refuses trailing bytes. What survives is still only a
//! claim: the stream checks it against its volume, its scope and the volume's own records before giving anything.

use slates_db::catalog::{SnapshotId, VolumeId};
use slates_vfs::recover::InodeReferences;
use slates_wire::Wire;

/// Format: the tables' format, the word they begin with. A new format takes the next number; a restore refuses tables
/// of another.
pub(super) const FORMAT: u32 = 1;
/// Format: the bytes of the format word before the body, and of the check after it (a little-endian `u32` each).
const WORD_BYTES: usize = size_of::<u32>();

/// What a stream's guest holds of its share at a VM's save.
#[derive(Wire, Clone, Debug, PartialEq, Eq)]
pub(super) struct StreamTables {
  /// The volume the stream served.
  pub(super) volume: VolumeId,
  /// The snapshot the SAVE took of it at the cut.
  pub(super) snapshot: SnapshotId,
  /// The inode of the directory the stream presented; none for the whole volume.
  pub(super) scope: Option<u64>,
  /// The owners the guest's run named, swapped (A-281); none for the identity.
  pub(super) owners: Option<SwappedOwners>,
  /// Each inode the guest holds, in rising inode order, with its references and the opens among them.
  pub(super) held: Vec<InodeReferences>,
}

/// The two owners an owner view swaps (A-281): the volume's and the run's, uid and gid each.
#[derive(Wire, Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SwappedOwners {
  /// The volume's owner's uid.
  pub(super) kept_uid: u32,
  /// The volume's owner's gid.
  pub(super) kept_gid: u32,
  /// The run's uid.
  pub(super) run_uid: u32,
  /// The run's gid.
  pub(super) run_gid: u32,
}

/// Why tables were refused, each with the reason the refusal carries to Shards, which reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TablesRefused {
  /// Longer than any tables this daemon's shard could have made.
  TooLong,
  /// Shorter than a format word and a check.
  Truncated,
  /// The check does not match: the bytes were damaged.
  Damaged,
  /// Of another format.
  Format,
  /// The body is no canonical encoding of the tables.
  Malformed,
}

impl TablesRefused {
  /// The reason its refusal carries.
  pub(super) fn reason(self) -> &'static str {
    match self {
      TablesRefused::TooLong => {
        "the tables are longer than any this daemon's shard could have made"
      }
      TablesRefused::Truncated => "the tables are cut short",
      TablesRefused::Damaged => "the tables are damaged: their check does not match",
      TablesRefused::Format => "the tables are of a format this daemon does not read",
      TablesRefused::Malformed => "the tables do not decode",
    }
  }
}

/// The bytes of `tables`: the format word, the body, the check.
pub(super) fn encode(tables: &StreamTables) -> Vec<u8> {
  let mut bytes = FORMAT.to_le_bytes().to_vec();
  tables.encode(&mut bytes);
  let check = slates_wire::crc32c::crc32c(&bytes);
  bytes.extend_from_slice(&check.to_le_bytes());
  bytes
}

/// The tables `bytes` hold, refused as the module's notes say; `most` is [`most_bytes`] for the shard decoding them.
pub(super) fn decode(bytes: &[u8], most: usize) -> Result<StreamTables, TablesRefused> {
  if bytes.len() > most {
    return Err(TablesRefused::TooLong);
  }
  let checked_len = bytes
    .len()
    .checked_sub(WORD_BYTES)
    .filter(|len| *len >= WORD_BYTES)
    .ok_or(TablesRefused::Truncated)?;
  let (checked, check) = bytes
    .split_at_checked(checked_len)
    .ok_or(TablesRefused::Truncated)?;
  let check =
    u32::from_le_bytes(<[u8; WORD_BYTES]>::try_from(check).map_err(|_| TablesRefused::Truncated)?);
  if slates_wire::crc32c::crc32c(checked) != check {
    return Err(TablesRefused::Damaged);
  }
  let (format, body) = checked
    .split_at_checked(WORD_BYTES)
    .ok_or(TablesRefused::Truncated)?;
  let format =
    u32::from_le_bytes(<[u8; WORD_BYTES]>::try_from(format).map_err(|_| TablesRefused::Truncated)?);
  if format != FORMAT {
    return Err(TablesRefused::Format);
  }
  StreamTables::from_bytes(body).map_err(|_| TablesRefused::Malformed)
}

/// Derived: the longest tables a shard holding at most `max_inodes` inode versions could make: the fixed fields at
/// their longest, one entry for every inode it could hold, and the two words, from the encoding itself. A restore
/// refuses longer bytes before it reads them.
pub(super) fn most_bytes(max_inodes: usize) -> usize {
  let fixed = StreamTables {
    volume: VolumeId::default(),
    snapshot: SnapshotId::default(),
    scope: Some(u64::MAX),
    owners: Some(SwappedOwners {
      kept_uid: u32::MAX,
      kept_gid: u32::MAX,
      run_uid: u32::MAX,
      run_gid: u32::MAX,
    }),
    held: Vec::new(),
  }
  .to_bytes()
  .len();
  let entry = InodeReferences {
    inode: u64::MAX,
    count: u32::MAX,
    opens: u32::MAX,
  }
  .to_bytes()
  .len();
  fixed
    .saturating_add(entry.saturating_mul(max_inodes))
    .saturating_add(WORD_BYTES.saturating_mul(2))
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Tables holding the run's owner swapped with the host's, a scope, an open file, a held directory and a file the
  /// guest holds with no name left.
  fn tables() -> StreamTables {
    StreamTables {
      volume: VolumeId { bytes: [7; 16] },
      snapshot: SnapshotId { value: 1 << 32 | 3 },
      scope: Some(9),
      owners: Some(SwappedOwners {
        kept_uid: 501,
        kept_gid: 20,
        run_uid: 1000,
        run_gid: 1001,
      }),
      held: vec![
        InodeReferences {
          inode: 9,
          count: 2,
          opens: 0,
        },
        InodeReferences {
          inode: 12,
          count: 3,
          opens: 1,
        },
        InodeReferences {
          inode: 40,
          count: 1,
          opens: 1,
        },
      ],
    }
  }

  /// A-303. Do: encode tables and decode them under the bound of a shard that holds as many inodes as they name, and
  /// encode them twice. Expect them back whole, the two encodings byte for byte the same (a determinism gate: Shards
  /// keeps these bytes in a VM's snapshot).
  #[test]
  fn tables_come_back_whole_and_encode_the_same_each_time() {
    let tables = tables();
    let bytes = encode(&tables);
    assert_eq!(
      decode(&bytes, most_bytes(tables.held.len())),
      Ok(tables.clone())
    );
    assert_eq!(encode(&tables), bytes);
  }

  /// A-303, hostile input. Do: hand the decoder tables longer than the shard's bound, cut to every length short of
  /// whole, with each single bit flipped, of another format with a matching check, with a vector length of `u32::MAX`
  /// and a matching check, and with a byte past the body and a matching check. Expect each refused, by length before
  /// anything else, damage by the check, the format and the body each for its own reason, and never a panic.
  #[test]
  fn tables_from_outside_are_refused_before_they_are_trusted() {
    let tables = tables();
    let bytes = encode(&tables);
    let bound = most_bytes(tables.held.len());
    assert_eq!(decode(&bytes, bytes.len() - 1), Err(TablesRefused::TooLong));
    for cut in 0..bytes.len() {
      let refused = decode(&bytes[..cut], bound).unwrap_err();
      assert!(
        matches!(refused, TablesRefused::Truncated | TablesRefused::Damaged),
        "cut to {cut}: {refused:?}"
      );
    }
    for at in 0..bytes.len() * 8 {
      let mut flipped = bytes.clone();
      flipped[at / 8] ^= 1 << (at % 8);
      assert_eq!(
        decode(&flipped, bound),
        Err(TablesRefused::Damaged),
        "bit {at}"
      );
    }
    let rechecked = |mut checked: Vec<u8>| {
      let check = slates_wire::crc32c::crc32c(&checked);
      checked.extend_from_slice(&check.to_le_bytes());
      checked
    };
    let checked = &bytes[..bytes.len() - WORD_BYTES];
    let mut other = checked.to_vec();
    other[..WORD_BYTES].copy_from_slice(&(FORMAT + 1).to_le_bytes());
    assert_eq!(decode(&rechecked(other), bound), Err(TablesRefused::Format));
    let mut trailing = checked.to_vec();
    trailing.push(0);
    assert_eq!(
      decode(&rechecked(trailing), bound + 1),
      Err(TablesRefused::Malformed)
    );
    // The held vector's length prefix sits right before its entries: claim u32::MAX of them.
    let entries_bytes = tables.held.len() * 16;
    let prefix_at = checked.len() - entries_bytes - WORD_BYTES;
    let mut endless = checked.to_vec();
    endless[prefix_at..prefix_at + WORD_BYTES].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(
      decode(&rechecked(endless), bound),
      Err(TablesRefused::Malformed)
    );
  }

  /// A-303. Do: work out the bound for shards of 0, 1 and 1,000 inodes. Expect it to grow by one entry's encoding per
  /// inode from the fixed part's, and the largest real tables of that many inodes to fit it.
  #[test]
  fn the_bound_is_the_fixed_part_and_an_entry_per_inode() {
    let entry = most_bytes(1) - most_bytes(0);
    assert_eq!(most_bytes(1_000), most_bytes(0) + 1_000 * entry);
    let mut tables = tables();
    tables.held = (0..1_000u64)
      .map(|inode| InodeReferences {
        inode,
        count: u32::MAX,
        opens: u32::MAX,
      })
      .collect();
    assert!(encode(&tables).len() <= most_bytes(1_000));
  }
}
