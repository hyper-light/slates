//! Content homed by its identity across a node's shards (A-724 piece 5; §4.17.5 "Giant repositories"; main's ruling of
//! 2026-11-20 on piece 6; R8).
//!
//! **Why.** A repository's owner shard keeps its register, its ordered landing, its indexes' tails and fences and its
//! hot set (§4.10's family placement). Everything else of a repository need not live there: a monorepo's content past
//! one shard's budget would otherwise bound the repository by that shard, and its CPU by that shard's core. Content
//! outside the owner's hot set is homed by its identity instead: the content id ranks the node's shards by rendezvous
//! (`slates_db::register::rendezvous_ranked_by_key`, the one weight every placement in slates ranks by), and the first is
//! its home. The fleet's half (piece 6) ranks the region's members by the same weight, holding f+1 homes; within one
//! node one home, since every shard of a node shares its RAM and fails with it.
//!
//! **What is homed.** A window: content of at most one window (`ContentStore::put`), named by its plain BLAKE3, so a
//! sender knows its home before it sends. An object longer than a window is homed as its windows, each by its own
//! identity, so one large object spreads over the node's shards. A home keeps a window in its shard's one content store
//! (A-662), deduplicated by identity with what its own repositories hold, charged to its shard's memory budget as
//! retention as everything that store holds is (the store lent the room its shard could admit for the step, then
//! settled), held once a sender and freed with its last holder.
//!
//! **How it is reached.** By a cross-shard call judged in the home's own time (`crate::xshard::call_in_shard_time`,
//! A-432), the caller's budget its bound; a call to the caller's own shard runs at once, the same code at one shard and
//! many (R8). No catalog is consulted: the identity is the route (§2 ban 12), and no coordination call is made.
//!
//! **Durable before named** (main's ruling; A-727's barrier). A window kept on its home answers a [`HomeTicket`]: its
//! home shard and the generation of the first image of that shard that will hold it. An entry naming the window commits
//! only once [`published`] answers that the home has published that image. The home's image barrier (A-727, w-perf's)
//! tells this module two things on the home's own thread: [`image_begun`] when an image captures the shard's content
//! table (the next image is the one after it), and [`image_published`] when it is durable. The wait runs as a task on
//! the home (`crate::xshard::task_in_shard_time`), so the owner's request awaits it and no step ever waits. One barrier
//! serves every window kept before it: group commit per shard, never a publication per push.
//!
//! **In batches** ([`home_many`], [`read_many`], [`let_go_many`]; agreed with w-perf). A request's windows go to their
//! homes in one cross-shard call a home shard a turn, never one a window: each call works through its items until the
//! home's step quantum (§4.3) is spent, at least one, and answers the rest to be sent in the next turn, so a batch of any
//! size holds no home past a quantum. Answers come back in the request's order.
//!
//! **From the region** ([`serve_remote`], A-724 piece 6's region half). Another member's home exchange
//! (`slates_cluster::homes`) arrives on this node's record session and is served here: kept on the node's own home
//! shard for the window, never in `ContentHold` (w-fed-faults' answer to Q3), only where the configuration ranks this
//! node among the window's homes, and acknowledged only once that shard's image holding it is published.
//!
//! **The healer** ([`heal_region`]). At each configuration change, and only then (§2 ban 10), the control shard walks the
//! windows this node holds for the region a turn a period: each is offered to its homes (an offer of a window a home
//! holds already is idempotent: a home keeps one region hold a window, [`keep_for_region`]), and once every home has
//! answered `Kept` this node lets its own hold go where it is not one of them (`slates_db::identity_homes::heal`: add
//! before remove). Its inputs are the configuration and what the homes answered, never an owner's record (Q3).
//!
//! **Refused** ([`HomeRefused`]): bytes past a window; a home whose content store refused (its budget); a home that did
//! not answer within the budget; an answer naming another identity than the bytes' (never in a correct home).

use slates_db::register::rendezvous_ranked_by_key;
use slates_vcs::{ContentId, ContentRefused, ContentStore};

use crate::state::ShardState;

