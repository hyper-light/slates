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
//! The control shard drains Linux rendezvous connections through its driver's one-shot
//! readiness; other platforms use the shared-memory word watcher ([`doorbell`]). Nothing here touches a host path but
//! the read-only base of an overlay volume; nothing writes a disk.
//!
//! Modules: [`config`] (the daemon's derivations from the profile), [`state`] (the shard's
//! state and the client and volume records), [`verbs`] (the dispatcher), [`daemon`] (start,
//! recovery, the control shard, shutdown), [`doorbell`] (the thread that turns a client's
//! ring into the driver's kick), [`fleet`] (the membership loop and the record plane a fleet node
//! runs), [`deploy`] (a node's socket map and member ids from the operator's one shared manifest),
//! [`telemetry`] (the chokepoint spans' per-shard rings and the bounded `Telemetry` drain the status
//! surfaces read, §4.14), [`error`].

// The no-panic law (CLAUDE.md, banned item 6): shipped code never overflows or divides by zero. Test builds
// are exempt. Once a crate is clean this holds it there; out-of-bounds indexing and slicing are denied
// workspace-wide.
// The panic rule: a print macro panics on a refused write; lines go through `slates_machine::log_line!` (A-213).
#![deny(clippy::print_stdout, clippy::print_stderr)]
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

