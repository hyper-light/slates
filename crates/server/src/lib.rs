//! `slates-server` — the daemon (design §2.5, §2.6 "Boot order", §4.4 "Operations", §4.7,
//! §4.8, §4.13; Phase 2 task 4): N shards on the runtime, each owning its volumes, its
//! partition of the database (recovered from the anchor segment at start) and the clients
//! pinned to it; the control shard running the rendezvous; the lifecycle verbs of §4.4 served
//! from each client's command ring with exactly-once completion records, leases with epochs,
//! client-form attachments, and quotas fed by the shard reserve and the host's live memory.
//!
//! Shape: every shard's state lives in a thread-local cell on that shard's thread
//! ([`state`]); the server task of the shard is a registered poller of the runtime, woken
//! when any of its clients' rings holds a request, and it serves each request inline on the
//! shard (one step, no awaits inside, as §4.8 "Transactions" says); a new client reaches its
//! shard as a spawned task that moves the daemon end into that state (sharing by move, D-8).
//! The control shard's task drains the rendezvous every time the doorbell thread ([`doorbell`])
//! kicks it, or when a client is already talking to it. Nothing here touches a host path but
//! the read-only base of an overlay volume; nothing writes a disk.
//!
//! Modules: [`config`] (the daemon's derivations from the profile), [`state`] (the shard's
//! state and the client and volume records), [`verbs`] (the dispatcher), [`daemon`] (start,
//! recovery, the control shard, shutdown), [`doorbell`] (the thread that turns a client's
//! ring into the driver's kick), [`error`].

pub mod config;
pub mod daemon;
pub mod doorbell;
pub mod error;
pub mod state;
pub mod verbs;

pub use config::DaemonConfig;
pub use daemon::{Daemon, SegmentSource};

pub use error::ServerError;