/// Counter: windows a home kept for a sender. Format: a counter name.
pub(crate) const HOMED: &str = "vcs_store.homes.homed";
/// Counter: windows a home read back for a reader. Format: a counter name.
pub(crate) const READ: &str = "vcs_store.homes.read";
/// Counter: windows a home let go for a holder. Format: a counter name.
pub(crate) const LET_GO: &str = "vcs_store.homes.let_go";

/// Why a window was not homed, read or let go (module docs, "Refused").
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HomeRefused {
  /// The bytes are longer than a window: home each window by its own identity.
  PastWindow,
  /// The home's content store refused (its budget, or a count exhausted).
  Content(ContentRefused),
  /// The home did not answer within the caller's budget.
  Unanswered,
  /// The home named another identity than the bytes': never in a correct home.
  Misnamed,
  /// The home holds no such content.
  NotHeld,
}

/// Where and when a homed window is durable (module docs, "Durable before named"): its home shard, and the generation of
/// the first of that shard's images that holds it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct HomeTicket {
  /// The home shard.
  pub(crate) shard: u16,
  /// The first image generation of the home that holds the window.
  pub(crate) generation: u64,
}

/// A home shard's images as its barrier reports them (module docs, "Durable before named").
#[derive(Debug, Default)]
struct Images {
  /// The generation of the next image to capture the content table: what a window kept now is first held by.
  next: u64,
  /// The newest generation published.
  published: u64,
  /// The waits for a newer publication, woken at each: one an awaiting ticket, so bounded by the requests waiting.
  waiters: Vec<std::task::Waker>,
}

std::thread_local! {
  /// This shard's images, as a home (module docs).
  static IMAGES: std::cell::RefCell<Images> = std::cell::RefCell::new(Images { next: 1, ..Images::default() });
}

/// The home's barrier: an image of `generation` captured this shard's content table, so a window kept from now on is
/// first held by the image after it (module docs). Called on the home's own thread.
pub(crate) fn image_begun(generation: u64) {
  IMAGES.with(|images| {
    if let Ok(mut images) = images.try_borrow_mut() {
      images.next = images.next.max(generation.saturating_add(1));
    }
  });
}

/// The home's barrier: the image of `generation` is durable (module docs); every wait for it or an older one ends.
/// Called on the home's own thread.
pub(crate) fn image_published(generation: u64) {
  let woken = IMAGES.with(|images| {
    images.try_borrow_mut().map_or_else(
      |_| Vec::new(),
      |mut images| {
        images.published = images.published.max(generation);
        core::mem::take(&mut images.waiters)
      },
    )
  });
  for waker in woken {
    waker.wake();
  }
}

/// The ticket of a window kept on this shard now.
fn ticket_here(shard: u16) -> HomeTicket {
  let generation = IMAGES.with(|images| images.try_borrow().map_or(1, |images| images.next));
  HomeTicket { shard, generation }
}

/// Waits, on this shard, until its image of `generation` is published.
struct PublishedHere {
  generation: u64,
}

impl core::future::Future for PublishedHere {
  type Output = bool;

  fn poll(
    self: core::pin::Pin<&mut Self>,
    context: &mut core::task::Context<'_>,
  ) -> core::task::Poll<bool> {
    let generation = self.generation;
    IMAGES.with(|images| match images.try_borrow_mut() {
      Ok(images) if images.published >= generation => core::task::Poll::Ready(true),
      Ok(mut images) => {
        images.waiters.push(context.waker().clone());
        core::task::Poll::Pending
      }
      Err(_) => {
        context.waker().wake_by_ref();
        core::task::Poll::Pending
      }
    })
  }
}

/// Whether the window of `ticket` is durable at its home, asked from shard `origin` (module docs): waits on the home, in
/// its own time up to `budget_ns`, for its image of the ticket's generation; false when that did not come within it.
pub(crate) async fn published(origin: u16, ticket: HomeTicket, budget_ns: u64) -> bool {
  let generation = ticket.generation;
  crate::xshard::task_in_shard_time(
    origin,
    ticket.shard,
    move || PublishedHere { generation },
    budget_ns,
  )
  .await
  .unwrap_or(false)
}