// A request that meets a pending S3 base waits for the fetch it met (§4.15 "An S3 base", A-555).
mod base_wait;
mod carry;
pub mod catalog;
pub mod config;
mod consensus;
mod consensus_recovery;
mod content_cipher;
mod content_holder;
mod content_pool;
pub mod content_retention;
pub use consensus_recovery::{RecoveryKey, recovery_proof};
pub mod daemon;
pub mod deploy;
mod direct_map;
pub mod discovery;
mod pressure;
mod retention;
// Name resolution for a peer named by DNS in the manifest (the Kubernetes deployment): an asynchronous
// A-record client over the runtime's UDP, resolved at every dial so a rescheduled peer is found.
// The daemon's end of the anchor's hold (A-61, A-113): paired per platform, one API; Windows' answers that no anchor
// holds (docs/wip/platforms.md, gap 2).
#[cfg(unix)]
mod anchor_hold;
#[cfg(windows)]
#[path = "anchor_hold_windows.rs"]
mod anchor_hold;
pub mod dns;
pub mod doorbell;
pub mod error;
pub mod fleet;
mod forward_lane;
mod fs_verbs;
#[cfg(target_os = "linux")]
mod fuse;
// The descriptors the anchor holds for this daemon's attachments: FUSE devices (Linux) and Shards streams' resync ends
// (Linux, macOS; A-316, A-339).
#[cfg(unix)]
mod fuse_hold;
// The macOS NFS mounts this daemon serves, recorded as its own (A-227).
#[cfg(target_os = "macos")]
mod own_nfs;
// A writable direct map's copy that outlives its daemon: held by the anchor, taken back by the successor (A-197).
mod checkout;
// A green's takeover successor adopts its holder ledger rather than copying it (§4.16 owner-loss recovery, A-333).
mod green_adopt;
pub mod greens;
pub mod head;
#[cfg(target_os = "linux")]
mod held_copy;
// A held NFS connection's send ledger and the judgment its adoption takes (A-550): kept on Linux alone; elsewhere its
// pure parts are compiled and held by every host's tests, and only its no-op seams are called.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
mod held_send;
pub mod histogram;
mod image;
pub mod landing;
mod landing_entries;
pub mod lease;
mod lease_wait;
mod lfs;
mod live_hold;
mod live_ship;
mod member_task;
// Version 0 of a green over a base, made, shipped and seeded a slice at a time at `f > 0` (§4.16, A-286).
mod merge_origin;
pub mod merge_service;
mod mirror;
/// The planned move of a volume to another fleet member (§4.10, A-291).
mod moves;
// A host admitted again under its own id yields the volumes it catalogued (A-334, §4.8 "Authority scope").
mod readmission;
mod remote_attach;
mod work_home;
// The writes acknowledged since the last publication, kept in anchor RAM (A-63): a FUSE mount's (Linux) and a Shards
// stream's (every platform; A-316, A-339, A-402).
mod write_log;
// Each range a Shards stream's held DAX backing has answered, kept in anchor RAM for a successor (A-365; Windows'
// sections, A-448).
mod dax_ranges;
// The Windows WinFsp transport (§4.6 "Windows"): a volume at a drive letter, served on its owner shard.
#[cfg(windows)]
mod winfsp;
// The NFS server: the macOS mount's loopback transport, and on every platform the network export (a fleet node's
// export, `slates export`) that remote and virtual-machine clients mount (Windows mounts locally through WinFsp). It
// rides the runtime's TCP, which every platform has since A-182.
mod callback;
mod delegation;
// A wait whose bound is learned from the waits it measured (RFC 6298, Karn's rule, back-off; A-431, A-442).
mod learned_wait;
// The helper processes a shard waits on (the FUSE helpers), each wait learned as `learned_wait` learns one (A-525).
mod helper_runs;
// The anchor's answers on the Windows hold channel, each exchange's wait learned as `learned_wait` learns one (A-552).
mod anchor_answers;
mod listing;
mod lock_ledger;
pub mod nfs;
// A stale page an NFS client writes back over another path's newer change is refused (A-192).
mod nfs_replay;
mod nfs_state;
pub mod nfs_tls;
mod nfs_writeback;
pub mod observe;
mod owner_location;
mod staging;
mod work_charge;
pub mod work_files;
mod work_log;
// A work's route after its owner's takeover ends once the green's owner holds none of its completions (A-288); public
// for `nudge`, which a test planting a route calls as a takeover's adoption does.
pub mod work_route;
// The container bind form of `attach` (§4.6 A-9): the host mount verified through the kernel's mount
// table and the runtime-specification entry; paired per platform inside, so it compiles everywhere.
pub mod oci;
// The deadlines of what a shard's clients leave behind (staging buffers, status captures), kept on the shard's timing
// wheel by one keeper task (A-522).
mod deadlines;
// A client's hang-up, the event its death is seen at (A-475): its control socket's end of stream (Linux) or its
// process's exit (macOS, Windows), awaited through the shard's driver.
mod hangup;
mod reap;
mod repository;
// git's `wildmatch` with `WM_PATHNAME`: the glob of a git repository's ref patterns and of a Subversion glob rule's
// segments (A-597). No caller outside tests until the version-control servers' policy reads it (their pieces).
#[cfg_attr(not(test), allow(dead_code))]
mod wildmatch;
// §4.17.1's policy by name pattern, each tool's own precedence, indexed (A-597), and the push's check point over the
// policy the repository's record holds (`admit`, A-622). No caller outside tests until the servers' push paths ask it
// (their pieces: git's receive-pack, Subversion's commit editor, Mercurial's unbundle).
#[cfg_attr(not(test), allow(dead_code))]
mod vcs_policy;
// §4.17.1's hooks: the points, their inputs, each hook's learned deadline and the runner (A-597). No caller outside
// tests until the servers' push paths start hooks (their pieces).
#[cfg_attr(not(test), allow(dead_code))]
mod vcs_hooks;
// A hook run as a process of its own over a read-only view of the work, held to `vcs_hooks`' runner (A-622 part 3):
// Unix alone until its Windows part (a job object, `slates_ipc::win_spawn`). No caller outside tests until the servers'
// push paths start hooks (their pieces).
#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
mod hook_driver;
// The version-control credentials a human enrolls for a principal (A-625 part 3): SSH keys by fingerprint and HTTP tokens
// by their hash, on the partition each id routes to; the enroll and revoke verbs the issuer proves, and the lookup the
// HTTP and SSH edges authenticate through. Public for the issuer's proofs, the token's text and the key line's reading.
pub mod credentials;
// Subversion's hooks as the server runs them (A-661): the glue between the Subversion server's call points and the hook
// driver, Unix alone with it. No caller outside tests until the commit drive and the svn:// connection task call it
// (A-655, A-656) and DAV's LOCK and UNLOCK (A-661's next part).
#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
mod svn_hooks;
// The version-control servers' HTTP edge on the runtime's TCP (A-625 parts 2 and 3): the listener, one connection a task,
// each phase's wait learned, the bounds derived from the shard's budget, git's site, a connection moved to its
// repository's shard, and the daemon's start. Upload-pack's store lender has no caller outside tests until each
// repository's store lives on its owner shard (A-662).
#[cfg_attr(not(test), allow(dead_code))]
mod vcs_http;
// A FUSE or WinFsp mount's journal reader's patience, learned from its own rounds as `learned_wait` learns a wait
// (A-548).
mod reader_patience;
// Whether Windows Error Reporting writes this daemon full crash dumps (A-239): read, logged once and reported.
pub mod local_dumps;
mod recovery_slices;
pub mod seal_keys;
// A client's session key (GAP-A9-9's threat model): what a client resuming under its id proves before its seat is served.
pub mod session_key;
// Signed work (A-321): SSH signatures over a merge's or a landing's statement, verified as provenance, never a permission.
pub mod signed_work;
mod snapshot_view;
// The Shards fs transport (§4.6, the Shards route; A-161): a volume served to a Shards microVM over an anonymous
// stream, on the volume's owning shard: a socket pair on Unix, a duplex named pipe on Windows served by the runtime's
// completion driver (A-179).
pub mod shards;
// The Shards stream's socket route (A-161): macOS hands a client its stream end through it, having no control socket.
#[cfg(target_os = "macos")]
mod shards_route;
pub mod state;
mod status_pages;
pub mod takeover;
pub mod telemetry;
// A writer outside FUSE answered once every FUSE mount of its volume was told (AC-3.3).
mod told;
pub mod tombstone;
mod tracked;
// The transport capability report of §4.6 A-9 (what `attach` and `status` say each transport can do
// on this host); pure over one platform seam, so it compiles and tests everywhere.
pub mod transports;
pub mod verbs;
pub mod watchdog;
// The virtio-fs guest transport (§4.6 A-9): a guest device served on the volume's owning shard; its
// loop rides the runtime's Unix descriptor readiness, so it is gated like the NFS transport.
#[cfg(unix)]
pub mod virtiofs;
pub mod xshard;

pub use config::{BootFault, BootFaultKind, DaemonConfig, DurabilityBound, FleetMembership};
pub use daemon::{
  Daemon, DrainReport, HeldRecordView, LeadershipHandoff, LinkAfter, SegmentSource, SessionHold,
};
pub use deploy::{DeployError, FleetManifest, FleetNodeEntry, FleetPlan, NodeAddress};
pub use dns::Resolver;
pub use fleet::{
  FleetPeer, FleetTransport, ServeAddresses, ServeBindError, ServeFault, ServeSockets,
};

pub use error::ServerError;
