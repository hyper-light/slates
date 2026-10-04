//! Bounded SWIM dissemination (§4.8 Membership). A changed member replaces its previous
//! pending report; the least-transmitted report goes first, for the caller's derived
//! λ·ln(n+1) budget. Evidence: SWIM (DSN 2002), and the sparse-mesh regression in the dated bug
//! report (slates `docs/bugs/2026-09-17-sparse-mesh-drops-membership-gossip.md`: one bounded
//! entry per known member).
//!
//! The reports sit in one queue per transmit count, oldest first, so a drain reads the least
//! transmitted without sorting the membership. Every structure keeps its capacity, so once the
//! membership has been seen a change spreads without allocating (`docs/benchmarks.md`,
//! "hyper-swim"). A replaced report leaves its old queue entry behind; the entry is skipped when
//! reached, and a record that finds the queues holding more than twice the pending reports purges
//! them of such entries. A drain never adds entries, so the queues hold at most twice the most
//! reports ever pending, plus one: twice the membership, which its bound bounds. A member the view
//! forgets takes its report with it.

use std::collections::{HashMap, VecDeque};

use crate::HostId;

use crate::membership::MemberState;

/// One member's pending report: its state, how often it has been sent, and the generation that
/// tells its live queue entry from those of reports it replaced.
#[derive(Clone, Copy)]
struct Report {
    state: MemberState,
    sent: u32,
    generation: u64,
}

/// One pending report per admitted member. Callers enqueue only changes their membership has
/// actually adopted.
#[derive(Default)]
pub(crate) struct Gossip {
    reports: HashMap<HostId, Report>,
    /// Queue `i` holds the members whose report has been sent `i` times, oldest first, each with
    /// the generation of the report it was queued for.
    queues: Vec<VecDeque<(HostId, u64)>>,
    /// The entries across every queue, live and replaced.
    queued: usize,
    /// The generation the next report takes.
    generation: u64,
    /// The reports a drain selects, held across drains so draining allocates nothing once grown.
    selected: Vec<HostId>,
}

impl Gossip {
    pub(crate) fn record(&mut self, member: HostId, state: MemberState) {
        let generation = self.generation;
        self.generation = generation.saturating_add(1);
        self.reports.insert(
            member,
            Report {
                state,
                sent: 0,
                generation,
            },
        );
        if self.queues.is_empty() {
            self.queues.push(VecDeque::new());
        }
        if let Some(fresh) = self.queues.first_mut() {
            fresh.push_back((member, generation));
            self.queued = self.queued.saturating_add(1);
        }
        if self.queued > self.reports.len().saturating_mul(2) {
            self.purge();
        }
    }

    /// Drops `member`'s pending report: the view forgot it. Its queue entry is skipped when
    /// reached, as a replaced report's is.
    pub(crate) fn forget(&mut self, member: HostId) {
        self.reports.remove(&member);
    }

    /// The reports pending.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.reports.len()
    }

    /// Replaces `batch` with up to `max` reports, the least transmitted first, and counts each as
    /// sent once more; a report sent `transmits` times is dropped.
    pub(crate) fn drain(
        &mut self,
        max: usize,
        transmits: u32,
        batch: &mut Vec<(HostId, MemberState)>,
    ) {
        batch.clear();
        let budget = usize::try_from(transmits).unwrap_or(usize::MAX);
        if self.queues.len() < budget {
            self.queues.resize_with(budget, VecDeque::new);
        }
        // Select before re-queuing: a packet carries a member at most once.
        let mut selected = std::mem::take(&mut self.selected);
        selected.clear();
        self.select(max, &mut selected);
        for &member in &selected {
            let Some(report) = self.reports.get_mut(&member) else {
                continue;
            };
            batch.push((member, report.state));
            report.sent = report.sent.saturating_add(1);
            let next = usize::try_from(report.sent)
                .ok()
                .and_then(|sent| self.queues.get_mut(sent));
            match next {
                Some(queue) if report.sent < transmits => {
                    queue.push_back((member, report.generation));
                    self.queued = self.queued.saturating_add(1);
                }
                _ => {
                    self.reports.remove(&member);
                }
            }
        }
        self.selected = selected;
    }

    /// Takes up to `max` live entries off the queues, the least transmitted first, dropping the
    /// replaced entries it passes.
    fn select(&mut self, max: usize, selected: &mut Vec<HostId>) {
        for queue in &mut self.queues {
            while selected.len() < max {
                let Some((member, generation)) = queue.pop_front() else {
                    break;
                };
                self.queued = self.queued.saturating_sub(1);
                if self
                    .reports
                    .get(&member)
                    .is_some_and(|report| report.generation == generation)
                {
                    selected.push(member);
                }
            }
            if selected.len() >= max {
                return;
            }
        }
    }

    /// Drops every queue entry a newer report replaced.
    fn purge(&mut self) {
        let reports = &self.reports;
        let mut queued = 0usize;
        for queue in &mut self.queues {
            queue.retain(|(member, generation)| {
                reports
                    .get(member)
                    .is_some_and(|report| report.generation == *generation)
            });
            queued = queued.saturating_add(queue.len());
        }
        self.queued = queued;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::membership::Liveness;

    fn alive(incarnation: u64) -> MemberState {
        MemberState {
            liveness: Liveness::Alive,
            incarnation,
        }
    }

    fn drained(gossip: &mut Gossip, max: usize, transmits: u32) -> Vec<(HostId, MemberState)> {
        let mut batch = Vec::new();
        gossip.drain(max, transmits, &mut batch);
        batch
    }

    #[test]
    fn the_least_transmitted_go_first_and_each_is_sent_its_budget() {
        let mut gossip = Gossip::default();
        gossip.record(HostId(1), alive(0));
        gossip.record(HostId(2), alive(0));
        assert_eq!(drained(&mut gossip, 1, 2), vec![(HostId(1), alive(0))]);
        // Host 2 has been sent less than host 1.
        assert_eq!(drained(&mut gossip, 1, 2), vec![(HostId(2), alive(0))]);
        assert_eq!(
            drained(&mut gossip, 4, 2),
            vec![(HostId(1), alive(0)), (HostId(2), alive(0))]
        );
        assert_eq!(drained(&mut gossip, 4, 2), vec![], "each sent twice");
    }

    #[test]
    fn a_replaced_report_is_sent_at_its_new_state_with_a_fresh_budget() {
        let mut gossip = Gossip::default();
        gossip.record(HostId(1), alive(0));
        assert_eq!(drained(&mut gossip, 4, 3).len(), 1);
        gossip.record(HostId(1), alive(1));
        for _ in 0..3 {
            assert_eq!(drained(&mut gossip, 4, 3), vec![(HostId(1), alive(1))]);
        }
        assert_eq!(
            drained(&mut gossip, 4, 3),
            vec![],
            "three sends of the new report"
        );
    }

    #[test]
    fn replaced_entries_are_bounded_by_twice_the_reports() {
        let mut gossip = Gossip::default();
        for incarnation in 0..1_000 {
            gossip.record(HostId(1), alive(incarnation));
            gossip.record(HostId(2), alive(incarnation));
            assert!(gossip.queued <= 4, "{}", gossip.queued);
        }
        assert_eq!(
            drained(&mut gossip, 4, 3),
            vec![(HostId(1), alive(999)), (HostId(2), alive(999))]
        );
    }
}