/// The shard of `shards` that is `identity`'s home (module docs): the first by rendezvous over the identity; none for
/// no shards.
pub(crate) fn home_of(shards: &[u16], identity: &ContentId) -> Option<u16> {
  let members: Vec<u64> = shards.iter().map(|shard| u64::from(*shard)).collect();
  rendezvous_ranked_by_key(&members, &identity.0)
    .first()
    .and_then(|member| u16::try_from(*member).ok())
}

/// Keeps the window `bytes` on this shard, its home, held once more for its sender: its identity, and the ticket an
/// entry naming it waits on (module docs, "Durable before named"). The shard's content
/// store is made if it has none (as a first repository makes it), lent the room the shard could admit for the step,
/// and settled after.
pub(crate) fn keep_here(
  state: &mut ShardState,
  bytes: &[u8],
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  if state.vcs_stores.content.is_none() {
    let window = state.store.content.chunk_bytes();
    let made = ContentStore::new(state.vcs_stores.charged, window).map_err(HomeRefused::Content)?;
    state.vcs_stores.content = Some(made);
  }
  let need = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  super::lend(&mut state.store, &mut state.vcs_stores, need);
  let kept = state
    .vcs_stores
    .content
    .as_mut()
    .map(|content| content.put(bytes));
  super::settle(state);
  match kept {
    Some(Ok(identity)) => {
      state.count(HOMED, 1);
      Ok((identity, ticket_here(state.shard)))
    }
    Some(Err(ContentRefused::PastWindow { .. })) => Err(HomeRefused::PastWindow),
    Some(Err(refused)) => Err(HomeRefused::Content(refused)),
    None => Err(HomeRefused::NotHeld),
  }
}

/// The bytes of the window `identity` this shard keeps as its home, if it keeps it.
pub(crate) fn read_here(state: &mut ShardState, identity: &ContentId) -> Option<Vec<u8>> {
  let bytes = state
    .vcs_stores
    .content
    .as_ref()
    .and_then(|content| content.bytes_of(identity))
    .map(std::borrow::Cow::into_owned)?;
  state.count(READ, 1);
  Some(bytes)
}

/// Lets go one holder of the window `identity` this shard keeps as its home; freed with its last, the charge settled.
pub(crate) fn let_go_here(state: &mut ShardState, identity: &ContentId) -> Result<(), HomeRefused> {
  let held = state
    .vcs_stores
    .content
    .as_ref()
    .is_some_and(|content| content.contains(identity));
  if !held {
    return Err(HomeRefused::NotHeld);
  }
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let released = state
    .vcs_stores
    .content
    .as_mut()
    .map(|content| content.release(identity));
  super::settle(state);
  match released {
    Some(Ok(_)) => {
      state.count(LET_GO, 1);
      Ok(())
    }
    Some(Err(refused)) => Err(HomeRefused::Content(refused)),
    None => Err(HomeRefused::NotHeld),
  }
}

/// Homes the window `bytes` from shard `origin` at its home among `shards` (module docs), waiting for the home at most
/// `budget_ns` of its own time: the window's identity, held once more for the caller, and its ticket.
pub(crate) async fn home(
  origin: u16,
  shards: &[u16],
  bytes: Vec<u8>,
  budget_ns: u64,
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  let identity = ContentId::of(&bytes);
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  let kept = crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| keep_here(state, &bytes),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)??;
  if kept.0 == identity {
    Ok(kept)
  } else {
    Err(HomeRefused::Misnamed)
  }
}

/// The bytes of the window `identity` from its home among `shards`, read from shard `origin` within `budget_ns` of
/// the home's own time (module docs): none when it is not homed there or the home did not answer.
pub(crate) async fn read(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget_ns: u64,
) -> Option<Vec<u8>> {
  let target = home_of(shards, &identity)?;
  let bytes = crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| read_here(state, &identity),
    budget_ns,
  )
  .await
  .flatten()?;
  // A home answers the bytes it keeps under the identity, which its store named by their hash; checked again here, so a
  // window read across shards is never other bytes than its name (§4.17.1 item 5's rule on arrival, kept on reading).
  (ContentId::of(&bytes) == identity).then_some(bytes)
}

/// Lets go one holder of the window `identity` at its home among `shards`, from shard `origin` within `budget_ns` of
/// the home's own time.
pub(crate) async fn let_go(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget_ns: u64,
) -> Result<(), HomeRefused> {
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| let_go_here(state, &identity),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)?
}

