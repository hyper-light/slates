//! Async TCP on the runtime (§4.6): the NFS loopback bridge's production server accepts connections
//! and serves each over the executor's own driver — no foreign runtime (R6, D-9). `accept` and `read`
//! await read-readiness through the shard's driver; `write_all` awaits write-readiness so a write to a
//! stalled peer (a soft-mounted NFS client that stopped reading, filling the send buffer) yields the
//! shard instead of blocking it; `connect` awaits write-readiness for the handshake to complete.
//!
//! One code path on every platform: the async logic is here, and the socket calls are the platform seam's
//! (`crate::netsys`: `rustix` on Unix, since the lint wall reserves `std::net`; Winsock 2 on Windows), as
//! the datagram socket's are. The readiness-native drivers back it: kqueue and epoll on Unix, and on Windows
//! the IOCP driver's AFD reactor, whose readable edge includes a pending accept, a disconnect and a failed
//! connect, and whose writable edge a completed or failed handshake (`crate::afd`). TCP is host-local, so
//! unlike the UDP fleet plane (§4.10a) it has no simulated fabric: it runs on a real runtime.
//!
//! Where the platforms differ, each answers for itself rather than pretending:
//! - **Whole sends** ([`TcpStream::make_sends_whole`]): macOS and the BSDs make a send of up to the record bound
//!   all or none (`SO_SNDLOWAT`); Linux and Windows do not, and say so ([`WholeSends::NotOffered`]).
//! - **A receive low-water mark** ([`TcpStream::want_bytes`]): Unix keeps one; Windows' TCP provider refuses
//!   `SO_RCVLOWAT` (Microsoft Learn, SOL_SOCKET options), and its refusal reaches the caller typed.
//! - **Handing a stream to another process** (`SCM_RIGHTS` of the borrowed descriptor) is Unix's; Windows
//!   duplicates a socket into another process with `WSADuplicateSocketW` instead (`docs/wip/platforms.md`, gap 2).
//! - **The address a listener binds** on Windows is its alone (`SO_EXCLUSIVEADDRUSE`), so no other process can take
//!   its port's connections by binding it with `SO_REUSEADDR`.

use crate::error::RtError;
use crate::netsys::{self, Io, Socket};
use crate::readiness::{readable, writable};

// The socket address types are `core::net`'s (the same ones `rustix::net` re-exports and the seam uses);
// re-exported here so a consumer of this API names a bind address without depending on a socket backend.
pub use core::net::{Ipv4Addr, SocketAddrV4};

/// The owned OS handle a listener or a stream is handed over as: a file descriptor on Unix, a Winsock socket on
/// Windows ([`TcpListener::into_owned`], [`TcpStream::from_owned`]).
pub type OwnedStream = netsys::OwnedStream;

/// What [`TcpStream::send_records`] did with a run of records.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sent {
  /// One send took the whole run.
  Whole,
  /// The kernel took part of the run, and this process sent the rest: the stream ended mid-record for a moment.
  Completed,
}

/// Whether a stream's sends of up to its record bound are whole ([`TcpStream::make_sends_whole`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WholeSends {
  /// Every such send takes all of its bytes or none.
  Guaranteed,
  /// The kernel may take part of a send.
  NotOffered,
}

/// A listening TCP socket on the runtime: a non-blocking socket whose `accept` awaits the shard's driver for
/// the next connection.
#[derive(Debug)]
pub struct TcpListener {
  socket: Socket,
}

/// A connected TCP stream on the runtime: a non-blocking socket whose `read` and `write_all` await the shard's
/// driver.
#[derive(Debug)]
pub struct TcpStream {
  socket: Socket,
}

impl TcpListener {
  /// Binds a non-blocking listening socket to `addr` (use port 0 for an OS-assigned port, then
  /// [`TcpListener::local_addr`]) with a `backlog` of pending connections the kernel queues before
  /// the accept loop takes them — the caller derives it from its connection fan-in, and the OS clamps
  /// it to the system maximum.
  pub fn bind(addr: SocketAddrV4, backlog: i32) -> Result<TcpListener, RtError> {
    let socket = netsys::stream_socket()?;
    netsys::bind(&socket, addr)?;
    netsys::listen(&socket, backlog)?;
    Ok(TcpListener { socket })
  }

  /// Adopts an already-bound, listening socket — a listener a supervisor bound and handed over so its port
  /// survives a daemon restart (§4.6, "One TCP loopback listener held by the anchor"). It must name a bound,
  /// listening INET stream socket; it is made non-blocking (the driver waits on it) and kept from the adopting
  /// process's own children (close-on-exec on Unix, inheritance off on Windows), then owned here. The caller has
  /// already taken ownership of the raw handle (its own `unsafe` at the inheritance boundary).
  pub fn from_owned(owned: OwnedStream) -> Result<TcpListener, RtError> {
    Ok(TcpListener {
      socket: netsys::adopt_listener(owned)?,
    })
  }

