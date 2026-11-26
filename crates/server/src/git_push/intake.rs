//! A push's pack read straight into its quarantine (A-719 part 3; §4.17.1 "A push's intake", rule 1):
//! the session's site while it reads and checks the pack.
//!
//! **Read, then admitted.** A session call reads the repository's index and the quarantine as they
//! stand, read only, and the pack reader hands this site each object as it resolves, with the hold
//! its bytes took on the push's admission ([`Intake`]); the site queues it. Once the call returns,
//! the driver admits the queue to the quarantine on the shard's state ([`admit_queued`]) and gives
//! each hold back, so a push holds at most what one call of the client's bytes resolved, never its
//! whole pack. A delta's base is read back from the queue or from the quarantine; a thin pack's
//! from the repository's index.
//!
//! **Checked from the quarantine** ([`Quarantined`]): the pack's checks and a verdict read each object's bytes back by
//! name, where it lies in the push's pack (main's ruling B part 2: the pack is kept as the client sent it, its objects
//! never inflated beside it). A read that meets a let-go window (a thin pack's REF_DELTA against the repository's
//! object, whose window a landed pack's let-go gave back) answers none with the window noted, on the repository for the
//! push's driver to bring back and on the quarantine for its reader to stop at ([`Contents::content_away`]; goal 93
//! item 2b, precondition B1 of the landed let-go's lift, main's order of 2026-11-26).

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::BTreeMap;

use slates_git::object_name::{Kind, ObjectId};
use slates_git::receive::Repository;
use slates_git::store::views::IndexedObjects;
use slates_git::store::{IndexBases, Push, Repository as GitRepository, packed};
use slates_git::unpack::{self, Contents, Keep, Object};
use slates_git::{Admission, Hold};
use slates_vcs::ContentStore;

use crate::state::ShardState;
use crate::vcs_store::{GitPush, GitRefused};

/// The session's site while it reads a pack into the quarantine (the module docs).
pub(super) struct Intake<'s> {
  /// The repository's index, as a thin pack's bases and a push of deletes' checks read it.
  pub(super) indexed: IndexedObjects<'s>,
  /// The quarantine as it stood when the call began.
  pub(super) quarantined: Quarantined<'s>,
  /// The objects resolved by this call, by position, with their holds, for the driver to admit.
  pub(super) queue: BTreeMap<usize, (Object, Hold)>,
}

impl Repository for Intake<'_> {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    self.indexed.object(id)
  }

  fn holds(&self, id: &ObjectId) -> bool {
    self.indexed.holds(id)
  }

  fn away(&self) -> Option<u64> {
    Repository::away(&self.indexed)
  }
}

impl Contents for Intake<'_> {
  fn content(&self, at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    if let Some((object, _)) = self.queue.get(&at) {
      return (object.id == *id).then_some(Cow::Borrowed(object.content.as_slice()));
    }
    self.quarantined.content(at, id)
  }

  fn content_away(&self) -> Option<u64> {
    self.quarantined.content_away()
  }
}

impl Keep for Intake<'_> {
  fn keep(
    &mut self,
    at: usize,
    object: Object,
    hold: Hold,
    _admission: &mut Admission,
  ) -> Result<(), unpack::Refusal> {
    self.queue.insert(at, (object, hold));
    Ok(())
  }

  fn release(&mut self, hold: Hold, admission: &mut Admission) {
    admission.release(hold);
  }
}

/// A push's quarantine, read only: the objects admitted so far, their bytes from the store.
pub(crate) struct Quarantined<'s> {
  /// The quarantine.
  pub(crate) push: Option<&'s Push>,
  /// The store its objects are in.
  pub(crate) store: &'s ContentStore,
  /// The repository, whose index holds the objects a pack resends that the quarantine passed over (A-753).
  repository: &'s GitRepository,
  /// The let-go, by the store's number for it, of the window the last read to answer none met
  /// ([`Contents::content_away`]).
  away: Cell<Option<u64>>,
}

impl<'s> Quarantined<'s> {
  /// The quarantine `push` (none before its first object) over `repository`'s objects in `store`, read only.
  pub(crate) fn new(
    push: Option<&'s Push>,
    store: &'s ContentStore,
    repository: &'s GitRepository,
  ) -> Self {
    Self {
      push,
      store,
      repository,
      away: Cell::new(None),
    }
  }

  /// `waited`, the let-go window a read met, noted on the repository for the push's driver to bring back
  /// (`upload::brought_back_waited`) and here for the reader to stop at.
  fn noted(&self, waited: Option<packed::Waited>) {
    if let Some(waited) = waited {
      self.repository.wait_for(waited);
      self.away.set(
        self
          .store
          .away_number(&waited.pack, waited.len, waited.offset),
      );
    }
  }
}

impl Contents for Quarantined<'_> {
  fn content(&self, _at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    self.away.set(None);
    // The push's object is read where it is: in the store, lent with no copy where it is one window there (A-727 a1), or
    // where it lies in the push's pack (`Push::bytes`, main's ruling B part 2), a REF_DELTA's base outside the push read
    // from the index.
    if let Some(push) = self.push {
      let outside = IndexBases {
        repository: self.repository,
        store: self.store,
      };
      if let Some(read) = push.bytes(self.store, &outside, id) {
        return read
          .map_err(|refused| {
            eprintln!(
              "b1dbg {:?} quarantine miss {id}: {refused:?}",
              std::thread::current().id()
            );
            self.noted(refused.waited());
            eprintln!(
              "b1dbg {:?} quarantine noted let-go {:?}",
              std::thread::current().id(),
              self.away.get()
            );
          })
          .ok();
      }
    }
    // An object the repository indexed already is passed over by the quarantine as it arrives (`admit_object`): a pack
    // that resends it is read back from the index (A-753; git's receive-pack takes such a pack).
    let (_, stored, len) = self.repository.stored(self.store, id)?;
    let read = self.repository.read_stored(self.store, &stored, len);
    if read.is_none() {
      eprintln!(
        "b1dbg {:?} quarantine stored miss {id}",
        std::thread::current().id()
      );
      // A stored read notes the window it met on the repository itself.
      self.noted(self.repository.peek_waiting());
    }
    read
  }

  fn content_away(&self) -> Option<u64> {
    self.away.get()
  }
}

/// Admits `queue`, the objects a session call resolved, to `quarantine` on the shard's state, each held where it lies in
/// the push's pack ([`GitPush::admit_placed_all`]), then gives each one's hold back to `admission` (the module docs).
/// Refused at the first object the store does not admit; every hold is given back whatever the outcome.
pub(super) fn admit_queued(
  state: &mut ShardState,
  quarantine: &mut GitPush,
  queue: BTreeMap<usize, (Object, Hold)>,
  admission: &mut Admission,
) -> Result<(), GitRefused> {
  let (objects, holds): (Vec<Object>, Vec<Hold>) = queue.into_values().unzip();
  let admitted = quarantine.admit_placed_all(state, objects);
  for hold in holds {
    admission.release(hold);
  }
  // What the push's reads keep lately widened for the chains it carries now, within what the shard grants.
  quarantine.widen_reading(state);
  admitted
}
