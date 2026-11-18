//! A replica of a green on a shard that holds its works (A-273 steps 2 and 3; §4.16 "Rebase, the only corrective path"):
//! the green's engine made from its owner's at the owner's head, then kept equal to it, version by version, by the
//! owner's feed. The feed carries each accepted version's increment with what its verdict resolved it to, replayed
//! through the engine's own replay (`Green::apply_recorded`, the property recovery already holds), and each fold of the
//! owner's history, in the order the owner sent them. So a replica at version `v` is the owner's green at `v` by
//! construction, and a rebase or a read at a version it holds can be answered there instead of on the owner's one
//! thread.
//!
//! **Why (A-273, measured).** W1 (A-247) moved a work's edits and composition to the work's shard; the verdict and the
//! rebase still ran on the green's owner. At the 3,000-client rung over `kernel/` of linux 7.4-rc1 the owner spent 51 to
//! 53% of its wall on them, a verdict 21.4 to 21.7 µs of its CPU and a rebase 12.3 to 12.4 µs, and rebases were the
//! larger share: 71,000 to 72,000 a run against 49,000 verdicts. A rebase changes nothing on the green, so nothing but
//! the engine's state ties it to the owner.
//!
//! **Content once, in place (step 3).** A replica holds no byte of content of its own: the owner's content is in sealed
//! blocks (`crate::store`, `slates_mem::sealed`), and the replica names them as the owner does and reads them where
//! they are. It is made by [`Green::replica`]: every index, history and delta cloned, the content named by the same
//! blocks, and the replica registered with the owner's blocks, so the owner keeps every block the replica may read
//! until the replica has passed the message that released it ([`Fed::Applied`]'s `Passed`, which its holder sends back
//! to the owner's [`Green::acknowledge_replica`]). A replay's own seals adopt the blocks the owner sealed in the same
//! message, in the owner's order, each checked to hold exactly the bytes the replay made; at the replica's end its
//! registration is given back ([`Replica::leave`], for the owner's [`Green::leave_replica`]). Measured by A-273 at the
//! 3,000-client rung: about 14.2 MB of metadata a replica, where a copy of the content as well would have been about
//! 49.5 MB, and about 1.3 GB for a whole kernel tree.
//!
//! **The feed.** The owner stamps each message with its feed position and moves past it ([`Green::feed_position`]), so
//! each message is one past the one before; it carries the blocks the owner sealed and released in it. A replica
//! applies the message at its next position. One below it is already in what the replica was made from: a message in
//! flight while the replica was rebuilt, passed over. One above it means a message never came: the replica changes
//! nothing and refuses it ([`FeedRefusal::Gap`]), and its holder rebuilds it from the owner, counted. A version that
//! would replay as another is refused the same way before anything changes ([`FeedRefusal::Diverged`]), and a replay
//! that seals other bytes than the owner did after it ([`FeedRefusal::NotTheOwners`]): a replica that is not the owner's
//! green must not answer for it.
//!
//! **Its history is the owner's.** A replica made from the owner holds every version the owner holds, so it answers a
//! rebase over any base the owner can (`Outcome::BaseUnavailable` below the owner's own floor, as the owner does), and
//! folds where the owner folds. Laptop degenerate (R8): with one shard there is no other shard and no replica.
//!
//! **Bounds (step 4).** Everything a replica adds has a bound its holder derives from the machine and a typed refusal
//! at it, and every refusal ends with the replica given back ([`Replica::leave`]) and its work going to the owner:
//! - **Its own memory.** A replica's metadata ([`Green::metadata_bytes`]: its piece lists, change index, IPC history,
//!   deltas and accepted record, which grow with the versions it holds) is held to the budget its holder gives it, a
//!   share of its shard's metadata class: refused at its making ([`Unfit::OverBudget`]) and after any message that
//!   takes it past ([`FeedRefusal::OverBudget`]). The key indexes, one entry a path, are the owner's own bound.
//! - **The owner's keeping for it.** The owner keeps a released block for its slowest replica; past the bytes its
//!   holder lets it keep ([`Green::set_replica_held_limit`], from its memory class's headroom), the slowest replica is
//!   evicted and refuses its next message ([`FeedRefusal::Evicted`]), keeping its frees until it leaves.
//! - **How many.** The owner takes as many replicas as its holder allows (one a shard that may hold its works):
//!   past that, [`Green::replica`] is refused ([`ReplicaRefusal::Full`]).
//! - **One message at a time.** The owner records a message's blocks until it sends it; a change made before the last
//!   was sent evicts every replica, which could not be fed in order (`slates_mem::sealed`, "Bounds").
//! - **Its replay.** The blocks a message sealed, queued for adoption, are that message's alone, and its reader's live
//!   table is the owner's live blocks.
//!
//! An idle replica, one no work on its shard has used for its derived idle period, is its holder's to give back, on
//! the shards (step 5).

