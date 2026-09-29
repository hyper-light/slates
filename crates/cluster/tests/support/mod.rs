//! Shared test support for the cluster crate's consensus experiments (`docs/wip/research/consensus-enhancements.md`
//! §5). Each test binary compiles it on its own and uses only part of it.
#![allow(dead_code)]

pub(crate) mod azure;
pub(crate) mod exhaustive;
pub(crate) mod timed;
