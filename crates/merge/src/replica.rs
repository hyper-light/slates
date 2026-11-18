//! A replica of a green on a shard that holds its works (A-273 step 2; §4.16 "Rebase, the only corrective path"): the
//! green's engine built from its owner's at a version, then kept equal to it, version by version, by the owner's feed.
//! The feed carries each accepted version's increment with what its verdict resolved it to, replayed through the engine's
//! own replay (`Green::apply_recorded`, the property recovery already holds), and each fold of the owner's history, in
//! the order the owner sent them. So a replica at version `v` is the owner's green at `v` by construction, and a rebase or
//! a read at a version it holds can be answered there instead of on the owner's one thread.
//!
//! **Why (A-273, measured).** W1 (A-247) moved a work's edits and composition to the work's shard; the verdict and the
//! rebase still ran on the green's owner. At the 3,000-client rung over `kernel/` of linux 7.4-rc1 the owner spent 51 to
//! 53% of its wall on them, a verdict 21.4 to 21.7 µs of its CPU and a rebase 12.3 to 12.4 µs, and rebases were the
//! larger share: 71,000 to 72,000 a run against 49,000 verdicts. A rebase changes nothing on the green, so nothing but
//! the engine's state ties it to the owner.
//!
//! **The feed.** The owner stamps each message with its feed position ([`Green::feed_position`]) and moves past it, so
//! each message is one past the one before; the content a version or a fold released is stamped with the same position
//! (step 1, `crate::history`), which a replica acknowledges once it has applied the message. A replica applies the
//! message at its next position. One below it is already in what the replica was built from: a message in flight while
//! the replica was rebuilt, passed over. One above it means a message never came: the replica changes nothing and
//! refuses it ([`FeedRefusal::Gap`]), and its holder rebuilds it from the owner, counted. A version that would replay as
//! another is refused the same way ([`FeedRefusal::Diverged`]), before anything changes, since a replica that is not the
//! owner's green must not answer for it.
//!
//! **Built from the owner at a version.** A replica starts from the owner's checkpoint at its head, the form recovery
//! restores a green from, over the green's origin, joined at the owner's feed position then
//! ([`Green::replica_seed`]). Its history starts there: a rebase whose base is below the version it was built at, or a
//! read below it, is not its to answer (`Outcome::BaseUnavailable` names its floor), and goes to the owner.
//!
//! **Not yet.** Here the replica's content is its own (the origin's and the checkpoint's bytes, and each version's
//! post-state). Reading the owner's content in place instead (A-64, D-7; content once) is step 3, the replica's bounds
//! are step 4, and serving a work's rebase and its named-version reads from it on the shards is step 5 (A-273's "Owed,
//! in order"). Laptop degenerate (R8): with one shard there is no other shard and no replica.

use crate::chain_entry::{Applied, Signature};
use crate::checkpoint::Checkpoint;
use crate::engine::{Green, Increment};
use crate::origin::Origin;

/// One message of a green's feed to its replicas (A-273).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FeedMessage {
  /// Its place in the feed: one past the message before it.
  pub position: u64,
  /// What it carries.
  pub body: FeedBody,
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

/// What a shard needs to build a replica of a green (A-273): the owner's checkpoint at its head and the feed position
/// of the first message the checkpoint does not hold. The green's origin comes from its base, as recovery's does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaSeed {
  /// The owner's checkpoint at its head.
  pub checkpoint: Checkpoint,
  /// The feed position of the next message the owner sends.
  pub position: u64,
}

impl Green {
  /// The message that carries `version`, just committed from `increment` as `applied` resolved it, to the green's
  /// replicas, at the feed's position, which it then moves past (A-273). Made in the same step as the commit, so the
  /// content the commit released was stamped with this message's position, and no seed is ever taken between the two.
  pub fn feed_version(
    &mut self,
    version: u64,
    increment: Increment,
    applied: Vec<Applied>,
    signature: Option<Signature>,
  ) -> FeedMessage {
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

  /// The seed of a replica of this green now (A-273): its checkpoint at the head, and the feed position of the next
  /// message, so the replica holds every message before it. `None` when the head is below the green's fold floor, where
  /// no checkpoint exists.
  pub fn replica_seed(&self) -> Option<ReplicaSeed> {
    Some(ReplicaSeed {
      checkpoint: self.checkpoint(self.head())?,
      position: self.feed_position(),
    })
  }

  /// The message of a fold whose floor moved past `before`, or none.
  fn feed_fold_since(&mut self, before: u64) -> Option<FeedMessage> {
    let floor = self.folded_below();
    (floor > before).then(|| self.feed(FeedBody::Fold { floor }))
  }

  /// `body` at the feed's position, which then moves past it.
  fn feed(&mut self, body: FeedBody) -> FeedMessage {
    let message = FeedMessage {
      position: self.feed_position(),
      body,
    };
    self.advance_feed();
    message
  }
}

/// Why a replica refused a feed message (A-273): it changed nothing, and its holder rebuilds it from the owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedRefusal {
  /// A message before this one never came: the replica's next position was `expected`, and it was sent `sent`.
  Gap {
    /// The position the replica applies next.
    expected: u64,
    /// The position it was sent.
    sent: u64,
  },
  /// A version the owner committed as `version` would replay here as `replayed`: the replica is not the owner's green.
  Diverged {
    /// The version the owner committed.
    version: u64,
    /// The version it would replay as here.
    replayed: u64,
  },
}

