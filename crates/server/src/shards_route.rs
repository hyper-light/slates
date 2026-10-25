//! The daemon's side of the socket route on macOS (§4.6; A-161, A-173). The protocol, its keys and its evidence are
//! `slates_ipc::route`'s; this module keeps the offers, listens, and runs the exchanges.
//!
//! **Offers.** An attach in the Shards form on macOS keeps its stream end as an offer on the volume's owner shard, with
//! a fresh 256-bit secret and an expiry, and replies `ShardsOffered` with the secret (`crate::shards::attach_verb`). A
//! direct map keeps the read-only descriptor of its copy as an offer the same way and replies `MapOffered`
//! (`crate::direct_map`). Each kind has its own table, so a mapping and an attachment never meet under one id, and a
//! claim names its kind (a version-2 hello). Both ids carry their owner partition in their high 16 bits, which is how
//! an exchange finds the shard that holds the offer.
//! The offer is claimed once: an exchange asks the owner shard to check the claim's proof and hand over the end
//! ([`take`]), so the secret never leaves the shard that made it. An offer unclaimed at its expiry is dropped by its
//! timer, which closes the end: the stream then sees its peer gone and ends its attachment, as every stream does.
//!
//! **The listener and its exchanges.** The listener is bound when the daemon starts and served by one task on the
//! control shard, which accepts each connection without blocking and runs one exchange task for it there. Exchanges
//! are bounded by the shard's client seats (`clients_per_shard`): past them a connection is refused `Busy`. Each runs
//! within one liveness budget, after which a silent client is dropped, so a stranger that connects and says nothing
//! holds a seat for at most that long. Every refusal is counted by its kind, and every claim by its success, which is
//! the route's non-vacuity counter.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use slates_ipc::error::RouteRefusal;
use slates_ipc::route::{
  Exchange, HELLO_BYTES, Hello, KIND_CHALLENGE, Listener, NONCE_BYTES, OfferKind, SEALED_BYTES,
  SECRET_BYTES,
};
use slates_rt::futures;
use slates_vfs::clock::Clock;

use crate::state::{self, ShardState};

/// Derived: how long an offer waits for its claim: the client's reply deadline for the attach's reply to reach it (one
/// liveness budget, `slates_client::Deadlines::derive`), and one more for the claim's own exchange.
pub(crate) const OFFER_LIFE_NS: u64 = 2 * crate::daemon::LIVENESS_BUDGET_NS;
/// Derived: how long one exchange may take: one liveness budget, after which a silent client is as gone as one its
/// daemon would presume dead.
const EXCHANGE_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;

/// Format: the refusal-ledger name of a claimed offer: the end handed over (the route's non-vacuity counter).
const CLAIMED: &str = "shards.route_claimed";
/// Format: the refusal-ledger name of a claim for an attachment with no offer (none made, claimed, or expired).
const NO_OFFER: &str = "shards.route_no_offer";
/// Format: the refusal-ledger name of a claim whose proof did not open under the offer's secret.
const PROOF: &str = "shards.route_proof";
/// Format: the refusal-ledger name of an exchange message not the protocol's (cut short, too long, another version).
const MALFORMED: &str = "shards.route_malformed";
/// Format: the refusal-ledger name of a connection refused at the bound of exchanges.
const BUSY: &str = "shards.route_busy";
/// Format: the refusal-ledger name of an exchange that outlived its liveness budget.
const TIMED_OUT: &str = "shards.route_timed_out";
/// Format: the refusal-ledger name of an exchange the daemon could not carry through (a socket error, no random
/// nonce, the owner shard unreachable).
const FAILED: &str = "shards.route_failed";
/// Format: the refusal-ledger name of an offer dropped unclaimed at its expiry.
const EXPIRED: &str = "shards.route_expired";

/// A descriptor offered on the route, a stream end or a map's copy: claimable once, with `secret`, until `expires_ns`.
pub(crate) struct Offer {
  secret: [u8; SECRET_BYTES],
  expires_ns: u64,
  descriptor: OwnedFd,
}

/// A shard's offers of one kind, by id. Stream offers are bounded with the shard's handoffs
/// (`crate::shards::handoff_bound`: an attach past the bound is refused before an offer is made); map offers by its
/// clients' shares of the direct maps (`crate::direct_map`).
pub(crate) type Offers = BTreeMap<u64, Offer>;

