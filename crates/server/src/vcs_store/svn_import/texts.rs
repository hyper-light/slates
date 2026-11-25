//! An import's texts put into the shard's content store as the dump is read (goal 93, P7 finding (2); main 01a20182's
//! order of 2026-11-22 05:28): the daemon's [`TextSink`] for `slates_svn`'s import, whose module docs ("Texts") state
//! the seam.
//!
//! **What.** The import stages each text with its header's length where it has one, on the one window buffer the
//! import owns and hands from text to text (`ContentStore::begin_object_reusing`; main 01a20182's routing of 2026-11-22
//! 07:38: a buffer a text was among the import's churners), grown once to the window over the import; every piece is kept a window at a time as it
//! arrives (`ContentStore::put_window`), within the budget the call is lent (`crate::vcs_store::within_budget`); a text
//! is sealed under the identity the import named it by, which must be the store's own. So no text is held whole on the
//! heap: before, the importer read each into a buffer of its header's length (26.2 MB of churn for 26.2 MB of text in
//! w-svn's attribution, `Importer::node_record`), the revision carried it to its landing, and the landing put it into
//! the store from there.
//!
//! **Holds.** Each sealed text holds one count of the store. A revision's sealed texts are handed to its landing once
//! the import hands the revision over ([`ImportTexts::take_sealed`]), which keeps those the revision names stored as the
//! texts it put and gives back the rest (`svn_commit::stage_texts`). An import that ends otherwise gives every staged and
//! sealed text back ([`ImportTexts::give_back`]).

use std::borrow::Cow;
use std::cell::RefCell;

use slates_svn::commit::drive::{StagedText, TextRefused, TextSink};
use slates_svn::index::ContentIdentity;
use slates_vcs::{ContentId, ContentRefused, ContentStore, Writing};

/// The texts an import put into the content store (module docs), kept with the import between its calls: the one being
/// put and those sealed since its revision began, each sealed one holding one count of the store.
#[derive(Debug, Default)]
pub(crate) struct ImportTexts {
  /// The text being put: its number, its writer, and the bytes put so far.
  staged: Option<(StagedText, Writing, u64)>,
  /// The texts sealed since the open revision began, each with its length.
  sealed: Vec<(ContentIdentity, u64)>,
  /// The window buffer the import owns, handed from each text to the next (module docs).
  spare: Vec<u8>,
  /// The next text's number.
  next: u64,
}

impl ImportTexts {
  /// The texts sealed since the open revision began, taken out with their holds: the revision's, once the import hands
  /// it over (module docs, "Holds").
  pub(crate) fn take_sealed(&mut self) -> Vec<ContentIdentity> {
    std::mem::take(&mut self.sealed)
      .into_iter()
      .map(|(identity, _)| identity)
      .collect()
  }

  /// Gives back every text staged and sealed: an import that ended without handing its revision over.
  pub(crate) fn give_back(&mut self, store: &mut ContentStore) {
    if let Some((_, mut writing, _)) = self.staged.take() {
      self.spare = writing.take_buffer();
      store.abandon_object(writing);
    }
    for (identity, _) in std::mem::take(&mut self.sealed) {
      store.unpin_unlanded(&ContentId(identity));
      let _ = store.release(&ContentId(identity));
    }
  }
}

/// [`ImportTexts`] over the content store for one call of the import: the sink it writes through.
pub(crate) struct InStore<'a> {
  /// The import's texts.
  texts: RefCell<&'a mut ImportTexts>,
  /// The shard's content store, lent its budget for the call.
  store: RefCell<&'a mut ContentStore>,
}

impl<'a> InStore<'a> {
  /// `texts` put into `store` for one call.
  pub(crate) fn new(texts: &'a mut ImportTexts, store: &'a mut ContentStore) -> Self {
    Self {
      texts: RefCell::new(texts),
      store: RefCell::new(store),
    }
  }
}

/// The sink's refusal for the content store's: past the shard's room, the room it could still take; any other, the
/// store's table at its room.
fn refused(refused: &ContentRefused) -> TextRefused {
  match *refused {
    ContentRefused::OverBudget { room, .. } => TextRefused::Room { available: room },
    _ => TextRefused::Held,
  }
}

impl TextSink for InStore<'_> {
  /// Begins the text's object on the import's one window buffer (module docs); one text at a time.
  fn stage(&self, len: Option<u64>) -> Result<StagedText, TextRefused> {
    let mut texts = self.texts.borrow_mut();
    if texts.staged.is_some() {
      return Err(TextRefused::Held);
    }
    let number = StagedText(texts.next);
    texts.next = texts.next.checked_add(1).ok_or(TextRefused::Held)?;
    let spare = std::mem::take(&mut texts.spare);
    let writing = self.store.borrow().begin_object_reusing(len, spare);
    texts.staged = Some((number, writing, 0));
    Ok(number)
  }

  fn append(&self, staged: StagedText, bytes: &[u8]) -> Result<(), TextRefused> {
    let mut texts = self.texts.borrow_mut();
    let (number, writing, put) = texts.staged.as_mut().ok_or(TextRefused::Unknown)?;
    if *number != staged {
      return Err(TextRefused::Unknown);
    }
    self
      .store
      .borrow_mut()
      .put_window(writing, bytes)
      .map_err(|refusal| refused(&refusal))?;
    *put = put.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    Ok(())
  }

  /// Finishes the text's object, which must be the store's own under `identity`; refused, the object given up.
  fn seal(&self, staged: StagedText, identity: ContentIdentity) -> Result<(), TextRefused> {
    let mut texts = self.texts.borrow_mut();
    match texts.staged.take() {
      Some((number, mut writing, put)) if number == staged => {
        let mut store = self.store.borrow_mut();
        let made = store.finish_object(&mut writing);
        texts.spare = writing.take_buffer();
        let made = match made {
          Ok(made) => made,
          Err(refusal) => {
            store.abandon_object(writing);
            return Err(refused(&refusal));
          }
        };
        if made != ContentId(identity) {
          let _ = store.release(&made);
          return Err(TextRefused::Unknown);
        }
        // Unlanded until its batch lands: no landed text's let-go takes a window it shares meanwhile (main's ruling (B)
        // of 2026-11-24), whoever lands then, its own import's batch or a client's commit between two batches. Unpinned
        // where its hold is given back: its batch's landing, or however its write ends.
        if let Err(refusal) = store.pin_unlanded(&made) {
          let _ = store.release(&made);
          return Err(refused(&refusal));
        }
        texts.sealed.push((identity, put));
        Ok(())
      }
      other => {
        texts.staged = other;
        Err(TextRefused::Unknown)
      }
    }
  }

  /// A text sealed since the open revision began, read whole from the store: a later delta of the same revision may
  /// name it as its base.
  fn sealed(&self, identity: &ContentIdentity) -> Option<Cow<'_, [u8]>> {
    let len = self
      .texts
      .borrow()
      .sealed
      .iter()
      .find(|(sealed, _)| sealed == identity)?
      .1;
    let store = self.store.borrow();
    store
      .object_bytes(&store.root_of(&ContentId(*identity)), len)
      .map(Cow::Owned)
  }

  fn sealed_piece(&self, identity: &ContentIdentity, offset: u64, len: usize) -> Option<Vec<u8>> {
    let size = self
      .texts
      .borrow()
      .sealed
      .iter()
      .find(|(sealed, _)| sealed == identity)?
      .1;
    if offset >= size {
      return (offset == size).then(Vec::new);
    }
    let store = self.store.borrow();
    let run = store.run_at(&ContentId(*identity), size, offset)?;
    Some(run.get(..len).unwrap_or(&run).to_vec())
  }
}
