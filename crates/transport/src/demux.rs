//! One socket, many sessions — the connection-ID demultiplexer (§4.10a §8 "connection IDs"; §4.8 the
//! fleet's serve side). A node serving its peers binds **one** socket per plane and lets every peer dial
//! it: the [`Demux`] owns the socket, runs its receive loop, and routes each datagram to the session it
//! belongs to — a raw handshake datagram by its **source address** (the session under handshake from
//! that dialer), a 1-RTT packet by the **connection id** in its short header (`endpoint.rs`: eight bytes
//! both ends derive from the TLS exporter once the handshake completes, so the id needs no wire
//! negotiation and is unique per session). A datagram from an unknown source opens a new server session
//! and hands it to whoever awaits [`Demux::accept`]; a 1-RTT packet naming no session is dropped and
//! counted, never a panic.
//!
//! **Why this shape.** Before it, the fleet bound one serve socket per peer per plane because an
//! accepting endpoint pinned the first source it heard — `N·(N−1)` sockets for the mesh, a block of `2N`
//! ports per node in the deployment manifest, and no way for a peer to *re-dial* after losing its
//! session (its pinned accept side never rebuilt). With the id in every packet, one socket carries every
//! peer, a node advertises two ports, and a re-dialing peer's new session **replaces** its old one
//! ([`Demux::bind`] closes the session previously established under the same peer certificate, whose
//! serve loop then ends with [`EndpointError::Closed`]) — reconnection after a mid-run loss, by
//! construction.
//!
//! **Ownership and bounds (D-8, banned item 8).** The demultiplexer is leaked to `&'static` — one per
//! socket per boot, process-lifetime like the socket — and its state is a `RefCell` on the one shard
//! thread that runs it (no lock, no `Arc`). Sessions live in a slab of at most `max_sessions` slots
//! (the caller derives it: the fleet passes two per peer — the live session and a re-dial replacing it),
//! named by generational [`Slot`]s so a stale handle is a typed miss. Each session's inbox holds as
//! many datagrams as the socket's own kernel receive buffer would (`SO_RCVBUF` over the minimum datagram
//! — the queue the per-peer socket it replaces had); past that a datagram is dropped and counted, and
//! the peer's tail-loss probe retransmits it. The receive loop yields after every datagram it routes, so
//! the sessions drain their inboxes between arrivals instead of after a whole burst. An [`Endpoint`]
//! built on a shared link releases its slot when dropped.
//!
//! An endpoint on a shared link names its demultiplexer by a [`DemuxId`] looked up in this shard's
//! thread-local table, never by reference: the demultiplexer's state is this thread's alone (a
//! `RefCell`), so a reference to it could not cross threads, while the endpoint stays `Send` as every
//! other endpoint is. Used from another thread, a shared-link endpoint finds no demultiplexer and is
//! refused typed (`Closed`) — never a data race.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::task::{Context, Poll, Waker};

use rustls::pki_types::CertificateDer;
use slates_rt::udp::{SocketAddrV4, UdpSocket};

use crate::endpoint::{
  ConnectionId, Endpoint, EndpointError, MIN_DATAGRAM_BYTES, connection_id_of, is_short_header,
};
use crate::handshake::{HandshakeError, Identity, server_connection};

/// Shape: the largest datagram the receive loop reads — the same buffer the endpoint uses; a fleet
/// packet never exceeds the RFC 9000 §14.1 minimum datagram.
const DATAGRAM_BYTES: usize = 2048;

/// A demultiplexer's id in this shard's table (see the module doc).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DemuxId(u32);

thread_local! {
  /// The demultiplexers this shard runs, by id. Grows by one per socket per boot (each is leaked for the
  /// process's lifetime), so it is bounded by the sockets a node serves on.
  static DEMUXES: RefCell<Vec<&'static Demux>> = const { RefCell::new(Vec::new()) };
}

/// Runs `f` on the demultiplexer `id` names on this shard, or `None` if this thread runs no such
/// demultiplexer.
pub(crate) fn with_demux<R>(id: DemuxId, f: impl FnOnce(&'static Demux) -> R) -> Option<R> {
  let demux = DEMUXES.with(|table| {
    table
      .borrow()
      .get(usize::try_from(id.0).unwrap_or(usize::MAX))
      .copied()
  })?;
  Some(f(demux))
}

/// A session's slot in the demultiplexer's table: an index and the generation it was allotted under,
/// so a released and reused slot never answers to its old handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot {
  index: u32,
  generation: u32,
}