/// The shard's table of offers of `kind`.
fn offers_of(s: &mut ShardState, kind: OfferKind) -> &mut Offers {
  match kind {
    OfferKind::Stream => &mut s.stream_offers,
    OfferKind::Map => &mut s.map_offers,
  }
}

/// Keeps `descriptor` as the `kind` offer `id`, claimable with `secret` until its life ends, and arms the timer that
/// drops it then. `false` when the timer could not be armed: the offer is dropped at once, closing its descriptor (a
/// stream then ends its attachment rather than wait forever for a claim; a map's client reads by copy).
pub(crate) fn offer(
  s: &mut ShardState,
  (kind, id): (OfferKind, u64),
  secret: [u8; SECRET_BYTES],
  descriptor: OwnedFd,
) -> bool {
  let expires_ns = s.clock.monotonic_ns().saturating_add(OFFER_LIFE_NS);
  offers_of(s, kind).insert(
    id,
    Offer {
      secret,
      expires_ns,
      descriptor,
    },
  );
  if futures::spawn(expire_later(kind, id, expires_ns))
    .and_then(futures::detach)
    .is_err()
  {
    offers_of(s, kind).remove(&id);
    return false;
  }
  true
}

/// Withdraws the `kind` offer `id` if it is still unclaimed, its descriptor closed now rather than at its expiry: what a
/// map's copy let go before its claim does, so no offer keeps an object alive past its copy. Whether there was one.
pub(crate) fn withdraw(s: &mut ShardState, (kind, id): (OfferKind, u64)) -> bool {
  offers_of(s, kind).remove(&id).is_some()
}

/// Drops the `kind` offer `id` at its expiry if it is still the one made then. A refused sleep drops it now: an offer
/// is never kept past its life for want of a timer.
async fn expire_later(kind: OfferKind, id: u64, expires_ns: u64) {
  // Either the sleep ran its course or the runtime refused it; the offer goes now in both.
  let _slept = futures::sleep(OFFER_LIFE_NS).await;
  let _ = state::with_state_counted(|s| {
    let offers = offers_of(s, kind);
    if offers
      .get(&id)
      .is_some_and(|offer| offer.expires_ns == expires_ns)
    {
      offers.remove(&id);
      s.count(EXPIRED, 1);
    }
  });
}

/// Hands over the descriptor of the offer `hello` names to the claim whose `proof` opens under the offer's secret with
/// these nonces, retiring the offer, with the daemon's confirmation to send beside it; or the refusal: no offer (none
/// made, claimed, or expired, the expired one dropped here), a proof that does not hold. Run on the shard that holds the
/// offer, where its secret is.
pub(crate) fn take(
  s: &mut ShardState,
  hello: Hello,
  daemon_nonce: [u8; NONCE_BYTES],
  proof: [u8; SEALED_BYTES],
) -> Result<([u8; SEALED_BYTES], OwnedFd), RouteRefusal> {
  let now = s.clock.monotonic_ns();
  let offers = offers_of(s, hello.kind);
  let Some(offer) = offers.get(&hello.id) else {
    return Err(RouteRefusal::NoOffer);
  };
  if offer.expires_ns <= now {
    offers.remove(&hello.id);
    return Err(RouteRefusal::NoOffer);
  }
  let exchange = Exchange::new(
    &offer.secret,
    (hello.kind, hello.id),
    &hello.client_nonce,
    &daemon_nonce,
  )
  .map_err(|_| RouteRefusal::Malformed)?;
  if !exchange.proof_holds(&proof) {
    return Err(RouteRefusal::Proof);
  }
  let confirmation = exchange
    .confirmation()
    .map_err(|_| RouteRefusal::Malformed)?;
  let offer = offers.remove(&hello.id).ok_or(RouteRefusal::NoOffer)?;
  Ok((confirmation, offer.descriptor))
}

/// Serves the route's listener on this shard (the control shard): each wake accepts every waiting connection and runs
/// an exchange for it. Ends only if the runtime refuses the listener's readiness, counted.
pub(crate) async fn serve(listener: Listener) {
  let raw = listener.socket().as_raw_fd();
  loop {
    if slates_rt::readiness::readable(raw).await.is_err() {
      let _ = state::with_state_counted(|s| s.count(FAILED, 1));
      return;
    }
    loop {
      match listener.accept() {
        Ok(Some(connection)) => admit(connection),
        Ok(None) => break,
        Err(_) => {
          let _ = state::with_state_counted(|s| s.count(FAILED, 1));
          break;
        }
      }
    }
  }
}

