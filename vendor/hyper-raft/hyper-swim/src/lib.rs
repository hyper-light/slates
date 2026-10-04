//! SWIM membership and failure detection, sans-io: the detector slates built (slates
//! `crates/cluster/src/{detector,membership,gossip,coordinates,fixed}.rs` and the codec half of
//! `swim.rs`, at slates `5cce86a`), shared by mantle, focal and slates (mantle note 32 §3.6).
//!
//! The detector decides nothing durable. Its verdicts (alive, suspect, dead) are hints to each
//! owner's committed membership.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::cognitive_complexity
    )
)]

pub mod codec;
pub mod coordinates;
pub mod detector;
pub mod extension;
pub mod fixed;
pub mod gossip;
pub mod membership;

/// A member's identity, as the owner names it: one unsigned 64-bit word.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostId(pub u64);