/// What a replica did with a feed message it took.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fed {
  /// Applied: the replica is at the owner's state after the message.
  Applied,
  /// Passed over: what the replica was built from already holds it.
  PassedOver,
}

/// A replica of a green, kept equal to its owner by the owner's feed (module docs).
#[derive(Debug)]
pub struct Replica {
  /// The green as the owner's feed has made it.
  green: Green,
  /// The feed position of the next message it applies.
  next: u64,
  /// Messages applied since it was first built.
  applied: u64,
  /// Messages passed over as already held.
  passed_over: u64,
  /// Messages refused, each followed by a rebuild.
  refused: u64,
  /// Times its holder rebuilt it from the owner after a refusal.
  rebuilds: u64,
}

impl Replica {
  /// A replica built from the owner's `seed` over the green's `origin` ([`Green::replica_seed`]).
  pub fn build(origin: Origin, seed: &ReplicaSeed) -> Replica {
    Replica {
      green: Green::restore(origin, &seed.checkpoint),
      next: seed.position,
      applied: 0,
      passed_over: 0,
      refused: 0,
      rebuilds: 0,
    }
  }

  /// Rebuilds this replica from a fresh `seed` of the owner's after a refusal, counted; its counters carry on.
  pub fn rebuild(&mut self, origin: Origin, seed: &ReplicaSeed) {
    self.green = Green::restore(origin, &seed.checkpoint);
    self.next = seed.position;
    self.rebuilds = self.rebuilds.saturating_add(1);
  }