  /// Gives up ownership of the underlying handle — the counterpart to [`TcpListener::from_owned`], for a
  /// supervisor that binds the listener, then hands it to the daemon it spawns so the port survives a restart
  /// (§4.6). The caller owns the returned handle and its lifetime.
  pub fn into_owned(self) -> OwnedStream {
    netsys::into_owned(self.socket)
  }

  /// The local address the listener is bound to (the OS-assigned port on an ephemeral bind).
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    netsys::local_addr(&self.socket)
  }

  /// Accepts the next connection, awaiting readability through the driver when none is pending. The
  /// accepted socket is made non-blocking and kept from children, like the listener, and `TCP_NODELAY`.
  pub async fn accept(&self) -> Result<TcpStream, RtError> {
    loop {
      match netsys::accept(&self.socket)? {
        Io::Ready(socket) => return Ok(TcpStream { socket }),
        Io::WouldBlock => readable(self.socket.raw_id()).await?,
        Io::Interrupted => continue,
      }
    }
  }
}

impl TcpStream {
  /// Connects to `addr`, awaiting the driver until the handshake completes. A non-blocking connect
  /// returns at once while the handshake runs; the socket becomes writable when it finishes, and a
  /// pending socket error (a refused connection) surfaces then, as a typed refusal.
  pub async fn connect(addr: SocketAddrV4) -> Result<TcpStream, RtError> {
    let socket = netsys::stream_socket()?;
    match netsys::connect(&socket, addr)? {
      Io::Ready(()) => {}
      Io::WouldBlock | Io::Interrupted => {
        writable(socket.raw_id()).await?;
        netsys::connected(&socket)?;
      }
    }
    // `TCP_NODELAY`, so a small write leaves at once instead of waiting for the peer to acknowledge the previous
    // one (Nagle, RFC 896). Every stream here carries request/reply traffic that the server already coalesces into
    // one write per batch, so Nagle has nothing left to merge; left on, a reply written while an earlier one is
    // unacknowledged waits for the client's delayed ACK — 40 ms on Linux (`TCP_DELACK_MIN`), measured as the median
    // round of `a_reply_in_two_writes_does_not_wait_for_the_peers_delayed_acknowledgement` on Linux (42 ms before;
    // docs/bugs/2026-10-05-nagle-delayed-ack.md). An accepted or adopted stream gets it in the seam.
    netsys::set_nodelay(&socket)?;
    Ok(TcpStream { socket })
  }

  /// Gives up the stream's handle, to move the connection to another shard (§4.6: a mount's connection is
  /// served on the shard that owns its volume). No readiness is armed between awaits — every registration is
  /// one-shot, re-armed by the next await — so nothing on this shard's driver waits on it once the serving
  /// task stops awaiting it; on epoll a fired one-shot entry stays in this shard's interest list disabled,
  /// never reported, until the descriptor closes.
  pub fn into_owned(self) -> OwnedStream {
    netsys::into_owned(self.socket)
  }

  /// Adopts a connected stream's handle on the current shard: the counterpart to [`TcpStream::into_owned`]. It
  /// is made non-blocking, kept from children and `TCP_NODELAY`, as an accepted stream is.
  pub fn from_owned(owned: OwnedStream) -> Result<TcpStream, RtError> {
    Ok(TcpStream {
      socket: netsys::adopt_stream(owned)?,
    })
  }

  /// The local address this stream is bound to.
  pub fn local_addr(&self) -> Result<SocketAddrV4, RtError> {
    netsys::local_addr(&self.socket)
  }

  /// Reads into `buf`, awaiting readability through the driver when nothing is ready; returns the
  /// byte count, or zero at end of stream (the peer closed its write half).
  pub async fn read(&self, buf: &mut [u8]) -> Result<usize, RtError> {
    loop {
      match netsys::read(&self.socket, &mut *buf)? {
        Io::Ready(n) => return Ok(n),
        Io::WouldBlock => readable(self.socket.raw_id()).await?,
        Io::Interrupted => continue,
      }
    }
  }

  /// Awaits readability through the driver: bytes queued, at least as many as the receive low-water mark
  /// ([`TcpStream::want_bytes`]; kqueue's read filter and Linux's `tcp_poll` both honour it), or end of stream. It
  /// borrows no buffer, so a caller can await it and then [`TcpStream::peek_now`] into a buffer it holds only across
  /// that synchronous call.
  pub async fn wait_readable(&self) -> Result<(), RtError> {
    readable(self.socket.raw_id()).await
  }