/// The counters an operator (or a test) reads to see what the demultiplexer refused or dropped.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DemuxCounters {
  /// 1-RTT packets whose connection id named no live session (a stale packet, a stray, or a session
  /// already closed).
  pub unknown_id: u64,
  /// Datagrams dropped because their session's inbox was full.
  pub inbox_full: u64,
  /// Handshakes from new sources refused because every session slot was taken.
  pub sessions_refused: u64,
  /// Sessions closed because their peer established a new one (a re-dial after a loss).
  pub replaced: u64,
  /// Server sessions opened for a new source.
  pub opened: u64,
}

/// A session's inbox: its peer, the datagrams waiting for it, the waker of the task reading it, and
/// whether the demultiplexer closed it.
struct Inbox {
  peer: SocketAddrV4,
  queue: VecDeque<Vec<u8>>,
  waker: Option<Waker>,
  closed: bool,
}

impl Inbox {
  fn wake(&mut self) {
    if let Some(waker) = self.waker.take() {
      waker.wake();
    }
  }
}

/// The mutable state behind the `RefCell`.
struct Inner {
  slots: Vec<Option<Inbox>>,
  generations: Vec<u32>,
  free: Vec<u32>,
  /// Every live session by the source it dials from (set when opened; a source maps to one session).
  by_source: BTreeMap<SocketAddrV4, Slot>,
  /// Every established session by its connection id.
  by_id: BTreeMap<ConnectionId, Slot>,
  /// Every established session by the peer certificate it authenticated with.
  by_peer: BTreeMap<Vec<u8>, Slot>,
  /// Server sessions opened for a new source, awaiting [`Demux::accept`].
  pending: VecDeque<Endpoint>,
  accept_waker: Option<Waker>,
  counters: DemuxCounters,
}

/// The demultiplexer over one socket: see the module doc.
pub struct Demux {
  id: DemuxId,
  socket: UdpSocket,
  identity: &'static Identity,
  allowed: Vec<CertificateDer<'static>>,
  frame_cap: usize,
  /// Derived: the datagrams one session's inbox holds — the socket's kernel receive buffer over the
  /// minimum datagram, the queue the per-peer socket this shares out used to give each peer.
  inbox_datagrams: usize,
  inner: RefCell<Inner>,
}

