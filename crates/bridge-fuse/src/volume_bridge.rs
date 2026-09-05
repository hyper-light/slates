//! The `Bridge` implementation over the volume core now lives in `slates-bridge-core` as the one
//! transport-independent operation layer (§4.6 "one implementation in the core"), shared by every
//! OS bridge. It is re-exported here so the FUSE crate and its tests keep one import path.

pub use slates_bridge_core::VolumeBridge;
