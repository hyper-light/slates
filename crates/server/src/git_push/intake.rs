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
//! object's bytes back by name, where it lies in the push's pack (main's ruling B part 2: the pack is
//! kept as the client sent it, its objects never inflated beside it).

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_git::object_name::{Kind, ObjectId};
use slates_git::receive::Repository;
use slates_git::store::packed::Waited;
use slates_git::store::views::IndexedObjects;
use slates_git::store::{IndexBases, Push, Repository as GitRepository};
use slates_git::unpack::{self, Contents, Keep, Object};
use slates_git::{Admission, Hold};
use slates_vcs::content::IDENTITY_BYTES;
use slates_vcs::{ContentId, ContentStore};

use crate::state::ShardState;
use crate::vcs_store::{GitPush, GitRefused};

/// Format: input bytes hashed by [`away_token`]: the 32-byte pack [`ContentId`] followed by the
/// 8-byte little-endian byte offset within that pack.
const AWAY_TOKEN_INPUT_BYTES: usize = IDENTITY_BYTES + core::mem::size_of::<u64>();

/// Clears any prior let-go pack window noted on `repository` before a new read or turn so a
/// non-away miss cannot inherit an earlier read's window and so `note_waited` records only the
/// current read's own miss.
pub(super) fn clear_waiting(repository: &GitRepository) {
  let _cleared = repository.waiting();
}

/// Hashes `waited`'s pack identity and byte offset into a 64-bit BLAKE3 prefix used as a progress
/// token by `away()` callers (`unpack::Reader`, `receive::Checking`, `receive::Verdicts`).
///
/// Using a 64-bit BLAKE3 prefix provides birthday-bound collision resistance across distinct
/// `(pack, offset)` pairs (probabilistic rather than mathematically injective) so multi-pack
/// `REF_DELTA`, connectivity-walk, and tag-peel chains advance across bring-back turns even when
/// different packs place objects at the same byte offset, while an unchanged `(pack, offset)`
/// after a failed bring-back yields the identical token so every progress guard still terminates.
fn away_token(waited: Waited) -> u64 {
  let mut input = [0_u8; AWAY_TOKEN_INPUT_BYTES];
  if let Some((pack_slot, offset_slot)) = input.split_at_mut_checked(IDENTITY_BYTES) {
    pack_slot.copy_from_slice(&waited.pack.0);
    offset_slot.copy_from_slice(&waited.offset.to_le_bytes());
  }
  let digest = ContentId::of(&input);
  let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = digest.0;
  u64::from_le_bytes([b0, b1, b2, b3, b4, b5, b6, b7])
}

/// [`IndexedObjects`] paired with its repository so [`Repository::away`] reports [`away_token`]
/// (combining `Waited::pack` and `Waited::offset`) instead of the raw byte offset alone.
pub(super) struct TrackedIndexed<'s> {
  /// The repository's indexed object view.
  indexed: IndexedObjects<'s>,
  /// The repository whose `waiting` cell carries the full [`Waited`] pack identity and offset.
  repository: &'s GitRepository,
}

impl<'s> TrackedIndexed<'s> {
  /// Constructs a tracked index view over `repository` and `store` within `room` bytes.
  pub(super) fn new(repository: &'s GitRepository, store: &'s ContentStore, room: u64) -> Self {
    Self {
      indexed: IndexedObjects::new(repository, store, room),
      repository,
    }
  }

  /// Borrows the underlying [`IndexedObjects`] view.
  pub(super) fn inner(&self) -> &IndexedObjects<'s> {
    &self.indexed
  }
}

impl Repository for TrackedIndexed<'_> {
  fn object(&self, id: &ObjectId) -> Option<(Kind, &[u8])> {
    self.indexed.object(id)
  }

  fn holds(&self, id: &ObjectId) -> bool {
    self.indexed.holds(id)
  }

  fn kind(&self, id: &ObjectId) -> Option<Kind> {
    self.indexed.kind(id)
  }

  fn away(&self) -> Option<u64> {
    let _ = Repository::away(&self.indexed)?;
    self.repository.peek_waiting().map(away_token)
  }
}

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
    clear_waiting(self.quarantined.repository);
    self.indexed.object(id)
  }

  fn holds(&self, id: &ObjectId) -> bool {
    self.indexed.holds(id)
  }

  fn kind(&self, id: &ObjectId) -> Option<Kind> {
    self.indexed.kind(id)
  }

  fn away(&self) -> Option<u64> {
    self.quarantined.repository.peek_waiting().map(away_token)
  }
}

impl Contents for Intake<'_> {
  fn content(&self, at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    clear_waiting(self.quarantined.repository);
    if let Some((object, _)) = self.queue.get(&at) {
      return (object.id == *id).then_some(Cow::Borrowed(object.content.as_slice()));
    }
    self.quarantined.content(at, id)
  }

  fn away(&self) -> Option<u64> {
    self.quarantined.repository.peek_waiting().map(away_token)
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
  pub(crate) repository: &'s GitRepository,
}

impl Contents for Quarantined<'_> {
  fn content(&self, _at: usize, id: &ObjectId) -> Option<Cow<'_, [u8]>> {
    clear_waiting(self.repository);
    // The push's object is read where it is: in the store, lent with no copy where it is one window there (A-727 a1), or
    // where it lies in the push's pack (`Push::pack_bytes`, main's ruling B part 2, covering objects passed over by the
    // quarantine because the repository already indexed them so in-RAM pack/recent reads never hit let-go seed windows),
    // a REF_DELTA's base outside the push read from the index.
    if let Some(push) = self.push {
      let outside = IndexBases {
        repository: self.repository,
        store: self.store,
      };
      match push.pack_bytes(self.store, &outside, id) {
        Some(Ok(bytes)) => return Some(bytes),
        Some(Err(refused)) => {
          if let Some(waited) = refused.waited() {
            self.repository.wait_for(waited);
          }
          return None;
        }
        None => {}
      }
    }
    // An object the repository indexed already and not carried in this push's pack is read back from the index (A-753).
    let (_, stored, len) = self.repository.stored(self.store, id)?;
    self.repository.read_stored(self.store, &stored, len)
  }

  fn away(&self) -> Option<u64> {
    self.repository.peek_waiting().map(away_token)
  }
}

/// Admits `settled`, pre-verified objects settled from a multi-shard compute batch, to
/// `quarantine` without re-hashing on the owner shard, then releases every hold back to `admission`.
pub(super) fn admit_settled(
  state: &mut ShardState,
  quarantine: &mut GitPush,
  settled: Vec<slates_git::receive::SettledObject>,
  admission: &mut Admission,
) -> Result<(), GitRefused> {
  let mut objects = Vec::with_capacity(settled.len());
  let mut holds = Vec::with_capacity(settled.len());
  for item in settled {
    objects.push((item.placed, item.content));
    holds.push(item.hold);
  }
  let admitted = quarantine.admit_precomputed_all(state, objects);
  for hold in holds {
    admission.release(hold);
  }
  admitted
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
  admitted
}
