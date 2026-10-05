//! POSIX discretionary access control: the rules live beside the shared bridge
//! ([`slates_bridge_core::access`]) so every transport and every OS applies one; the NFS export's paths keep
//! their names through this re-export.

pub use slates_bridge_core::access::*;
