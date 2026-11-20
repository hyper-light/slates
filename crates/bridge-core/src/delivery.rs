//! Kernel-coherence delivery (§4.6 "Cache posture"; AUD-02): the discipline by which a transport
//! whose kernel caches names, attributes and bytes keeps that cache true. The seam reports every
//! invalidation owed since a cursor ([`Bridge::invalidations`]: a change through another attachment
//! or the SDK, an outsider's change beneath a base directory); this module turns that into **rounds**
//! the transport runs at every wake — a kernel request *or* a change signalled by another mutation
//! source — and advances the cursor only past what the kernel was actually told. The sink is a
//! closure, so the rules are pure, cfg-free and unit-tested on every host; the Linux FUSE channel
//! (`slates-bridge-fuse`) and the Windows WinFsp mount (`slates-server`'s `winfsp`) run them against
//! real mounts. Moved here from the FUSE crate on 2026-10-25 so the WinFsp mount runs the same rounds
//! rather than a second copy of the rules.
//!
//! **Rounds in slices** (A-562). A round is gathered and built on the serving thread, and what it names is as large as
//! the journal's backlog. So a gather takes what its turn allows ([`Bridge::invalidations`]' `more`, the owner's share
//! of the shard's step quantum) and at least one record, and the cursor moves through what it took: the next round
//! resumes there, at once ([`Coherence::owes`]), rather than at the next wake. Nothing is lost, since the cursor never
//! passes a record that was not gathered and written. A round that outgrows its turn is counted
//! ([`Coherence::take_sliced_counts`]), and so is every slice that resumed one, the sliced path's non-vacuity count.
//!
//! The two losses this closes (AUD-02, `docs/bugs/2026-09-14_AUDIT.md`): a seam refusal to gather
//! left the cursor where it was, but the advance after the request then skipped the ungathered
//! changes for good; and delivery ran only before a kernel request, so a change while the kernel
//! answered from its cache was never told — the "infinite cache lifetimes require proven invalidation
//! delivery" rule of §4.6 was advertised, not held.

use crate::{Bridge, Invalidation, InvalidationCursor, OpContext};

/// What one delivery round did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delivered {
  /// The invalidations written to the kernel this round.
  pub written: usize,
  /// The seam refused to gather: nothing was written, the cursor stayed, and the round is owed
  /// again at the next wake.
  pub gather_refused: bool,
  /// The gather stopped where its slice ended, with records still owed (A-562): the cursor stands through the last
  /// record gathered, and the rest is owed now ([`Coherence::owes`]).
  pub cut: bool,
}

/// A transport's coherence state: where its kernel's cache stands, and what it is owed.
#[derive(Debug, Default)]
pub struct Coherence {
  /// Where the kernel's cache stands: nothing before the mount's first round can be in it, so the
  /// first round starts at the seam's cursor of that moment.
  cursor: Option<InvalidationCursor>,
  /// The invalidations gathered for the round in progress — one allocation reused, emptied every
  /// round; bounded by what one gather reports (the journal's retention budget and the entries
  /// loaded under the hinted directories, [`Bridge::invalidations`]).
  owed: Vec<Invalidation>,
  /// Gathers the seam refused so far — the counter a transport reports, and the non-vacuity of the
  /// retry: a missed change is delivered only if a refusal was actually retried.
  gather_refusals: u64,
  /// Whether the last gather stopped where its slice ended, with records still owed (A-562): the next round resumes
  /// from the cursor, and its owner turns at once rather than waiting for a wake ([`Coherence::owes`]).
  cut: bool,
  /// Rounds whose gather outgrew its turn since this was last read: each one's first slice stopped with records still
  /// owed. The signal of a backlog larger than a turn (CLAUDE.md, bounded work). Read and cleared by the owner.
  rounds_outgrown: u64,
  /// Slices that resumed a round where an earlier one stopped, since this was last read: the sliced path's non-vacuity
  /// count. Read and cleared by the owner.
  slices_resumed: u64,
}

impl Coherence {
  /// A transport's coherence state at mount time.
  pub fn new() -> Coherence {
    Coherence::default()
  }

