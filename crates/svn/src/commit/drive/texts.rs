//! Where a commit's new texts go as their windows are applied (§4.17.1 "A push's intake", rule 1; A-716; main's order
//! of 2026-11-20 after a 500 MB commit by stock svn was refused E210009 and every new text was held whole).
//!
//! **The seam.** A drive applies each svndiff window into one window's room and hands the bytes to the caller's
//! [`TextSink`] at once; it names the text (BLAKE3, the records' rule) and checks its MD5 and SHA-1 as the windows pass,
//! so it never holds a text. The sink keeps them in the repository's one byte store (the green's staged block,
//! `slates_merge` `Green::stage`, `append_staged`, `seal_staged`), charged as written against the shard's room, never a
//! request's (rule 2). The caller owns what a drive staged and sealed: it keeps the sealed texts as the revision's when
//! the work lands, and gives back every staged and sealed text of a drive that ends otherwise (an abort, a refusal, the
//! connection's end).
//!
//! [`HeldTexts`] keeps them in memory: the tests' sink, and a caller's that has no store of its own.

use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use crate::index::ContentIdentity;

/// A text being staged, by the sink's number for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct StagedText(pub u64);

/// Why a sink refused a text's bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRefused {
  /// The shard has no room for them: `available` bytes left (§4.17.1 rule 2).
  Room {
    /// The bytes the shard could still take.
    available: u64,
  },
  /// The store's table of held texts is at its room, or a text's holders at their most.
  Held,
  /// No such staged text: one the sink never staged, or sealed already.
  Unknown,
}

/// Where a drive puts its new texts (the module documentation). Its methods take `&self`: it is lent with the
/// repository's sources for one call, and keeps its state inside.
pub trait TextSink {
  /// Starts a new text.
  ///
  /// # Errors
  /// [`TextRefused`].
  fn stage(&self) -> Result<StagedText, TextRefused>;

  /// Appends `bytes`, a window's new text, to `staged`.
  ///
  /// # Errors
  /// [`TextRefused`]: past the shard's room, or an unknown text.
  fn append(&self, staged: StagedText, bytes: &[u8]) -> Result<(), TextRefused>;

  /// Names `staged` `identity` once its last window is in.
  ///
  /// # Errors
  /// [`TextRefused`].
  fn seal(&self, staged: StagedText, identity: ContentIdentity) -> Result<(), TextRefused>;

  /// A text this sink sealed, read back: a later delta of the same commit may name it as its base.
  fn sealed(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>>;
}

/// A sink in memory (the module documentation): staged texts by number, sealed ones by identity.
#[derive(Debug, Default)]
pub struct HeldTexts {
  /// The texts being staged.
  staged: RefCell<BTreeMap<StagedText, Vec<u8>>>,
  /// The texts sealed.
  sealed: RefCell<BTreeMap<ContentIdentity, Vec<u8>>>,
  /// The next staged text's number.
  next: Cell<u64>,
}

impl HeldTexts {
  /// An empty sink.
  pub fn new() -> Self {
    Self::default()
  }

  /// The sealed texts, taken out: the revision's, once its work lands.
  pub fn take_sealed(&self) -> BTreeMap<ContentIdentity, Vec<u8>> {
    std::mem::take(&mut *self.sealed.borrow_mut())
  }

  /// Gives back every staged and sealed text: a drive that ended without landing, or one whose revision now holds
  /// them.
  pub fn clear(&self) {
    self.staged.borrow_mut().clear();
    self.sealed.borrow_mut().clear();
  }

  /// How many texts are being staged.
  pub fn staged_count(&self) -> usize {
    self.staged.borrow().len()
  }
}

impl TextSink for HeldTexts {
  fn stage(&self) -> Result<StagedText, TextRefused> {
    let number = self.next.get();
    self
      .next
      .set(number.checked_add(1).ok_or(TextRefused::Held)?);
    let staged = StagedText(number);
    self.staged.borrow_mut().insert(staged, Vec::new());
    Ok(staged)
  }

  fn append(&self, staged: StagedText, bytes: &[u8]) -> Result<(), TextRefused> {
    self
      .staged
      .borrow_mut()
      .get_mut(&staged)
      .ok_or(TextRefused::Unknown)?
      .extend_from_slice(bytes);
    Ok(())
  }

  fn seal(&self, staged: StagedText, identity: ContentIdentity) -> Result<(), TextRefused> {
    let bytes = self
      .staged
      .borrow_mut()
      .remove(&staged)
      .ok_or(TextRefused::Unknown)?;
    self.sealed.borrow_mut().entry(identity).or_insert(bytes);
    Ok(())
  }

  fn sealed(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    self.sealed.borrow().get(identity).cloned().map(Cow::Owned)
  }
}

impl std::fmt::Debug for dyn TextSink + '_ {
  /// The sink by its kind alone: its texts are the commit's, no one's to print.
  fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    formatter.write_str("TextSink")
  }
}
