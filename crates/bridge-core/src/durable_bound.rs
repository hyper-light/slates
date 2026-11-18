//! When a reply held for a live-shipped volume's durability gives up (§4.10 status "FUSE `fsync` at durability" and
//! "The wait is judged by what it hears"; A-200, A-292, A-301): the one rule every transport that holds such a reply
//! judges by — the FUSE `fsync`, an NFS stable write or `COMMIT`, a Shards `FSYNC` and a WinFsp flush in the daemon
//! (`slates-server`'s `live_ship`), and the virtio-fs device's `FSYNC` (`slates-bridge-virtiofs`). Pure: the caller
//! hands it what the volume has heard ([`Heard`]) and the clock, so its judgment is tested on every host and no
//! transport keeps a second mechanism.
//!
//! **The rule.** A wait runs in windows of a fixed length (the daemon's liveness budget). A window that ends with
//! something heard (a holder's acknowledged position advancing, the volume's head or catalog placement gaining an
//! acknowledgement) starts another. A window that ends with nothing heard gives up, but only once the owner has asked
//! something a whole window ago that is still unanswered, and only once it has read what its candidates sent: the shard
//! that receives their acknowledgements (the daemon's control shard, where the sessions live) must have idled twice
//! since the silence was first seen, and the verdict is given at the look after that. Before 2026-11-09 the wait was one
//! wall-clock deadline of a single window from the hold, so load that slowed the candidates or the owner spent it: a
//! healthy FUSE `fsync` was answered `EIO` under load with its bytes already held by a candidate (1 in 6 runs on main,
//! 2 in 9 on raft-7).
//!
//! **Why the owner must have asked (A-301).** Silence is the holders' only when they were asked. Some of what a
//! durable reply waits for is asked only at the owner's own pace: a fresh volume's head and catalog are shipped by its
//! coordinator's period, after that period's other rounds, each awaited in turn (`slates-server`'s
//! `fleet::ship_shard_heads`), and a period a loaded machine stretches past a window leaves a window in which nothing
//! was asked, so nothing heard, through no holder's fault. The receiver's idles prove the owner has read what came, not
//! that it asked. So the caller also hands over when the owner first asked a holder something on the volume's durability
//! path since it last heard anything there ([`Heard::unanswered_since_ns`]: a log entry or install sent, a head or
//! catalog placement asked), and a silent window is confirmed only once that ask is a window old: a holder has had a
//! whole liveness budget to answer it. An owner that never asks holds the verdict only to the total bound.
//!
//! **Why the receiver must idle first.** A window is judged at the first look after its end, on the shard that holds
//! the reply. An owner kept off its CPU across that end (a loaded machine, a frozen or stopped process) runs that look
//! before its control shard has read the acknowledgements waiting in its sockets and handed them over, so a window it
//! could not observe read as silent (raft's failing run showed the owner itself 600 ms late). A shard waits in its driver
//! only when nothing is ready to run, and each wait collects everything readable: so once the receiver has waited twice
//! after a moment, everything that reached it before that moment has been read, and what it handed to the reply's shard
//! was queued there before that shard's next step. The verdict at a later look than the one that saw the second wait is
//! therefore made after those acknowledgements were counted. A stopped candidate sends nothing, so its silence is still
//! judged about a window after the hold, two looks later.
//!
//! **The total bound.** Progress that trickles in, other writes' acknowledgements moving the count while this reply's
//! own position is never acknowledged, would start window after window without end, and a receiver that never idles
//! would hold the verdict for ever (banned item 8; the supervisor's landing condition, 2026-11-11). So a wait ends at its
//! total bound whatever it hears: the failover SLO from the hold, the operator's bound on how long an object may go
//! unserved before a takeover serves it (§4.8). A write the fleet has not made durable within it is past what the fleet
//! promises to keep. The bound is kept on the clock rather than as a count of windows, since a window whose silence
//! awaited its receiver lasts longer than a window.