  /// One round: gathers the invalidations owed since the cursor that its slice allows — `more` is asked before each
  /// record after the first ([`Bridge::invalidations`]) — and hands each to `sink`, then advances the cursor, only past
  /// what was gathered **and** written. A gather that `more` stopped leaves the rest owed now ([`Delivered::cut`],
  /// [`Coherence::owes`]). A seam refusal to gather leaves the cursor where it was (the round is owed again at the next
  /// wake, counted); a sink refusal is the transport's own and is returned, with the cursor unmoved, so a re-established
  /// transport gathers the same round again (an invalidation delivered twice is harmless — the kernel drops nothing it
  /// does not hold).
  pub fn deliver<E>(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    more: &mut dyn FnMut() -> std::ops::ControlFlow<()>,
    sink: &mut dyn FnMut(&Invalidation) -> Result<(), E>,
  ) -> Result<Delivered, E> {
    let since = self.cursor.unwrap_or_else(|| bridge.seen(cx));
    self.owed.clear();
    // Whether `more` stopped the gather: it is asked only before a record the gather would take next, so a stop means
    // one was left.
    let mut stopped = false;
    let mut slice = || {
      let go_on = more();
      stopped |= go_on.is_break();
      go_on
    };
    let next = match bridge.invalidations(cx, since, &mut self.owed, &mut slice) {
      Ok(next) => next,
      Err(_) => {
        self.owed.clear();
        self.gather_refusals = self.gather_refusals.saturating_add(1);
        self.cursor = Some(since);
        // A refused gather is owed at the next wake, not at once: a refusal that persists must not spin its owner.
        self.cut = false;
        return Ok(Delivered {
          written: 0,
          gather_refused: true,
          cut: false,
        });
      }
    };
    let mut written: usize = 0;
    for invalidation in &self.owed {
      sink(invalidation)?;
      written = written.saturating_add(1);
    }
    self.owed.clear();
    self.cursor = Some(next);
    if self.cut {
      self.slices_resumed = self.slices_resumed.saturating_add(1);
    } else if stopped {
      self.rounds_outgrown = self.rounds_outgrown.saturating_add(1);
    }
    self.cut = stopped;
    Ok(Delivered {
      written,
      gather_refused: false,
      cut: stopped,
    })
  }

  /// Whether the last round's gather stopped where its slice ended, with records still owed (A-562): its owner turns
  /// again at once, since a kernel serving from its cache sends no request and nothing may change again for a while.
  pub fn owes(&self) -> bool {
    self.cut
  }

  /// The rounds whose gather outgrew its turn and the slices that resumed one since this was last asked, and zero
  /// afterwards, as `(outgrown, resumed)` (A-562).
  pub fn take_sliced_counts(&mut self) -> (u64, u64) {
    (
      std::mem::take(&mut self.rounds_outgrown),
      std::mem::take(&mut self.slices_resumed),
    )
  }

  /// The transport served a kernel request of its own after the round `delivered`: the request's
  /// records are this kernel's own doing, so the cursor moves past them — but only when that round
  /// was delivered whole. After a refused gather, or one its slice cut (A-562), the changes the round owed lie before
  /// the request's records and are still owed, so the cursor stays; the next round delivers them, and
  /// the kernel's own change with them (a redundant invalidation of what it did itself, harmless).
  pub fn served_own_request(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    delivered: Delivered,
  ) {
    if !delivered.gather_refused && !delivered.cut {
      self.cursor = Some(bridge.seen(cx));
    }
  }

  /// Moves the cursor back to `cursor`, so the next round delivers what changed since, the transport's own requests
  /// included. A request whose effect was applied but whose reply was replaced by an error (a refused barrier's
  /// `EIO`) left the kernel believing the change did not happen: a cached negative name, or old attributes. Only a
  /// cursor of an earlier round moves it; `None` leaves it.
  pub fn rewind(&mut self, cursor: Option<InvalidationCursor>) {
    if cursor.is_some() {
      self.cursor = cursor;
    }
  }

  /// Starts the rounds at `cursor` when none has run yet: what a restored mount's adoption read its files from, so the
  /// first round feeds them every change after it ([`Coherence::deliver`]). A cursor already set is kept.
  pub fn begin_at(&mut self, cursor: InvalidationCursor) {
    if self.cursor.is_none() {
      self.cursor = Some(cursor);
    }
  }

  /// Gathers the seam refused so far.
  pub fn gather_refusals(&self) -> u64 {
    self.gather_refusals
  }

