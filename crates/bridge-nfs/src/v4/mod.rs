//! NFSv4.1 and NFSv4.2 (§4.6 "NFS versions", A-35): the server's minor versions 1 and 2, on the same
//! listener and the same volumes as NFSv3. ONC RPC carries program 100003 version 3 and version 4 side
//! by side, and a v4 `COMPOUND` names its minor version, so one server speaks all three and each client
//! negotiates; nothing is a mode switch.
//!
//! **Shape.** v4 is a protocol front end over the NFS semantic layer slates already has, not a second
//! implementation of it. Each file operation a `COMPOUND` carries is served by the corresponding NFSv3
//! procedure on the same [`crate::multi::NfsService`] (LOOKUP by LOOKUP3, READ by READ3, and so on)
//! through a backend the caller supplies ([`compound::Backend`]). So routing by file handle, capability
//! validation, the POSIX permission rules, the AppleDouble view and the daemon's durability barrier apply
//! to v4 unchanged, and a fix to one is a fix to both. What v4 adds is its own:
//! - the `COMPOUND` frame and the current and saved file handles ([`compound`]);
//! - sessions: client ids, slot tables and the per-slot reply cache that gives exactly-once semantics
//!   (RFC 8881 §2.10, [`session`]);
//! - the attribute encoding (`fattr4`, [`attr`]).
//!
//! Minor version 0 is not served: its state machine (OPEN_CONFIRM, per-owner sequence ids, no
//! sessions) is the part D-2 rejected, and every client that speaks v4.1 prefers it.
//!
//! **Bounds.** Every table is bounded and refuses at its bound with the protocol's own error: clients
//! and sessions (`NFS4ERR_RESOURCE`), a session's slots (the negotiated `ca_maxrequests`, refused
//! `NFS4ERR_BADSLOT`), a compound's operations (`NFS4ERR_TOO_MANY_OPS`), a request's and a reply's size
//! (`NFS4ERR_REQ_TOO_BIG`, `NFS4ERR_REP_TOO_BIG`).
//!
//! Evidence: RFC 8881 (NFSv4.1), RFC 7862 (NFSv4.2) and RFC 7863 (its XDR), read 2026-09-26; the
//! numbers here are transcribed from RFC 7863's XDR.

pub mod attr;
pub mod backend;
pub mod compound;
pub mod session;
pub mod status;
pub mod types;
pub mod v3call;

pub use status::Nfsstat4;

/// Format: the NFS program's version 4 (RFC 7530 §16; RFC 8881 §16).
pub const NFS_V4: u32 = 4;
/// Format: `NFSPROC4_NULL`.
pub const NFSPROC4_NULL: u32 = 0;
/// Format: `NFSPROC4_COMPOUND`.
pub const NFSPROC4_COMPOUND: u32 = 1;
/// Format: the lowest minor version served (NFSv4.1).
pub const MINOR_LOWEST: u32 = 1;
/// Format: the highest minor version served (NFSv4.2).
pub const MINOR_HIGHEST: u32 = 2;
