//! A made commit owed by a submit whose green's map is built whole after it (A-664's live whole builds, part 2; main's
//! ruling of 2026-11-20: the effect and its completion record as one atom, CLAUDE.md §6's `run_recorded` pattern).
//!
//! **Why.** A version a work submits to a git repository's green becomes the commit the server makes over the version's
//! trees (`super::commits`). Where the green keeps no map of the version before (its first submit, one past a copy),
//! those trees are the whole head's, built in slices after the submit's step (`super::paths::whole_build`), so the
//! commit is made after the version is durable. What makes it (its message, its git identity and its date, the owner's
//! second at the submit) would otherwise live only in the submit's step, and a restart between the two would leave the
//! version with no commit, ever.
//!
//! **What.** The submit stages an owed commit ([`Owed`]) in its own transaction, as an entry of the repository's register
//! holding it alone (`RepositoryEntry::objects`, the way a made commit's record rides one), after the version's record:
//! the green's key and version, the message, the identity, and the second. The commit made at the build's end is made
//! from it, never from the clock (`super::commits::made_owed`), so the commit a recovery makes is byte for byte the one
//! the live store would have made. The made commit's record settles it: as its move of the green to that version commits
//! (`super::commit_moves`, live and in a replay alike), the owed commit goes. A store made again from the log replays
//! the owed record ([`super::replay_git`]); one no made commit settled is owed still, and its build starts again.
//!
//! **Bounds.** One owed a green: a submit to a green whose build runs is parked behind it (`super::paths::whole_build`),
//! so no second is staged while one is owed; the store keeps them by the green's key.
//!
//! **The encoding**, little-endian as a push's record is (`super::record`): the magic "SGOC" and the version; the
//! green's key, its version and the second, each a word of eight bytes; then the message, the identity's name and its
//! email, each its length in eight bytes and its UTF-8 bytes. Every length is checked against the bytes that remain
//! before anything is allocated, and bytes after the email are refused, each typed ([`OwedRefused`]), never a panic.

/// Format: the record's magic, "SGOC" (a slates git owed commit) little-endian, so a push's record ("SGPR") and this
/// are told apart at the first word.
const MAGIC: u32 = u32::from_le_bytes(*b"SGOC");

/// Format: the version this build writes.
const VERSION: u32 = 1;

/// Format: the bytes of the magic and of the version.
const WORD_BYTES: usize = size_of::<u32>();

/// Format: the bytes of the key, the version, the second and each length.
const LONG_BYTES: usize = size_of::<u64>();

/// A commit owed by a submit (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owed {
  /// The green's key in the store.
  pub(crate) key: u64,
  /// The green's version the commit is of.
  pub(crate) version: u64,
  /// The owner's second at the submit, in UTC: the commit's author and committer date.
  pub(crate) seconds: u64,
  /// The submit's message.
  pub(crate) message: String,
  /// The identity's name.
  pub(crate) name: String,
  /// The identity's email.
  pub(crate) email: String,
}

/// Why bytes are no owed commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OwedRefused {
  /// Not this record's magic or version.
  Foreign,
  /// A length past the bytes that remain, or bytes cut short.
  Short,
  /// A text that is not UTF-8.
  Text,
  /// Bytes after the email.
  Trailing,
}

/// Whether `bytes` are an owed commit's record, by its magic alone.
pub(crate) fn is_owed(bytes: &[u8]) -> bool {
  bytes
    .first_chunk::<WORD_BYTES>()
    .is_some_and(|word| u32::from_le_bytes(*word) == MAGIC)
}