use crate::chain_entry::{Applied, Signature};
use crate::engine::{Green, Increment};
use slates_mem::sealed::{Changes, Left, Passed, ReaderRefusal};

/// Why an owner made no replica (A-273 step 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplicaRefusal {
  /// The green is itself a replica: it has no blocks of its own to give.
  NotAnOwner,
  /// The owner has `limit` replicas already, evicted ones that have not left among them.
  Full {
    /// The replicas it has.
    readers: usize,
    /// The most it takes.
    limit: usize,
  },
}

/// Why a green cannot be a replica within its budget (A-273 step 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unfit {
  /// It is an owner's own green, no replica.
  NotAReplica,
  /// Its metadata, `bytes`, is past the `budget` its holder gives it.
  OverBudget {
    /// Its metadata's bytes.
    bytes: usize,
    /// The budget.
    budget: usize,
  },
}

/// A green that did not become a replica, and its registration for the owner's [`Green::leave_replica`].
#[derive(Debug)]
pub struct NotMade {
  /// Why.
  pub unfit: Unfit,
  /// What the owner takes to keep nothing for it.
  pub left: Option<Left>,
}

/// One message of a green's feed to its replicas (A-273).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedMessage {
  /// Its place in the feed: one past the message before it.
  pub position: u64,
  /// What it carries.
  pub body: FeedBody,
  /// The blocks the owner sealed and released in it, which a replica adopts and lets go of (step 3).
  pub changes: Changes,
}

/// What a feed message carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedBody {
  /// A version the owner committed: its increment, what its verdict resolved it to (as its chain entry records it), and
  /// how its submit's signature was judged where it carried one (A-321).
  Version {
    /// The version it committed as.
    version: u64,
    /// The increment.
    increment: Increment,
    /// What the verdict resolved it to.
    applied: Vec<Applied>,
    /// How its submit's signature was judged, where it carried one.
    signature: Option<Signature>,
  },
  /// The owner folded its history below `floor` ([`Green::fold_history_before`]): the replica folds there too, so it
  /// keeps no value the owner has dropped.
  Fold {
    /// The owner's fold floor after the fold.
    floor: u64,
  },
}

impl Green {
  /// The message that carries `version`, just committed from `increment` as `applied` resolved it, to the green's
  /// replicas, at the feed's position, which it then moves past (A-273). Made in the same step as the commit, so the
  /// content the commit sealed and released is this message's, and no replica is made between the two. `None` on a
  /// replica, which feeds nothing.
  pub fn feed_version(
    &mut self,
    version: u64,
    increment: Increment,
    applied: Vec<Applied>,
    signature: Option<Signature>,
  ) -> Option<FeedMessage> {
    self.feed(FeedBody::Version {
      version,
      increment,
      applied,
      signature,
    })
  }

  /// Folds the history below `floor` ([`Green::fold_history_before`]) and returns the bytes it released and the message
  /// that carries the fold to the replicas, or no message when the floor did not move, which changes nothing.
  pub fn fold_and_feed(&mut self, floor: u64) -> (usize, Option<FeedMessage>) {
    let before = self.folded_below();
    let released = self.fold_history_before(floor);
    (released, self.feed_fold_since(before))
  }

