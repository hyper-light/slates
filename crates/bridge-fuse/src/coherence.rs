//! Kernel-coherence delivery (§4.6 "Cache posture"; AUD-02): the discipline by which a transport
//! whose kernel caches names and attributes **forever** keeps that cache true. The seam reports every
//! invalidation owed since a cursor ([`Bridge::invalidations`]: a change through another attachment
//! or the SDK, an outsider's change beneath a base directory); this module turns that into **rounds**
//! the transport runs at every wake — a kernel request *or* a change signalled by another mutation
//! source — and advances the cursor only past what the kernel was actually told. The sink is a
//! closure, so the rules are pure, cfg-free and unit-tested on every host; the Linux transport
//! ([`crate::channel`]) runs them against a real mount.
//!
//! The two losses this closes (AUD-02, `docs/bugs/2026-09-14_AUDIT.md`): a seam refusal to gather
//! left the cursor where it was, but the advance after the request then skipped the ungathered
//! changes for good; and delivery ran only before a kernel request, so a change while the kernel
//! answered from its cache was never told — the "infinite cache lifetimes require proven invalidation
//! delivery" rule of §4.6 was advertised, not held.

use slates_bridge_core::{Bridge, Invalidation, InvalidationCursor, OpContext};

/// What one delivery round did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Delivered {
  /// The invalidations written to the kernel this round.
  pub written: usize,
  /// The seam refused to gather: nothing was written, the cursor stayed, and the round is owed
  /// again at the next wake.
  pub gather_refused: bool,
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
}

impl Coherence {
  /// A transport's coherence state at mount time.
  pub fn new() -> Coherence {
    Coherence::default()
  }

  /// One round: gathers every invalidation owed since the cursor and hands each to `sink`, then
  /// advances the cursor — only past what was gathered **and** written. A seam refusal to gather
  /// leaves the cursor where it was (the round is owed again at the next wake, counted); a sink
  /// refusal is the transport's own and is returned, with the cursor unmoved, so a re-established
  /// transport gathers the same round again (an invalidation delivered twice is harmless — the
  /// kernel drops nothing it does not hold).
  pub fn deliver<E>(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    sink: &mut dyn FnMut(&Invalidation) -> Result<(), E>,
  ) -> Result<Delivered, E> {
    let since = self.cursor.unwrap_or_else(|| bridge.seen(cx));
    self.owed.clear();
    let next = match bridge.invalidations(cx, since, &mut self.owed) {
      Ok(next) => next,
      Err(_) => {
        self.owed.clear();
        self.gather_refusals = self.gather_refusals.saturating_add(1);
        self.cursor = Some(since);
        return Ok(Delivered {
          written: 0,
          gather_refused: true,
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
    Ok(Delivered {
      written,
      gather_refused: false,
    })
  }

  /// The transport served a kernel request of its own after the round `delivered`: the request's
  /// records are this kernel's own doing, so the cursor moves past them — but only when that round
  /// was delivered whole. After a refused gather the changes the round owed lie before the
  /// request's records and are still owed, so the cursor stays; the next round delivers them, and
  /// the kernel's own change with them (a redundant invalidation of what it did itself, harmless).
  pub fn served_own_request(
    &mut self,
    bridge: &mut dyn Bridge,
    cx: &OpContext,
    delivered: Delivered,
  ) {
    if !delivered.gather_refused {
      self.cursor = Some(bridge.seen(cx));
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
