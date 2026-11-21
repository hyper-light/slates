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
//! serves every window kept before it: group commit per shard, never a publication per push. The daemon drives it at
//! each publication it commits ([`publication_committed`], from `crate::content_holder::held_committed`): a publication
//! captures and commits in one turn, so the image begins and is published at once.
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
use slates_vcs::{ContentId, ContentRefused};

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

/// Whether this shard's image of `generation` is published ([`image_published`]).
pub(crate) fn published_here(generation: u64) -> bool {
  IMAGES.with(|images| {
    images
      .try_borrow()
      .is_ok_and(|images| images.published >= generation)
  })
}

/// A publication of this shard's image committed (`crate::content_holder::held_committed`, the end of every checkpoint
/// and delta `crate::verbs::publish_shard` commits): every window kept before it is in it, since a publication captures
/// and commits in one turn of the shard's thread, so the image a ticket kept now names begins and is published at once,
/// and a window kept after waits for the next.
pub(crate) fn publication_committed() {
  let generation = IMAGES.with(|images| images.try_borrow().map_or(1, |images| images.next));
  image_begun(generation);
  image_published(generation);
}

/// The ticket of a window kept on this shard now.
pub(crate) fn ticket_here(shard: u16) -> HomeTicket {
  let generation = IMAGES.with(|images| images.try_borrow().map_or(1, |images| images.next));
  HomeTicket { shard, generation }
}

/// Waits, on this shard, until its image of `generation` is published. Its first poll that finds it unpublished
/// publishes the shard itself (`crate::verbs::publish_shard`: a delta, made and committed in one turn), since nothing
/// else publishes on a schedule (persistence before reply): every window kept before that publication rides it, so the
/// waits behind it find their generation published (group commit, one publication for all of them). A publication
/// refused leaves the wait for the next one that commits.
struct PublishedHere {
  generation: u64,
  /// Whether this wait has published the shard already.
  asked: bool,
}

impl core::future::Future for PublishedHere {
  type Output = bool;