/// What a held reply's wait is judged against at a look: two counts the caller reads, each of which only grows, and the
/// owner's oldest unanswered ask.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Heard {
  /// The volume's acknowledgements heard on its durability path: a holder's acknowledged position advancing, or its
  /// head's or catalog's placement gaining an acknowledgement.
  pub acknowledged: u64,
  /// The waits the shard that receives those acknowledgements has entered in its driver, each with nothing ready to run
  /// (`slates_rt::registry::Pulse::waits`).
  pub receiver_idles: u64,
  /// When, on the clock the wait is judged by, the owner first asked a holder something on the volume's durability path
  /// (a log entry or install sent, its head's or catalog's placement asked) since it last heard anything there; `None`
  /// while it has asked nothing that is still unanswered.
  pub unanswered_since_ns: Option<u64>,
}

/// Derived: the receiver's waits after a moment that prove it has read and handed on everything that reached it before
/// that moment: the first collects all that was readable, and the second begins only once all of it has run.
const DRAINED_AFTER_IDLES: u64 = 2;

/// Why a [`DurableBound`] gave up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GaveUp {
  /// A whole window passed with nothing heard on the volume's durability path, an ask of the owner's went a window
  /// unanswered, and the receiver had read what came.
  Silent,
  /// The total bound was reached, whatever was heard.
  AtTheBound,
}

impl GaveUp {
  /// How a refusal's record names it.
  #[must_use]
  pub const fn named(self) -> &'static str {
    match self {
      GaveUp::Silent => {
        "nothing heard in its last window, an ask unanswered for a window, its receiver drained"
      }
      GaveUp::AtTheBound => "its total bound reached, the failover SLO from its hold",
    }
  }
}

/// Where a window that ended with nothing heard stands ([`DurableBound::judge`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confirming {
  /// The receiver's idle count when the silence was first seen: it must idle twice more.
  Since(u64),
  /// The receiver has idled twice since: the next look gives the verdict.
  Drained,
}

/// The lengths a [`DurableBound`] is judged in, each from the caller's derivation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spans {
  /// One window: the daemon's liveness budget, past which a holder that has not answered is not a live one to wait for.
  pub window_ns: u64,
  /// The whole wait at most, from the hold: the failover SLO. At least one window ([`DurableBound::begin`]).
  pub total_ns: u64,
  /// How far apart the looks are that confirm a silence: a span within which a receiver that runs idles at least once.
  pub look_ns: u64,
}

/// One held reply's wait for durability, in windows ([`DurableBound::judge`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableBound {
  /// [`Heard::acknowledged`] as this window began.
  heard: u64,
  /// When the window now open ends, or, while a silence is confirmed, when the next look is due.
  ends_ns: u64,
  /// When the wait ends whatever it hears: its total bound, from the hold.
  gives_up_by_ns: u64,
  /// The windows begun, this one included.
  windows: u32,
  /// A silent window's confirmation, while one runs.
  confirming: Option<Confirming>,
  /// The lengths it is judged in.
  spans: Spans,
}

impl DurableBound {
  /// The first window of a wait that has heard `heard` by `now_ns`, the hold, judged in `spans`. A total bound shorter
  /// than a window is taken as one window.
  #[must_use]
  pub fn begin(heard: Heard, now_ns: u64, spans: Spans) -> DurableBound {
    let gives_up_by_ns = now_ns.saturating_add(spans.total_ns.max(spans.window_ns));
    DurableBound {
      heard: heard.acknowledged,
      ends_ns: now_ns.saturating_add(spans.window_ns).min(gives_up_by_ns),
      gives_up_by_ns,
      windows: 1,
      confirming: None,
      spans,
    }
  }

  /// When the wait is next judged: the open window's end, a confirming look, or the total bound, whichever is first. A
  /// waiter asks [`DurableBound::judge`] then.
  #[must_use]
  pub const fn next_look_ns(&self) -> u64 {
    if self.ends_ns < self.gives_up_by_ns {
      self.ends_ns
    } else {
      self.gives_up_by_ns
    }
  }

  /// The windows begun so far.
  #[must_use]
  pub const fn windows(&self) -> u32 {
    self.windows
  }