/// Counter: cross-shard calls the batch forms made, one a home shard a turn (module docs, "In batches"). Format: a
/// counter name.
pub(crate) const BATCH_CALLS: &str = "vcs_store.homes.batch_calls";

/// Runs `work` on each of `items` at its identity's home among `shards` (module docs, "In batches"): one cross-shard
/// call a home shard a turn, each working through its items until the home's step quantum is spent and answering the
/// rest to be sent again, so no call holds a home past a quantum. Each item's answer in `items`' order; none for an item
/// whose home did not answer within `budget_ns` of its own time.
async fn per_home<I, O>(
  origin: u16,
  shards: &[u16],
  items: Vec<(ContentId, I)>,
  budget_ns: u64,
  work: fn(&mut ShardState, &ContentId, I) -> O,
) -> Vec<Option<O>>
where
  I: Send + 'static,
  O: Send + 'static,
{
  let mut answers: Vec<Option<O>> = items.iter().map(|_| None).collect();
  let mut by_home: std::collections::BTreeMap<u16, Vec<(usize, ContentId, I)>> =
    std::collections::BTreeMap::new();
  for (at, (identity, item)) in items.into_iter().enumerate() {
    if let Some(home) = home_of(shards, &identity) {
      by_home.entry(home).or_default().push((at, identity, item));
    }
  }
  for (home, mut pending) in by_home {
    while !pending.is_empty() {
      let batch = core::mem::take(&mut pending);
      let answered = crate::xshard::call_in_shard_time(
        origin,
        home,
        move |state| {
          let quantum_ns = state.config.step_quantum_ns();
          let began = slates_rt::futures::now_ns();
          let mut done = Vec::new();
          let mut rest = Vec::new();
          for (at, identity, item) in batch {
            // At least one item a call, so a turn always progresses.
            if !done.is_empty() && slates_rt::futures::now_ns().saturating_sub(began) >= quantum_ns
            {
              rest.push((at, identity, item));
            } else {
              done.push((at, work(state, &identity, item)));
            }
          }
          state.count(BATCH_CALLS, 1);
          (done, rest)
        },
        budget_ns,
      )
      .await;
      let Some((done, rest)) = answered else {
        break;
      };
      for (at, answer) in done {
        if let Some(slot) = answers.get_mut(at) {
          *slot = Some(answer);
        }
      }
      pending = rest;
      if !pending.is_empty() {
        slates_rt::futures::yield_now().await;
      }
    }
  }
  answers
}

/// [`keep_here`] for one item of a batch: the window's bytes, checked against the identity it was sent under.
fn keep_item(
  state: &mut ShardState,
  identity: &ContentId,
  bytes: Vec<u8>,
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  let kept = keep_here(state, &bytes)?;
  if kept.0 == *identity {
    Ok(kept)
  } else {
    Err(HomeRefused::Misnamed)
  }
}

/// [`read_here`] for one item of a batch.
fn read_item(state: &mut ShardState, identity: &ContentId, (): ()) -> Option<Vec<u8>> {
  read_here(state, identity)
}

/// [`let_go_here`] for one item of a batch.
fn let_go_item(state: &mut ShardState, identity: &ContentId, (): ()) -> Result<(), HomeRefused> {
  let_go_here(state, identity)
}

/// Homes every window of `windows` from shard `origin` (module docs, "In batches"): each one's identity and ticket, in
/// order, or why not (`Unanswered` where its home did not answer within `budget_ns` of its own time).
pub(crate) async fn home_many(
  origin: u16,
  shards: &[u16],
  windows: Vec<Vec<u8>>,
  budget_ns: u64,
) -> Vec<Result<(ContentId, HomeTicket), HomeRefused>> {
  let items: Vec<(ContentId, Vec<u8>)> = windows
    .into_iter()
    .map(|bytes| (ContentId::of(&bytes), bytes))
    .collect();
  per_home(origin, shards, items, budget_ns, keep_item)
    .await
    .into_iter()
    .map(|answer| answer.unwrap_or(Err(HomeRefused::Unanswered)))
    .collect()
}