  fn poll(
    mut self: core::pin::Pin<&mut Self>,
    context: &mut core::task::Context<'_>,
  ) -> core::task::Poll<bool> {
    let generation = self.generation;
    if !self.asked && !published_here(generation) {
      self.asked = true;
      if crate::state::with_state(|state| crate::verbs::publish_shard(state).is_ok()) == Some(true)
      {
        crate::state::with_state(|state| state.count(HOME_PUBLISHED, 1));
      } else if !published_here(generation) {
        // The shard's publication was refused (its image lost, or no room): answered not durable at once, typed, rather
        // than left to wait for a publication that may never come.
        return core::task::Poll::Ready(false);
      }
    }
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

/// Counter: publications a home's wait for its ticket made itself (A-724 piece 8c), nothing else having published since
/// the window was kept. Format: a counter name.
pub(crate) const HOME_PUBLISHED: &str = "vcs_store.homes.published_for_ticket";

/// Whether the window of `ticket` is durable at its home, asked from shard `origin` (module docs): waits on the home, in
/// its own time up to `budget_ns`, for its image of the ticket's generation; false when that did not come within it.
pub(crate) async fn published(origin: u16, ticket: HomeTicket, budget_ns: u64) -> bool {
  published_within(origin, ticket, budget_ns).await == Some(true)
}

/// [`published`], telling a home that did not answer (none) from one whose image of the ticket's generation was not
/// published within the wait (false): what `super::durable::content_durable` refuses by kind.
pub(crate) async fn published_within(
  origin: u16,
  ticket: HomeTicket,
  budget_ns: u64,
) -> Option<bool> {
  let generation = ticket.generation;
  crate::xshard::task_in_shard_time(
    origin,
    ticket.shard,
    move || PublishedHere {
      generation,
      asked: false,
    },
    budget_ns,
  )
  .await
}

/// The shard of `shards` that is `identity`'s home (module docs): the first by rendezvous over the identity; none for
/// no shards.
pub(crate) fn home_of(shards: &[u16], identity: &ContentId) -> Option<u16> {
  let members: Vec<u64> = shards.iter().map(|shard| u64::from(*shard)).collect();
  rendezvous_ranked_by_key(&members, &identity.0)
    .first()
    .and_then(|member| u16::try_from(*member).ok())
}

/// Keeps the object `bytes` on this shard, its home, held once more for its sender (cut at the store's windows, named by
/// the BLAKE3 of its bytes, the identity rule): its identity, and the ticket an
/// entry naming it waits on (module docs, "Durable before named"). The shard's content
/// store is made if it has none (as a first repository makes it), lent the room the shard could admit for the step,
/// and settled after.
pub(crate) fn keep_here(
  state: &mut ShardState,
  bytes: &[u8],
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  let window = state.store.content.chunk_bytes();
  state
    .vcs_stores
    .content_made(window)
    .map_err(HomeRefused::Content)?;
  let need = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  super::lend(&mut state.store, &mut state.vcs_stores, need);
  let kept = {
    let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
    lent
      .stores
      .content
      .as_mut()
      .map(|content| content.put_object(bytes))
  };
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
  // The content store made first where a recovery left it unmade (no repository yet), so what the shard image restored
  // (windows, lists, names, region holds) is taken in before the read.
  let window = state.store.content.chunk_bytes();
  state.vcs_stores.content_made(window).ok()?;
  let bytes = {
    let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
    lent.stores.content.as_ref().and_then(|content| {
      // The object whole, its windows in order (an object longer than a window is a list of them, named by its
      // BLAKE3 through the store's alias): answered only when they hash to its identity.
      let mut bytes = Vec::new();
      for window in content.windows(identity) {
        bytes.extend_from_slice(&window);
      }
      (!bytes.is_empty() && ContentId::of(&bytes) == *identity).then_some(bytes)
    })
  }?;
  state.count(READ, 1);
  Some(bytes)
}

/// Lets go one holder of the window `identity` this shard keeps as its home; freed with its last, the charge settled.
pub(crate) fn let_go_here(state: &mut ShardState, identity: &ContentId) -> Result<(), HomeRefused> {
  let window = state.store.content.chunk_bytes();
  state
    .vcs_stores
    .content_made(window)
    .map_err(HomeRefused::Content)?;
  let held = state
    .vcs_stores
    .content
    .as_ref()
    .is_some_and(|content| content.contains(identity));
  if !held {
    return Err(HomeRefused::NotHeld);
  }
  super::lend(&mut state.store, &mut state.vcs_stores, 0);
  let released = {
    let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
    lent
      .stores
      .content
      .as_mut()
      .map(|content| content.release(identity))
  };
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
  /// The region holds kept (true) or let go (false) since the shard's last committed publication: what a delta of the
  /// shard image carries ([`region_changes`]), so a home's holds survive its restart (A-724 piece 8c). Bounded by them.
  static REGION_CHANGES: std::cell::RefCell<std::collections::BTreeMap<ContentId, bool>> =
    const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Marks the region hold of `identity` kept or let go since the last committed publication ([`REGION_CHANGES`]).
fn region_changed(identity: ContentId, held: bool) {
  REGION_CHANGES.with(|changes| {
    if let Ok(mut changes) = changes.try_borrow_mut() {
      changes.insert(identity, held);
    }
  });
}

/// The region holds kept (true) or let go (false) since this shard's last committed publication, for its delta
/// (`crate::content_holder::held_changes`): a home's holds ride its shard image, so they survive its restart.
pub(crate) fn region_changes() -> Vec<(ContentId, bool)> {
  REGION_CHANGES.with(|changes| {
    changes
      .try_borrow()
      .map(|changes| {
        changes
          .iter()
          .map(|(identity, held)| (*identity, *held))
          .collect()
      })
      .unwrap_or_default()
  })
}

/// A publication of this shard committed: the region holds' changes it carried are forgotten.
pub(crate) fn region_changes_committed() {
  REGION_CHANGES.with(|changes| {
    if let Ok(mut changes) = changes.try_borrow_mut() {
      changes.clear();
    }
  });
}

/// The region holds a recovered shard image carried, taken back once its content store is made over the restored
/// windows (`super::VcsStores::content_made`): each held again where the store has it (a restored window is held by no
/// one until something holds it, and is freed once every record is replayed, `release_unheld`). How many were held.
pub(crate) fn restore_region_holds(
  content: &mut slates_vcs::ContentStore,
  identities: &[ContentId],
) -> u64 {
  let mut held = 0u64;
  for identity in identities {
    if content.contains(identity) && content.hold(identity).is_ok() {
      REGION_HELD.with(|region| {
        if let Ok(mut region) = region.try_borrow_mut() {
          region.insert(*identity);
        }
      });
      held = held.saturating_add(1);
    }
  }
  held
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
  region_changed(kept.0, true);
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
  region_changed(*identity, false);
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

/// A whole put from the region kept at its home ([`serve_remote`]): only where the configuration ranks this node among
/// the object's homes and its bytes hash to their name, answered `Kept` once the home's image holding it is published.
async fn keep_put(
  origin: u16,
  shards: &[u16],
  (local, members, f): (slates_db::HostId, &[slates_db::HostId], usize),
  (identity, bytes): ([u8; 32], Vec<u8>),
  budget_ns: u64,
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::{HomeOutcome, HomeRefusal, admits};
  if let Err(refusal) = admits(&identity, &bytes, (local, members, f)) {
    return HomeOutcome::Refused(refusal);
  }
  match keep_region_at_home(origin, shards, bytes, budget_ns).await {
    Ok((_, ticket)) if published(origin, ticket, budget_ns).await => HomeOutcome::Kept {
      generation: ticket.generation,
    },
    Ok(_) | Err(HomeRefused::Unanswered) => HomeOutcome::Refused(HomeRefusal::Unanswered),
    Err(HomeRefused::Misnamed | HomeRefused::PastWindow) => {
      HomeOutcome::Refused(HomeRefusal::Misnamed)
    }
    Err(HomeRefused::Content(_)) => HomeOutcome::Refused(HomeRefusal::Room),
    Err(HomeRefused::NotHeld) => HomeOutcome::Refused(HomeRefusal::NotHeld),
  }
}

/// What a put's part did to its object's staging ([`stage_part`]).
enum Staged {
  /// Refused: the staging is dropped.
  Refused(slates_cluster::homes::HomeRefusal),
  /// Taken: the bytes received so far.
  Taken(u64),
  /// The object whole, to keep.
  Whole(Vec<u8>),
}

/// An object being staged from the region a part at a time: its length and the bytes received, in order.
type RegionStaging = (u64, Vec<u8>);

std::thread_local! {
  /// The objects put from the region a part at a time and not yet whole, on this serving shard (A-724 piece 8c): each its
  /// length and the bytes received, in order. Bounded: their bytes together never pass what the shard can admit.
  static STAGING: std::cell::RefCell<std::collections::BTreeMap<[u8; 32], RegionStaging>> =
    const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
  /// The object a region read takes a part at a time on this serving shard, read whole at its first part and let go at its
  /// last ([`read_part`]): one object.
  static READING: std::cell::RefCell<Option<(ContentId, Vec<u8>)>> = const { std::cell::RefCell::new(None) };
}

/// Stages a put's part (A-724 piece 8c): the first part (offset 0) only where the configuration ranks this node among
/// the object's homes and the object's length fits what the shard can admit beside what is staged already; each next
/// part only at the offset the staging has reached and within the length. Whole once every byte is in, its BLAKE3
/// checked by [`keep_put`].
fn stage_part(
  (local, members, f): (slates_db::HostId, &[slates_db::HostId], usize),
  (identity, total, offset): ([u8; 32], u64, u64),
  bytes: Vec<u8>,
) -> Staged {
  use slates_cluster::homes::HomeRefusal;
  let room = crate::state::with_state(|state| state.store.admittable()).unwrap_or(0);
  STAGING.with(|staging| {
    let Ok(mut staging) = staging.try_borrow_mut() else {
      return Staged::Refused(HomeRefusal::Unanswered);
    };
    if offset == 0 {
      staging.remove(&identity);
      if !slates_db::identity_homes::homes(&identity, members, f).contains(&local) {
        return Staged::Refused(HomeRefusal::NotAHome);
      }
      let staged: u64 = staging
        .values()
        .map(|(_, got)| u64::try_from(got.len()).unwrap_or(u64::MAX))
        .sum();
      if total.saturating_add(staged) > room {
        return Staged::Refused(HomeRefusal::Room);
      }
      let mut got = Vec::new();
      if got
        .try_reserve_exact(usize::try_from(total).unwrap_or(usize::MAX))
        .is_err()
      {
        return Staged::Refused(HomeRefusal::Room);
      }
      staging.insert(identity, (total, got));
    }
    let Some((length, got)) = staging.get_mut(&identity) else {
      return Staged::Refused(HomeRefusal::NotHeld);
    };
    let at = u64::try_from(got.len()).unwrap_or(u64::MAX);
    let end = at.saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    if *length != total || at != offset || end > total {
      staging.remove(&identity);
      return Staged::Refused(HomeRefusal::NotHeld);
    }
    got.extend_from_slice(&bytes);
    if end < total {
      return Staged::Taken(end);
    }
    staging
      .remove(&identity)
      .map_or(Staged::Refused(HomeRefusal::NotHeld), |(_, got)| {
        Staged::Whole(got)
      })
  })
}

/// A region read's part (A-724 piece 8c): the object read whole at its first part (this node's own home shard), its part
/// from `offset` answered, at most `slates_cluster::homes::PART_BYTES`, with its length; let go once its last part is.
async fn read_part(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  offset: u64,
  budget_ns: u64,
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::{HomeOutcome, HomeRefusal, PART_BYTES};
  let cached = READING.with(|reading| {
    reading
      .try_borrow()
      .is_ok_and(|reading| reading.as_ref().is_some_and(|(held, _)| *held == identity))
  });
  if !cached || offset == 0 {
    let Some(bytes) = read(origin, shards, identity, budget_ns).await else {
      return HomeOutcome::Refused(HomeRefusal::NotHeld);
    };
    READING.with(|reading| {
      if let Ok(mut reading) = reading.try_borrow_mut() {
        *reading = Some((identity, bytes));
      }
    });
  }
  READING.with(|reading| {
    let Ok(mut reading) = reading.try_borrow_mut() else {
      return HomeOutcome::Refused(HomeRefusal::Unanswered);
    };
    let Some((_, bytes)) = reading.as_ref() else {
      return HomeOutcome::Refused(HomeRefusal::NotHeld);
    };
    let total = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let start = usize::try_from(offset).unwrap_or(usize::MAX);
    let end = usize::try_from(offset.saturating_add(PART_BYTES).min(total)).unwrap_or(usize::MAX);
    let Some(part) = bytes.get(start..end) else {
      return HomeOutcome::Refused(HomeRefusal::NotHeld);
    };
    let part = part.to_vec();
    if u64::try_from(end).unwrap_or(u64::MAX) >= total {
      *reading = None;
    }
    HomeOutcome::Part { total, bytes: part }
  })
}

/// Serves one home exchange from another member of the region on shard `origin` (module docs, "From the region";
/// `slates_cluster::homes`): a put kept only where the configuration this node knows ranks it among the window's homes
/// and its bytes hash to their name, on the node's own home shard for the window, and answered `Kept` only once that
/// shard's image holding it is published ([`published`], A-727's barrier); a read answered with the bytes; a let-go
/// answered once its holder is let go. Each wait is judged in its target's own time within `budget_ns`. Malformed bytes
/// answer nothing, as the session's other streams do.
pub(crate) async fn serve_remote(origin: u16, request: &[u8], budget_ns: u64) -> Vec<u8> {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal};
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
    HomeMessage::Put { identity, bytes } => (
      identity,
      keep_put(
        origin,
        &shards,
        (local, &members, f),
        (identity, bytes),
        budget_ns,
      )
      .await,
    ),
    HomeMessage::PutPart {
      identity,
      total,
      offset,
      bytes,
    } => {
      let staged = stage_part((local, &members, f), (identity, total, offset), bytes);
      let outcome = match staged {
        Staged::Refused(refusal) => HomeOutcome::Refused(refusal),
        Staged::Taken(received) => HomeOutcome::Taken { received },
        Staged::Whole(bytes) => {
          keep_put(
            origin,
            &shards,
            (local, &members, f),
            (identity, bytes),
            budget_ns,
          )
          .await
        }
      };
      (identity, outcome)
    }
    HomeMessage::ReadPart { identity, offset } => (
      identity,
      read_part(origin, &shards, ContentId(identity), offset, budget_ns).await,
    ),
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

/// Counter: a push's names asked to be homed that no edge took by the next turn: the record written as version 2
/// (A-724 piece 8c). Format: a counter name.
pub(crate) const HOMING_UNTAKEN: &str = "vcs_store.homes.homing_untaken";

/// Counts a homing no edge took ([`HOMING_UNTAKEN`]).
pub(crate) fn count_untaken(state: &mut crate::state::ShardState) {
  state.count(HOMING_UNTAKEN, 1);
}

/// Counter: objects homed in the region at `f + 1` before a record names them (A-724 piece 8c). Format: a counter name.
pub(crate) const HOMED_IN_REGION: &str = "vcs_store.homes.homed_in_region";
/// Counter: homings in the region that fell short of `f + 1` within their budget. Format: a counter name.
pub(crate) const HOMED_SHORT: &str = "vcs_store.homes.homed_short";

/// The object `bytes` homed in the region before a record names it (main's ruling of 2026-11-20 (ii), A-727's rule:
/// content durable at `f + 1` before the record that names it): kept on each of its region homes
/// (`slates_db::identity_homes::homes` over its BLAKE3), this node's own through its home shard, every other through
/// [`slates_cluster::homes`]; each answers kept only once its home's image holding it is published. Its identity once
/// `f + 1` homes (every member, in a region of fewer) answered kept within `budget`; refused `Unanswered` otherwise, and
/// the caller records the names inline instead (version 2). Asked from shard `origin`; at most one window
/// (an object of any length a home keeps whole, cut at its own windows; one region message carries at most
/// `slates_archive::format::MAX_CHUNK_BYTES` until an object past that is sent in parts).
/// This node, one of the object's homes, keeps `bytes`: 1 once its image holding them is durable within `budget_ns`,
/// else 0.
async fn kept_here(
  origin: u16,
  shards: &[u16],
  bytes: &[u8],
  budget_ns: u64,
) -> Result<usize, HomeRefused> {
  let (_, ticket) = keep_region_at_home(origin, shards, bytes.to_vec(), budget_ns).await?;
  let durable = super::durable::content_durable(origin, ticket, budget_ns)
    .await
    .is_ok();
  Ok(usize::from(durable))
}

pub(crate) async fn home_in_region(
  origin: u16,
  shards: &[u16],
  bytes: Vec<u8>,
  budget: slates_cluster::CommitBudget,
) -> Result<ContentId, HomeRefused> {
  let (local, members, f) = crate::state::with_state(|state| {
    (
      state.fleet.host(),
      state.fleet.members().to_vec(),
      usize::try_from(state.fleet.configuration().quorum.f).unwrap_or(usize::MAX),
    )
  })
  .ok_or(HomeRefused::Unanswered)?;
  let identity = ContentId::of(&bytes);
  let homes = slates_db::identity_homes::homes(&identity.0, &members, f);
  let budget_ns = budget.max_deadline_ns();
  let mut kept = 0usize;
  if homes.contains(&local) {
    kept = kept_here(origin, shards, &bytes, budget_ns).await?;
  }
  let others: Vec<slates_db::HostId> = homes
    .iter()
    .copied()
    .filter(|home| *home != local)
    .collect();
  // The other homes reached from the control shard, which holds the record sessions (`crate::fleet::take_sessions`):
  // on any other shard none is found and every homing there fell short (the three-node A/B, 2026-11-20).
  if !others.is_empty() {
    let control = shards.first().copied().unwrap_or(origin);
    kept = kept.saturating_add(
      crate::xshard::task_in_shard_time(
        origin,
        control,
        move || put_remote(others, identity, bytes, budget),
        budget_ns,
      )
      .await
      .unwrap_or(0),
    );
  }
  let homed = kept >= homes.len() && !homes.is_empty();
  crate::state::with_state(|state| {
    state.count(if homed { HOMED_IN_REGION } else { HOMED_SHORT }, 1)
  });
  if homed {
    Ok(identity)
  } else {
    Err(HomeRefused::Unanswered)
  }
}

/// The object `identity` read from the region (A-724 piece 8c: a version-3 record's names, read where its replay runs):
/// from this node's own home shard when it holds it, else from each of its region homes in turn, the first answer whose
/// bytes hash to `identity` taken (a home answering other bytes is passed over). None when no home answered it within
/// `budget`. Asked from shard `origin`.
pub(crate) async fn read_in_region(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget: slates_cluster::CommitBudget,
) -> Option<Vec<u8>> {
  let budget_ns = budget.max_deadline_ns();
  if let Some(bytes) = read(origin, shards, identity, budget_ns).await
    && ContentId::of(&bytes) == identity
  {
    return Some(bytes);
  }
  let (local, members, f) = crate::state::with_state(|state| {
    (
      state.fleet.host(),
      state.fleet.members().to_vec(),
      usize::try_from(state.fleet.configuration().quorum.f).unwrap_or(usize::MAX),
    )
  })?;
  let others: Vec<slates_db::HostId> = slates_db::identity_homes::homes(&identity.0, &members, f)
    .into_iter()
    .filter(|home| *home != local)
    .collect();
  if others.is_empty() {
    return None;
  }
  // From the control shard, which holds the record sessions, a part at a time from each home in turn (A-724 piece 8c).
  let control = shards.first().copied().unwrap_or(origin);
  crate::xshard::task_in_shard_time(
    origin,
    control,
    move || read_remote(others, identity, budget),
    budget_ns,
  )
  .await
  .flatten()
}

/// One region exchange of `request` with each of `hosts` (A-724 piece 8c): sent over each host's record session, a
/// session out on another loan waited for until it comes back (`crate::fleet::session_returned`), never skipped, until
/// every host answered or `budget` is spent: each host's reply. Run on the control shard, which holds the sessions.
async fn exchange(
  hosts: &[slates_db::HostId],
  request: &[u8],
  budget: slates_cluster::CommitBudget,
) -> Vec<(slates_db::HostId, Vec<u8>)> {
  use slates_cluster::homes::HOME_STREAM;
  let began = slates_rt::futures::now_ns();
  let spend = budget.max_deadline_ns();
  let mut left: Vec<slates_db::HostId> = hosts.to_vec();
  let mut answers = Vec::new();
  while !left.is_empty() {
    let sessions =
      crate::fleet::take_sessions("vcs_store.homes.exchange", |host| left.contains(&host));
    let outgoing: Vec<_> = sessions
      .into_iter()
      .map(|(host, endpoint)| (host, request.to_vec(), endpoint))
      .collect();
    if !outgoing.is_empty() {
      let (replied, _stragglers) = slates_cluster::broadcast(
        outgoing,
        HOME_STREAM,
        slates_transport::connection::Priority::Bulk,
        budget,
      )
      .await;
      let mut returned = Vec::with_capacity(replied.len());
      for (host, reply, endpoint) in replied {
        left.retain(|waiting| *waiting != host);
        answers.push((host, reply.bytes));
        returned.push((host, endpoint));
      }
      crate::fleet::return_sessions(returned);
    }
    let spent = slates_rt::futures::now_ns().saturating_sub(began);
    if left.is_empty() || spent >= spend {
      break;
    }
    // A session out on another exchange's loan: wait for its return (or its link's change), then look again.
    if crate::fleet::session_returned(&left, spend.saturating_sub(spent)).await != Some(true) {
      break;
    }
  }
  answers
}

/// The object `bytes` named `identity` sent to the other homes `homes` (A-724 piece 8c), whole within one message or a
/// part at a time past it ([`put_in_parts`]): how many answered kept. Run on the control shard.
async fn put_remote(
  homes: Vec<slates_db::HostId>,
  identity: ContentId,
  bytes: Vec<u8>,
  budget: slates_cluster::CommitBudget,
) -> usize {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, PART_BYTES};
  if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > PART_BYTES {
    return put_in_parts(&homes, identity, &bytes, budget).await.len();
  }
  let request = HomeMessage::Put {
    identity: identity.0,
    bytes,
  }
  .encode();
  exchange(&homes, &request, budget)
    .await
    .into_iter()
    .filter(|(_, reply)| {
      matches!(
        HomeMessage::decode(reply),
        Ok(HomeMessage::Answer { identity: answered, outcome: HomeOutcome::Kept { .. } }) if answered == identity.0
      )
    })
    .count()
}

/// The object named `identity` read from one of the other homes `homes`, a part at a time from each in turn
/// ([`read_in_parts`]): the first whole one. Run on the control shard.
async fn read_remote(
  homes: Vec<slates_db::HostId>,
  identity: ContentId,
  budget: slates_cluster::CommitBudget,
) -> Option<Vec<u8>> {
  for home in &homes {
    if let Some(bytes) = read_in_parts(*home, identity, budget).await {
      return Some(bytes);
    }
  }
  None
}

/// Sends the object `bytes` named `identity` to each of `homes` a part at a time, in order (A-724 piece 8c: an object past
/// one message), each part to every home still taking it ([`exchange`]), a home dropped at its first answer other than
/// the part taken: the homes that answered the last part kept (each only once its image holding it is published).
async fn put_in_parts(
  homes: &[slates_db::HostId],
  identity: ContentId,
  bytes: &[u8],
  budget: slates_cluster::CommitBudget,
) -> Vec<slates_db::HostId> {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, PART_BYTES};
  let total = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
  let part = usize::try_from(PART_BYTES).unwrap_or(usize::MAX).max(1);
  let mut taking: Vec<slates_db::HostId> = homes.to_vec();
  let mut kept = Vec::new();
  for (at, chunk) in bytes.chunks(part).enumerate() {
    let offset = u64::try_from(at.saturating_mul(part)).unwrap_or(u64::MAX);
    let end = offset.saturating_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX));
    let request = HomeMessage::PutPart {
      identity: identity.0,
      total,
      offset,
      bytes: chunk.to_vec(),
    }
    .encode();
    let mut still = Vec::new();
    for (host, reply) in exchange(&taking, &request, budget).await {
      match HomeMessage::decode(&reply) {
        Ok(HomeMessage::Answer {
          identity: answered,
          outcome: HomeOutcome::Taken { received },
        }) if answered == identity.0 && received == end && end < total => still.push(host),
        Ok(HomeMessage::Answer {
          identity: answered,
          outcome: HomeOutcome::Kept { .. },
        }) if answered == identity.0 && end == total => kept.push(host),
        _ => {}
      }
    }
    taking = still;
    if taking.is_empty() {
      break;
    }
  }
  kept
}