  /// Where the kernel's cache stands, once a round has run.
  pub fn cursor(&self) -> Option<InvalidationCursor> {
    self.cursor
  }
}

#[cfg(test)]
mod tests {
  use std::ops::ControlFlow;

  use super::{Coherence, Delivered};
  use crate::{Attachments, Bridge, Invalidation, ObjectId, OpContext, Rights, View, VolumeBridge};

  /// Shape: the store's page in the test.
  const PAGE: usize = 4096;
  /// Shape: the pages the store's region maps: room for the test's few files.
  const REGION_PAGES: usize = 256;
  /// Shape: a journal that holds every record the test makes.
  const JOURNAL: usize = 1 << 16;
  /// Shape: the files another path makes: more names than one slice allows.
  const MADE: usize = 10;
  /// Shape: the records one slice of the test's rounds may take.
  const PER_SLICE: usize = 3;
  /// Shape: the volume the test's bridges serve.
  const VOLUME: slates_db::catalog::VolumeId = slates_db::catalog::VolumeId { bytes: [0; 16] };

  /// A store and a volume in it.
  fn store_and_volume() -> (slates_vfs::volume::Store, slates_vfs::volume::Volume) {
    let mut arena = slates_mem::arena::ChunkArena::new(PAGE);
    arena
      .add_region(slates_mem::region::Region::map(PAGE * REGION_PAGES, PAGE, false).unwrap())
      .unwrap();
    let mut store = slates_vfs::volume::Store::new(
      &slates_vfs::volume::StoreConfig {
        page: PAGE,
        cache_line: 128,
        max_dirs: 16,
        max_inodes: 64,
        max_chunks: REGION_PAGES,
        max_dir_blocks: 16,
        dir_cutover: 16,
      },
      arena,
      0,
    );
    let volume = slates_vfs::volume::Volume::create(
      &mut store,
      slates_vfs::volume::VolumeConfig {
        prefix: 1,
        names: slates_vfs::names::NameEquivalence::Exact,
        quota: slates_vfs::quota::Quota::Bounded { limit: 1 << 30 },
        journal_bytes: JOURNAL,
        clock: Box::new(slates_vfs::clock::HostClock::default()),
      },
    )
    .unwrap();
    (store, volume)
  }

  /// A read-write context of an attachment to the test's volume: the mount whose kernel the rounds tell.
  fn context() -> OpContext {
    let mut attachments = Attachments::new();
    let attachment = attachments
      .attach(
        VOLUME,
        View::Current,
        slates_db::catalog::Principal::Uid {
          host: slates_db::HostId(0),
          uid: 0,
        },
        Rights {
          read: true,
          write: true,
        },
      )
      .unwrap();
    attachments.context(attachment).unwrap()
  }

  /// The names a round's entry invalidations carry, in order.
  fn names(told: &[Invalidation]) -> Vec<String> {
    told
      .iter()
      .filter_map(|invalidation| match invalidation {
        Invalidation::Entry { name, .. } => Some(name.clone()),
        Invalidation::Inode { .. } | Invalidation::Everything => None,
      })
      .collect()
  }

  /// One round whose slice allows [`PER_SLICE`] records, and the invalidations it told; a cut
  /// round owes the rest at once.
  fn one_slice(
    coherence: &mut Coherence,
    bridge: &mut VolumeBridge<'_>,
    cx: &OpContext,
  ) -> (Delivered, Vec<Invalidation>) {
    let mut left = PER_SLICE - 1;
    let mut slice = || {
      if left == 0 {
        ControlFlow::Break(())
      } else {
        left -= 1;
        ControlFlow::Continue(())
      }
    };
    let mut told = Vec::new();
    let delivered = coherence
      .deliver(bridge, cx, &mut slice, &mut |invalidation| {
        told.push(invalidation.clone());
        Ok::<(), ()>(())
      })
      .unwrap();
    assert_eq!(
      delivered.cut,
      coherence.owes(),
      "a cut round owes the rest at once"
    );
    (delivered, told)
  }