  /// Copies the bytes at the head of the receive queue into `buf` without consuming them (`MSG_PEEK`), without
  /// waiting: `Some(0)` at end of stream, `None` when nothing is queued. The bytes stay in the kernel until
  /// [`TcpStream::discard`] consumes them, so a process that dies holding a peeked request leaves it for the next owner
  /// of the descriptor (A-113).
  pub fn peek_now(&self, buf: &mut [u8]) -> Result<Option<usize>, RtError> {
    loop {
      match netsys::peek(&self.socket, &mut *buf)? {
        Io::Ready(n) => return Ok(Some(n)),
        Io::WouldBlock => return Ok(None),
        Io::Interrupted => continue,
      }
    }
  }

  /// Consumes `count` bytes from the head of the receive queue that a [`TcpStream::peek_now`] already saw, so they are
  /// queued and the call never waits. Linux discards them in the kernel (`MSG_TRUNC` on a TCP socket, tcp(7)); elsewhere
  /// they are copied into `scratch` and dropped. A queue holding fewer than `count` bytes is a typed refusal, never a
  /// wait: the caller's view of the queue was wrong.
  pub fn discard(&self, count: usize, scratch: &mut [u8]) -> Result<(), RtError> {
    let mut left = count;
    while left > 0 {
      let take = left.min(scratch.len());
      let into = scratch
        .get_mut(..take)
        .filter(|into| !into.is_empty())
        .ok_or_else(|| netsys::invalid("recv(discard)"))?;
      match netsys::discard(&self.socket, into)? {
        Io::Ready(0) => return Err(netsys::broken_pipe("recv(discard)")),
        Io::Ready(n) => left = left.saturating_sub(n.min(take)),
        Io::Interrupted => continue,
        Io::WouldBlock => return Err(netsys::would_block("recv(discard)")),
      }
    }
    Ok(())
  }

  /// Sends `records`, a run of whole RPC records, awaiting writability through the driver until the kernel takes them.
  /// On a stream whose sends are whole ([`TcpStream::make_sends_whole`] answered [`WholeSends::Guaranteed`]) and a run
  /// no longer than its record bound, one send takes all of it or nothing, so a process that dies here leaves the stream
  /// at a record boundary (A-113). A short send is completed by this call and answered [`Sent::Completed`] so the caller
  /// can count it: the stream crossed a moment when it ended mid-record.
  pub async fn send_records(&self, records: &[u8]) -> Result<Sent, RtError> {
    loop {
      match netsys::write(&self.socket, records)? {
        Io::Ready(0) if !records.is_empty() => return Err(netsys::broken_pipe("write")),
        Io::Ready(n) if n >= records.len() => return Ok(Sent::Whole),
        Io::Ready(n) => {
          self.write_all(records.get(n..).unwrap_or_default()).await?;
          return Ok(Sent::Completed);
        }
        Io::WouldBlock => writable(self.socket.raw_id()).await?,
        Io::Interrupted => continue,
      }
    }
  }

  /// [`TcpStream::send_records`] for a run held in pieces: one gathering send of `records` in order, so a run of replies
  /// is sent without first being copied into one buffer (each 256 KiB READ reply was copied into the run once more,
  /// 3.4% of the daemon's samples on a Linux NFSv4.2 read, 2026-10-09). The kernel takes a vectored send by its total
  /// length (XNU `sosend` judges `uio_resid` against the send low-water mark), so a run no longer than the record bound
  /// on a whole-send stream is still taken all or nothing (A-113). A short send is completed here and answered
  /// [`Sent::Completed`].
  pub async fn send_records_vectored(&self, records: &[&[u8]]) -> Result<Sent, RtError> {
    let total = records
      .iter()
      .map(|record| record.len())
      .fold(0usize, usize::saturating_add);
    let slices: Vec<std::io::IoSlice<'_>> = records
      .iter()
      .map(|record| std::io::IoSlice::new(record))
      .collect();
    loop {
      match netsys::write_vectored(&self.socket, &slices)? {
        Io::Ready(0) if total > 0 => return Err(netsys::broken_pipe("writev")),
        Io::Ready(n) if n >= total => return Ok(Sent::Whole),
        Io::Ready(n) => {
          self.write_all_from(records, n).await?;
          return Ok(Sent::Completed);
        }
        Io::WouldBlock => writable(self.socket.raw_id()).await?,
        Io::Interrupted => continue,
      }
    }
  }

  /// Writes every byte of `records` from byte `sent` of their concatenation on, record by record, awaiting writability
  /// whenever the send buffer is full.
  async fn write_all_from(&self, records: &[&[u8]], sent: usize) -> Result<(), RtError> {
    let mut skip = sent;
    for record in records {
      if skip >= record.len() {
        skip = skip.saturating_sub(record.len());
        continue;
      }
      self
        .write_all(record.get(skip..).unwrap_or_default())
        .await?;
      skip = 0;
    }
    Ok(())
  }

