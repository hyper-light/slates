//! Each repository's usage, counted on its owner shard (A-724 piece 1; §4.17.5 "What a repository costs, counted").
//!
//! **What it is.** One server serves many repositories, from toys to monorepos the size of Google's or Meta's, and
//! divides its memory, CPU and admission among them by what their agents and clients actually do, never by fixed shares
//! (§4.17.5). That division needs inputs. This module is where they are counted: for each repository, for each of its
//! two kinds of work (a push's or commit's intake, a fetch's or clone's answer), how many began, how many were
//! answered, how many shard steps they took, how long those steps ran on the shard's clock and how many bytes the
//! client sent; when the repository was first and last active; and which principals (agents or clients) brought its
//! work, in a heavy-hitters summary of bounded size.
//!
//! **Where it lives.** A [`Ledger`] is kept by one shard for the repositories whose owner it is, beside their stores, and
//! is touched only by that shard's own steps: no lock, no atomic, no message on the path (CLAUDE.md §2 item 1; D-7). A
//! row is made at a repository's first event and dropped with the repository ([`Ledger::forget`]), so the ledger holds
//! at most a row a repository of the shard's partition.
//!
//! **The principals, bounded.** A repository may be touched by any number of principals over its life, so the ledger
//! cannot keep a count for each (CLAUDE.md §2 item 8). It keeps the Space-Saving summary instead [A: Metwally, Agrawal
//! and El Abbadi, "Efficient Computation of Frequent and Top-k Elements in Data Streams", ICDT 2005]: `k` slots, each a
//! principal, its count and the most its count may overstate. A principal seen again adds to its slot; a new one takes
//! a free slot, or else the slot with the smallest count, inheriting that count as its overstatement. Every principal
//! whose true count exceeds `total / k` holds a slot, and every slot's count bounds its principal's true count from
//! above, and its count less its overstatement from below. `k` is the caller's to derive: the owner passes the shard's
//! client seats (`clients_per_shard`), the most principals one shard can serve at once, so any principal with more than
//! one seat's share of a repository's work is always named. Its cost is `k` small entries a repository.
//!
//! **What it is not.** The ledger counts; it decides nothing. The learned demand and the shares derived from it are
//! A-724's later pieces, which read these rows. Every count saturates, so a long-lived repository's counts never wrap to
//! a small number.

use std::collections::BTreeMap;

/// The two kinds of work a repository's clients bring (module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
  /// A push's or commit's intake: the client's bytes read, checked, admitted and landed.
  Intake,
  /// A fetch's or clone's answer: refs listed and a pack or its like written.
  Fetch,
}

/// The counts of one kind of work of one repository (module docs). Every field saturates.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorkUsage {
  /// Works begun.
  pub begun: u64,
  /// Works answered: the exchange ended with the tool's answer, accepted or refused. `begun - answered` is the works
  /// under way and those abandoned (a client gone, an exchange refused).
  pub answered: u64,
  /// Shard steps the works took.
  pub steps: u64,
  /// The shard clock's nanoseconds those steps ran.
  pub step_ns: u64,
  /// Bytes the clients sent for the works.
  pub bytes_in: u64,
}

/// One slot of a repository's principal summary (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrincipalCount<P> {
  /// The principal.
  pub principal: P,
  /// Works begun by the principal, overstated by at most `overstated`.
  pub count: u64,
  /// The most `count` may exceed the principal's true count by: the count of the slot it took over.
  pub overstated: u64,
}

/// One repository's row (module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Usage<P> {
  /// Its intakes.
  pub intake: WorkUsage,
  /// Its fetches.
  pub fetch: WorkUsage,
  /// The shard clock's nanoseconds at its first event.
  pub first_ns: u64,
  /// The shard clock's nanoseconds at its latest event.
  pub last_ns: u64,
  /// The principal summary: at most the ledger's `principal_slots` slots, in no order.
  principals: Vec<PrincipalCount<P>>,
}

