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
//! **Checked from the quarantine** ([`Quarantined`]): the pack's checks and a verdict read each
//! object's bytes back from the store, by name.

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_git::object_name::{Kind, ObjectId};
use slates_git::receive::Repository;
use slates_git::store::Push;
use slates_git::store::kind_of;
use slates_git::store::views::IndexedObjects;
use slates_git::unpack::{self, Contents, Keep, Object};
use slates_git::{Admission, Hold};
use slates_vcs::ContentStore;

use super::incoming_of;
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
}

impl Contents for Intake<'_> {
  fn content(&self, at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    if let Some((object, _)) = self.queue.get(&at) {
      return (object.id == *id).then(|| Cow::Borrowed(object.content.as_slice()));
    }
    self.quarantined.content(at, id)
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
pub(super) struct Quarantined<'s> {
  /// The quarantine.
  pub(super) push: Option<&'s Push>,
  /// The store its objects are in.
  pub(super) store: &'s ContentStore,
}

impl Contents for Quarantined<'_> {
  fn content(&self, _at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    let (number, content, len) = self.push?.object(id)?;
    kind_of(number)?;
    // An object of one window is that window in the store: lent with no copy.
    let mut windows = self.store.windows(&content);
    if let (Some(window), None) = (windows.next(), windows.next())
      && u64::try_from(window.len()).ok() == Some(len)
    {
      return Some(Cow::Borrowed(window));
    }
    self.store.object_bytes(&content, len).map(Cow::Owned)
  }
}

/// Admits `queue`, the objects a session call resolved, to `quarantine` on the shard's state, then
/// gives each one's hold back to `admission` (the module docs). Refused at the first object the
/// store does not admit; every hold is given back whatever the outcome.
pub(super) fn admit_queued(
  state: &mut ShardState,
  quarantine: &mut GitPush,
  queue: BTreeMap<usize, (Object, Hold)>,
  admission: &mut Admission,
) -> Result<(), GitRefused> {
  let (objects, holds): (Vec<Object>, Vec<Hold>) = queue.into_values().unzip();
  let incoming: Vec<_> = objects.iter().map(incoming_of).collect();
  let admitted = quarantine.admit_all(state, &incoming);
  for hold in holds {
    admission.release(hold);
  }
  admitted
}
