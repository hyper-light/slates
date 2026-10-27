//! The daemon's end of the anchor's hold on Windows (A-61, A-113): no anchor holds a Windows daemon's connections yet
//! (`docs/wip/platforms.md`, gap 2), so every hold is answered `None`, as for a Unix daemon run without an anchor, and
//! nothing is handed back at a start. A Windows daemon's NFS connections end with it; their clients reconnect to its
//! successor and keep their file handles, which name files, not connections. This is the same API as the Unix arm
//! (`anchor_hold.rs`), so the NFS server is one code path; holding sockets here is gap 2's work
//! (`WSADuplicateSocketW` into the anchor), and these answers change with it.

use slates_rt::tcp::{OwnedStream, TcpStream};

/// Why a hold was refused. Nothing is ever sent on Windows, so nothing is refused.
pub(crate) type HoldRefusal = std::convert::Infallible;

/// A connection the anchor handed back: its id and the socket. None is handed back on Windows.
#[derive(Debug)]
pub(crate) struct HandedConnection {
  /// The connection's id, kept for its life.
  pub(crate) connection: u64,
  /// The connected socket.
  pub(crate) socket: OwnedStream,
}

/// Everything the anchor handed this daemon at its start: on Windows, nothing.
#[derive(Debug, Default)]
pub(crate) struct Handed {
  /// The held NFS connections.
  pub(crate) connections: Vec<HandedConnection>,
}

/// What the anchor handed this daemon at its start: nothing, since no anchor holds on Windows.
pub(crate) fn adopt() -> Handed {
  Handed::default()
}

/// Asks the anchor to hold `stream`'s socket as `connection`: `None`, no anchor holds.
pub(crate) fn hold_connection(
  _connection: u64,
  _stream: &TcpStream,
) -> Option<Result<(), HoldRefusal>> {
  None
}

/// Tells the anchor `connection` ended: `None`, no anchor holds.
pub(crate) fn release_connection(_connection: u64) -> Option<Result<(), HoldRefusal>> {
  None
}

/// Whether a supervising anchor holds for this daemon: never on Windows.
pub(crate) fn anchored() -> bool {
  false
}