impl Demux {
  /// Takes ownership of `socket` and serves up to `max_sessions` peers on it, presenting `identity` and
  /// requiring each dialer's certificate among `allowed` (mutual TLS — the same trust a single-peer
  /// server enforces). Leaked to `&'static`: one demultiplexer per socket per boot. The caller spawns
  /// [`Demux::run`] on the shard that will drive the sessions, and owns that task.
  pub fn start(
    socket: UdpSocket,
    identity: &'static Identity,
    allowed: Vec<CertificateDer<'static>>,
    frame_cap: usize,
    max_sessions: usize,
  ) -> &'static Demux {
    let max_sessions = max_sessions.max(1);
    let mut slots = Vec::with_capacity(max_sessions);
    slots.resize_with(max_sessions, || None);
    let free: Vec<u32> = (0..max_sessions)
      .rev()
      .map(|index| u32::try_from(index).unwrap_or(u32::MAX))
      .collect();
    let id = DEMUXES.with(|table| DemuxId(u32::try_from(table.borrow().len()).unwrap_or(u32::MAX)));
    // A socket that will not say has the smallest queue a datagram socket can be given: one datagram.
    let inbox_datagrams = socket
      .recv_buffer_bytes()
      .map(|bytes| bytes / MIN_DATAGRAM_BYTES)
      .unwrap_or(1)
      .max(1);
    let demux: &'static Demux = Box::leak(Box::new(Demux {
      id,
      socket,
      identity,
      allowed,
      frame_cap,
      inbox_datagrams,
      inner: RefCell::new(Inner {
        slots,
        generations: vec![0; max_sessions],
        free,
        by_source: BTreeMap::new(),
        by_id: BTreeMap::new(),
        by_peer: BTreeMap::new(),
        pending: VecDeque::new(),
        accept_waker: None,
        counters: DemuxCounters::default(),
      }),
    }));
    DEMUXES.with(|table| table.borrow_mut().push(demux));
    demux
  }

  /// This demultiplexer's id on its shard — what an endpoint on its socket names it by.
  pub fn id(&self) -> DemuxId {
    self.id
  }

  /// A fresh TLS server state presenting this demultiplexer's identity and pinning its allowed peers —
  /// one per accepted session.
  pub(crate) fn server_connection(&self) -> Result<rustls::quic::ServerConnection, HandshakeError> {
    server_connection(self.identity, &self.allowed)
  }

  /// The frame cap every session on this socket frames at.
  pub(crate) fn frame_cap(&self) -> usize {
    self.frame_cap
  }

  /// The socket's local address (the port peers dial).
  pub fn local_addr(&self) -> Result<SocketAddrV4, EndpointError> {
    self.socket.local_addr().map_err(EndpointError::Io)
  }

  /// The counters so far.
  pub fn counters(&self) -> DemuxCounters {
    self.inner.borrow().counters
  }

  /// How many sessions are live (opened and not released).
  pub fn sessions(&self) -> usize {
    self
      .inner
      .borrow()
      .slots
      .iter()
      .filter(|s| s.is_some())
      .count()
  }

  /// The receive loop: reads every datagram off the socket and routes it. Runs until the socket refuses;
  /// the caller owns the task (spawns it on this shard and cancels it at shutdown).
  pub async fn run(&'static self) -> Result<(), EndpointError> {
    let mut buf = [0u8; DATAGRAM_BYTES];
    loop {
      let (n, from) = self.socket.recv_from(&mut buf).await?;
      self.route(&buf[..n], from);
      // Yield between datagrams: a burst queued in the kernel would otherwise be routed whole before any
      // session task ran, filling an inbox the session had no chance to drain.
      slates_rt::futures::yield_now().await;
    }
  }

  /// The datagrams one session's inbox holds before a further one is dropped (see the module doc).
  pub fn inbox_datagrams(&self) -> usize {
    self.inbox_datagrams
  }

  /// A future that yields the next server session a new source opened — un-established: the caller
  /// drives [`Endpoint::establish`] as for any endpoint.
  pub fn accept(&'static self) -> Accept {
    Accept { demux: self }
  }

  /// Closes the session established under `peer` (its certificate), if any: its reader gets
  /// [`EndpointError::Closed`] and its slot is released when it drops. For a peer the fleet retired.
  pub fn close_peer(&self, peer: &CertificateDer<'_>) {
    let mut inner = self.inner.borrow_mut();
    if let Some(slot) = inner.by_peer.remove(peer.as_ref()) {
      inner.close(slot);
    }
  }

  /// Sends `datagram` to `peer` on the shared socket (every session sends through it directly).
  pub(crate) fn send_to(&self, datagram: &[u8], peer: SocketAddrV4) -> Result<(), EndpointError> {
    self
      .socket
      .send_to(datagram, peer)
      .map(|_| ())
      .map_err(EndpointError::Io)
  }

  /// Polls the session's inbox: a datagram (copied into `buf`, with its source), `Closed` once the
  /// demultiplexer closed the session, or `Pending` with the task's waker parked in the inbox.
  pub(crate) fn poll_recv(
    &self,
    slot: Slot,
    buf: &mut [u8],
    cx: &mut Context<'_>,
  ) -> Poll<Result<(usize, SocketAddrV4), EndpointError>> {
    let mut inner = self.inner.borrow_mut();
    let Some(inbox) = inner.inbox_mut(slot) else {
      return Poll::Ready(Err(EndpointError::Closed));
    };
    if let Some(datagram) = inbox.queue.pop_front() {
      let n = datagram.len().min(buf.len());
      buf[..n].copy_from_slice(&datagram[..n]);
      return Poll::Ready(Ok((n, inbox.peer)));
    }
    if inbox.closed {
      return Poll::Ready(Err(EndpointError::Closed));
    }
    inbox.waker = Some(cx.waker().clone());
    Poll::Pending
  }

  /// Records an established session's connection id and peer certificate so 1-RTT packets route to it,
  /// and **replaces** the session previously established under the same certificate (the peer
  /// re-dialed after losing its session: the old one is closed, its serve loop ends).
  pub(crate) fn bind(&self, slot: Slot, id: ConnectionId, peer: Option<Vec<u8>>) {
    let mut inner = self.inner.borrow_mut();
    if inner.inbox_mut(slot).is_none() {
      return;
    }
    inner.by_id.insert(id, slot);
    if let Some(peer) = peer
      && let Some(previous) = inner.by_peer.insert(peer, slot)
      && previous != slot
    {
      inner.counters.replaced += 1;
      inner.close(previous);
    }
  }

  /// Releases a session's slot (its endpoint dropped): every route to it is forgotten and the slot
  /// is reusable under a new generation.
  pub(crate) fn release(&self, slot: Slot) {
    let mut inner = self.inner.borrow_mut();
    if inner.inbox_mut(slot).is_none() {
      return;
    }
    let index = usize::try_from(slot.index).unwrap_or(usize::MAX);
    inner.by_source.retain(|_, s| *s != slot);
    inner.by_id.retain(|_, s| *s != slot);
    inner.by_peer.retain(|_, s| *s != slot);
    if let Some(entry) = inner.slots.get_mut(index) {
      *entry = None;
    }
    if let Some(generation) = inner.generations.get_mut(index) {
      *generation = generation.wrapping_add(1);
    }
    inner.free.push(slot.index);
  }

  /// Routes one datagram: a 1-RTT packet by its connection id; a raw handshake datagram by its source,
  /// opening a session for a source not yet seen.
  fn route(&'static self, datagram: &[u8], from: SocketAddrV4) {
    let mut inner = self.inner.borrow_mut();
    if is_short_header(datagram) {
      let target = connection_id_of(datagram).and_then(|id| inner.by_id.get(&id).copied());
      match target {
        Some(slot) => inner.deliver(self.inbox_datagrams, slot, datagram),
        None => inner.counters.unknown_id += 1,
      }
      return;
    }
    if let Some(slot) = inner.by_source.get(&from).copied() {
      inner.deliver(self.inbox_datagrams, slot, datagram);
      return;
    }
    let Some(slot) = inner.open(self, from) else {
      inner.counters.sessions_refused += 1;
      return;
    };
    inner.deliver(self.inbox_datagrams, slot, datagram);
    if let Some(waker) = inner.accept_waker.take() {
      waker.wake();
    }
  }
}

