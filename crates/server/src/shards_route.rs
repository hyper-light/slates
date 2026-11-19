//! The daemon's side of the socket route on macOS (§4.6; A-161, A-173). The protocol, its keys and its evidence are
//! `slates_ipc::route`'s; this module keeps the offers, listens, and runs the exchanges.
//!
//! **Offers.** An attach in the Shards form on macOS keeps its stream end as an offer on the volume's owner shard, with
//! the device's end of the stream's resync channel beside it when one was made (A-339) and a fresh 256-bit secret, for
//! the client that asked for it, and replies `ShardsOffered` with the secret (`crate::shards::attach_verb`). A
//! direct map keeps the read-only descriptor of its copy as an offer the same way and replies `MapOffered`
//! (`crate::direct_map`). Each kind has its own table, so a mapping and an attachment never meet under one id, and a
//! claim names its kind (a version-2 hello). Both ids carry their owner partition in their high 16 bits, which is how
//! an exchange finds the shard that holds the offer.
//! The offer is claimed once: an exchange asks the owner shard to check the claim's proof and hand over the end and its
//! resync end ([`take`]), so the secret never leaves the shard that made it.
//!
//! **An offer waits for its claim while the client it is for lives (A-492).** It goes when it is claimed, when what it
//! offers ends first (a stream's attachment, a map's copy let go: [`withdraw`]), or when the reaper retires its client on
//! every shard ([`let_go_of_client`], `crate::reap`), which closes the ends: a stream then sees its peer gone and ends its
//! attachment, as every stream does. Until A-492 an offer lived two liveness budgets on a timer, so a live client slower
//! than that to claim (a loaded machine) found no offer, while a client that died before its claim held nothing a timer
//! did not.
//!
//! **The listener and its exchanges.** The listener is bound when the daemon starts and served by one task on the
//! control shard, which accepts each connection without blocking and runs one exchange task for it there. Exchanges
//! are bounded by the shard's client seats (`clients_per_shard`): past them a connection is refused `Busy`. The client's
//! part of each, and the wait for its close once it has the ends, last while its peer's connection lives (A-492): a
//! peer that dies or closes ends the exchange at once, by its end of the connection, and a peer alive and slow, however
//! slow, is served, as a client's claim waits while its daemon lives (A-186, A-443). The socket is the user's alone
//! (`slates_ipc::route`: owner-only, in the user's own temporary directory), so a peer that holds a seat open is one of
//! the user's own processes, holding its own seat until it closes it or exits. The owner shard's take is waited for in
//! that shard's own time (A-440). Every refusal is counted by its kind, and every claim by its success, which is the
//! route's non-vacuity counter.

use std::collections::BTreeMap;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};

use slates_ipc::error::RouteRefusal;
use slates_ipc::route::{
  Exchange, HELLO_BYTES, Hello, KIND_CHALLENGE, Listener, NONCE_BYTES, OfferKind, SEALED_BYTES,
  SECRET_BYTES,
};
use slates_rt::futures;

use crate::state::{self, ShardState};

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
/// Format: the refusal-ledger name of an exchange the daemon could not carry through (a socket error, no random
/// nonce, the owner shard unreachable).
const FAILED: &str = "shards.route_failed";
/// Format: the refusal-ledger name of an offer let go unclaimed because the reaper retired the client it was for
/// ([`let_go_of_client`], A-492): the non-vacuity counter of an offer's life ending with its client's.
const CLIENT_RETIRED: &str = "shards.route_client_retired";

/// A descriptor offered on the route, a stream end or a map's copy: claimable once, with `secret`, while `client` (the
/// client that asked for it, who alone has the secret) lives; a stream's with the device's end of its resync channel
/// beside it, when one was made (A-339).
pub(crate) struct Offer {
  secret: [u8; SECRET_BYTES],
  client: u32,
  descriptor: OwnedFd,
  resync: Option<OwnedFd>,
}