  /// Folds the history only as far as `budget` needs ([`Green::fold_history_to_budget`]) and returns the bytes it
  /// released and the message that carries the floor it reached, or no message when the floor did not move.
  pub fn fold_to_budget_and_feed(
    &mut self,
    reachable_floor: u64,
    budget: usize,
  ) -> (usize, Option<FeedMessage>) {
    let before = self.folded_below();
    let released = self.fold_history_to_budget(reachable_floor, budget);
    (released, self.feed_fold_since(before))
  }

  /// The message of a fold whose floor moved past `before`, or none.
  fn feed_fold_since(&mut self, before: u64) -> Option<FeedMessage> {
    let floor = self.folded_below();
    if floor > before {
      self.feed(FeedBody::Fold { floor })
    } else {
      None
    }
  }

  /// `body` at the feed's position, with the blocks sealed and released since the last message; the position then
  /// moves past it.
  fn feed(&mut self, body: FeedBody) -> Option<FeedMessage> {
    let changes = self.take_changes()?;
    Some(FeedMessage {
      position: changes.position(),
      body,
      changes,
    })
  }
}

/// Why a replica refused a feed message (A-273): its holder rebuilds it from the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedRefusal {
  /// A message before this one never came: the replica's next position was `expected`, and it was sent `sent`. Nothing
  /// changed.
  Gap {
    /// The position the replica applies next.
    expected: u64,
    /// The position it was sent.
    sent: u64,
  },
  /// A version the owner committed as `version` would replay here as `replayed`: the replica is not the owner's green.
  /// Nothing changed.
  Diverged {
    /// The version the owner committed.
    version: u64,
    /// The version it would replay as here.
    replayed: u64,
  },
  /// The message at `position` replayed here to other content than the owner sealed (step 3): the replica is not the
  /// owner's green, and its state is no longer to be answered from.
  NotTheOwners {
    /// The message's position.
    position: u64,
  },
  /// The owner evicted this replica (step 4): it kept too much for it, or a change went unfed. Nothing changed; the
  /// holder gives it back, and its work goes to the owner.
  Evicted,
  /// The replica's metadata grew past its budget with this message (step 4): it is the owner's green, but too large to
  /// keep here; the holder gives it back, and its work goes to the owner.
  OverBudget {
    /// Its metadata's bytes.
    bytes: usize,
    /// The budget.
    budget: usize,
  },
}

/// What a replica did with a feed message it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fed {
  /// Applied: the replica is at the owner's state after the message, and has passed it; its holder sends the owner
  /// `passed` ([`Green::acknowledge_replica`]), so the owner may free what the message released.
  Applied {
    /// What the owner frees by.
    passed: Passed,
  },
  /// Passed over: what the replica was made from already holds it.
  PassedOver,
}

/// A replica of a green, kept equal to its owner by the owner's feed (module docs).
#[derive(Debug)]
pub struct Replica {
  /// The green as the owner's feed has made it, its content the owner's, read in place.
  green: Green,
  /// The feed position of the next message it applies.
  next: u64,
  /// Messages applied since it was first made.
  applied: u64,
  /// Messages passed over as already held.
  passed_over: u64,
  /// Messages refused, each followed by a rebuild.
  refused: u64,
  /// Times its holder rebuilt it from the owner after a refusal.
  rebuilds: u64,
  /// The bytes of metadata its holder lets it hold (step 4).
  budget: usize,
}

impl Replica {
  /// The replica `green` is, within `budget` bytes of metadata (step 4): a green its owner made with [`Green::replica`].
  /// Refused, with its registration for the owner, when it is an owner's own green or already past its budget.
  pub fn new(green: Green, budget: usize) -> Result<Replica, NotMade> {
    let green = fit(green, budget)?;
    Ok(Replica {
      next: green.feed_position(),
      green,
      applied: 0,
      passed_over: 0,
      refused: 0,
      rebuilds: 0,
      budget,
    })
  }

  /// Rebuilt after a refusal from `fresh`, a green its owner made now ([`Green::replica`]), within the same budget,
  /// counted; its counters carry on. Returns the old registration for the owner's [`Green::leave_replica`]; a `fresh`
  /// that does not fit is refused with its own, the replica unchanged.
  pub fn rebuild(&mut self, fresh: Green) -> Result<Option<Left>, NotMade> {
    let fresh = fit(fresh, self.budget)?;
    self.next = fresh.feed_position();
    self.rebuilds = self.rebuilds.saturating_add(1);
    Ok(std::mem::replace(&mut self.green, fresh).into_left())
  }

