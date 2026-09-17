//! Bounded SWIM dissemination (§4.8 Membership). A changed member replaces its previous
//! pending report; the least-transmitted report goes first, for the caller's derived
//! λ·ln(n+1) budget. Two ordered indexes avoid sorting the membership on every packet.
//! Evidence: SWIM (DSN 2002), and the sparse-mesh regression in the dated bug report.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::register::HostId;

use crate::membership::MemberState;

/// One pending report per admitted member, with O(log N) replacement and O(batch·log N)
/// draining. Callers enqueue only changes their membership has actually adopted.
#[derive(Default)]
pub(crate) struct Gossip {
  reports: BTreeMap<HostId, (MemberState, u32)>,
  order: BTreeSet<(u32, HostId)>,
}

impl Gossip {
  pub(crate) fn record(&mut self, member: HostId, state: MemberState) {
    if let Some((_, sent)) = self.reports.insert(member, (state, 0)) {
      self.order.remove(&(sent, member));
    }
    self.order.insert((0, member));
  }

  pub(crate) fn drain(&mut self, max: usize, transmits: u32) -> Vec<(HostId, MemberState)> {
    let mut batch = Vec::new();
    // Select before re-inserting: a packet carries a member at most once.
    let selected: Vec<_> = self.order.iter().copied().take(max).collect();
    for (sent, member) in selected {
      self.order.remove(&(sent, member));
      if let Some((state, _)) = self.reports.remove(&member) {
        batch.push((member, state));
        let sent = sent.saturating_add(1);
        if sent < transmits {
          self.reports.insert(member, (state, sent));
          self.order.insert((sent, member));
        }
      }
    }
    batch
  }
}