  /// Whether, and why, the wait has given up at `now_ns`, the volume having heard `heard` by then: past the total bound
  /// it has; within the open window it goes on; a window that ended with something heard starts the next from `now_ns`;
  /// one that ended with nothing heard is looked at again every [`Spans::look_ns`], first until the owner's oldest
  /// unanswered ask is a window old (`DurableBound::asked_a_window_before`), then until the receiver has idled twice
  /// since that was seen, and given up silent at the look after that, unless something was heard meanwhile, which
  /// starts the next window.
  pub fn judge(&mut self, heard: Heard, now_ns: u64) -> Option<GaveUp> {
    if now_ns >= self.gives_up_by_ns {
      return Some(GaveUp::AtTheBound);
    }
    if now_ns < self.ends_ns {
      return None;
    }
    if heard.acknowledged != self.heard {
      self.heard = heard.acknowledged;
      self.ends_ns = now_ns
        .saturating_add(self.spans.window_ns)
        .min(self.gives_up_by_ns);
      self.windows = self.windows.saturating_add(1);
      self.confirming = None;
      return None;
    }
    // Nothing heard since the window began. It is the holders' silence only if they were asked: until an ask of the
    // owner's has gone a window unanswered, the owner's own delay in asking is not theirs, and the confirmation waits.
    self.confirming = if self.asked_a_window_before(heard, now_ns) {
      Some(match self.confirming {
        None => Confirming::Since(heard.receiver_idles),
        Some(Confirming::Since(seen))
          if heard.receiver_idles >= seen.saturating_add(DRAINED_AFTER_IDLES) =>
        {
          Confirming::Drained
        }
        Some(Confirming::Since(seen)) => Confirming::Since(seen),
        Some(Confirming::Drained) => return Some(GaveUp::Silent),
      })
    } else {
      None
    };
    self.ends_ns = now_ns
      .saturating_add(self.spans.look_ns.max(1))
      .min(self.gives_up_by_ns);
    None
  }

  /// Whether the owner's oldest unanswered ask ([`Heard::unanswered_since_ns`]) was made at least a window before
  /// `now_ns`: a holder has had the whole liveness budget to answer it.
  fn asked_a_window_before(&self, heard: Heard, now_ns: u64) -> bool {
    heard
      .unanswered_since_ns
      .is_some_and(|asked| now_ns.saturating_sub(asked) >= self.spans.window_ns)
  }
}

#[cfg(test)]
mod tests {
  use super::{DurableBound, GaveUp, Heard, Spans};

  /// Shape: a window's length in these tests: the daemon's liveness budget.
  const WINDOW_NS: u64 = 1_000_000_000;
  /// Shape: the default failover SLO, ten windows.
  const SLO_NS: u64 = 10_000_000_000;
  /// Shape: the looks that confirm a silence: the daemon's heartbeat, a tenth of a window.
  const LOOK_NS: u64 = 100_000_000;
  /// Shape: the hold's instant.
  const HELD_AT_NS: u64 = 1_000;

  /// The spans the daemon judges in at its defaults.
  fn spans() -> Spans {
    Spans {
      window_ns: WINDOW_NS,
      total_ns: SLO_NS,
      look_ns: LOOK_NS,
    }
  }

  /// What the volume has heard: `acknowledged` acknowledgements, its receiver idle `idles` times, and the owner's ask at
  /// the hold (its barrier ships the write) still unanswered.
  fn heard(acknowledged: u64, idles: u64) -> Heard {
    Heard {
      acknowledged,
      receiver_idles: idles,
      unanswered_since_ns: Some(HELD_AT_NS),
    }
  }

  /// [`heard`] with the owner's oldest unanswered ask made at `asked`, or none.
  fn heard_asked(acknowledged: u64, idles: u64, asked: Option<u64>) -> Heard {
    Heard {
      unanswered_since_ns: asked,
      ..heard(acknowledged, idles)
    }
  }

  /// Judges `bound` at each of its looks, the volume hearing `acknowledged` and its receiver idling once a look, until
  /// it gives up or `looks` looks pass: why it ended, and the instant it did.
  fn judged_until_it_ends(
    bound: &mut DurableBound,
    acknowledged: u64,
    looks: u32,
  ) -> Option<(GaveUp, u64)> {
    let mut idles = 0;
    for _ in 0..looks {
      let at = bound.next_look_ns();
      idles += 1;
      if let Some(why) = bound.judge(heard(acknowledged, idles), at) {
        return Some((why, at));
      }
    }
    None
  }