impl Inner {
  fn inbox_mut(&mut self, slot: Slot) -> Option<&mut Inbox> {
    let index = usize::try_from(slot.index).ok()?;
    if self.generations.get(index).copied() != Some(slot.generation) {
      return None;
    }
    self.slots.get_mut(index)?.as_mut()
  }

  /// Queues a datagram for a session and wakes its reader; an inbox at `capacity` drops it, counted.
  fn deliver(&mut self, capacity: usize, slot: Slot, datagram: &[u8]) {
    let Some(inbox) = self.inbox_mut(slot) else {
      self.counters.unknown_id += 1;
      return;
    };
    if inbox.queue.len() >= capacity {
      self.counters.inbox_full += 1;
      return;
    }
    inbox.queue.push_back(datagram.to_vec());
    inbox.wake();
  }

  /// Closes a session: its reader is woken with `Closed` and every route to it is forgotten (the slot
  /// itself is released when the endpoint drops).
  fn close(&mut self, slot: Slot) {
    self.by_source.retain(|_, s| *s != slot);
    self.by_id.retain(|_, s| *s != slot);
    if let Some(inbox) = self.inbox_mut(slot) {
      inbox.closed = true;
      inbox.wake();
    }
  }

  /// Opens a server session for a new source: takes a free slot, builds the un-established endpoint on
  /// the shared link, and queues it for `accept`. `None` when no slot is free or the TLS server state
  /// refused (counted by the caller).
  fn open(&mut self, demux: &'static Demux, from: SocketAddrV4) -> Option<Slot> {
    let index = self.free.pop()?;
    let at = usize::try_from(index).ok()?;
    let generation = self.generations.get(at).copied()?;
    let slot = Slot { index, generation };
    let endpoint = match Endpoint::accepted(demux.id, slot, from, demux) {
      Ok(endpoint) => endpoint,
      Err(_) => {
        self.free.push(index);
        return None;
      }
    };
    if let Some(entry) = self.slots.get_mut(at) {
      *entry = Some(Inbox {
        peer: from,
        queue: VecDeque::new(),
        waker: None,
        closed: false,
      });
    }
    self.by_source.insert(from, slot);
    self.pending.push_back(endpoint);
    self.counters.opened += 1;
    Some(slot)
  }
}

/// The future [`Demux::accept`] returns: the next server session a new source opened.
pub struct Accept {
  demux: &'static Demux,
}

impl std::future::Future for Accept {
  type Output = Endpoint;
  fn poll(self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Endpoint> {
    let mut inner = self.demux.inner.borrow_mut();
    if let Some(endpoint) = inner.pending.pop_front() {
      return Poll::Ready(endpoint);
    }
    inner.accept_waker = Some(cx.waker().clone());
    Poll::Pending
  }
}

impl Unpin for Accept {}