  /// The ten took several rounds, the first its slice's three records, each at most a slice's,
  /// every one but the last cut, none refused.
  fn each_round_took_at_most_a_slice(rounds: &[(Delivered, Vec<String>)]) {
    assert!(
      rounds.len() >= MADE.div_ceil(PER_SLICE),
      "the ten took several rounds: {rounds:?}"
    );
    assert_eq!(
      rounds[0].1,
      vec!["f0", "f1", "f2"],
      "the first round took its slice's three records"
    );
    for (at, (delivered, named)) in rounds.iter().enumerate() {
      assert!(!delivered.gather_refused, "round {at}");
      assert_eq!(
        delivered.cut,
        at + 1 < rounds.len(),
        "round {at}: {rounds:?}"
      );
      let records = named.iter().filter(|name| name.starts_with('f')).count();
      assert!(
        records <= PER_SLICE,
        "round {at} took {records} records: {rounds:?}"
      );
    }
  }

  /// Every file made elsewhere told exactly once across the rounds, and in order.
  fn every_file_made_elsewhere_told_once_in_order(rounds: &[(Delivered, Vec<String>)]) {
    let told: Vec<String> = rounds
      .iter()
      .flat_map(|(_, named)| named.iter().filter(|name| name.starts_with('f')).cloned())
      .collect();
    let made: Vec<String> = (0..MADE).map(|made| format!("f{made}")).collect();
    assert_eq!(told, made, "every file made elsewhere told once, in order");
  }

  /// A-562 (B9; GAPS "Two rounds are built whole in one turn"). Do: start a mount's rounds at the head, make ten files
  /// through another path, then run rounds whose slice allows three records each until one is whole; between the first
  /// two, serve a request of the mount's own, which makes a file of its own. Expect at least four rounds, the first to
  /// take the first three files, each to take at most three, every one but the last cut and owing the rest at once;
  /// every file made elsewhere named exactly once and in order, none lost to the own request's cursor move; the counts to
  /// say one round outgrew its turn and every later slice resumed it; and a later round with nothing owed to be whole,
  /// owe nothing and count nothing.
  #[test]
  fn a_round_over_more_names_than_a_slice_allows_takes_several_turns_and_loses_nothing() {
    let (mut store, mut volume) = store_and_volume();
    let cx = context();
    let mut coherence = Coherence::new();
    let mut whole = || ControlFlow::Continue(());
    let first = coherence
      .deliver(
        &mut VolumeBridge::new(VOLUME, &mut volume, &mut store),
        &cx,
        &mut whole,
        &mut |_| Ok::<(), ()>(()),
      )
      .unwrap();
    assert_eq!(
      first,
      Delivered::default(),
      "a mount's first round starts at the head"
    );
    let root = volume.root();
    for made in 0..MADE {
      volume
        .create_file(&mut store, root, &format!("f{made}"), 0o644)
        .unwrap();
    }

    let mut rounds: Vec<(Delivered, Vec<String>)> = Vec::new();
    loop {
      let mut bridge = VolumeBridge::new(VOLUME, &mut volume, &mut store);
      let (delivered, told) = one_slice(&mut coherence, &mut bridge, &cx);
      if rounds.is_empty() {
        let root_ino = bridge.root(&cx).unwrap();
        bridge
          .create(ObjectId::new(root_ino, 0), &cx, "own", 0o644, 0)
          .unwrap();
      }
      coherence.served_own_request(&mut bridge, &cx, delivered);
      rounds.push((delivered, names(&told)));
      if !delivered.cut {
        break;
      }
      assert!(rounds.len() <= MADE, "the rounds end: {rounds:?}");
    }

    // The mount's own request journaled a record or more after the ten, which the later rounds gather too.
    each_round_took_at_most_a_slice(&rounds);
    every_file_made_elsewhere_told_once_in_order(&rounds);
    assert_eq!(
      coherence.take_sliced_counts(),
      (1, u64::try_from(rounds.len() - 1).unwrap()),
      "one round outgrew its turn and its later slices resumed it"
    );
    let mut bridge = VolumeBridge::new(VOLUME, &mut volume, &mut store);
    assert_eq!(
      coherence.cursor(),
      Some(bridge.seen(&cx)),
      "the rounds reached the head"
    );

    let after = coherence
      .deliver(&mut bridge, &cx, &mut whole, &mut |_| Ok::<(), ()>(()))
      .unwrap();
    assert!(!after.cut && !coherence.owes(), "nothing more is owed");
    assert_eq!(coherence.take_sliced_counts(), (0, 0));
  }
}