/// Reads the object named `identity` back from `home` a part at a time (A-724 piece 8c, [`exchange`]), answered only when
/// its parts make its whole length and hash to its identity.
async fn read_in_parts(
  home: slates_db::HostId,
  identity: ContentId,
  budget: slates_cluster::CommitBudget,
) -> Option<Vec<u8>> {
  use slates_cluster::homes::{HomeMessage, HomeOutcome};
  let mut bytes: Vec<u8> = Vec::new();
  loop {
    let offset = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let request = HomeMessage::ReadPart {
      identity: identity.0,
      offset,
    }
    .encode();
    let (total, got) = exchange(&[home], &request, budget)
      .await
      .into_iter()
      .find_map(|(_, reply)| match HomeMessage::decode(&reply) {
        Ok(HomeMessage::Answer {
          identity: answered,
          outcome: HomeOutcome::Part { total, bytes: got },
        }) if answered == identity.0 && !got.is_empty() => Some((total, got)),
        _ => None,
      })?;
    bytes.extend_from_slice(&got);
    let read = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    if read > total {
      return None;
    }
    if read == total {
      return (ContentId::of(&bytes) == identity).then_some(bytes);
    }
  }
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

/// Lets this node's hold of `identity` go (counted) when the heal plan for the homes that hold it (`held`) releases it.
async fn release_if_healed(
  origin: u16,
  shards: &[u16],
  (local, members, f): (slates_db::HostId, &[slates_db::HostId], usize),
  identity: ContentId,
  budget: slates_cluster::CommitBudget,
  held: &std::collections::BTreeSet<slates_db::HostId>,
) {
  let plan = slates_db::identity_homes::heal(&identity.0, members, f, held);
  if plan.release.contains(&local)
    && let_go_region_at_home(origin, shards, identity, budget.max_deadline_ns())
      .await
      .is_ok()
  {
    crate::state::with_state(|state| state.count(HEAL_RELEASED, 1));
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
    // An object past one message is offered a part at a time (A-724 piece 8c), as `home_in_region` sends it.
    if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > slates_cluster::homes::PART_BYTES {
      held.extend(put_in_parts(&others, identity, &bytes, budget).await);
      crate::state::with_state(|state| state.count(HEAL_OFFERED, 1));
      release_if_healed(origin, shards, (local, members, f), identity, budget, &held).await;
      return;
    }
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
  release_if_healed(origin, shards, (local, members, f), identity, budget, &held).await;
}

#[cfg(test)]
mod tests;