/// Reads every window of `identities` from its home, from shard `origin` (module docs, "In batches"): each one's bytes,
/// in order, checked against its name, or none.
pub(crate) async fn read_many(
  origin: u16,
  shards: &[u16],
  identities: &[ContentId],
  budget_ns: u64,
) -> Vec<Option<Vec<u8>>> {
  let items: Vec<(ContentId, ())> = identities.iter().map(|identity| (*identity, ())).collect();
  per_home(origin, shards, items, budget_ns, read_item)
    .await
    .into_iter()
    .zip(identities)
    .map(|(answer, identity)| {
      answer
        .flatten()
        .filter(|bytes| ContentId::of(bytes) == *identity)
    })
    .collect()
}

/// Lets go one holder of every window of `identities` at its home, from shard `origin` (module docs, "In batches"):
/// each one's answer, in order.
pub(crate) async fn let_go_many(
  origin: u16,
  shards: &[u16],
  identities: &[ContentId],
  budget_ns: u64,
) -> Vec<Result<(), HomeRefused>> {
  let items: Vec<(ContentId, ())> = identities.iter().map(|identity| (*identity, ())).collect();
  per_home(origin, shards, items, budget_ns, let_go_item)
    .await
    .into_iter()
    .map(|answer| answer.unwrap_or(Err(HomeRefused::Unanswered)))
    .collect()
}

std::thread_local! {
  /// The windows this shard keeps for the region (module docs, "From the region"), one hold each however often the
  /// region offers it: so the healer's offers, sent again at every configuration change, are idempotent. Bounded by the
  /// windows its content store holds.
  static REGION_HELD: std::cell::RefCell<std::collections::BTreeSet<ContentId>> =
    const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// Keeps the window `bytes` on this shard for the region (module docs, "From the region"): a window held for the region
/// already is held no more, only answered; its identity and ticket.
pub(crate) fn keep_for_region(
  state: &mut ShardState,
  bytes: &[u8],
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  let identity = ContentId::of(bytes);
  let held = REGION_HELD.with(|held| held.try_borrow().is_ok_and(|held| held.contains(&identity)));
  if held
    && state
      .vcs_stores
      .content
      .as_ref()
      .is_some_and(|content| content.contains(&identity))
  {
    return Ok((identity, ticket_here(state.shard)));
  }
  let kept = keep_here(state, bytes)?;
  REGION_HELD.with(|held| {
    if let Ok(mut held) = held.try_borrow_mut() {
      held.insert(kept.0);
    }
  });
  Ok(kept)
}

/// Lets go this shard's region hold of the window `identity` (module docs, "From the region"): `NotHeld` where the region
/// holds none here.
pub(crate) fn let_go_for_region(
  state: &mut ShardState,
  identity: &ContentId,
) -> Result<(), HomeRefused> {
  let held = REGION_HELD.with(|held| {
    held
      .try_borrow_mut()
      .is_ok_and(|mut held| held.remove(identity))
  });
  if !held {
    return Err(HomeRefused::NotHeld);
  }
  let_go_here(state, identity)
}

/// The windows this shard holds for the region, in identity order: what the healer walks (module docs, "The healer").
pub(crate) fn region_held_here() -> Vec<ContentId> {
  REGION_HELD.with(|held| {
    held
      .try_borrow()
      .map(|held| held.iter().copied().collect())
      .unwrap_or_default()
  })
}

/// Counter: home exchanges a member served from the region (module docs, "From the region"). Format: a counter name.
pub(crate) const REMOTE_SERVED: &str = "vcs_store.homes.remote_served";

/// Serves one home exchange from another member of the region on shard `origin` (module docs, "From the region";
/// `slates_cluster::homes`): a put kept only where the configuration this node knows ranks it among the window's homes
/// and its bytes hash to their name, on the node's own home shard for the window, and answered `Kept` only once that
/// shard's image holding it is published ([`published`], A-727's barrier); a read answered with the bytes; a let-go
/// answered once its holder is let go. Each wait is judged in its target's own time within `budget_ns`. Malformed bytes
/// answer nothing, as the session's other streams do.
pub(crate) async fn serve_remote(origin: u16, request: &[u8], budget_ns: u64) -> Vec<u8> {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal, admits};
  let Ok(message) = HomeMessage::decode(request) else {
    return Vec::new();
  };
  let Some((local, members, f, shards)) = crate::state::with_state(|state| {
    state.count(REMOTE_SERVED, 1);
    (
      state.fleet.host(),
      state.fleet.members().to_vec(),
      usize::try_from(state.fleet.configuration().quorum.f).unwrap_or(usize::MAX),
      state.shards.clone(),
    )
  }) else {
    return Vec::new();
  };
  let (identity, outcome) = match message {
    HomeMessage::Put { identity, bytes } => {
      let outcome = match admits(&identity, &bytes, (local, &members, f)) {
        Err(refusal) => HomeOutcome::Refused(refusal),
        Ok(()) => match keep_region_at_home(origin, &shards, bytes, budget_ns).await {
          Ok((_, ticket)) if published(origin, ticket, budget_ns).await => HomeOutcome::Kept {
            generation: ticket.generation,
          },
          Ok(_) | Err(HomeRefused::Unanswered) => HomeOutcome::Refused(HomeRefusal::Unanswered),
          Err(HomeRefused::Misnamed | HomeRefused::PastWindow) => {
            HomeOutcome::Refused(HomeRefusal::Misnamed)
          }
          Err(HomeRefused::Content(_)) => HomeOutcome::Refused(HomeRefusal::Room),
          Err(HomeRefused::NotHeld) => HomeOutcome::Refused(HomeRefusal::NotHeld),
        },
      };
      (identity, outcome)
    }
    HomeMessage::Read { identity } => {
      let outcome = match read(origin, &shards, ContentId(identity), budget_ns).await {
        Some(bytes) => HomeOutcome::Bytes(bytes),
        None => HomeOutcome::Refused(HomeRefusal::NotHeld),
      };
      (identity, outcome)
    }
    HomeMessage::LetGo { identity } => {
      let outcome =
        match let_go_region_at_home(origin, &shards, ContentId(identity), budget_ns).await {
          Ok(()) => HomeOutcome::LetGo,
          Err(HomeRefused::NotHeld) => HomeOutcome::Refused(HomeRefusal::NotHeld),
          Err(_) => HomeOutcome::Refused(HomeRefusal::Unanswered),
        };
      (identity, outcome)
    }
    HomeMessage::Answer { .. } => return Vec::new(),
  };
  HomeMessage::Answer { identity, outcome }.encode()
}

