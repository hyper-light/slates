//! `slates-bridge-fuse`: the Linux FUSE bridge (§4.6, D-2; Phase 3). Ordinary Linux programs
//! see volumes at `<root>/<volume>` and at chosen paths; the kernel's FUSE client sends
//! requests over `/dev/fuse` and this crate turns them into shard operations by handle and
//! writes the replies back.
//!
//! This module is the ABI codec — the pure, transport-free layer (Phase 3 task 1): it parses
//! the kernel's request headers and bodies and encodes the daemon's replies, and it computes
//! the `FUSE_INIT` negotiation. It is a parser of external bytes, so every request body is
//! bounds-checked before use and the parser has hostile-input tests and golden vectors (Part
//! 6). The `/dev/fuse` transport, the mount establishment, the per-shard channels and the
//! `Bridge` implementation over the volume core come next; they are Linux-only and run in the
//! CI Linux lane. The codec here is pure and tested on every host.
//!
//! The layouts match the Linux `fuse` ABI (`include/uapi/linux/fuse.h`); every field is
//! little-endian on the wire and the structs are read and written through byte slices, never a
//! cast, so the crate holds no `unsafe` and no `#[repr(C)]` transmute (the design's rule for
//! parsers of external bytes). The ABI version slates speaks is 7.31 as a floor, negotiated up
//! to whatever the kernel offers.

pub mod abi;
pub mod bridge;
#[cfg(target_os = "linux")]
pub mod channel;
pub mod error;
pub mod init;
pub mod reply;
pub mod request;
pub mod volume_bridge;
pub mod wire;

pub use abi::{FUSE_KERNEL_MINOR_VERSION, FUSE_KERNEL_VERSION, Opcode};
pub use bridge::{Bridge, DirEntry, dispatch};
pub use error::FuseError;
pub use init::{InitNegotiation, negotiate};
pub use reply::{Attr, EntryOut, OpenOut, ReplyHeader, StatfsOut, WriteOut};
pub use request::{InHeader, Request};
pub use volume_bridge::VolumeBridge;
