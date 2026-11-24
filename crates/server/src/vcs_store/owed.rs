//! A made commit owed by a submit whose green's map was built whole after it (A-664's live whole builds, part 2): a
//! record no server writes since main 01a20182's ruling of 2026-11-24 01:36, kept so a log holding one is read and
//! refused typed.
//!
//! **Why it was.** A submit to a git repository's green outside a recorded request committed its version in its own
//! step, and where the green kept no map of the version before (its first submit, one past a copy), its commit was made
//! after the map's whole build. So the submit staged what makes the commit ([`Owed`]: the green's key and version, the
//! message, the identity and the owner's second) in an entry of the repository's register holding it alone, and the
//! build's end made the commit from it.
//!
//! **Why it is gone.** A version committed before its commit is made is a version committed alone, which main's ruling
//! forbids: a git green's submit is now a client's recorded request alone, refused `RecordedSubmitRequired` otherwise
//! (`crate::made_commit::take_turn`), and its made commit's map, whole or stepped, is put over the held verdict's preview
//! before its one step commits both (`super::commits::make_deferred`). Nothing makes an owed commit any longer, so a
//! store made again from a log that holds one refuses it typed, naming the green's key and the version
//! (`super::ReplayRefused::OwedCommit`, [`super::replay_git`]), rather than keep a version whose commit nothing will
//! make.
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

/// A commit owed by a submit (module docs). A daemon build reads its key and version alone, to name them.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(not(test), allow(dead_code))]
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
  /// The record's bytes (module docs, "The encoding"), as a server before main's ruling wrote them: a test's log holds
  /// one.
  #[cfg(test)]
  pub(crate) fn encode(&self) -> Vec<u8> {
    let texts = [&self.message, &self.name, &self.email];
    let len = texts
      .iter()
      .map(|text| text.len().saturating_add(LONG_BYTES))
      .fold(
        // Format: the magic and the version (two words), then the green's key, its version and the second (three
        // longs), before the texts (module docs, "The encoding").
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