/// [`keep_for_region`] on the home shard among `shards` of the window `bytes`, asked from shard `origin`.
async fn keep_region_at_home(
  origin: u16,
  shards: &[u16],
  bytes: Vec<u8>,
  budget_ns: u64,
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  let identity = ContentId::of(&bytes);
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| keep_for_region(state, &bytes),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)?
}

/// [`let_go_for_region`] on the home shard among `shards` of the window `identity`, asked from shard `origin`.
async fn let_go_region_at_home(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget_ns: u64,
) -> Result<(), HomeRefused> {
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| let_go_for_region(state, &identity),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)?
}

/// Counter: identities the healer offered to their region homes. Format: a counter name.
pub(crate) const HEAL_OFFERED: &str = "vcs_store.homes.heal_offered";
/// Counter: identities this node let go once every region home held them. Format: a counter name.
pub(crate) const HEAL_RELEASED: &str = "vcs_store.homes.heal_released";

std::thread_local! {
  /// The control shard's healer pass (module docs, "The healer"): the configuration version it serves and the
  /// identities still to visit. Bounded by the windows this node holds for the region.
  static HEALING: std::cell::RefCell<(Option<u64>, std::collections::VecDeque<ContentId>)> =
    const { std::cell::RefCell::new((None, std::collections::VecDeque::new())) };
}