  /// The replica is done: its registration, for the owner's [`Green::leave_replica`].
  pub fn leave(self) -> Option<Left> {
    self.green.into_left()
  }

  /// Takes `message` from the owner's feed (module docs): applies it at the replica's next position, passes over one
  /// below it, and refuses one above it, or a version that would replay as another, changing nothing; a replay that seals
  /// other content than the owner did is refused after it.
  pub fn apply(&mut self, message: &FeedMessage) -> Result<Fed, FeedRefusal> {
    if message.position < self.next {
      self.passed_over = self.passed_over.saturating_add(1);
      return Ok(Fed::PassedOver);
    }
    if message.position > self.next {
      return Err(self.refuse(FeedRefusal::Gap {
        expected: self.next,
        sent: message.position,
      }));
    }
    if let FeedBody::Version {
      version, increment, ..
    } = &message.body
    {
      self.check_version(*version, increment)?;
    }
    let not_the_owners = FeedRefusal::NotTheOwners {
      position: message.position,
    };
    match self.green.begin_replay(&message.changes) {
      Ok(()) => {}
      Err(ReaderRefusal::Evicted) => return Err(self.refuse(FeedRefusal::Evicted)),
      Err(_) => return Err(self.refuse(not_the_owners)),
    }
    match &message.body {
      FeedBody::Version {
        increment,
        applied,
        signature,
        ..
      } => {
        let replayed = self.green.apply_recorded(increment, applied);
        if let Some(signature) = signature {
          self.green.record_signature(replayed, *signature);
        }
      }
      FeedBody::Fold { floor } => {
        self.green.fold_history_before(*floor);
      }
    }
    let Some(passed) = self.green.finish_replay(&message.changes) else {
      return Err(self.refuse(not_the_owners));
    };
    self.next = self.next.saturating_add(1);
    self.applied = self.applied.saturating_add(1);
    let bytes = self.green.metadata_bytes();
    if bytes > self.budget {
      return Err(self.refuse(FeedRefusal::OverBudget {
        bytes,
        budget: self.budget,
      }));
    }
    Ok(Fed::Applied { passed })
  }

  /// Refuses, unchanged, a `version` that is not the one after the replica's head, or whose increment the replica
  /// accepted before.
  fn check_version(&mut self, version: u64, increment: &Increment) -> Result<(), FeedRefusal> {
    let next_version = self.green.head().saturating_add(1);
    let held = self.green.accepted_version(&increment.id);
    if version != next_version || held.is_some() {
      return Err(self.refuse(FeedRefusal::Diverged {
        version,
        replayed: held.unwrap_or(next_version),
      }));
    }
    Ok(())
  }

  /// Counts a refusal and returns it.
  fn refuse(&mut self, refusal: FeedRefusal) -> FeedRefusal {
    self.refused = self.refused.saturating_add(1);
    refusal
  }

  /// The green as the feed has made it.
  pub fn green(&self) -> &Green {
    &self.green
  }

  /// The green, for a rebase, which reads it through `&mut` though it changes nothing ([`Green::rebase`]).
  pub fn green_mut(&mut self) -> &mut Green {
    &mut self.green
  }

  /// The feed position of the next message it applies.
  pub fn position(&self) -> u64 {
    self.next
  }

  /// Messages applied, passed over and refused, and the rebuilds after refusals: the non-vacuity counters of A-273's feed.
  pub fn counts(&self) -> FeedCounts {
    FeedCounts {
      applied: self.applied,
      passed_over: self.passed_over,
      refused: self.refused,
      rebuilds: self.rebuilds,
    }
  }
}

/// `green` if it is a replica within `budget` bytes of metadata, else why not, with its registration for the owner.
fn fit(green: Green, budget: usize) -> Result<Green, NotMade> {
  let unfit = if !green.is_replica() {
    Unfit::NotAReplica
  } else {
    let bytes = green.metadata_bytes();
    if bytes <= budget {
      return Ok(green);
    }
    Unfit::OverBudget { bytes, budget }
  };
  Err(NotMade {
    unfit,
    left: green.into_left(),
  })
}