impl<P> Usage<P> {
  /// A row whose first event is at `now_ns`.
  fn new(now_ns: u64) -> Self {
    Usage {
      intake: WorkUsage::default(),
      fetch: WorkUsage::default(),
      first_ns: now_ns,
      last_ns: now_ns,
      principals: Vec::new(),
    }
  }

  /// The counts of `work`.
  pub fn work(&self, work: Work) -> &WorkUsage {
    match work {
      Work::Intake => &self.intake,
      Work::Fetch => &self.fetch,
    }
  }

  /// The principal summary's slots (module docs), in no order.
  pub fn principals(&self) -> &[PrincipalCount<P>] {
    &self.principals
  }

  fn work_mut(&mut self, work: Work) -> &mut WorkUsage {
    match work {
      Work::Intake => &mut self.intake,
      Work::Fetch => &mut self.fetch,
    }
  }

  fn touched(&mut self, now_ns: u64) {
    self.last_ns = self.last_ns.max(now_ns);
  }
}

impl<P: Clone + Eq> Usage<P> {
  /// Counts one work of `principal`'s in the summary of at most `slots` slots (module docs, Space-Saving).
  fn count_principal(&mut self, principal: &P, slots: usize) {
    if let Some(slot) = self
      .principals
      .iter_mut()
      .find(|slot| slot.principal == *principal)
    {
      slot.count = slot.count.saturating_add(1);
      return;
    }
    if self.principals.len() < slots {
      self.principals.push(PrincipalCount {
        principal: principal.clone(),
        count: 1,
        overstated: 0,
      });
      return;
    }
    if let Some(least) = self.principals.iter_mut().min_by_key(|slot| slot.count) {
      let inherited = least.count;
      *least = PrincipalCount {
        principal: principal.clone(),
        count: inherited.saturating_add(1),
        overstated: inherited,
      };
    }
  }
}

/// Every repository's row on one shard (module docs), keyed by the repository's id `R`, its principals of type `P`.
#[derive(Clone, Debug)]
pub struct Ledger<R, P> {
  /// One row a repository with an event since it was made here.
  rows: BTreeMap<R, Usage<P>>,
  /// The principal summary's slots a repository: the caller's derived `k` (module docs), at least one.
  principal_slots: usize,
}

impl<R: Ord + Clone, P: Clone + Eq> Ledger<R, P> {
  /// An empty ledger keeping `principal_slots` principals a repository (module docs: the owner passes its shard's client
  /// seats). A summary needs a slot to name anyone, so zero keeps one.
  pub fn new(principal_slots: usize) -> Self {
    Ledger {
      rows: BTreeMap::new(),
      principal_slots: principal_slots.max(1),
    }
  }

  /// The principal summary's slots a repository.
  pub fn principal_slots(&self) -> usize {
    self.principal_slots
  }

  /// `principal` began a work of kind `work` on `repository` at `now_ns`.
  pub fn begun(&mut self, repository: &R, principal: &P, work: Work, now_ns: u64) {
    let slots = self.principal_slots;
    let row = self.row(repository, now_ns);
    let counts = row.work_mut(work);
    counts.begun = counts.begun.saturating_add(1);
    row.count_principal(principal, slots);
  }

  /// A step of a work of kind `work` on `repository` ended at `now_ns`, having run `step_ns` on the shard's clock and read
  /// `bytes_in` of the client's bytes.
  pub fn stepped(
    &mut self,
    repository: &R,
    work: Work,
    (step_ns, bytes_in): (u64, u64),
    now_ns: u64,
  ) {
    let counts = self.row(repository, now_ns).work_mut(work);
    counts.steps = counts.steps.saturating_add(1);
    counts.step_ns = counts.step_ns.saturating_add(step_ns);
    counts.bytes_in = counts.bytes_in.saturating_add(bytes_in);
  }

  /// A work of kind `work` on `repository` was answered at `now_ns`.
  pub fn answered(&mut self, repository: &R, work: Work, now_ns: u64) {
    let counts = self.row(repository, now_ns).work_mut(work);
    counts.answered = counts.answered.saturating_add(1);
  }

  /// Drops `repository`'s row: it is gone from this shard.
  pub fn forget(&mut self, repository: &R) {
    self.rows.remove(repository);
  }