/// Runs an exchange for `connection` within the bound of exchanges; past it the connection is refused `Busy`.
fn admit(connection: OwnedFd) {
  let admitted = state::with_state(|s| {
    if s.route_exchanges >= s.config.clients_per_shard {
      s.count(BUSY, 1);
      false
    } else {
      s.route_exchanges = s.route_exchanges.saturating_add(1);
      true
    }
  })
  .unwrap_or(false);
  if !admitted {
    refuse(connection.as_fd(), RouteRefusal::Busy);
    return;
  }
  if futures::spawn(exchange(connection))
    .and_then(futures::detach)
    .is_err()
  {
    let _ = state::with_state_counted(|s| {
      s.route_exchanges = s.route_exchanges.saturating_sub(1);
      s.count(FAILED, 1);
    });
  }
}

/// Why an exchange ended without a claim.
enum Ended {
  /// Refused, with the kind the client is told.
  Refused(RouteRefusal),
  /// The daemon could not carry it through; the client is told nothing and sees the end.
  Failed,
}

/// One exchange on `connection`, within one liveness budget: a refusal is sent as its kind byte, and every ending is
/// counted; the connection closes when the task ends.
///
/// **A sent end is kept until the client has its own.** XNU flushes a Unix socket whose every reference is inside a
/// message in flight when its collector of such descriptors runs (`unp_gc`): the socket's receive buffer is emptied and
/// its reads answer end of file. So the end, once sent, stays open here until the client closes the route connection,
/// which it does after its receive has returned, and only then is closed: it always has a reference outside the
/// message. Closed at once, an end arrived flushed in 4 of 20 claims at load 35-47, and in up to 40 of 48 with another
/// thread opening and closing sockets
/// (`docs/bugs/2026-10-24-a-stream-end-arrived-flushed-by-the-kernel-in-flight.md`).
async fn exchange(connection: OwnedFd) {
  let ended = match futures::within(EXCHANGE_NS, run(connection.as_fd())).await {
    Ok(Some(Ok(sent))) => {
      // The client's close, or its deadline, ends the wait; the end is closed only after it.
      let _closed = futures::within(EXCHANGE_NS, client_closes(connection.as_fd())).await;
      drop(sent);
      None
    }
    Ok(Some(Err(ended))) => Some(Ok(ended)),
    Ok(None) | Err(_) => Some(Err(())),
  };
  if let Some(Ok(Ended::Refused(refusal))) = &ended {
    refuse(connection.as_fd(), *refusal);
  }
  let _ = state::with_state_counted(|s| {
    s.route_exchanges = s.route_exchanges.saturating_sub(1);
    let kind = match ended {
      None => CLAIMED,
      Some(Ok(Ended::Refused(refusal))) => refusal_name(refusal),
      Some(Ok(Ended::Failed)) => FAILED,
      Some(Err(())) => TIMED_OUT,
    };
    s.count(kind, 1);
  });
}

/// The ledger name of a refusal an exchange sends.
fn refusal_name(refusal: RouteRefusal) -> &'static str {
  match refusal {
    RouteRefusal::NoOffer => NO_OFFER,
    RouteRefusal::Proof => PROOF,
    RouteRefusal::Busy => BUSY,
    RouteRefusal::Malformed
    | RouteRefusal::Confirmation
    | RouteRefusal::Flushed
    | RouteRefusal::SharedDirectory
    | RouteRefusal::PathTooLong
    | RouteRefusal::InUse => MALFORMED,
  }
}

/// Waits until the client closes `connection`, or sends anything: either says its receive of the end has returned.
async fn client_closes(connection: BorrowedFd<'_>) {
  let mut sink = [0u8; 1];
  loop {
    match slates_ipc::stream::recv(connection, &mut sink) {
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
        if slates_rt::readiness::readable(connection.as_raw_fd())
          .await
          .is_err()
        {
          return;
        }
      }
      Ok(_) | Err(_) => return,
    }
  }
}