/// One period's turn of the region healer on the control shard `origin` (module docs, "The healer"): at a new
/// configuration version a pass begins over every window this node holds for the region; each identity is offered to
/// its homes (`slates_db::identity_homes::homes`) by `slates_cluster::homes`'s `Put`, and once every home answered
/// `Kept` this node lets its own hold go where it is not a home (`slates_db::identity_homes::heal`). A pass runs a turn
/// a period, stopping when `budget`'s deadline is spent, and nothing runs between configuration changes.
pub(crate) async fn heal_region(origin: u16, shards: &[u16], budget: slates_cluster::CommitBudget) {
  let Some((version, local, members, f)) = crate::state::with_state(|state| {
    (
      state.fleet.configuration().version,
      state.fleet.host(),
      state.fleet.members().to_vec(),
      usize::try_from(state.fleet.configuration().quorum.f).unwrap_or(usize::MAX),
    )
  }) else {
    return;
  };
  let begun = HEALING.with(|healing| {
    healing
      .try_borrow()
      .map_or(true, |healing| healing.0 == Some(version))
  });
  if !begun {
    let mut identities = std::collections::VecDeque::new();
    for shard in shards {
      let held = crate::xshard::call_in_shard_time(
        origin,
        *shard,
        |_| region_held_here(),
        budget.max_deadline_ns(),
      )
      .await
      .unwrap_or_default();
      identities.extend(held);
    }
    HEALING.with(|healing| {
      if let Ok(mut healing) = healing.try_borrow_mut() {
        *healing = (Some(version), identities);
      }
    });
  }
  let began = slates_rt::futures::now_ns();
  let spend = budget.max_deadline_ns();
  loop {
    if slates_rt::futures::now_ns().saturating_sub(began) >= spend {
      return;
    }
    let next = HEALING.with(|healing| {
      healing
        .try_borrow_mut()
        .ok()
        .and_then(|mut healing| healing.1.pop_front())
    });
    let Some(identity) = next else {
      return;
    };
    heal_one(origin, shards, (local, &members, f), identity, budget).await;
  }
}

/// Offers one window to its region homes and lets this node's hold go once every home holds it and this node is not
/// one ([`heal_region`]).
async fn heal_one(
  origin: u16,
  shards: &[u16],
  (local, members, f): (slates_db::HostId, &[slates_db::HostId], usize),
  identity: ContentId,
  budget: slates_cluster::CommitBudget,
) {
  use slates_cluster::homes::{HOME_STREAM, HomeMessage, HomeOutcome};
  let homes = slates_db::identity_homes::homes(&identity.0, members, f);
  let others: Vec<slates_db::HostId> = homes
    .iter()
    .copied()
    .filter(|home| *home != local)
    .collect();
  let mut held: std::collections::BTreeSet<slates_db::HostId> = std::collections::BTreeSet::new();
  held.insert(local);
  if !others.is_empty() {
    let Some(bytes) = read(origin, shards, identity, budget.max_deadline_ns()).await else {
      return;
    };
    let request = HomeMessage::Put {
      identity: identity.0,
      bytes,
    }
    .encode();
    let sessions =
      crate::fleet::take_sessions("vcs_store.homes.heal", |host| others.contains(&host));
    let outgoing: Vec<(
      slates_db::HostId,
      Vec<u8>,
      slates_transport::endpoint::Endpoint,
    )> = sessions
      .into_iter()
      .map(|(host, endpoint)| (host, request.clone(), endpoint))
      .collect();
    let (replied, _stragglers) = slates_cluster::broadcast(
      outgoing,
      HOME_STREAM,
      slates_transport::connection::Priority::Bulk,
      budget,
    )
    .await;
    let mut returned = Vec::with_capacity(replied.len());
    for (host, reply, endpoint) in replied {
      if let Ok(HomeMessage::Answer {
        identity: answered,
        outcome: HomeOutcome::Kept { .. },
      }) = HomeMessage::decode(&reply.bytes)
        && answered == identity.0
      {
        held.insert(host);
      }
      returned.push((host, endpoint));
    }
    crate::fleet::return_sessions(returned);
    crate::state::with_state(|state| state.count(HEAL_OFFERED, 1));
  }
  let plan = slates_db::identity_homes::heal(&identity.0, members, f, &held);
  if plan.release.contains(&local)
    && let_go_region_at_home(origin, shards, identity, budget.max_deadline_ns())
      .await
      .is_ok()
  {
    crate::state::with_state(|state| state.count(HEAL_RELEASED, 1));
  }
}

#[cfg(test)]
mod tests;