  /// Makes the stream's kernel buffers hold `buffer_bytes` each way, so a whole record can sit in the receive queue to
  /// be peeked and a whole record fits the send queue. On macOS, the BSDs and Windows the buffers are set (`SO_SNDBUF`,
  /// `SO_RCVBUF`), refused unless the kernel keeps at least that much. Linux is left to autotune: an unprivileged
  /// `SO_RCVBUF` is clamped at `net.core.rmem_max` (about 208 KiB by default, below a record) and turns autotuning off,
  /// while a receive low-water mark grows an unlocked buffer to fit it, up to half of `tcp_rmem[2]`
  /// (`tcp_set_rcvlowat`, net/ipv4/tcp.c, read 2026-10-06) — what [`TcpStream::want_bytes`] asks and checks. Linux
  /// sends are not made whole ([`WholeSends::NotOffered`]), so its send buffer needs no floor.
  pub fn reserve_buffers(&self, buffer_bytes: usize) -> Result<(), RtError> {
    netsys::reserve_buffers(&self.socket, buffer_bytes)
  }

  /// Makes every send of at most `record_bytes` whole — all of it or none — where the kernel offers that, and says
  /// whether it does. On macOS and the BSDs a non-blocking `sosend` refuses `EWOULDBLOCK`, copying nothing, while the
  /// free space is below both the request and the send low-water mark, and allocates its buffers waiting rather than
  /// failing midway (XNU `bsd/kern/uipc_socket.c` `sosend`, read 2026-10-06); with `SO_SNDLOWAT` at `record_bytes` a
  /// send that size or smaller is whole. The kernel clamps the mark to the send buffer without saying so, so a buffer
  /// smaller than `record_bytes` is refused here rather than trusted. Linux keeps `SO_SNDLOWAT` fixed (socket(7)) and
  /// copies what fits, and a non-blocking Windows send may take part of its bytes, so both answer
  /// [`WholeSends::NotOffered`].
  pub fn make_sends_whole(&self, record_bytes: usize) -> Result<WholeSends, RtError> {
    Ok(if netsys::make_sends_whole(&self.socket, record_bytes)? {
      WholeSends::Guaranteed
    } else {
      WholeSends::NotOffered
    })
  }

  /// Sets the receive low-water mark (`SO_RCVLOWAT`): readability is reported once `bytes` are queued (or at end of
  /// stream), so a reader waiting for a whole record is woken once, not for every segment of it.
  /// The kernel caps the mark without saying so (Linux at half of `tcp_rmem[2]`, BSD at the receive buffer); a mark it
  /// kept below `bytes` would report readable before the record is whole, so it is read back and refused. Windows'
  /// TCP provider keeps no such mark, and its refusal is returned.
  pub fn want_bytes(&self, bytes: usize) -> Result<(), RtError> {
    netsys::want_bytes(&self.socket, bytes)
  }

  /// Whether the connection has left the established state — the peer closed its half (its FIN arrived: close-wait) or
  /// the connection is gone. A reader that peeks and never consumes an unfinished record cannot learn of the peer's close
  /// from a zero-length read, since the unfinished bytes stay queued; it asks this once readability says something
  /// changed (A-113). This process never half-closes a stream it still reads, so every other state means the peer is
  /// done. Read from the kernel's own connection record (Linux `TCP_INFO`, macOS `TCP_CONNECTION_INFO`, Windows
  /// `SIO_TCP_INFO`).
  pub fn peer_closed(&self) -> Result<bool, RtError> {
    Ok(!netsys::established(&self.socket)?)
  }

  /// Ends both directions of the connection, whoever else holds a copy of its handle: a close drops only this
  /// process's copy, and the connection lives while another process (the anchor) holds one.
  pub fn shutdown(&self) -> Result<(), RtError> {
    netsys::shutdown(&self.socket)
  }

  /// The stream's descriptor, borrowed: to hand a duplicate to another process by `SCM_RIGHTS`.
  #[cfg(unix)]
  pub fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
    netsys::as_fd(&self.socket)
  }

  /// Writes all of `buf`, awaiting writability through the driver whenever the send buffer is full,
  /// so a stalled peer yields the shard rather than blocking it. Returns when every byte is accepted.
  pub async fn write_all(&self, buf: &[u8]) -> Result<(), RtError> {
    let mut sent = 0;
    while let Some(unsent) = buf.get(sent..).filter(|unsent| !unsent.is_empty()) {
      match netsys::write(&self.socket, unsent)? {
        // A non-blocking write of a non-empty slice returns bytes written, a would-block, or an error; a zero here
        // would mean the kernel accepted nothing without blocking, which is a broken pipe.
        Io::Ready(0) => return Err(netsys::broken_pipe("write")),
        Io::Ready(n) => sent = sent.saturating_add(n),
        Io::WouldBlock => writable(self.socket.raw_id()).await?,
        Io::Interrupted => continue,
      }
    }
    Ok(())
  }
}
