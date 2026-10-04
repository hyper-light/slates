//! Witnessed extensions for an accused host under load (from focal
//! `crates/focal-node/src/liveness/suspicion.rs`, `ExtensionTracker`; mantle note 32 S13).
//!
//! A host that is suspected because it is slow, not dead, asks its accuser for more time. It
//! carries a progress witness it cannot fake while stuck, and says whether its admission is
//! refusing capacity. The rules:
//! - an overloaded host is never extended: shedding load heals it, and extending it would hide a
//!   host that cannot keep up;
//! - a grant needs the witness to have risen since the last grant;
//! - there is at most one grant per protocol period.
//!
//! focal measured grants in milliseconds and capped them at five. Here they are counted in the
//! base window's unit and every bound is derived:
//! - grants halve, from half the base window (Lifeguard's logarithmically decaying extensions,
//!   Dadgar et al., DSN 2018), never below one;
//! - all grants together never exceed one base window, so a stuck host can at most double the time
//!   before it is declared dead. That replaces the literal count.
//!
//! The detector's base window is the one probe that tells a suspect it is suspected
//! (`docs/timing.md` §2.7), so a grant is one more such probe, and one is all a suspect gets.

/// Why an extension was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionDenial {
    /// The host is not suspected: there is nothing to extend.
    NotSuspected,
    /// The host reports its admission is refusing capacity: heal, not extend.
    Overloaded,
    /// No progress since the previous grant.
    NoProgress,
    /// A grant was already made this period.
    RateLimited,
    /// Another grant would take the total past one base suspicion window.
    Exhausted,
}

/// What a request for more time decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtensionDecision {
    /// The suspicion window moved out by this many of its units.
    Granted {
        /// The units granted: probes, for the detector.
        periods: u32,
    },
    /// Refused, and why.
    Denied(ExtensionDenial),
}

/// One suspected member's extensions.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExtensionTracker {
    count: u32,
    last_witness: Option<u64>,
    last_grant_period: Option<u64>,
    total: u32,
}

impl ExtensionTracker {
    /// The units granted so far.
    pub fn total(&self) -> u32 {
        self.total
    }

    /// Decides one request at protocol period `period`, under a base suspicion window of `base`
    /// units. A grant is committed here.
    pub fn request(
        &mut self,
        period: u64,
        base: u32,
        witness: u64,
        overloaded: bool,
    ) -> ExtensionDecision {
        if overloaded {
            return ExtensionDecision::Denied(ExtensionDenial::Overloaded);
        }
        if self.last_grant_period == Some(period) {
            return ExtensionDecision::Denied(ExtensionDenial::RateLimited);
        }
        if self.last_witness.is_some_and(|last| witness <= last) {
            return ExtensionDecision::Denied(ExtensionDenial::NoProgress);
        }
        let grant = base
            .checked_shr(self.count.saturating_add(1))
            .unwrap_or(0)
            .max(1);
        let Some(total) = self.total.checked_add(grant).filter(|total| *total <= base) else {
            return ExtensionDecision::Denied(ExtensionDenial::Exhausted);
        };
        self.count = self.count.saturating_add(1);
        self.last_witness = Some(witness);
        self.last_grant_period = Some(period);
        self.total = total;
        ExtensionDecision::Granted { periods: grant }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grants(base: u32) -> Vec<u32> {
        let mut tracker = ExtensionTracker::default();
        let mut grants = Vec::new();
        for period in 0..64 {
            match tracker.request(period, base, period, false) {
                ExtensionDecision::Granted { periods } => grants.push(periods),
                ExtensionDecision::Denied(ExtensionDenial::Exhausted) => break,
                other => panic!("{other:?}"),
            }
        }
        grants
    }

    #[test]
    fn grants_halve_from_half_the_window_and_total_exactly_one_window() {
        assert_eq!(grants(16), vec![8, 4, 2, 1, 1]);
        assert_eq!(grants(10), vec![5, 2, 1, 1, 1]);
        assert_eq!(grants(1), vec![1]);
        assert!(grants(0).is_empty(), "a window of nothing grants nothing");
        for base in 0..200 {
            assert!(grants(base).iter().sum::<u32>() <= base, "base {base}");
        }
    }

    #[test]
    fn overload_progress_and_rate_are_checked() {
        let mut tracker = ExtensionTracker::default();
        assert_eq!(
            tracker.request(0, 16, 1, true),
            ExtensionDecision::Denied(ExtensionDenial::Overloaded)
        );
        assert_eq!(
            tracker.request(0, 16, 1, false),
            ExtensionDecision::Granted { periods: 8 }
        );
        assert_eq!(
            tracker.request(0, 16, 2, false),
            ExtensionDecision::Denied(ExtensionDenial::RateLimited)
        );
        assert_eq!(
            tracker.request(1, 16, 1, false),
            ExtensionDecision::Denied(ExtensionDenial::NoProgress)
        );
        assert_eq!(
            tracker.request(1, 16, 2, false),
            ExtensionDecision::Granted { periods: 4 }
        );
        assert_eq!(tracker.total(), 12);
    }
}