impl Owed {
  /// The record's bytes (module docs, "The encoding").
  pub(crate) fn encode(&self) -> Vec<u8> {
    let texts = [&self.message, &self.name, &self.email];
    let len = texts
      .iter()
      .map(|text| text.len().saturating_add(LONG_BYTES))
      .fold(
        WORD_BYTES
          .saturating_mul(2)
          .saturating_add(LONG_BYTES.saturating_mul(3)),
        usize::saturating_add,
      );
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&MAGIC.to_le_bytes());
    out.extend_from_slice(&VERSION.to_le_bytes());
    for word in [self.key, self.version, self.seconds] {
      out.extend_from_slice(&word.to_le_bytes());
    }
    for text in texts {
      let text_len = u64::try_from(text.len()).unwrap_or(u64::MAX);
      out.extend_from_slice(&text_len.to_le_bytes());
      out.extend_from_slice(text.as_bytes());
    }
    out
  }

  /// The owed commit `bytes` hold, checked (module docs, "The encoding").
  pub(crate) fn decode(bytes: &[u8]) -> Result<Owed, OwedRefused> {
    let mut reader = Reader { rest: bytes };
    if reader.word()? != MAGIC || reader.word()? != VERSION {
      return Err(OwedRefused::Foreign);
    }
    let (key, version, seconds) = (reader.long()?, reader.long()?, reader.long()?);
    let (message, name, email) = (reader.text()?, reader.text()?, reader.text()?);
    if !reader.rest.is_empty() {
      return Err(OwedRefused::Trailing);
    }
    Ok(Owed {
      key,
      version,
      seconds,
      message,
      name,
      email,
    })
  }
}

/// The bytes of a record not read yet.
struct Reader<'a> {
  /// What remains.
  rest: &'a [u8],
}

impl Reader<'_> {
  /// The next four bytes as a word.
  fn word(&mut self) -> Result<u32, OwedRefused> {
    let (word, rest) = self
      .rest
      .split_first_chunk::<WORD_BYTES>()
      .ok_or(OwedRefused::Short)?;
    self.rest = rest;
    Ok(u32::from_le_bytes(*word))
  }

  /// The next eight bytes as a long.
  fn long(&mut self) -> Result<u64, OwedRefused> {
    let (long, rest) = self
      .rest
      .split_first_chunk::<LONG_BYTES>()
      .ok_or(OwedRefused::Short)?;
    self.rest = rest;
    Ok(u64::from_le_bytes(*long))
  }

  /// The next text: its length, checked against what remains, then its bytes.
  fn text(&mut self) -> Result<String, OwedRefused> {
    let len = usize::try_from(self.long()?).map_err(|_| OwedRefused::Short)?;
    if len > self.rest.len() {
      return Err(OwedRefused::Short);
    }
    let (text, rest) = self.rest.split_at(len);
    self.rest = rest;
    String::from_utf8(text.to_vec()).map_err(|_| OwedRefused::Text)
  }
}

#[cfg(test)]
mod tests {
  use super::{Owed, OwedRefused, is_owed};

  fn owed() -> Owed {
    Owed {
      key: 7,
      version: 3,
      seconds: 1_700_000_000,
      message: "edit the readme".to_owned(),
      name: "A U Thor".to_owned(),
      email: "author@example.com".to_owned(),
    }
  }

  /// A-664's live whole builds, part 2 (the owed commit's record). Do: encode an owed commit, decode it; then decode it
  /// cut at every length, with a byte after it, and with a push record's magic. Expect: decoded equal; every cut refused
  /// `Short` or `Foreign`, never a panic; the trailing byte `Trailing`; the push's magic `Foreign` and no owed record.
  #[test]
  fn an_owed_commit_reads_back_whole_and_refuses_every_damaged_form() {
    let bytes = owed().encode();
    assert!(is_owed(&bytes));
    assert_eq!(Owed::decode(&bytes), Ok(owed()));
    for cut in 0..bytes.len() {
      let refused = Owed::decode(bytes.get(..cut).unwrap_or_default());
      assert!(
        matches!(refused, Err(OwedRefused::Short | OwedRefused::Foreign)),
        "{cut}: {refused:?}"
      );
    }
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(Owed::decode(&trailing), Err(OwedRefused::Trailing));
    let mut push = bytes;
    push.splice(..4, *b"SGPR");
    assert!(!is_owed(&push));
    assert_eq!(Owed::decode(&push), Err(OwedRefused::Foreign));
  }
}