  /// `repository`'s row, when it has had an event here.
  pub fn usage(&self, repository: &R) -> Option<&Usage<P>> {
    self.rows.get(repository)
  }

  /// Every row, in repository order.
  pub fn rows(&self) -> impl Iterator<Item = (&R, &Usage<P>)> {
    self.rows.iter()
  }

  /// `repository`'s row, made at `now_ns` when it has none, and marked active at `now_ns`.
  fn row(&mut self, repository: &R, now_ns: u64) -> &mut Usage<P> {
    let row = self
      .rows
      .entry(repository.clone())
      .or_insert_with(|| Usage::new(now_ns));
    row.touched(now_ns);
    row
  }
}

#[cfg(test)]
mod tests {
  use super::{Ledger, Work, WorkUsage};
  use proptest::prelude::*;
  use std::collections::BTreeMap;

  /// One event of a generated history: a repository, a principal, what happened, and the step's figures.
  #[derive(Clone, Debug)]
  enum Event {
    Begun(u8, u8, bool),
    Stepped(u8, bool, u32, u32),
    Answered(u8, bool),
    Forget(u8),
  }

  fn work_of(intake: bool) -> Work {
    if intake { Work::Intake } else { Work::Fetch }
  }

  /// Any event, each kind alike. The union is built as `prop_oneof!` builds one of more than ten arms, from boxed
  /// strategies: its shorter forms hold their arms in `Arc`s (R2).
  fn event() -> impl Strategy<Value = Event> {
    proptest::strategy::Union::new([
      (0u8..6, 0u8..24, any::<bool>())
        .prop_map(|(r, p, w)| Event::Begun(r, p, w))
        .boxed(),
      (0u8..6, any::<bool>(), any::<u32>(), any::<u32>())
        .prop_map(|(r, w, n, b)| Event::Stepped(r, w, n, b))
        .boxed(),
      (0u8..6, any::<bool>())
        .prop_map(|(r, w)| Event::Answered(r, w))
        .boxed(),
      (0u8..6).prop_map(Event::Forget).boxed(),
    ])
  }

  /// The serial model: exact counts of everything, every principal counted.
  #[derive(Default)]
  struct Model {
    intake: WorkUsage,
    fetch: WorkUsage,
    first_ns: u64,
    last_ns: u64,
    principals: BTreeMap<u8, u64>,
  }

