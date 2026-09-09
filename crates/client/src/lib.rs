//! `slates-client`: the Rust client of the daemon (§4.7 "Protocol" and "Wake strategy", §4.9
//! "Exactly-once", §4.4's lifecycle verbs; Phase 2 task 5). The SDKs of Phase 5, the CLI and
//! the MCP server all speak through it, so the ring protocol has one client implementation.
//!
//! What it holds: the rendezvous (`slates_ipc::connect`, with the client id the daemon
//! assigned), the region's two rings, and a request sequence. A call is one request in flight:
//! the body is framed into a slot (inline, or through the bulk chunk the slot's ring index
//! owns), the client spins for the daemon's published window and then parks on the wake word
//! (`ClientEnd::wait`), and the reply is decoded and typed.
//!
//! Exactly-once (§4.9): request ids are `(client id, sequence)`; the daemon records every
//! reply before sending it, so a retry under the same id returns the original reply without
//! executing again. The client uses that in two places: a reply that stalls past the deadline
//! while the daemon is gone (§4.7 "Failure matrix": the control channel reset) makes the
//! client reconnect under its old id, which the daemon honours when no live client holds it,
//! and resend the same request; and a session (`Session`) can be resumed by a later client
//! (a process that restarted) to retry what it had in flight.
//!
//! Deadlines are derived (`Deadlines::derive`): a reply is waited for as long as the anchor
//! waits for the daemon's heartbeat, since a daemon silent longer than that is being
//! restarted; a reconnect is tried for the recovery budget the restart is bounded by.
//!
//! The synchronous form is the base; an async form over the completion descriptor (Linux) or
//! the wake word is Phase 5's, thin over this one (R6: sync facades are thin wrappers; here the
//! sync form is the primitive because a parked wait is a single word wait).

pub mod client;
pub mod error;

pub use client::{Attachment, Client, CreateSpec, Deadlines, Landing, Rebased, Session, Submitted};
pub use error::ClientError;
pub use slates_ipc::protocol::{
  AbsenceIs, ActionCount, AuditEntry, DaemonReport, Filter, GrantScope, GrantSummary, Intent,
  LandingOutcome, LandingSummary, NamePolicy, PlacedState, Refusal, RefusalCount, Scope,
  ShardReport, Signal, SizeClass, SnapshotId, StatusReport, VolumeId, VolumeSummary, WorkOp,
};
/// The request id [`Client::begin`] returns and [`Client::poll_reply`] matches on — the async
/// caller holds it between the send and the reply the completion fd signals (§4.7, D-19).
pub use slates_wire::request::RequestId;
