//! The guest side of the mount-route tests (`docs/wip/virtiofs.md` §8 item 7; §4.6).
//!
//! A slates volume reaches a VM as a host mount that the VMM shares in over virtio-fs, mapping the host's cached copy of
//! a file into the guest through its own DAX window. Testing that path by use takes a real guest that maps a file
//! through the window, stores into it, syncs it and watches another path's writes arrive, while the host drives each
//! step and checks the volume. This crate is that guest. Its binary is a Linux guest's `/init`, and this library is the
//! line protocol the guest and the host's test speak over the guest's serial console ([`protocol`]).
//!
//! The protocol is text, so a run's console log reads as its own transcript, and pure, so it is parsed and encoded the
//! same on every host and tested here. The guest is Linux only (`src/guest.rs`); on another host the binary says so and
//! exits. It is test infrastructure: it never runs on a host and is never shipped (`publish = false`).

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod protocol;