/// The exchange's steps (`slates_ipc::route`'s module doc): the hello, the daemon's nonce, the proof (with nothing
/// after it), the owner shard's verdict, the confirmation with the end; the end returned, still held, for the caller to
/// close once the client has its own.
async fn run(connection: BorrowedFd<'_>) -> Result<OwnedFd, Ended> {
  let mut hello = [0u8; HELLO_BYTES];
  read_exact(connection, &mut hello).await?;
  let hello = Hello::decode(&hello).map_err(|_| Ended::Refused(RouteRefusal::Malformed))?;
  let daemon_nonce = slates_ipc::route::random::<NONCE_BYTES>().map_err(|_| Ended::Failed)?;
  let mut challenge = [0u8; 1 + NONCE_BYTES];
  if let Some((kind, nonce)) = challenge.split_first_mut() {
    *kind = KIND_CHALLENGE;
    nonce.copy_from_slice(&daemon_nonce);
  }
  write_all(connection, &challenge).await?;
  let mut proof = [0u8; SEALED_BYTES];
  read_exact(connection, &mut proof).await?;
  // A claim is exactly its two messages: anything after the proof is not the protocol's.
  if slates_ipc::stream::recv(connection, &mut [0u8; 1]).is_ok_and(|read| read > 0) {
    return Err(Ended::Refused(RouteRefusal::Malformed));
  }
  let route = state::with_state(|s| {
    // An attachment and a mapping alike carry their owner partition in their high 16 bits.
    let partition = crate::verbs::owner_of_attachment(hello.id);
    crate::verbs::shard_of_partition(s, partition).map(|owner| (s.shard, owner))
  })
  .flatten();
  let Some((origin, owner)) = route else {
    return Err(Ended::Refused(RouteRefusal::NoOffer));
  };
  let taken = crate::xshard::call_within(
    origin,
    owner,
    move |s| take(s, hello, daemon_nonce, proof),
    EXCHANGE_NS,
  )
  .await;
  let (confirmation, descriptor) = match taken {
    Some(Ok(taken)) => taken,
    Some(Err(refusal)) => return Err(Ended::Refused(refusal)),
    None => return Err(Ended::Failed),
  };
  slates_ipc::route::send_confirmation(connection, &confirmation, descriptor.as_fd())
    .map_err(|_| Ended::Failed)?;
  Ok(descriptor)
}

/// Fills `buffer` from the non-blocking `connection`, waiting on its readiness; the client's closing before it is full
/// is a message cut short, refused `Malformed`.
async fn read_exact(connection: BorrowedFd<'_>, buffer: &mut [u8]) -> Result<(), Ended> {
  let mut filled = 0usize;
  while filled < buffer.len() {
    let Some(room) = buffer.get_mut(filled..) else {
      return Err(Ended::Failed);
    };
    match slates_ipc::stream::recv(connection, room) {
      Ok(0) => return Err(Ended::Refused(RouteRefusal::Malformed)),
      Ok(read) => filled = filled.saturating_add(read),
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
        slates_rt::readiness::readable(connection.as_raw_fd())
          .await
          .map_err(|_| Ended::Failed)?;
      }
      Err(_) => return Err(Ended::Failed),
    }
  }
  Ok(())
}

/// Writes all of `bytes` to the non-blocking `connection`, waiting on its writability.
async fn write_all(connection: BorrowedFd<'_>, mut bytes: &[u8]) -> Result<(), Ended> {
  while !bytes.is_empty() {
    match slates_ipc::stream::send(connection, bytes) {
      Ok(0) => return Err(Ended::Failed),
      Ok(sent) => bytes = bytes.get(sent..).unwrap_or(&[]),
      Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {
        slates_rt::readiness::writable(connection.as_raw_fd())
          .await
          .map_err(|_| Ended::Failed)?;
      }
      Err(_) => return Err(Ended::Failed),
    }
  }
  Ok(())
}

/// Tells the client on `connection` that its claim was refused, by the kind's one byte. A courtesy only: the end
/// that follows tells the client the same, so a byte the socket will not take is not sent again.
fn refuse(connection: BorrowedFd<'_>, refusal: RouteRefusal) {
  if let Some(kind) = slates_ipc::route::refusal_kind(refusal) {
    // The close after this tells the client its claim ended whether or not the byte goes out.
    let _courtesy = slates_ipc::stream::send(connection, &[kind]);
  }
}
