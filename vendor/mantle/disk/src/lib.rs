//! Local storage devices: what the device under a path is and what it measures as. How bytes
//! move to a device, with the alignment and durability the device and OS guarantee, is
//! hyper-block's (vendor/hyper-block), which this crate measures through.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

pub mod calibrate;
pub mod histogram;
pub mod identity;
pub mod measure;
pub mod probe;
pub mod profile;
pub mod rounds;