  proptest! {
    #![proptest_config(slates_test_seeds::unseeded(ProptestConfig::with_cases(300)))]

    /// A-724 piece 1: do any history of works begun, stepped, answered and repositories forgotten on one ledger, expect
    /// every row's counts to equal an exact model's, every row to be dropped with its repository, and its principal
    /// summary to hold Space-Saving's guarantees: never more than `k` slots, the slots' counts summing to the works
    /// begun, each slot's count bounding its principal's true count from above and its count less its overstatement from
    /// below, and every principal with more than `begun / k` works named.
    #[test]
    fn a_ledger_equals_an_exact_model_and_its_principal_summary_keeps_space_savings_bounds(
      slots in 1usize..8,
      events in proptest::collection::vec(event(), 0..400),
    ) {
      let mut ledger: Ledger<u8, u8> = Ledger::new(slots);
      let mut model: BTreeMap<u8, Model> = BTreeMap::new();
      for (at, event) in (1u64..).zip(events) {
        let now = at;
        let row = |model: &mut BTreeMap<u8, Model>, repository: u8| {
          let row = model.entry(repository).or_insert_with(|| Model { first_ns: now, ..Model::default() });
          row.last_ns = now;
        };
        match event {
          Event::Begun(repository, principal, intake) => {
            ledger.begun(&repository, &principal, work_of(intake), now);
            row(&mut model, repository);
            let row = model.get_mut(&repository).unwrap();
            let counts = if intake { &mut row.intake } else { &mut row.fetch };
            counts.begun += 1;
            *row.principals.entry(principal).or_insert(0) += 1;
          }
          Event::Stepped(repository, intake, ns, bytes) => {
            ledger.stepped(&repository, work_of(intake), (u64::from(ns), u64::from(bytes)), now);
            row(&mut model, repository);
            let row = model.get_mut(&repository).unwrap();
            let counts = if intake { &mut row.intake } else { &mut row.fetch };
            counts.steps += 1;
            counts.step_ns += u64::from(ns);
            counts.bytes_in += u64::from(bytes);
          }
          Event::Answered(repository, intake) => {
            ledger.answered(&repository, work_of(intake), now);
            row(&mut model, repository);
            let row = model.get_mut(&repository).unwrap();
            let counts = if intake { &mut row.intake } else { &mut row.fetch };
            counts.answered += 1;
          }
          Event::Forget(repository) => {
            ledger.forget(&repository);
            model.remove(&repository);
          }
        }
      }
      prop_assert_eq!(ledger.rows().map(|(r, _)| *r).collect::<Vec<_>>(), model.keys().copied().collect::<Vec<_>>());
      for (repository, expected) in &model {
        let usage = ledger.usage(repository).unwrap();
        prop_assert_eq!(usage.intake, expected.intake);
        prop_assert_eq!(usage.fetch, expected.fetch);
        prop_assert_eq!((usage.first_ns, usage.last_ns), (expected.first_ns, expected.last_ns));
        let begun = expected.intake.begun + expected.fetch.begun;
        let summary = usage.principals();
        prop_assert!(summary.len() <= slots);
        prop_assert_eq!(summary.iter().map(|slot| slot.count).sum::<u64>(), begun);
        for slot in summary {
          let truth = expected.principals.get(&slot.principal).copied().unwrap_or(0);
          prop_assert!(slot.count >= truth, "a slot's count understates its principal");
          prop_assert!(slot.count - slot.overstated <= truth, "a slot's floor overstates its principal");
        }
        let floor = begun / u64::try_from(slots).unwrap();
        for (principal, count) in &expected.principals {
          if *count > floor {
            prop_assert!(summary.iter().any(|slot| slot.principal == *principal), "a heavy principal went unnamed");
          }
        }
      }
    }
  }

  /// A-724 piece 1: do one principal's works on a repository past every other's, with more principals than slots, expect
  /// the heavy principal named with its exact count, since it never lost its slot.
  #[test]
  fn a_principal_heavier_than_a_slots_share_is_named_with_its_count() {
    let mut ledger: Ledger<&str, u32> = Ledger::new(2);
    for round in 0..50u32 {
      ledger.begun(&"monorepo", &7, Work::Intake, u64::from(round));
      ledger.begun(&"monorepo", &(100 + round), Work::Fetch, u64::from(round));
    }
    let usage = ledger.usage(&"monorepo").unwrap();
    let heavy = usage
      .principals()
      .iter()
      .find(|slot| slot.principal == 7)
      .unwrap();
    assert_eq!((heavy.count, heavy.overstated), (50, 0));
    assert_eq!((usage.intake.begun, usage.fetch.begun), (50, 50));
    assert_eq!(usage.principals().len(), 2);
  }

  /// A-724 piece 1: do events on two repositories, expect each counted on its own row and none on the other's.
  #[test]
  fn each_repository_is_counted_on_its_own_row() {
    let mut ledger: Ledger<u32, u32> = Ledger::new(4);
    ledger.begun(&1, &10, Work::Intake, 5);
    ledger.stepped(&1, Work::Intake, (900, 4096), 6);
    ledger.answered(&1, Work::Intake, 7);
    ledger.begun(&2, &11, Work::Fetch, 8);
    let toy = ledger.usage(&2).unwrap();
    assert_eq!(toy.intake, WorkUsage::default());
    assert_eq!((toy.fetch.begun, toy.first_ns), (1, 8));
    let pushed = ledger.usage(&1).unwrap();
    assert_eq!(
      pushed.intake,
      WorkUsage {
        begun: 1,
        answered: 1,
        steps: 1,
        step_ns: 900,
        bytes_in: 4096
      }
    );
    assert_eq!((pushed.first_ns, pushed.last_ns), (5, 7));
  }
}