/// A replica's feed counters ([`Replica::counts`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FeedCounts {
  /// Messages applied.
  pub applied: u64,
  /// Messages passed over as already held.
  pub passed_over: u64,
  /// Messages refused.
  pub refused: u64,
  /// Rebuilds after refusals.
  pub rebuilds: u64,
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::engine::Outcome;
  use crate::ops_doc::{Op, OpKind, OpsDoc};

  /// An increment, `tag`'s, based at `base`: `path` created holding `bytes`, or its first bytes overwritten with them.
  fn increment(tag: u8, base: u64, path: &str, bytes: &[u8], create: bool) -> Increment {
    let mut doc = OpsDoc::new();
    let index = doc.paths.intern(path);
    let len = bytes.len() as u64;
    let op = |kind, len, src| Op {
      kind,
      flags: 0,
      path: index,
      at: 0,
      len,
      src,
    };
    if create {
      doc.ops.push(op(OpKind::Create, 0, u64::MAX));
      doc.ops.push(op(OpKind::Insert, len, 0));
    } else {
      doc.ops.push(op(OpKind::Overwrite, len, 0));
    }
    Increment {
      id: [tag; 32],
      base,
      doc,
      post_state: bytes.to_vec(),
      evidence: Vec::new(),
    }
  }

  /// An increment, `tag`'s, based at `base`, that creates `path` holding `bytes`.
  fn create(tag: u8, base: u64, path: &str, bytes: &[u8]) -> Increment {
    increment(tag, base, path, bytes, true)
  }

  /// Submits `increment` to `owner` and returns the feed message of the version it committed.
  fn commit(owner: &mut Green, increment: Increment) -> FeedMessage {
    let (outcome, applied) = owner.submit_resolved(&increment);
    let Outcome::Accepted { version } = outcome else {
      panic!("{outcome:?}");
    };
    owner
      .feed_version(version, increment, applied.unwrap(), None)
      .unwrap()
  }

  /// Shape: the replicas a test's owner takes, more than any test keeps at once.
  const READERS: usize = 4;
  /// Shape: a replica's budget in the tests that do not drive it: far past any test's metadata.
  const BUDGET: usize = 1 << 20;

  /// A replica of `owner` made now.
  fn replica_of(owner: &mut Green) -> Replica {
    Replica::new(owner.replica(READERS).unwrap(), BUDGET).unwrap()
  }

  /// Applies `message` to `replica` and hands the owner what it passed; returns what the replica did.
  fn feed(
    owner: &mut Green,
    replica: &mut Replica,
    message: &FeedMessage,
  ) -> Result<Fed, FeedRefusal> {
    let fed = replica.apply(message);
    if let Ok(Fed::Applied { passed }) = fed {
      owner.acknowledge_replica(passed);
    }
    fed
  }

  /// A-273: a replica made from its owner and fed the owner's messages in order is the owner's green after each: its
  /// head, its identity, and a file's bytes at every version; and those bytes are the owner's own, read in place, never
  /// a copy (step 3).
  #[test]
  fn a_replica_fed_in_order_is_its_owners_green_and_reads_its_bytes_in_place() {
    let mut owner = Green::new();
    let first = commit(&mut owner, create(1, 0, "a", b"one"));
    let mut replica = replica_of(&mut owner);
    assert_eq!(
      replica.position(),
      first.position + 1,
      "made after the first message"
    );
    assert_eq!(feed(&mut owner, &mut replica, &first), Ok(Fed::PassedOver));
    for (tag, name) in [(2, "b"), (3, "c")] {
      let base = owner.head();
      let message = commit(&mut owner, create(tag, base, name, b"more"));
      assert!(matches!(
        feed(&mut owner, &mut replica, &message),
        Ok(Fed::Applied { .. })
      ));
      assert_eq!(replica.green().head(), owner.head());
      assert_eq!(replica.green_mut().head_identity(), owner.head_identity());
    }
    for version in 1..=owner.head() {
      assert_eq!(
        replica.green().content_at("b", version),
        owner.content_at("b", version)
      );
    }
    for name in ["a", "b", "c"] {
      let (mine, theirs) = (
        replica.green().content(name).unwrap(),
        owner.content(name).unwrap(),
      );
      assert_eq!(mine, theirs);
      assert!(
        std::ptr::eq(mine.as_ptr(), theirs.as_ptr()),
        "{name} is read in place, not copied"
      );
    }
    assert_eq!(replica.green().content_reads_refused(), 0);
    assert_eq!(
      replica.counts(),
      FeedCounts {
        applied: 2,
        passed_over: 1,
        refused: 0,
        rebuilds: 0
      }
    );
    owner.leave_replica(replica.leave().unwrap());
  }

  /// A-273: a message after one that never came is refused with nothing changed, and a rebuild from the owner joins the
  /// feed again; a message the rebuild already holds is passed over, and the old registration is given back.
  #[test]
  fn a_gap_is_refused_unchanged_and_a_rebuild_joins_again() {
    let mut owner = Green::new();
    let mut replica = replica_of(&mut owner);
    let lost = commit(&mut owner, create(1, 0, "a", b"one"));
    let after = commit(&mut owner, create(2, 1, "b", b"two"));
    let head = replica.green().head();
    assert_eq!(
      feed(&mut owner, &mut replica, &after),
      Err(FeedRefusal::Gap {
        expected: lost.position,
        sent: after.position
      })
    );
    assert_eq!(
      replica.green().head(),
      head,
      "a refused message changes nothing"
    );
    let left = replica
      .rebuild(owner.replica(READERS).unwrap())
      .unwrap()
      .unwrap();
    owner.leave_replica(left);
    assert_eq!(feed(&mut owner, &mut replica, &lost), Ok(Fed::PassedOver));
    assert_eq!(feed(&mut owner, &mut replica, &after), Ok(Fed::PassedOver));
    let next = commit(&mut owner, create(3, 2, "c", b"three"));
    assert!(matches!(
      feed(&mut owner, &mut replica, &next),
      Ok(Fed::Applied { .. })
    ));
    assert_eq!(replica.green_mut().head_identity(), owner.head_identity());
    let counts = replica.counts();
    assert_eq!(
      (counts.refused, counts.rebuilds, counts.passed_over),
      (1, 1, 2)
    );
    owner.leave_replica(replica.leave().unwrap());
  }

  /// A-273: a version message that would replay as another version is refused before anything changes: one whose
  /// version is not the replica's head plus one, and one whose increment the replica already holds.
  #[test]
  fn a_version_that_would_replay_as_another_is_refused_unchanged() {
    let mut owner = Green::new();
    let mut replica = replica_of(&mut owner);
    let message = commit(&mut owner, create(1, 0, "a", b"one"));
    let mut skewed = message.clone();
    if let FeedBody::Version { version, .. } = &mut skewed.body {
      *version = 7;
    }
    assert_eq!(
      feed(&mut owner, &mut replica, &skewed),
      Err(FeedRefusal::Diverged {
        version: 7,
        replayed: 1
      })
    );
    assert_eq!(replica.green().head(), 0);
    assert!(matches!(
      feed(&mut owner, &mut replica, &message),
      Ok(Fed::Applied { .. })
    ));
    let mut again = message;
    again.position = replica.position();
    if let FeedBody::Version { version, .. } = &mut again.body {
      *version = 2;
    }
    assert_eq!(
      feed(&mut owner, &mut replica, &again),
      Err(FeedRefusal::Diverged {
        version: 2,
        replayed: 1
      }),
      "an increment already replayed here"
    );
    assert_eq!(replica.green().head(), 1);
    owner.leave_replica(replica.leave().unwrap());
  }

  /// A-273 step 3: a replay whose content is not the owner's is refused: one whose increment makes other bytes than the
  /// owner sealed in the message, after its replay, and one carrying another message's blocks, at their admission.
  #[test]
  fn a_replay_to_other_content_than_the_owners_is_refused() {
    let mut owner = Green::new();
    let mut forged_to = replica_of(&mut owner);
    let mut swapped_to = replica_of(&mut owner);
    let first = commit(&mut owner, create(1, 0, "a", b"one"));
    let second = commit(&mut owner, create(2, 1, "b", b"two"));
    let other = create(9, 0, "a", b"xyz");
    let (_, applied) = Green::new().submit_resolved(&other);
    let mut forged = first.clone();
    forged.body = FeedBody::Version {
      version: 1,
      increment: other,
      applied: applied.unwrap(),
      signature: None,
    };
    assert_eq!(
      feed(&mut owner, &mut forged_to, &forged),
      Err(FeedRefusal::NotTheOwners { position: 0 }),
      "its replay sealed other bytes"
    );
    let mut swapped = first;
    swapped.changes = second.changes.clone();
    assert_eq!(
      feed(&mut owner, &mut swapped_to, &swapped),
      Err(FeedRefusal::NotTheOwners { position: 0 }),
      "another message's blocks"
    );
    assert_eq!(
      swapped_to.green().head(),
      0,
      "refused at admission, unchanged"
    );
    owner.leave_replica(forged_to.leave().unwrap());
    owner.leave_replica(swapped_to.leave().unwrap());
  }

  /// A-273: a fold of the owner's history is a feed message in order with the versions, so the replica keeps no value
  /// the owner dropped, and the owner frees what the fold released once the replica passes it; a fold that moves no
  /// floor sends nothing and moves no position.
  #[test]
  fn a_fold_is_fed_in_order_and_a_fold_that_moves_nothing_sends_nothing() {
    let mut owner = Green::new();
    let mut replica = replica_of(&mut owner);
    for tag in 1..=3u8 {
      let base = u64::from(tag) - 1;
      let message = commit(&mut owner, increment(tag, base, "a", &[tag; 4], tag == 1));
      assert!(matches!(
        feed(&mut owner, &mut replica, &message),
        Ok(Fed::Applied { .. })
      ));
    }
    let position = owner.feed_position();
    let (_, fold) = owner.fold_and_feed(2);
    let fold = fold.unwrap();
    assert_eq!(fold.position, position);
    assert_eq!(fold.body, FeedBody::Fold { floor: 2 });
    let held = owner.content_frees_held();
    assert!(held >= 1, "the fold's release kept for the replica: {held}");
    assert!(matches!(
      feed(&mut owner, &mut replica, &fold),
      Ok(Fed::Applied { .. })
    ));
    assert_eq!(replica.green().folded_below(), 2);
    assert_eq!(replica.green().content_at("a", 1), None, "folded on both");
    assert_eq!(owner.content_at("a", 1), None);
    assert_eq!(replica.green().content_at("a", 2), owner.content_at("a", 2));
    let (released, none) = owner.fold_and_feed(2);
    assert_eq!((released, none), (0, None));
    assert_eq!(owner.feed_position(), position + 1);
    owner.leave_replica(replica.leave().unwrap());
  }

  /// A-273 step 4: a replica past its budget is refused at its making, its registration given back; one made within it
  /// is refused once a message takes it past, though its state is the owner's, and gives itself back; made again within
  /// a larger budget, it applies the owner's next message, and the owner keeps nothing for the one that left.
  #[test]
  fn a_replica_past_its_budget_is_refused_and_is_made_again_within_a_larger_one() {
    let mut owner = Green::new();
    commit(&mut owner, create(1, 0, "a", b"one"));
    let fresh = owner.replica(READERS).unwrap();
    let bytes = fresh.metadata_bytes();
    let Err(NotMade { unfit, left }) = Replica::new(fresh, bytes - 1) else {
      panic!("made past its budget");
    };
    assert_eq!(
      unfit,
      Unfit::OverBudget {
        bytes,
        budget: bytes - 1
      }
    );
    owner.leave_replica(left.unwrap());
    let mut replica = Replica::new(owner.replica(READERS).unwrap(), bytes).unwrap();
    let message = commit(&mut owner, create(2, 1, "b", b"two"));
    let refusal = feed(&mut owner, &mut replica, &message);
    assert!(
      matches!(refusal, Err(FeedRefusal::OverBudget { budget, .. }) if budget == bytes),
      "{refusal:?}"
    );
    assert_eq!(
      replica.green().head(),
      owner.head(),
      "the owner's state, only too large to keep"
    );
    owner.leave_replica(replica.leave().unwrap());
    let mut larger = replica_of(&mut owner);
    let next = commit(&mut owner, create(3, 2, "c", b"three"));
    assert!(matches!(
      feed(&mut owner, &mut larger, &next),
      Ok(Fed::Applied { .. })
    ));
    owner.leave_replica(larger.leave().unwrap());
    assert_eq!(owner.content_held_bytes(), 0);
  }

  /// A-273 step 4: an owner keeping more than its limit for a lagging replica evicts it: the replica applies the
  /// messages sent before, refuses the next, changing nothing, and gives itself back, after which the owner keeps
  /// nothing for it; a replica made again applies the owner's next message.
  #[test]
  fn a_lagging_replica_is_evicted_past_the_owners_held_limit_and_is_made_again() {
    let mut owner = Green::new();
    commit(&mut owner, increment(1, 0, "a", &[1; 64], true));
    let mut replica = replica_of(&mut owner);
    owner.set_replica_held_limit(64);
    let second = commit(&mut owner, increment(2, 1, "a", &[2; 64], false));
    assert_eq!(
      owner.replicas_evicted(),
      (0, 0),
      "at the limit, not past it"
    );
    let third = commit(&mut owner, increment(3, 2, "a", &[3; 64], false));
    assert_eq!(
      owner.replicas_evicted(),
      (1, 0),
      "past it: the lagging replica evicted"
    );
    assert!(matches!(
      feed(&mut owner, &mut replica, &second),
      Ok(Fed::Applied { .. })
    ));
    assert_eq!(
      feed(&mut owner, &mut replica, &third),
      Err(FeedRefusal::Evicted)
    );
    assert_eq!(
      replica.green().head(),
      2,
      "a refused message changes nothing"
    );
    owner.leave_replica(replica.leave().unwrap());
    assert_eq!(owner.content_held_bytes(), 0, "nothing kept once it left");
    let mut again = replica_of(&mut owner);
    let fourth = commit(&mut owner, increment(4, 3, "a", &[4; 64], false));
    assert!(matches!(
      feed(&mut owner, &mut again, &fourth),
      Ok(Fed::Applied { .. })
    ));
    assert_eq!(again.green_mut().head_identity(), owner.head_identity());
    owner.leave_replica(again.leave().unwrap());
  }

  /// A-273 step 4: an owner takes no more replicas than its holder allows, and a replica makes none; once one leaves,
  /// the owner takes another.
  #[test]
  fn an_owner_refuses_a_replica_past_its_limit() {
    let mut owner = Green::new();
    let mut first = owner.replica(1).unwrap();
    assert_eq!(
      owner.replica(1).err(),
      Some(ReplicaRefusal::Full {
        readers: 1,
        limit: 1
      })
    );
    assert_eq!(first.replica(1).err(), Some(ReplicaRefusal::NotAnOwner));
    owner.leave_replica(first.into_left().unwrap());
    let again = owner.replica(1).unwrap();
    owner.leave_replica(again.into_left().unwrap());
  }

  /// A-273 step 4: a change made before the last was sent evicts every replica, which could not be fed in order: the
  /// replica refuses the next message, and one made again applies the owner's messages.
  #[test]
  fn a_change_left_unfed_evicts_every_replica() {
    let mut owner = Green::new();
    let mut replica = replica_of(&mut owner);
    let (outcome, _) = owner.submit_resolved(&create(1, 0, "a", b"one"));
    assert!(
      matches!(outcome, Outcome::Accepted { .. }),
      "committed, never sent"
    );
    let message = commit(&mut owner, create(2, 1, "b", b"two"));
    assert_eq!(owner.replicas_evicted(), (0, 1));
    assert_eq!(
      feed(&mut owner, &mut replica, &message),
      Err(FeedRefusal::Evicted)
    );
    owner.leave_replica(replica.leave().unwrap());
    let mut again = replica_of(&mut owner);
    let next = commit(&mut owner, create(3, 2, "c", b"three"));
    assert!(matches!(
      feed(&mut owner, &mut again, &next),
      Ok(Fed::Applied { .. })
    ));
    owner.leave_replica(again.leave().unwrap());
  }
}