  /// The total bound is the failover SLO from the hold, and never shorter than a window. Do: begin a wait at the default
  /// spans and at an SLO shorter than a window, and judge each at the instant before its bound and at it with
  /// acknowledgements still arriving. Expect it to go on before and end `AtTheBound` at the SLO, or at one window.
  #[test]
  fn a_held_replys_wait_ends_at_the_failover_slo_from_its_hold() {
    let mut bound = DurableBound::begin(heard(0, 0), HELD_AT_NS, spans());
    assert_eq!(bound.judge(heard(1, 0), HELD_AT_NS + SLO_NS - 1), None);
    assert_eq!(
      bound.judge(heard(2, 0), HELD_AT_NS + SLO_NS),
      Some(GaveUp::AtTheBound)
    );
    let short = Spans {
      total_ns: WINDOW_NS / 2,
      ..spans()
    };
    let mut bound = DurableBound::begin(heard(0, 0), HELD_AT_NS, short);
    assert_eq!(bound.next_look_ns(), HELD_AT_NS + WINDOW_NS);
    assert_eq!(
      bound.judge(heard(1, 0), HELD_AT_NS + WINDOW_NS),
      Some(GaveUp::AtTheBound)
    );
  }

  /// A window with nothing heard ends the wait once its receiver has drained, and one not yet ended goes on. Do: begin a
  /// wait, judge it within its first window, then at each look after its end with nothing heard and the receiver idling
  /// once a look. Expect nothing within; at the window's end, a confirmation, not a verdict; `Silent` at the look after the
  /// one that saw the receiver's second idle since the silence (with one idle a look, three looks after the window's
  /// end; a running receiver idles many times a look, so two); one window begun.
  #[test]
  fn a_window_with_nothing_heard_gives_up_once_its_receiver_has_drained() {
    let mut bound = DurableBound::begin(heard(7, 0), HELD_AT_NS, spans());
    assert_eq!(bound.judge(heard(7, 0), HELD_AT_NS + WINDOW_NS - 1), None);
    let ended = judged_until_it_ends(&mut bound, 7, 10);
    assert_eq!(
      ended,
      Some((GaveUp::Silent, HELD_AT_NS + WINDOW_NS + 3 * LOOK_NS)),
      "silent at the look after the receiver's second idle"
    );
    assert_eq!(bound.windows(), 1);
  }

  /// An owner kept off its CPU past its window's end (2026-11-17; the open sibling of the healthy `fsync` refused `EIO`):
  /// its first look after the end runs before its receiver has read the acknowledgements waiting for it. Do: begin a
  /// wait; look at its end with nothing counted yet and the receiver not idle since; then let the receiver idle twice,
  /// counting the acknowledgement it read, and look again. Expect no verdict at the first look, and the acknowledgement to
  /// start the next window. Red under the old rule, which gave up `Silent` at the first look.
  #[test]
  fn acknowledgements_read_after_a_late_window_end_start_the_next_window() {
    let mut bound = DurableBound::begin(heard(3, 40), HELD_AT_NS, spans());
    let late = HELD_AT_NS + WINDOW_NS + 600_000_000;
    assert_eq!(
      bound.judge(heard(3, 40), late),
      None,
      "no verdict before the receiver has read what came"
    );
    assert_eq!(
      bound.judge(heard(4, 42), bound.next_look_ns()),
      None,
      "what the receiver read starts the next window"
    );
    assert_eq!(bound.windows(), 2);
    assert_eq!(bound.next_look_ns(), late + LOOK_NS + WINDOW_NS);
  }

  /// A receiver that has not idled twice holds the verdict. Do: end a window with nothing heard, then look again and
  /// again with the receiver idling once in all. Expect no verdict at any of those looks before the total bound, and
  /// `AtTheBound` at it: a receiver that never drains cannot hold a wait past the failover SLO (banned item 8).
  #[test]
  fn a_receiver_that_never_drains_holds_the_verdict_only_to_the_total_bound() {
    let mut bound = DurableBound::begin(heard(0, 5), HELD_AT_NS, spans());
    let mut ended = None;
    for look in 0..1_000u64 {
      let at = bound.next_look_ns();
      let idles = if look == 0 { 5 } else { 6 };
      if let Some(why) = bound.judge(heard(0, idles), at) {
        ended = Some((why, at));
        break;
      }
    }
    assert_eq!(ended, Some((GaveUp::AtTheBound, HELD_AT_NS + SLO_NS)));
  }