  /// Takes `message` from the owner's feed (module docs): applies it at the replica's next position, passes over one
  /// below it, and refuses one above it, or a version that would replay as another, changing nothing.
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
    match &message.body {
      FeedBody::Version {
        version,
        increment,
        applied,
        signature,
      } => self.apply_version(*version, increment, applied, *signature)?,
      FeedBody::Fold { floor } => {
        self.green.fold_history_before(*floor);
      }
    }
    self.next = self.next.saturating_add(1);
    self.applied = self.applied.saturating_add(1);
    Ok(Fed::Applied)
  }

  /// Replays `version` from `increment` as `applied` resolved it, or refuses it unchanged when it is not the version
  /// after the replica's head or its increment was accepted here before.
  fn apply_version(
    &mut self,
    version: u64,
    increment: &Increment,
    applied: &[Applied],
    signature: Option<Signature>,
  ) -> Result<(), FeedRefusal> {
    let next_version = self.green.head().saturating_add(1);
    let held = self.green.accepted_version(&increment.id);
    if version != next_version || held.is_some() {
      return Err(self.refuse(FeedRefusal::Diverged {
        version,
        replayed: held.unwrap_or(next_version),
      }));
    }
    let replayed = self.green.apply_recorded(increment, applied);
    if let Some(signature) = signature {
      self.green.record_signature(replayed, signature);
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

  /// The feed position of the next message it applies; every message before it is applied or held, which is what it
  /// acknowledges to the owner (step 1's content holds).
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
    owner.feed_version(version, increment, applied.unwrap(), None)
  }

  /// A-273: a replica built from its owner's seed and fed the owner's messages in order is the owner's green after each:
  /// its head, its identity, and a file's bytes at every version it holds.
  #[test]
  fn a_replica_fed_in_order_is_its_owners_green() {
    let mut owner = Green::new();
    let first = commit(&mut owner, create(1, 0, "a", b"one"));
    let seed = owner.replica_seed().unwrap();
    assert_eq!(
      seed.position,
      first.position + 1,
      "the seed holds the first message"
    );
    let mut replica = Replica::build(Origin::default(), &seed);
    assert_eq!(
      replica.apply(&first),
      Ok(Fed::PassedOver),
      "a message the seed holds"
    );
    for (tag, name) in [(2, "b"), (3, "c")] {
      let base = owner.head();
      let message = commit(&mut owner, create(tag, base, name, b"more"));
      assert_eq!(replica.apply(&message), Ok(Fed::Applied));
      assert_eq!(replica.green().head(), owner.head());
      assert_eq!(replica.green_mut().head_identity(), owner.head_identity());
    }
    for version in 1..=owner.head() {
      assert_eq!(
        replica.green().content_at("b", version),
        owner.content_at("b", version)
      );
    }
    assert_eq!(
      replica.counts(),
      FeedCounts {
        applied: 2,
        passed_over: 1,
        refused: 0,
        rebuilds: 0
      }
    );
  }

  /// A-273: a message after one that never came is refused with nothing changed, and a rebuild from a fresh seed joins
  /// the feed again; a message the rebuild already holds is passed over.
  #[test]
  fn a_gap_is_refused_unchanged_and_a_rebuild_joins_again() {
    let mut owner = Green::new();
    let mut replica = Replica::build(Origin::default(), &owner.replica_seed().unwrap());
    let lost = commit(&mut owner, create(1, 0, "a", b"one"));
    let after = commit(&mut owner, create(2, 1, "b", b"two"));
    let head = replica.green().head();
    assert_eq!(
      replica.apply(&after),
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
    replica.rebuild(Origin::default(), &owner.replica_seed().unwrap());
    assert_eq!(replica.apply(&lost), Ok(Fed::PassedOver));
    assert_eq!(replica.apply(&after), Ok(Fed::PassedOver));
    let next = commit(&mut owner, create(3, 2, "c", b"three"));
    assert_eq!(replica.apply(&next), Ok(Fed::Applied));
    assert_eq!(replica.green_mut().head_identity(), owner.head_identity());
    let counts = replica.counts();
    assert_eq!(
      (counts.refused, counts.rebuilds, counts.passed_over),
      (1, 1, 2)
    );
  }

  /// A-273: a version message that would replay as another version is refused before anything changes: one whose
  /// version is not the replica's head plus one, and one whose increment the replica already holds.
  #[test]
  fn a_version_that_would_replay_as_another_is_refused_unchanged() {
    let mut owner = Green::new();
    let seed = owner.replica_seed().unwrap();
    let message = commit(&mut owner, create(1, 0, "a", b"one"));
    let mut skewed = message.clone();
    if let FeedBody::Version { version, .. } = &mut skewed.body {
      *version = 7;
    }
    let mut replica = Replica::build(Origin::default(), &seed);
    assert_eq!(
      replica.apply(&skewed),
      Err(FeedRefusal::Diverged {
        version: 7,
        replayed: 1
      })
    );
    assert_eq!(replica.green().head(), 0);
    assert_eq!(replica.apply(&message), Ok(Fed::Applied));
    let mut again = message;
    again.position = replica.position();
    if let FeedBody::Version { version, .. } = &mut again.body {
      *version = 2;
    }
    assert_eq!(
      replica.apply(&again),
      Err(FeedRefusal::Diverged {
        version: 2,
        replayed: 1
      }),
      "an increment already replayed here"
    );
    assert_eq!(replica.green().head(), 1);
  }

  /// A-273: a fold of the owner's history is a feed message in order with the versions, so the replica keeps no value
  /// the owner dropped; a fold that moves no floor sends nothing and moves no position.
  #[test]
  fn a_fold_is_fed_in_order_and_a_fold_that_moves_nothing_sends_nothing() {
    let mut owner = Green::new();
    let mut replica = Replica::build(Origin::default(), &owner.replica_seed().unwrap());
    for tag in 1..=3u8 {
      let base = u64::from(tag) - 1;
      let message = commit(&mut owner, increment(tag, base, "a", &[tag; 4], tag == 1));
      assert_eq!(replica.apply(&message), Ok(Fed::Applied));
    }
    let position = owner.feed_position();
    let (_, fold) = owner.fold_and_feed(2);
    let fold = fold.unwrap();
    assert_eq!(fold.position, position);
    assert_eq!(fold.body, FeedBody::Fold { floor: 2 });
    assert_eq!(replica.apply(&fold), Ok(Fed::Applied));
    assert_eq!(replica.green().folded_below(), 2);
    assert_eq!(replica.green().content_at("a", 1), None, "folded on both");
    assert_eq!(owner.content_at("a", 1), None);
    assert_eq!(replica.green().content_at("a", 2), owner.content_at("a", 2));
    let (released, none) = owner.fold_and_feed(2);
    assert_eq!((released, none), (0, None));
    assert_eq!(owner.feed_position(), position + 1);
  }
}