/// What a claim takes from an offer: the offered descriptor, and a stream's resync end beside it.
pub(crate) struct Taken {
  descriptor: OwnedFd,
  resync: Option<OwnedFd>,
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

/// Keeps `descriptor`, with `resync` beside it for a stream whose resync channel was made (A-339), as the `kind` offer
/// `id` for `client`, claimable with `secret` until it is claimed, withdrawn ([`withdraw`]) or let go with its client
/// ([`let_go_of_client`]): no clock ends it (A-492).
pub(crate) fn offer(
  s: &mut ShardState,
  (kind, id): (OfferKind, u64),
  client: u32,
  secret: [u8; SECRET_BYTES],
  (descriptor, resync): (OwnedFd, Option<OwnedFd>),
) {
  offers_of(s, kind).insert(
    id,
    Offer {
      secret,
      client,
      descriptor,
      resync,
    },
  );
}

/// Withdraws the `kind` offer `id` if it is still unclaimed, its descriptor closed now: what an attachment that ended
/// and a map's copy let go before their claim do, so no offer keeps an object alive past what it offers. Whether there
/// was one.
pub(crate) fn withdraw(s: &mut ShardState, (kind, id): (OfferKind, u64)) -> bool {
  offers_of(s, kind).remove(&id).is_some()
}

/// Lets go of every offer this shard keeps for `client`, unclaimed, its descriptors closed (A-492): the reaper retires a
/// dead client on every shard (`crate::reap`), and no one else holds the secret an offer is claimed with, so its claim
/// will never come. A stream's offer held the end the stream's other side waits on, so the stream then sees its peer
/// gone and ends its attachment, as every stream does. Each let go is counted (`shards.route_client_retired`); how many.
pub(crate) fn let_go_of_client(s: &mut ShardState, client: u32) -> usize {
  let mut let_go = 0usize;
  for kind in [OfferKind::Stream, OfferKind::Map] {
    let offers = offers_of(s, kind);
    let before = offers.len();
    offers.retain(|_, offer| offer.client != client);
    let_go = let_go.saturating_add(before.saturating_sub(offers.len()));
  }
  if let_go > 0 {
    s.count(CLIENT_RETIRED, u64::try_from(let_go).unwrap_or(u64::MAX));
  }
  let_go
}

/// Hands over the descriptors of the offer `hello` names to the claim whose `proof` opens under the offer's secret with
/// these nonces, retiring the offer, with the daemon's confirmation to send beside them; or the refusal: no offer (none
/// made, claimed, withdrawn, or let go with its client), a proof that does not hold. Run on the shard that holds the
/// offer, where its secret is.
pub(crate) fn take(
  s: &mut ShardState,
  hello: Hello,
  daemon_nonce: [u8; NONCE_BYTES],
  proof: [u8; SEALED_BYTES],
) -> Result<([u8; SEALED_BYTES], Taken), RouteRefusal> {
  let offers = offers_of(s, hello.kind);
  let Some(offer) = offers.get(&hello.id) else {
    return Err(RouteRefusal::NoOffer);
  };
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
  Ok((
    confirmation,
    Taken {
      descriptor: offer.descriptor,
      resync: offer.resync,
    },
  ))
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

/// One exchange on `connection`: the client's part (its hello and its proof) while its peer's connection lives, the
/// owner shard's take in that shard's own time ([`take_from_owner`], A-440), and the confirmation sent with the ends; a
/// refusal is sent as its kind byte, and every ending is counted; the connection closes when the task ends.
///
/// **Each wait on the client lasts while its peer's connection does (A-492).** A peer that dies has its end closed by
/// its kernel, which a read answers at once as the end of the stream; one that closes does the same; so neither needs a
/// clock to be noticed, and a peer alive and slow is served however slow it is. Until A-492 each wait was one liveness
/// budget on the wall, so a live client slower than that, a loaded machine's, was dropped mid-claim (`TimedOut`) and its
/// offer, already retired by nothing, stayed for a claim it no longer made.
///
/// **A sent end is kept until the client has its own.** XNU flushes a Unix socket whose every reference is inside a
/// message in flight when its collector of such descriptors runs (`unp_gc`): the socket's receive buffer is emptied and
/// its reads answer end of file. So the end, and a stream's resync end beside it (A-339), once sent, stay open here
/// until the client closes the route connection, which it does after its receive has returned, and only then are
/// closed: each always has a reference outside the message. Closed at once, an end arrived flushed in 4 of 20 claims at
/// load 35-47, and in up to 40 of 48 with another thread opening and closing sockets
/// (`docs/bugs/2026-10-24-a-stream-end-arrived-flushed-by-the-kernel-in-flight.md`).
async fn exchange(connection: OwnedFd) {
  let ended = match claim(connection.as_fd()).await {
    Ok(claim) => match hand_over(connection.as_fd(), claim).await {
      Ok(sent) => {
        // The client's close, or its process's end, says its receive has returned; the ends are closed only after it.
        client_closes(connection.as_fd()).await;
        drop(sent);
        None
      }
      Err(ended) => Some(ended),
    },
    Err(ended) => Some(ended),
  };
  if let Some(Ended::Refused(refusal)) = &ended {
    refuse(connection.as_fd(), *refusal);
  }
  let _ = state::with_state_counted(|s| {
    s.route_exchanges = s.route_exchanges.saturating_sub(1);
    let kind = match ended {
      None => CLAIMED,
      Some(Ended::Refused(refusal)) => refusal_name(refusal),
      Some(Ended::Failed) => FAILED,
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

/// What a client claims with: its hello, the daemon's nonce it was challenged with, and its proof.
struct Claim {
  hello: Hello,
  daemon_nonce: [u8; NONCE_BYTES],
  proof: [u8; SEALED_BYTES],
}

/// The client's part of the exchange (`slates_ipc::route`'s module doc): the hello, the daemon's nonce, the proof (with
/// nothing after it).
async fn claim(connection: BorrowedFd<'_>) -> Result<Claim, Ended> {
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
  Ok(Claim {
    hello,
    daemon_nonce,
    proof,
  })
}

/// The daemon's part: the owner shard's verdict ([`take_from_owner`]), then the confirmation with the end (and a
/// stream's resync end beside it); the ends returned, still held, for the caller to close once the client has its own.
async fn hand_over(connection: BorrowedFd<'_>, claim: Claim) -> Result<Taken, Ended> {
  let Claim {
    hello,
    daemon_nonce,
    proof,
  } = claim;
  let route = state::with_state(|s| {
    // An attachment and a mapping alike carry their owner partition in their high 16 bits.
    let partition = crate::verbs::owner_of_attachment(hello.id);
    crate::verbs::shard_of_partition(s, partition).map(|owner| (s.shard, owner))
  })
  .flatten();
  let Some((origin, owner)) = route else {
    return Err(Ended::Refused(RouteRefusal::NoOffer));
  };
  let (confirmation, taken) = take_from_owner(origin, owner, hello, daemon_nonce, proof).await?;
  slates_ipc::route::send_confirmation(
    connection,
    &confirmation,
    taken.descriptor.as_fd(),
    taken.resync.as_ref().map(AsFd::as_fd),
  )
  .map_err(|_| Ended::Failed)?;
  Ok(taken)
}

/// The owner shard's verdict on a claim ([`take`]), waited for in that shard's own time (A-440; R3's class A): judged by
/// the shard's progress over the liveness window, as every cross-shard wait in its target's own time is
/// (`crate::xshard::within_shard_time`, A-65's rule), never by a wait of the exchange's own (A-492). The take retires
/// the offer and hands its ends over, once: it was waited one window on the wall, so a busy owner shard took the offer
/// after the exchange had given up, the ends were dropped with the late answer, and the client, its claim failed, found
/// no offer to claim again. Only a shard that stopped, or ran its budget without answering, fails it.
async fn take_from_owner(
  origin: u16,
  owner: u16,
  hello: Hello,
  daemon_nonce: [u8; NONCE_BYTES],
  proof: [u8; SEALED_BYTES],
) -> Result<([u8; SEALED_BYTES], Taken), Ended> {
  let taken = crate::xshard::call_in_shard_time(
    origin,
    owner,
    move |s| take(s, hello, daemon_nonce, proof),
    crate::daemon::LIVENESS_BUDGET_NS,
  )
  .await;
  match taken {
    Some(Ok(taken)) => Ok(taken),
    Some(Err(refusal)) => Err(Ended::Refused(refusal)),
    None => Err(Ended::Failed),
  }
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

#[cfg(test)]
mod tests {
  use super::*;

  /// Shape: the window the owner shard's take is judged by ([`take_from_owner`]): the liveness window every wait in its
  /// target's own time starts from.
  const TAKE_WINDOW_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
  /// Shape: the owner shard's one busy poll in the take test: a window and a half of wall time, past what a wall wait of
  /// one window allows.
  const BUSY_NS: u64 = TAKE_WINDOW_NS + TAKE_WINDOW_NS / 2;
  /// Shape: how long the test thread waits for an answer from the daemon's tasks: four busy polls.
  const ANSWER_WAIT_NS: u64 = 4 * BUSY_NS;
  /// Shape: the fixed life an offer had until A-492, two liveness budgets: the life test takes the offer past it.
  const OLD_OFFER_LIFE_NS: u64 = 2 * crate::daemon::LIVENESS_BUDGET_NS;
  /// Shape: when the life test takes the offer: past the old life by half a liveness budget, the old timer's own wake on a
  /// loaded machine.
  const PAST_THE_OLD_LIFE_NS: u64 = OLD_OFFER_LIFE_NS + crate::daemon::LIVENESS_BUDGET_NS / 2;
  /// Shape: the fixed bound an exchange's client part had until A-492, one liveness budget.
  const OLD_EXCHANGE_NS: u64 = crate::daemon::LIVENESS_BUDGET_NS;
  /// Shape: how long the slow client waits before its hello: past the old bound by half a liveness budget.
  const SLOWER_THAN_THE_OLD_BOUND_NS: u64 = OLD_EXCHANGE_NS + crate::daemon::LIVENESS_BUDGET_NS / 2;
  /// Shape: the client the tests' offers are for.
  const CLIENT: u32 = 7;
  /// Shape: another client, whose offer the first one's retirement must leave alone.
  const OTHER_CLIENT: u32 = 8;
  /// Shape: offer ids on a one-shard daemon, partition 0 in their high 16 bits: one for each offer a test makes.
  const FIRST_ID: u64 = 1;
  /// Shape: see [`FIRST_ID`].
  const SECOND_ID: u64 = 2;
  /// Shape: the offer's secret, a byte repeated: any value does, since the test makes the proof under it.
  const SECRET: [u8; SECRET_BYTES] = [7; SECRET_BYTES];
  /// Shape: the client's nonce, a byte repeated: any value does.
  const CLIENT_NONCE: [u8; NONCE_BYTES] = [1; NONCE_BYTES];
  /// Shape: the daemon's nonce, a byte repeated: any value does, since the test hands it to the take itself.
  const DAEMON_NONCE: [u8; NONCE_BYTES] = [2; NONCE_BYTES];
  /// Shape: another exchange's daemon nonce, another byte repeated: any value but [`DAEMON_NONCE`]'s does.
  const OTHER_NONCE: [u8; NONCE_BYTES] = [3; NONCE_BYTES];

  /// Keeps `descriptor` as the `key` offer for `client` under [`SECRET`], as an attach or a map does.
  fn offered(s: &mut ShardState, key: (OfferKind, u64), client: u32, descriptor: OwnedFd) {
    offer(s, key, client, SECRET, (descriptor, None));
  }

  /// The hello of a claim of the `kind` offer `id` with [`CLIENT_NONCE`].
  fn hello_for(kind: OfferKind, id: u64) -> Hello {
    Hello {
      kind,
      id,
      client_nonce: CLIENT_NONCE,
    }
  }

  /// The proof of a claim of the `kind` offer `id` under [`SECRET`] with [`CLIENT_NONCE`] and `daemon_nonce`.
  fn proof_of(kind: OfferKind, id: u64, daemon_nonce: &[u8; NONCE_BYTES]) -> [u8; SEALED_BYTES] {
    Exchange::new(&SECRET, (kind, id), &CLIENT_NONCE, daemon_nonce)
      .unwrap()
      .proof()
      .unwrap()
  }

  /// Fills `buffer` from the blocking `end`: how many bytes came before the peer closed or the read timed out.
  fn read_up_to(end: &OwnedFd, buffer: &mut [u8]) -> usize {
    let mut filled = 0;
    while filled < buffer.len() {
      match rustix::net::recv(end, &mut buffer[filled..], rustix::net::RecvFlags::empty()) {
        Ok((0, _)) | Err(_) => break,
        Ok((read, _)) => filled += read,
      }
    }
    filled
  }

  /// R3's class A (A-440). Do: on a two-shard daemon, offer a map's descriptor on the second shard under a known secret,
  /// hold that shard in one poll for a window and a half of wall time on its CPU a fifth of it (a busy or starved shard),
  /// and from the first shard take the offer with a proof made under the secret. Expect: the take answered with the
  /// offer, after more than one window of waiting, and the offer gone from its shard. The take waited one window on the
  /// wall, so the exchange failed with the owner alive and working, and the owner's late take retired the offer and
  /// dropped its descriptor with the answer, leaving the client nothing to claim again.
  #[test]
  fn a_claims_take_waits_for_an_owner_shard_that_works_past_its_budget() {
    let daemon = crate::daemon::audit_daemon_of(2);
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |state| {
      let origin = state.shard;
      let (partition, owner) = state
        .shards
        .iter()
        .copied()
        .enumerate()
        .find(|(_, shard)| *shard != origin)
        .unwrap();
      // An offer's id carries its owner partition in its high 16 bits.
      let id = (u64::try_from(partition).unwrap() << 48) | FIRST_ID;
      let hello = hello_for(OfferKind::Map, id);
      let proof = proof_of(OfferKind::Map, id, &DAEMON_NONCE);
      let task = futures::spawn(async move {
        let made = crate::xshard::call_in_shard_time(
          origin,
          owner,
          move |s| {
            let (descriptor, _peer) = slates_ipc::stream::pair().unwrap();
            offered(s, (OfferKind::Map, id), CLIENT, descriptor);
          },
          TAKE_WINDOW_NS,
        )
        .await;
        // The busy poll goes first on the owner shard's channel, so the take waits behind it.
        let busy = crate::xshard::call_on(origin, owner, || {
          crate::daemon::hold_busy(BUSY_NS);
          Some(())
        });
        drop(busy);
        let started = std::time::Instant::now();
        let taken = match take_from_owner(origin, owner, hello, DAEMON_NONCE, proof).await {
          Ok(_) => "taken".to_owned(),
          Err(Ended::Refused(refusal)) => format!("refused {refusal:?}"),
          Err(Ended::Failed) => "failed".to_owned(),
        };
        let waited = started.elapsed();
        let left = crate::xshard::call_in_shard_time(
          origin,
          owner,
          move |s| s.map_offers.contains_key(&id),
          TAKE_WINDOW_NS,
        )
        .await;
        let _ = sent.send((made.is_some(), taken, waited, left));
      })
      .unwrap();
      futures::detach(task).unwrap();
    });
    let answer = answered.recv_timeout(std::time::Duration::from_nanos(ANSWER_WAIT_NS));
    daemon.stop();
    let (made, taken, waited, left) = answer.expect("the take answered");
    println!(
      "a take behind a busy poll of {BUSY_NS} ns: {taken} after {waited:?}; the offer left there: {left:?}"
    );
    assert!(made, "the offer was made");
    assert_eq!(
      taken, "taken",
      "the take on an owner shard alive and working was not answered with the offer (after {waited:?})"
    );
    assert!(
      waited > std::time::Duration::from_nanos(TAKE_WINDOW_NS),
      "the take ended within one window ({waited:?}), before its owner shard's busy poll did"
    );
    assert_eq!(left, Some(false), "the take retired the offer");
  }

  /// What the lost-answer take test saw: the first ask's confirmation, whether the stream's other side was still open
  /// after that answer was lost, the second ask's confirmation, the byte read through the end the second ask answered,
  /// and another exchange's take.
  struct TakenTwice {
    first: Result<[u8; SEALED_BYTES], String>,
    peer_open: bool,
    again: Result<[u8; SEALED_BYTES], String>,
    through: Option<u8>,
    other: Result<(), String>,
  }

  /// A-512 (A-488's sibling, goal 85). Do: on a daemon, offer a stream end for a client; take it with a claim's proof, as
  /// an exchange's first ask, and drop what the take answered, as a lost answer is dropped when its owner shard stopped
  /// for a window (`xshard::call_in_shard_time` forgets the call, and the shard runs it once it resumes); take it again
  /// with the same claim, as the exchange's second ask; write a byte on the stream's other side and read it through the
  /// end the second ask answered; then take the offer with another exchange's nonce and proof. Expect: the stream's other
  /// side still open after the lost answer; the second ask answered the same confirmation and an end of the same stream
  /// (the byte comes through); the other exchange refused `NoOffer`, so the ends are handed once. Before, the first take
  /// retired the offer and moved its ends into the answer, whose loss closed them: the stream's other side read its end,
  /// the second ask found no offer, and the client nothing to claim again.
  #[test]
  fn a_take_whose_answer_was_lost_is_answered_again_with_the_same_ends_and_never_handed_twice() {
    let daemon = crate::daemon::audit_daemon();
    let saw = crate::daemon::observe_first(&daemon, |s| {
      let (end, peer) = slates_ipc::stream::pair().unwrap();
      offered(s, (OfferKind::Stream, FIRST_ID), CLIENT, end);
      let hello = hello_for(OfferKind::Stream, FIRST_ID);
      let proof = proof_of(OfferKind::Stream, FIRST_ID, &DAEMON_NONCE);
      let first = take(s, hello, DAEMON_NONCE, proof)
        .map(|(confirmation, _lost)| confirmation)
        .map_err(|refusal| format!("{refusal:?}"));
      rustix::io::ioctl_fionbio(&peer, true).unwrap();
      let mut byte = [0u8; 1];
      let peer_open = matches!(
        rustix::net::recv(&peer, &mut byte, rustix::net::RecvFlags::empty()),
        Err(rustix::io::Errno::AGAIN)
      );
      let again = take(s, hello, DAEMON_NONCE, proof);
      let through = again.as_ref().ok().and_then(|(_, taken)| {
        rustix::net::send(&peer, b"x", rustix::net::SendFlags::empty()).ok()?;
        let mut read = [0u8; 1];
        match rustix::net::recv(&taken.descriptor, &mut read, rustix::net::RecvFlags::empty()) {
          Ok((1, _)) => Some(read[0]),
          _ => None,
        }
      });
      let other = take(
        s,
        hello,
        OTHER_NONCE,
        proof_of(OfferKind::Stream, FIRST_ID, &OTHER_NONCE),
      )
      .map(|_| ())
      .map_err(|refusal| format!("{refusal:?}"));
      TakenTwice {
        first,
        peer_open,
        again: again
          .map(|(confirmation, _)| confirmation)
          .map_err(|refusal| format!("{refusal:?}")),
        through,
        other,
      }
    });
    daemon.stop();
    println!(
      "a take asked again after a lost answer: first {}, the stream's other side open {}, again {}, through {:?}, \
       another exchange {:?}",
      saw.first.is_ok(),
      saw.peer_open,
      saw.again.is_ok(),
      saw.through,
      saw.other
    );
    assert!(saw.first.is_ok(), "the first ask took the offer: {:?}", saw.first);
    assert!(
      saw.peer_open,
      "the lost answer closed the stream's end: the ends were lost with it"
    );
    assert_eq!(
      saw.again, saw.first,
      "the second ask of the same claim was not answered that claim"
    );
    assert_eq!(
      saw.through,
      Some(b'x'),
      "the second ask's end is not the offered stream's"
    );
    assert_eq!(
      saw.other,
      Err("NoOffer".to_owned()),
      "another exchange took an offer this exchange holds: its ends handed twice"
    );
  }

  /// A-492 (R3's class C; §4.6 the socket route): an offer waits for its claim while its client lives, never for a fixed
  /// life. Do: on a daemon, offer a map's descriptor for a client that lives, wait past the life offers had until A-492
  /// (two liveness budgets) and half a budget more, then take it with a proof made under its secret. Expect: taken, the
  /// offer gone after. Before, the offer's timer let it go at two budgets (`shards.route_expired`) and the take was
  /// refused `NoOffer`: a live client slower than that to claim, a loaded machine's, found nothing.
  #[test]
  fn an_offer_waits_for_its_live_clients_claim_past_any_fixed_life() {
    let daemon = crate::daemon::audit_daemon();
    crate::daemon::observe_first(&daemon, |s| {
      let (descriptor, _peer) = slates_ipc::stream::pair().unwrap();
      offered(s, (OfferKind::Map, FIRST_ID), CLIENT, descriptor);
    });
    // The test thread waits past the old life; no runtime wheel serves this thread (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(std::time::Duration::from_nanos(PAST_THE_OLD_LIFE_NS));
    let (taken, left) = crate::daemon::observe_first(&daemon, |s| {
      let proof = proof_of(OfferKind::Map, FIRST_ID, &DAEMON_NONCE);
      let taken = take(s, hello_for(OfferKind::Map, FIRST_ID), DAEMON_NONCE, proof)
        .map(|_| ())
        .map_err(|refusal| format!("{refusal:?}"));
      (taken, s.map_offers.contains_key(&FIRST_ID))
    });
    daemon.stop();
    assert_eq!(
      taken,
      Ok(()),
      "an offer whose client lives was not claimable past the old fixed life"
    );
    assert!(!left, "the take retired the offer");
  }

  /// A-492 (R3's class C; §4.6 the socket route): an exchange waits on its client while the client's connection lives.
  /// Do: on a daemon, offer a map's descriptor for a client and run an exchange on one end of a socket pair, the test
  /// thread its client on the other, which waits past the bound the client's part had until A-492 (one liveness budget)
  /// and half a budget more before its hello, then answers the challenge with a proof made under the secret. Expect: the
  /// challenge, then the confirmation. Before, the exchange gave its client one budget and dropped it
  /// (`shards.route_timed_out`), so the slow client's hello met a closed connection.
  #[test]
  fn an_exchange_serves_a_live_client_slower_than_any_fixed_bound() {
    let daemon = crate::daemon::audit_daemon();
    let client_end = crate::daemon::observe_first(&daemon, |s| {
      let (descriptor, _peer) = slates_ipc::stream::pair().unwrap();
      offered(s, (OfferKind::Map, FIRST_ID), CLIENT, descriptor);
      let (daemon_end, client_end) = slates_ipc::stream::pair().unwrap();
      s.route_exchanges = s.route_exchanges.saturating_add(1);
      futures::detach(futures::spawn(exchange(daemon_end)).unwrap()).unwrap();
      client_end
    });
    rustix::net::sockopt::set_socket_timeout(
      &client_end,
      rustix::net::sockopt::Timeout::Recv,
      Some(std::time::Duration::from_nanos(ANSWER_WAIT_NS)),
    )
    .unwrap();
    rustix::net::sockopt::set_socket_nosigpipe(&client_end, true).unwrap();
    // The test thread is a client slower than the old bound; no runtime wheel serves this thread (D-9 allows a test).
    #[allow(clippy::disallowed_methods)]
    std::thread::sleep(std::time::Duration::from_nanos(
      SLOWER_THAN_THE_OLD_BOUND_NS,
    ));
    let hello_sent = rustix::net::send(
      &client_end,
      &hello_for(OfferKind::Map, FIRST_ID).encode(),
      rustix::net::SendFlags::empty(),
    );
    let mut challenge = [0u8; 1 + NONCE_BYTES];
    let challenged = read_up_to(&client_end, &mut challenge);
    let mut confirmation = [0u8; 1 + SEALED_BYTES];
    let mut confirmed = 0;
    if challenged == challenge.len() && challenge[0] == KIND_CHALLENGE {
      let daemon_nonce: [u8; NONCE_BYTES] = challenge[1..].try_into().unwrap();
      let proof = proof_of(OfferKind::Map, FIRST_ID, &daemon_nonce);
      let _proof_sent = rustix::net::send(&client_end, &proof, rustix::net::SendFlags::empty());
      confirmed = read_up_to(&client_end, &mut confirmation);
    }
    drop(client_end);
    daemon.stop();
    assert!(hello_sent.is_ok(), "the hello was sent: {hello_sent:?}");
    assert_eq!(
      (challenged, challenge[0]),
      (challenge.len(), KIND_CHALLENGE),
      "the exchange no longer waited for a client slower than the old bound"
    );
    assert!(
      confirmed >= 1 && confirmation[0] == slates_ipc::route::KIND_CONFIRM,
      "the slow client's claim was not confirmed ({confirmed} bytes, kind {})",
      confirmation[0]
    );
  }

  /// A-492 (R3's class C; §4.6 the socket route; §4.7 the reaper): an offer's life ends with its client's. Do: on a daemon,
  /// offer a stream end for one client and another for a second client, then retire the first as the reaper retires a
  /// dead client on every shard (`crate::reap::remove_on_owners`). Expect: the first client's offer let go and its end
  /// closed (the stream's other side reads the end), counted `shards.route_client_retired`; the second client's offer
  /// kept, its end open. Before, only the offer's two-budget timer let it go, a client alive or dead.
  #[test]
  fn a_retired_clients_offers_are_let_go_and_their_ends_closed() {
    let daemon = crate::daemon::audit_daemon();
    let (sent, answered) = std::sync::mpsc::sync_channel(1);
    crate::daemon::observe_first(&daemon, move |s| {
      let origin = s.shard;
      let batch = s.config.runtime.batch.max(1);
      let (retired_end, retired_peer) = slates_ipc::stream::pair().unwrap();
      let (kept_end, kept_peer) = slates_ipc::stream::pair().unwrap();
      offered(s, (OfferKind::Stream, FIRST_ID), CLIENT, retired_end);
      offered(s, (OfferKind::Stream, SECOND_ID), OTHER_CLIENT, kept_end);
      let task = futures::spawn(async move {
        let retired = crate::reap::remove_on_owners(
          origin,
          &[origin],
          batch,
          CLIENT,
          crate::daemon::LIVENESS_BUDGET_NS,
        )
        .await;
        let left = state::with_state(|s| {
          (
            s.stream_offers.contains_key(&FIRST_ID),
            s.stream_offers.contains_key(&SECOND_ID),
            s.refusals
              .get("shards.route_client_retired")
              .copied()
              .unwrap_or(0),
          )
        });
        let _ = sent.send((retired.is_ok(), left, retired_peer, kept_peer));
      })
      .unwrap();
      futures::detach(task).unwrap();
    });
    let answer = answered.recv_timeout(std::time::Duration::from_nanos(ANSWER_WAIT_NS));
    // Read before the daemon stops, which closes every end it keeps.
    let ends = answer.as_ref().ok().map(|(_, _, retired_peer, kept_peer)| {
      for peer in [retired_peer, kept_peer] {
        rustix::io::ioctl_fionbio(peer, true).unwrap();
      }
      let mut byte = [0u8; 1];
      let retired_closed = matches!(
        rustix::net::recv(retired_peer, &mut byte, rustix::net::RecvFlags::empty()),
        Ok((0, _))
      );
      let kept_open = matches!(
        rustix::net::recv(kept_peer, &mut byte, rustix::net::RecvFlags::empty()),
        Err(rustix::io::Errno::AGAIN)
      );
      (retired_closed, kept_open)
    });
    daemon.stop();
    let (retired, left, _, _) = answer.expect("the retirement answered");
    println!(
      "a client retired with its offers: retired {retired}, offers (theirs, the other's, counted) {left:?}, ends {ends:?}"
    );
    assert!(retired, "the reaper's retirement on the shard went through");
    assert_eq!(
      left,
      Some((false, true, 1)),
      "the retired client's offer let go and counted, the other client's kept"
    );
    assert_eq!(
      ends,
      Some((true, true)),
      "the retired client's end closed, the other client's open"
    );
  }
}