  /// The total bound with trickling acknowledgements (banned item 8). Do: begin a wait and, at every window's end, hear
  /// one more acknowledgement (other writes' placements moving while this reply's own position is never acknowledged),
  /// until the wait ends or a thousand looks pass. Expect it to end `AtTheBound` at the failover SLO from the hold, after
  /// the SLO's worth of windows. Red with the bound removed: a thousand windows and no end.
  #[test]
  fn acknowledgements_that_trickle_without_the_target_end_the_wait_at_its_bound() {
    let mut bound = DurableBound::begin(heard(0, 0), HELD_AT_NS, spans());
    let mut acknowledged = 0;
    let mut ended = None;
    for _ in 0..1_000 {
      acknowledged += 1;
      let at = bound.next_look_ns();
      if let Some(why) = bound.judge(heard(acknowledged, 0), at) {
        ended = Some((why, at));
        break;
      }
    }
    assert_eq!(
      ended,
      Some((GaveUp::AtTheBound, HELD_AT_NS + SLO_NS)),
      "the wait ends at its bound"
    );
    assert_eq!(bound.windows(), 10, "after the SLO's worth of windows");
  }

  /// A window that heard something starts the next from the moment it was judged. Do: hear once at the first window's
  /// end, judged a little late. Expect the wait to go on, its next window ending a whole window after the judgment.
  #[test]
  fn a_window_that_heard_something_starts_the_next() {
    let mut bound = DurableBound::begin(heard(0, 0), HELD_AT_NS, spans());
    let judged_at = bound.next_look_ns() + 5;
    assert_eq!(bound.judge(heard(1, 0), judged_at), None);
    assert_eq!(bound.next_look_ns(), judged_at + WINDOW_NS);
    assert_eq!(bound.windows(), 2);
  }

  /// The owner's own delay in asking is not its holders' silence (A-301; GAPS 2026-11-17: a fresh volume's head is asked
  /// only at its coordinator's period, which load stretches). Do: begin a wait with nothing of the owner's unanswered (the
  /// holders acknowledged the write's entry), and look at every look past the window's end with nothing heard and the
  /// receiver idling once a look; half a window past that end the owner asks (its head's placement) and nothing answers.
  /// Expect no verdict before that ask is a window old, however drained the receiver, and `Silent` at the look after the
  /// receiver's second idle from then. Red under A-292's rule alone, which gave up `Silent` three looks after the window's
  /// end, before the owner had asked anything.
  #[test]
  fn a_window_in_which_the_owner_asked_nothing_is_given_up_only_once_its_ask_is_a_window_old() {
    let mut bound = DurableBound::begin(heard_asked(4, 0, None), HELD_AT_NS, spans());
    let asked_at = HELD_AT_NS + WINDOW_NS + WINDOW_NS / 2;
    let mut idles = 0;
    let mut ended = None;
    for _ in 0..1_000 {
      let at = bound.next_look_ns();
      idles += 1;
      let asked = (at >= asked_at).then_some(asked_at);
      if let Some(why) = bound.judge(heard_asked(4, idles, asked), at) {
        ended = Some((why, at));
        break;
      }
    }
    assert_eq!(
      ended,
      Some((GaveUp::Silent, asked_at + WINDOW_NS + 3 * LOOK_NS)),
      "silent only once its ask went a window unanswered and the receiver drained after"
    );
    assert_eq!(bound.windows(), 1);
  }

  /// An owner that never asks holds the verdict only to the total bound (banned item 8). Do: begin a wait with nothing of
  /// the owner's unanswered, and look at every look with nothing heard, the receiver idling once a look and the owner
  /// asking nothing, until it ends or a thousand looks pass. Expect `AtTheBound` at the failover SLO from the hold. Red
  /// under A-292's rule alone, which gave up `Silent` three looks after the first window's end.
  #[test]
  fn an_owner_that_never_asks_holds_the_verdict_only_to_the_total_bound() {
    let mut bound = DurableBound::begin(heard_asked(0, 0, None), HELD_AT_NS, spans());
    let mut ended = None;
    for look in 1..=1_000u64 {
      let at = bound.next_look_ns();
      if let Some(why) = bound.judge(heard_asked(0, look, None), at) {
        ended = Some((why, at));
        break;
      }
    }
    assert_eq!(ended, Some((GaveUp::AtTheBound, HELD_AT_NS + SLO_NS)));
  }
}
