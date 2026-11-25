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
//! **What is homed.** An object (a push's names or pack, a window or longer), named by its plain BLAKE3, so a sender
//! knows its home before it sends: the shard that identity ranks first keeps it whole, cut at the store's windows there
//! (`ContentStore::put_object`), so its windows have identities of their own that rank anywhere. A home keeps it in its shard's one content store
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
//! **Kept for the region alone** (main 01a20182's ruling of 2026-11-22 21:15). What a home keeps it keeps for the
//! region ([`keep_for_region`], [`home_in_region`]'s node-local half, [`serve_remote`]'s puts): the single-window and
//! batch homing a sender once called (`home`, `home_many`, `read_many`, `let_go_many`) had no production caller and is
//! gone, so a window let go once landed is pinned only where this shard holds an object for the region
//! (`vcs_store::landed_pin`, [`region_holds_window`]), never by where its identity ranks.
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
//! not answer within the budget; content a home does not hold. A read is refused by its own kinds ([`HomeReadRefused`]):
//! those, bytes that do not hash to their identity, and a window let go once landed that was not brought back, with the
//! bring-back's kind ([`LandedRefused`]; a revision's from `hg_push::bring_back`), never a bare none (§2 item 9). Each
//! kind is counted where it is refused or where a reader can only say absent, and a member's read is answered the
//! nearest kind the wire carries.

use slates_cluster::homes::HomeOwner;
use slates_db::register::rendezvous_ranked_by_key;
use slates_vcs::preimage::Preimages;
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
  /// The home holds no such content.
  NotHeld,
}

/// Why a home's read gave no bytes ([`read`]; module docs, "Refused"): answered to the region as the nearest kind its
/// wire carries ([`HomeReadRefused::wire`]), and counted by its kind where a reader can only say absent
/// ([`HomeReadRefused::counter`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeReadRefused {
  /// Its home shard did not answer within the read's budget.
  Unanswered,
  /// No home among the shards named holds it, and no landed record brings it back.
  NotHeld,
  /// Its home's content store could not be made to read it (its budget, or its window).
  Content(ContentRefused),
  /// Let go once landed, and not brought back whole ([`bring_back_landed`]).
  NotBroughtBack(LandedRefused),
  /// The bytes its home answered do not hash to its identity: never handed on as its.
  Misnamed,
}

impl HomeReadRefused {
  /// The nearest kind the region's wire carries, as a home answers a member's read ([`serve_remote`]).
  fn wire(self) -> slates_cluster::homes::HomeRefusal {
    use slates_cluster::homes::HomeRefusal;
    match self {
      HomeReadRefused::Unanswered => HomeRefusal::Unanswered,
      HomeReadRefused::NotHeld => HomeRefusal::NotHeld,
      HomeReadRefused::Content(_) => HomeRefusal::Room,
      HomeReadRefused::NotBroughtBack(landed) => landed.wire(),
      HomeReadRefused::Misnamed => HomeRefusal::Misnamed,
    }
  }

  /// This kind's counter, counted where a reader can only say absent (module docs, "Refused").
  pub(crate) const fn counter(self) -> &'static str {
    match self {
      HomeReadRefused::Unanswered => READ_UNANSWERED,
      HomeReadRefused::NotHeld => READ_NOT_HELD,
      HomeReadRefused::Content(_) => READ_CONTENT_REFUSED,
      HomeReadRefused::NotBroughtBack(_) => READ_NOT_BROUGHT_BACK,
      HomeReadRefused::Misnamed => READ_MISNAMED,
    }
  }
}

/// Counter: a home's read refused, its home shard not answering within the read's budget, where its reader could only
/// say absent ([`HomeReadRefused::Unanswered`]). Format: a counter name.
pub(crate) const READ_UNANSWERED: &str = "vcs_store.homes.read_unanswered";
/// Counter: a home's read refused, no home holding it ([`HomeReadRefused::NotHeld`]), where its reader could only say
/// absent. Format: a counter name.
pub(crate) const READ_NOT_HELD: &str = "vcs_store.homes.read_not_held";
/// Counter: a home's read refused by its content store ([`HomeReadRefused::Content`]), where its reader could only say
/// absent. Format: a counter name.
pub(crate) const READ_CONTENT_REFUSED: &str = "vcs_store.homes.read_content_refused";
/// Counter: a home's read refused, a window let go once landed not brought back ([`HomeReadRefused::NotBroughtBack`]),
/// where its reader could only say absent; the bring-back's own kind is counted where it was refused. Format: a counter
/// name.
pub(crate) const READ_NOT_BROUGHT_BACK: &str = "vcs_store.homes.read_not_brought_back";
/// Counter: a home's read refused, its bytes not hashing to its identity ([`HomeReadRefused::Misnamed`]), where its
/// reader could only say absent. Format: a counter name.
pub(crate) const READ_MISNAMED: &str = "vcs_store.homes.read_misnamed";

/// Why [`bring_back_landed`] did not bring a window or object let go once landed back whole
/// ([`HomeReadRefused::NotBroughtBack`]): a revision's kind counted by its repository's reader (`hg_push::bring_back`),
/// the rest here ([`LandedRefused::counter`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LandedRefused {
  /// A Mercurial revision it lies in, refused by its repository's reader.
  Revision(crate::repository::hg_push::BringBackRefused),
  /// A landed pack's window not read whole from the node's store: its owner refused or answered short, or the heap
  /// refused room for its bytes.
  PackUnread,
  /// The content store refused a landed pack's window back: its room, or bytes not the window's own.
  Content(ContentRefused),
  /// Not held whole after its windows were brought back.
  NotWhole,
}

impl LandedRefused {
  /// The nearest kind the region's wire carries ([`HomeReadRefused::wire`]).
  fn wire(self) -> slates_cluster::homes::HomeRefusal {
    use crate::repository::hg_push::BringBackRefused;
    use slates_cluster::homes::HomeRefusal;
    match self {
      LandedRefused::Revision(BringBackRefused::StoreUnanswered) | LandedRefused::PackUnread => {
        HomeRefusal::Unanswered
      }
      LandedRefused::Revision(BringBackRefused::Content(_)) | LandedRefused::Content(_) => {
        HomeRefusal::Room
      }
      LandedRefused::Revision(_) | LandedRefused::NotWhole => HomeRefusal::NotHeld,
    }
  }

  /// This kind's counter, where it is counted here: none for a revision's, which its reader counts.
  const fn counter(self) -> Option<&'static str> {
    match self {
      LandedRefused::Revision(_) => None,
      LandedRefused::PackUnread => Some(LANDED_UNREAD),
      LandedRefused::Content(_) => Some(LANDED_CONTENT_REFUSED),
      LandedRefused::NotWhole => Some(LANDED_NOT_WHOLE),
    }
  }
}

/// Counter: a landed pack's window not read whole from the node's store ([`LandedRefused::PackUnread`]). Format: a
/// counter name.
pub(crate) const LANDED_UNREAD: &str = "vcs_store.homes.landed_unread";
/// Counter: a landed pack's window refused back by the content store ([`LandedRefused::Content`]). Format: a counter
/// name.
pub(crate) const LANDED_CONTENT_REFUSED: &str = "vcs_store.homes.landed_content_refused";
/// Counter: an object let go once landed not held whole after its windows were brought back
/// ([`LandedRefused::NotWhole`]). Format: a counter name.
pub(crate) const LANDED_NOT_WHOLE: &str = "vcs_store.homes.landed_not_whole";

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

/// A publication of this shard's image committed with no cut in flight (`crate::content_holder::image_committed`):
/// nothing was dirty, so every window kept before it is in it, since a publication captures and commits in one turn of
/// the shard's thread; the image a ticket kept now names begins and is published at once, and a window kept after waits
/// for the next. One that carried a cut publishes the cut's generation instead ([`cut_begun`]).
pub(crate) fn publication_committed() {
  let generation = IMAGES.with(|images| images.try_borrow().map_or(1, |images| images.next));
  image_begun(generation);
  image_published(generation);
}

/// An image cut begins (A-870 piece 2, F2): its generation, the image a window kept before now rides, so a window kept
/// from now on waits for the next. Published once the cut's publication commits ([`image_published`]).
pub(crate) fn cut_begun() -> u64 {
  let generation = IMAGES.with(|images| images.try_borrow().map_or(1, |images| images.next));
  image_begun(generation);
  generation
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
        // than left to wait for a publication that may never come. Counted and logged (gate 7's fast short homing).
        let _ = crate::state::with_state(|state| {
          state.count(HOME_PUBLISH_REFUSED, 1);
          slates_machine::log_line!(
            "slates-server: partition {}: a home's publication for generation {generation} was refused: answered not \
             durable at once",
            state.partition
          );
        });
        return core::task::Poll::Ready(false);
      }
    }
    // F2: a ticket not yet published waits for a cut's publication, owed while one is in flight and caught up by a task
    // (`crate::content_holder::image_committed`). Where none is owed nothing will publish it, and the wait would last
    // until the shard parks and the wait's time runs out: answered not durable at once, typed, and counted, a bug signal.
    let stranded = crate::state::with_state(|state| {
      let stranded = !published_here(generation) && !state.image_encoder.owed();
      if stranded {
        state.count(HOME_TICKET_STRANDED, 1);
        slates_machine::log_line!(
          "slates-server: partition {}: a home's ticket of generation {generation} is stranded: not published and no \
           image owed",
          state.partition
        );
      }
      stranded
    });
    if stranded == Some(true) {
      return core::task::Poll::Ready(false);
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

/// Counter: a ticket's wait that found its image unpublished and no publication owed (F2): nothing would publish it, so it
/// was answered not durable at once. A bug signal, asserted none by the image oracle. Format: a counter name.
pub(crate) const HOME_TICKET_STRANDED: &str = "vcs_store.homes.ticket_stranded";
/// Counter: a home's wait for its window's publication answered not durable at once because the shard's publication was
/// refused (its image lost, or no room). Format: a counter name.
pub(crate) const HOME_PUBLISH_REFUSED: &str = "vcs_store.homes.publish_refused";

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

/// The bytes of the window or object `identity` this shard keeps as its home: refused `NotHeld` where it keeps none of
/// it whole here, and by its content store's kind where the store could not be made.
pub(crate) fn read_here(
  state: &mut ShardState,
  identity: &ContentId,
) -> Result<Vec<u8>, HomeReadRefused> {
  // The content store made first where a recovery left it unmade (no repository yet), so what the shard image restored
  // (windows, lists, names, region holds) is taken in before the read.
  let window = state.store.content.chunk_bytes();
  state
    .vcs_stores
    .content_made(window)
    .map_err(HomeReadRefused::Content)?;
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
  }
  .ok_or(HomeReadRefused::NotHeld)?;
  state.count(READ, 1);
  Ok(bytes)
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
  let (released, freed) = {
    let lent = super::Lent::new(&mut state.store, &mut state.vcs_stores);
    lent
      .stores
      .content
      .as_mut()
      .map_or((None, Vec::new()), |content| {
        let layout = content.object_layout(identity);
        let released = content.release(identity);
        // The landed records of the windows freed with it dropped (goal 93 item 2b(b)): no read here names them again.
        let freed: Vec<ContentId> = layout
          .into_iter()
          .map(|(window, _)| window)
          .filter(|window| !content.contains(window))
          .collect();
        (Some(released), freed)
      })
  };
  super::settle(state);
  for window in freed {
    if landed(&window).is_some() {
      landed_changed(window, None);
    }
  }
  match released {
    Some(Ok(_)) => {
      state.count(LET_GO, 1);
      Ok(())
    }
    Some(Err(refused)) => Err(HomeRefused::Content(refused)),
    None => Err(HomeRefused::NotHeld),
  }
}

/// [`read`] for a reader that can only say absent: the bytes, or none with the refusal counted by its kind
/// ([`HomeReadRefused::counter`]), so no kind is dropped unseen (§2 item 9).
async fn read_else_counted(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget_ns: u64,
) -> Option<Vec<u8>> {
  match read(origin, shards, identity, budget_ns).await {
    Ok(bytes) => Some(bytes),
    Err(refused) => {
      crate::state::with_state(|state| state.count(refused.counter(), 1));
      None
    }
  }
}

/// The bytes of the window or object `identity` from its home among `shards`, read from shard `origin` within
/// `budget_ns` of the home's own time (module docs): refused by kind where no home among them holds it, the home did not
/// answer, or it was let go once landed and not brought back (module docs, "Refused").
pub(crate) async fn read(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget_ns: u64,
) -> Result<Vec<u8>, HomeReadRefused> {
  let target = home_of(shards, &identity).ok_or(HomeReadRefused::NotHeld)?;
  let at_home = || {
    crate::xshard::call_in_shard_time(
      origin,
      target,
      move |state| read_here(state, &identity),
      budget_ns,
    )
  };
  let bytes = match at_home().await {
    Some(Ok(bytes)) => bytes,
    // A window let go where a landed pack or revision carried it, or a home that did not answer in time: brought back on
    // its home from the node's store by its record, then read again (goal 93 item 2b(b)).
    Some(Err(HomeReadRefused::NotHeld)) | None => {
      crate::xshard::task_in_shard_time(
        origin,
        target,
        move || bring_back_landed(identity),
        budget_ns,
      )
      .await
      .ok_or(HomeReadRefused::Unanswered)??;
      at_home().await.ok_or(HomeReadRefused::Unanswered)??
    }
    Some(Err(refused)) => return Err(refused),
  };
  // A home answers the bytes it keeps under the identity, which its store named by their hash; checked again here, so a
  // window read across shards is never other bytes than its name (§4.17.1 item 5's rule on arrival, kept on reading).
  if ContentId::of(&bytes) == identity {
    Ok(bytes)
  } else {
    Err(HomeReadRefused::Misnamed)
  }
}

std::thread_local! {
  /// The objects this shard keeps for the region (module docs, "From the region"), each with its owners (A-727 (d1a),
  /// `slates_cluster::homes::HomeOwner`): one content-store hold an object however many owners it has and however often
  /// the region offers it, given back once its last owner lets it go, so a keep is idempotent per (object, owner) and
  /// the healer's offers, sent again at every configuration change, keep nothing twice. Bounded by the objects the
  /// content store holds; Derived: an object's owners are at most the attempts in flight on the repositories that home
  /// it (one each, a repository's landing turn admitting one) and those repositories' records.
  static REGION_HELD: std::cell::RefCell<
    std::collections::BTreeMap<ContentId, std::collections::BTreeSet<HomeOwner>>,
  > = const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
  /// The windows of the objects this shard holds for the region, each with how many of them it lies in: what pins a
  /// window once landed (`vcs_store::landed_pin`), since a region hold names its object and an object longer than a
  /// window is a list whose windows have identities of their own (codex-review, 2026-11-22: a Mercurial file sharing a
  /// region object's first window let it go, and the home read the object as none). Bounded by those objects' windows.
  static REGION_WINDOWS: std::cell::RefCell<std::collections::BTreeMap<ContentId, u32>> =
    const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
  /// The region holds kept (true) or let go (false) since the shard's last committed publication: what a delta of the
  /// shard image carries ([`region_changes`]), so a home's holds survive its restart (A-724 piece 8c). Bounded by them.
  static REGION_CHANGES: std::cell::RefCell<std::collections::BTreeMap<ContentId, bool>> =
    const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
  /// The image cut's pre-images of the region holds (A-870 piece 2, F2; `slates_vcs::preimage`): each hold's owners at
  /// the cut, kept before it changes in a page the cut has not yet encoded. Bound: E + I (`slates_vcs::preimage`).
  static CUT_REGION: std::cell::RefCell<Preimages<ContentId, Vec<HomeOwner>>> =
    std::cell::RefCell::new(Preimages::default());
  /// The image cut's pre-images of the landed records, as [`CUT_REGION`]'s. Bound: E + I.
  static CUT_LANDED: std::cell::RefCell<Preimages<ContentId, Landed>> =
    std::cell::RefCell::new(Preimages::default());
}

/// Before the region hold of `identity` in `held` changes: its owners at the cut kept where its page is frozen (F2).
fn region_before(
  held: &std::collections::BTreeMap<ContentId, std::collections::BTreeSet<HomeOwner>>,
  identity: &ContentId,
) {
  CUT_REGION.with(|cut| {
    if let Ok(mut cut) = cut.try_borrow_mut() {
      cut.before(
        *identity,
        || {
          held
            .get(identity)
            .map(|owners| owners.iter().copied().collect())
        },
        |owners: &Vec<HomeOwner>| {
          owners
            .len()
            .saturating_mul(core::mem::size_of::<HomeOwner>())
        },
      );
    }
  });
}

/// A paged family of homes', as a shard image's cut freezes it (A-870 piece 2, F2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CutFamily {
  /// The region holds.
  RegionHolds,
  /// The landed records.
  Landed,
}

/// Freezes `family`'s coordinates `lo..=hi`, a page of the shard image's cut (F2).
pub(crate) fn cut_freeze(family: CutFamily, (lo, hi): (u64, u64)) {
  match family {
    CutFamily::RegionHolds => CUT_REGION.with(|cut| {
      if let Ok(mut cut) = cut.try_borrow_mut() {
        cut.freeze(lo, hi);
      }
    }),
    CutFamily::Landed => CUT_LANDED.with(|cut| {
      if let Ok(mut cut) = cut.try_borrow_mut() {
        cut.freeze(lo, hi);
      }
    }),
  }
}

/// Thaws `family`'s coordinates `lo..=hi`, a page of the cut now encoded: its pre-images dropped.
pub(crate) fn cut_thaw(family: CutFamily, (lo, hi): (u64, u64)) {
  match family {
    CutFamily::RegionHolds => CUT_REGION.with(|cut| {
      if let Ok(mut cut) = cut.try_borrow_mut() {
        cut.thaw(lo, hi);
      }
    }),
    CutFamily::Landed => CUT_LANDED.with(|cut| {
      if let Ok(mut cut) = cut.try_borrow_mut() {
        cut.thaw(lo, hi);
      }
    }),
  }
}

/// The cut's publication committed: homes' pre-images dropped. How many pre-images and ranges were left (none where
/// every page was thawed as it was encoded).
pub(crate) fn cut_ended() -> usize {
  let region = CUT_REGION.with(|cut| cut.try_borrow_mut().map_or(0, |mut cut| cut.end()));
  let landed = CUT_LANDED.with(|cut| cut.try_borrow_mut().map_or(0, |mut cut| cut.end()));
  region.saturating_add(landed)
}

/// The pre-images homes kept since this was last asked, the counts started again.
pub(crate) fn take_cut_counts() -> slates_vcs::preimage::PreimageCounts {
  let region = CUT_REGION.with(|cut| {
    cut
      .try_borrow_mut()
      .map_or_else(|_| Default::default(), |mut cut| cut.take_counts())
  });
  region.and(CUT_LANDED.with(|cut| {
    cut
      .try_borrow_mut()
      .map_or_else(|_| Default::default(), |mut cut| cut.take_counts())
  }))
}

/// [`region_held_in`] as at the cut (F2): the holds now with their pre-images laid over them, read until their bytes
/// (an identity and its owners) pass `cap`; where the read stopped short of the range, the coordinate it stopped at.
/// None read where a map is borrowed.
pub(crate) fn region_held_at_cut(
  (lo, hi): (u64, u64),
  cap: usize,
) -> (Vec<(ContentId, Vec<HomeOwner>)>, Option<u64>) {
  REGION_HELD.with(|held| {
    CUT_REGION.with(|cut| {
      let (Ok(held), Ok(cut)) = (held.try_borrow(), cut.try_borrow()) else {
        return (Vec::new(), None);
      };
      let now = held
        .range(identities_in(lo, hi))
        .map(|(identity, owners)| (*identity, owners.iter().copied().collect::<Vec<_>>()));
      cut.overlay_within(
        (lo, hi),
        now,
        (cap, &|_, owners: &Vec<HomeOwner>| {
          slates_vcs::content::IDENTITY_BYTES.saturating_add(
            owners
              .len()
              .saturating_mul(core::mem::size_of::<HomeOwner>()),
          )
        }),
      )
    })
  })
}

/// [`landed_in`] as at the cut (F2), read until their bytes pass `cap`, as [`region_held_at_cut`].
pub(crate) fn landed_at_cut(
  (lo, hi): (u64, u64),
  cap: usize,
) -> (Vec<(ContentId, Landed)>, Option<u64>) {
  LANDED.with(|landed| {
    CUT_LANDED.with(|cut| {
      let (Ok(landed), Ok(cut)) = (landed.try_borrow(), cut.try_borrow()) else {
        return (Vec::new(), None);
      };
      let now = landed
        .range(identities_in(lo, hi))
        .map(|(identity, landed)| (*identity, landed.clone()));
      cut.overlay_within(
        (lo, hi),
        now,
        (cap, &|_, landed: &Landed| {
          slates_vcs::content::IDENTITY_BYTES.saturating_add(landed.image_bytes())
        }),
      )
    })
  })
}

/// Where a window a home holds lies on the node's store, once a landed push's pack carried it (goal 93 item 2b(b); main
/// 01a20182's ruling of 2026-11-22 06:58): the pack's identity (the plain BLAKE3 of its bytes, the key the node's store
/// keeps its `Payload` under), its length, and the window's offset in it. With it a home lets its copy go once landed and
/// brings it back by its identity on a read ([`bring_back_landed`]), so a home's reader reaches its bytes as a repository's
/// does, and the window needs no pin ([`landed_at`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LandedAt {
  /// The pack's identity on the node's store.
  pub(crate) pack: ContentId,
  /// The pack's length.
  pub(crate) len: u64,
  /// The window's offset in the pack.
  pub(crate) offset: u64,
}

/// Format: a [`LandedAt`]'s bytes in the shard image: the pack's identity, its length and the window's offset, each
/// little-endian after the identity.
const LANDED_AT_BYTES: usize = slates_vcs::content::IDENTITY_BYTES + 8 + 8;

impl LandedAt {
  /// Its bytes in the shard image.
  pub(crate) fn to_bytes(self) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(LANDED_AT_BYTES);
    bytes.extend_from_slice(&self.pack.0);
    bytes.extend_from_slice(&self.len.to_le_bytes());
    bytes.extend_from_slice(&self.offset.to_le_bytes());
    bytes
  }

  /// One read back from the shard image; none for bytes of another length.
  pub(crate) fn from_bytes(bytes: &[u8]) -> Option<LandedAt> {
    if bytes.len() != LANDED_AT_BYTES {
      return None;
    }
    let (pack, rest) = bytes.split_at_checked(slates_vcs::content::IDENTITY_BYTES)?;
    let (len, offset) = rest.split_at_checked(size_of::<u64>())?;
    Some(LandedAt {
      pack: ContentId::from_slice(pack)?,
      len: u64::from_le_bytes(len.try_into().ok()?),
      offset: u64::from_le_bytes(offset.try_into().ok()?),
    })
  }
}

/// What brings back a window this shard let go once landed (goal 93 item 2b(b)): where a landed pack carried it, read by
/// the pack's identity ([`LandedAt`], git), or the file revision it holds, read through its repository's own reader
/// ([`HgLanded`], Mercurial: a revision's text after its metadata block is no store row's key, so it is found by its path
/// and node; w-hg's note of 2026-11-22, main 01a20182's ruling of 18:43).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Landed {
  /// A window of a landed git pack.
  Pack(LandedAt),
  /// A window of a Mercurial file revision.
  Hg(HgLanded),
}

impl Landed {
  /// Its record's bytes in the shard image, the identity's apart.
  pub(crate) fn image_bytes(&self) -> usize {
    match self {
      Landed::Pack(_) => LANDED_AT_BYTES,
      Landed::Hg(hg) => HG_LANDED_HEAD.saturating_add(hg.path.len()),
    }
  }
}

/// A Mercurial file revision a window let go holds ([`Landed::Hg`]): its repository, its path and its node, by which the
/// repository's reader asks the node's store for its text (`StoreHandle::texts_by_tool`) and puts its windows back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HgLanded {
  /// The repository.
  pub(crate) repository: slates_db::catalog::RepositoryId,
  /// The file's path in it.
  pub(crate) path: Vec<u8>,
  /// The revision's node.
  pub(crate) node: [u8; 20],
}

/// Format: the fixed head of an [`HgLanded`]'s bytes in the shard image: the repository's 16 bytes, the node's 20; the
/// path follows, to the piece's end.
const HG_LANDED_HEAD: usize = REPOSITORY_ID_BYTES + NODE_BYTES;
/// Format: a repository id's bytes in an [`HgLanded`]'s head (`slates_db::catalog::RepositoryId`).
const REPOSITORY_ID_BYTES: usize = 16;
/// Format: a Mercurial node's bytes in an [`HgLanded`]'s head (SHA-1's twenty).
const NODE_BYTES: usize = 20;

impl HgLanded {
  /// Its bytes in the shard image.
  pub(crate) fn to_bytes(&self) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(HG_LANDED_HEAD.saturating_add(self.path.len()));
    bytes.extend_from_slice(&self.repository.bytes);
    bytes.extend_from_slice(&self.node);
    bytes.extend_from_slice(&self.path);
    bytes
  }

  /// One read back from the shard image; none for bytes shorter than its head.
  pub(crate) fn from_bytes(bytes: &[u8]) -> Option<HgLanded> {
    let (repository, rest) = bytes.split_at_checked(REPOSITORY_ID_BYTES)?;
    let (node, path) = rest.split_at_checked(NODE_BYTES)?;
    Some(HgLanded {
      repository: slates_db::catalog::RepositoryId {
        bytes: repository.try_into().ok()?,
      },
      path: path.to_vec(),
      node: node.try_into().ok()?,
    })
  }
}

std::thread_local! {
  /// What brings back each window this shard let go once landed ([`Landed`]); bounded by the windows it holds, each one
  /// dropped when the window is freed.
  static LANDED: std::cell::RefCell<std::collections::BTreeMap<ContentId, Landed>> =
    const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
  /// The records made (`true`) or dropped (`false`) since this shard's last committed publication, each with what it
  /// was, so the image names its kind: the image carries them.
  static LANDED_CHANGES: std::cell::RefCell<std::collections::BTreeMap<ContentId, (Landed, bool)>> =
    const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// What brings back the window `identity` this shard let go once landed, if anything does.
pub(crate) fn landed(identity: &ContentId) -> Option<Landed> {
  LANDED.with(|landed| landed.try_borrow().ok()?.get(identity).cloned())
}

/// Where the window `identity` this shard holds lies on the node's store, where a landed pack carried it.
pub(crate) fn landed_at(identity: &ContentId) -> Option<LandedAt> {
  match landed(identity)? {
    Landed::Pack(at) => Some(at),
    Landed::Hg(_) => None,
  }
}

/// Notes what brings back the window `identity` once its owner lets it go landed (goal 93 item 2b(b)): a Mercurial file
/// revision's, as its push or import lets its windows go (`repository::hg_run::let_go_windows`), on the shard that lets
/// them go and before it does. A window noted is not pinned (`vcs_store::landed_pin`), since a home's read brings it back
/// by its note ([`bring_back_landed`]).
pub(crate) fn note_landed(identity: ContentId, landed: Landed) {
  landed_changed(identity, Some(landed));
}

/// Notes `landed` for the window `identity`, or drops its record (`None`), and the change for the shard image.
fn landed_changed(identity: ContentId, landed: Option<Landed>) {
  let change = LANDED.with(|held| {
    let mut held = held.try_borrow_mut().ok()?;
    CUT_LANDED.with(|cut| {
      if let Ok(mut cut) = cut.try_borrow_mut() {
        cut.before(
          identity,
          || held.get(&identity).cloned(),
          Landed::image_bytes,
        );
      }
    });
    match landed {
      Some(landed) => {
        held.insert(identity, landed.clone());
        Some((landed, true))
      }
      None => held.remove(&identity).map(|was| (was, false)),
    }
  });
  if let Some(change) = change {
    LANDED_CHANGES.with(|changes| {
      if let Ok(mut changes) = changes.try_borrow_mut() {
        changes.insert(identity, change);
      }
    });
  }
}

/// The landed records made (`true`) or dropped (`false`) since this shard's last committed publication, each with what
/// it was, for its delta.
pub(crate) fn landed_changes() -> Vec<(ContentId, Landed, bool)> {
  LANDED_CHANGES.with(|changes| {
    changes
      .try_borrow()
      .map(|changes| {
        changes
          .iter()
          .map(|(identity, (landed, made))| (*identity, landed.clone(), *made))
          .collect()
      })
      .unwrap_or_default()
  })
}

/// Every landed record this shard keeps, for its checkpoint's image.
pub(crate) fn landed_here() -> Vec<(ContentId, Landed)> {
  LANDED.with(|landed| {
    landed
      .try_borrow()
      .map(|landed| {
        landed
          .iter()
          .map(|(identity, landed)| (*identity, landed.clone()))
          .collect()
      })
      .unwrap_or_default()
  })
}

/// A publication of this shard committed: the landed records' changes it carried are forgotten.
pub(crate) fn landed_changes_committed() {
  LANDED_CHANGES.with(|changes| {
    if let Ok(mut changes) = changes.try_borrow_mut() {
      changes.clear();
    }
  });
}

/// What this shard's homes hold beside the content store (the memory status's `mem.gauge.homes.*`): its region holds,
/// the windows of the objects they name, its landed records and their bytes as the shard image keeps them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct HomesHeld {
  /// The objects held for the region ([`REGION_HELD`]).
  pub(crate) region_holds: u64,
  /// The windows of those objects counted for their pin ([`REGION_WINDOWS`]).
  pub(crate) region_windows: u64,
  /// The landed records ([`LANDED`]).
  pub(crate) landed_records: u64,
  /// Their bytes in the image's form: each window's identity and its record.
  pub(crate) landed_record_bytes: u64,
}

/// What this shard's homes hold now ([`HomesHeld`]).
pub(crate) fn held() -> HomesHeld {
  let count = |len: usize| u64::try_from(len).unwrap_or(u64::MAX);
  let region_holds = REGION_HELD.with(|held| held.try_borrow().map_or(0, |held| count(held.len())));
  let region_windows = REGION_WINDOWS.with(|windows| {
    windows
      .try_borrow()
      .map_or(0, |windows| count(windows.len()))
  });
  let (landed_records, landed_record_bytes) = LANDED.with(|landed| {
    landed.try_borrow().map_or((0, 0), |landed| {
      let bytes = landed.values().fold(0u64, |sum, record| {
        let record = record.image_bytes();
        sum.saturating_add(count(
          slates_vcs::content::IDENTITY_BYTES.saturating_add(record),
        ))
      });
      (count(landed.len()), bytes)
    })
  });
  HomesHeld {
    region_holds,
    region_windows,
    landed_records,
    landed_record_bytes,
  }
}

/// The landed records a recovered shard image carried, taken back as they were: no store is read for them.
pub(crate) fn restore_landed(records: &[(ContentId, Landed)]) {
  LANDED.with(|landed| {
    if let Ok(mut landed) = landed.try_borrow_mut() {
      landed.extend(records.iter().cloned());
    }
  });
}

/// Records where each of `windows` lies on the node's store, on this shard, for the ones it holds, and lets each go once
/// landed (goal 93 item 2b(b)): a home, which reads by identity, brings it back through the record
/// ([`bring_back_landed`]), so nothing pins it. How many were let go, and their bytes. A window held here no more is
/// passed over.
pub(crate) fn record_landed_here(
  state: &mut ShardState,
  windows: &[(ContentId, LandedAt)],
) -> (u64, u64) {
  let mut recorded = Vec::new();
  let freed = super::within_budget(state, 0, |stores| {
    let Some(content) = stores.content_mut() else {
      return (0, 0);
    };
    let (mut count, mut bytes) = (0u64, 0u64);
    for (identity, at) in windows {
      if !content.contains(identity) || !content.mark_landed(identity) {
        continue;
      }
      recorded.push((*identity, *at));
      if let Ok(freed) = content.let_go_landed(identity, None) {
        count = count.saturating_add(1);
        bytes = bytes.saturating_add(freed);
      }
    }
    (count, bytes)
  });
  for (identity, at) in recorded {
    landed_changed(identity, Some(Landed::Pack(at)));
  }
  if freed.1 > 0 {
    state.count(LANDED_HOME_LET_GO_BYTES, freed.1);
  }
  freed
}

/// Format: the status counter of the bytes homes let go of windows a landed pack carried (goal 93 item 2b(b)).
pub(crate) const LANDED_HOME_LET_GO_BYTES: &str = "vcs_store.homes.landed_let_go_bytes";
/// Format: the status counter of windows a home brought back from the node's store by their record.
pub(crate) const LANDED_BROUGHT_BACK: &str = "vcs_store.homes.landed_brought_back";

/// Brings back the window or object `identity` this shard holds, each of its windows let go read where its record says
/// it lies on the node's store ([`Landed`]): a landed pack's window ([`LandedAt`]) a piece at a time off the shard's
/// step, then put back by its identity within the step's lent room; a Mercurial file revision's ([`HgLanded`]) by its
/// repository's reader (`repository::hg_push::bring_back`), once for each revision however many of the object's windows
/// it holds. Ok where it is held here whole after (brought back, or never let go); refused `NotHeld` where this shard
/// holds none of it, otherwise by the bring-back's kind ([`LandedRefused`]), counted where it was refused.
pub(crate) async fn bring_back_landed(identity: ContentId) -> Result<(), HomeReadRefused> {
  let (disk, (away, revisions)) = crate::state::with_state(|state| {
    // Each physical window once: an object may hold the same window at several offsets (two identical windows are one
    // entry), and the first bring-back puts it back for all of them (codex-review, 2026-11-22).
    let mut seen = std::collections::BTreeSet::new();
    let mut away: Vec<(ContentId, LandedAt, u64)> = Vec::new();
    let mut revisions: Vec<HgLanded> = Vec::new();
    if let Some(content) = state.vcs_stores.content() {
      for (window, _) in content.object_layout(&identity) {
        if !content.is_away(&window) || !seen.insert(window) {
          continue;
        }
        match landed(&window) {
          Some(Landed::Pack(at)) => {
            if let Some(len) = content.len_of(&window) {
              away.push((window, at, len));
            }
          }
          Some(Landed::Hg(revision)) if !revisions.contains(&revision) => revisions.push(revision),
          Some(Landed::Hg(_)) | None => {}
        }
      }
    }
    (state.vcs_disk.clone(), (away, revisions))
  })
  .unwrap_or_default();
  for revision in revisions {
    let HgLanded {
      repository,
      path,
      node,
    } = revision;
    crate::repository::hg_push::bring_back(repository, (path, node))
      .await
      .map_err(|refused| HomeReadRefused::NotBroughtBack(LandedRefused::Revision(refused)))?;
  }
  if let Some(disk) = disk {
    packs_brought_back(&disk, away)
      .await
      .map_err(landed_refused)?;
  }
  let (held, whole) = crate::state::with_state(|state| {
    state
      .vcs_stores
      .content()
      .map_or((false, false), |content| {
        (content.contains(&identity), content.holds_whole(&identity))
      })
  })
  .unwrap_or((false, false));
  match (held, whole) {
    (_, true) => Ok(()),
    (true, false) => Err(landed_refused(LandedRefused::NotWhole)),
    (false, false) => Err(HomeReadRefused::NotHeld),
  }
}

/// [`bring_back_landed`]'s windows `away` that landed packs carried, each read from `disk` where its record says it lies
/// and put back ([`landed_put_back`]); one another read brought back meanwhile is back, so nothing is read for it.
async fn packs_brought_back(
  disk: &slates_store::owner::StoreHandle,
  away: Vec<(ContentId, LandedAt, u64)>,
) -> Result<(), LandedRefused> {
  for (window, at, len) in away {
    let still_away = crate::state::with_state(|state| {
      state
        .vcs_stores
        .content()
        .is_some_and(|content| content.is_away(&window))
    })
    .unwrap_or(false);
    if !still_away {
      continue;
    }
    let bytes = landed_bytes(disk, at, len).await?;
    crate::state::with_state(|state| landed_put_back(state, &window, &bytes, len))
      .unwrap_or(Err(LandedRefused::NotWhole))?;
  }
  Ok(())
}

/// Puts the landed window `window`'s `bytes` (`len` of them) back within the room they take, counted once put: held
/// either way where another read put it back while this one read the store; the content store's refusal otherwise.
fn landed_put_back(
  state: &mut ShardState,
  window: &ContentId,
  bytes: &[u8],
  len: u64,
) -> Result<(), LandedRefused> {
  let put = super::within_budget(state, len, |stores| {
    let content = stores.content_mut().ok_or(LandedRefused::NotWhole)?;
    match content.bring_back(window, bytes) {
      Ok(()) => Ok(true),
      Err(_) if !content.is_away(window) => Ok(false),
      Err(refused) => Err(LandedRefused::Content(refused)),
    }
  })?;
  if put {
    state.count(LANDED_BROUGHT_BACK, 1);
  }
  Ok(())
}

/// `refused` as the home's read answers it ([`HomeReadRefused::NotBroughtBack`]), counted by its kind where this module
/// counts it ([`LandedRefused::counter`]).
fn landed_refused(refused: LandedRefused) -> HomeReadRefused {
  if let Some(counter) = refused.counter() {
    crate::state::with_state(|state| state.count(counter, 1));
  }
  HomeReadRefused::NotBroughtBack(refused)
}

/// The `len` bytes of a window where `at` says it lies on the node's store `disk`, read a piece at a time: refused
/// `PackUnread` where the store refuses or answers short, or the heap refuses room for them.
async fn landed_bytes(
  disk: &slates_store::owner::StoreHandle,
  at: LandedAt,
  len: u64,
) -> Result<Vec<u8>, LandedRefused> {
  let mut bytes = Vec::new();
  bytes
    .try_reserve_exact(usize::try_from(len).unwrap_or(usize::MAX))
    .map_err(|_| LandedRefused::PackUnread)?;
  let end = at.offset.saturating_add(len);
  let mut offset = at.offset;
  while offset < end {
    let piece = disk
      .piece(at.pack.0, at.len, offset)
      .await
      .ok()
      .flatten()
      .ok_or(LandedRefused::PackUnread)?;
    if piece.is_empty() {
      return Err(LandedRefused::PackUnread);
    }
    let want = usize::try_from(end.saturating_sub(offset)).unwrap_or(usize::MAX);
    let taken = piece.get(..want.min(piece.len())).unwrap_or_default();
    bytes.extend_from_slice(taken);
    offset = offset.saturating_add(u64::try_from(taken.len()).unwrap_or(u64::MAX));
  }
  Ok(bytes)
}

/// The pack `pack` landed on the node's store (goal 93 item 2b(b)): where this shard holds it for the region, as its
/// home, each of its windows recorded where it lies in the pack and let go ([`record_landed_here`]). How many were let
/// go, and their bytes; nothing where the region's hold of it is not here.
pub(crate) fn landed_pack_here(state: &mut ShardState, pack: ContentId) -> (u64, u64) {
  if region_holds_here(&pack) == 0 {
    return (0, 0);
  }
  let records: Vec<(ContentId, LandedAt)> =
    state.vcs_stores.content().map_or_else(Vec::new, |content| {
      let layout = content.object_layout(&pack);
      // The pack's length, its windows' summed: its root names a list where it is longer than a window.
      let len = layout.last().map_or(0, |(window, offset)| {
        offset.saturating_add(content.len_of(window).unwrap_or(0))
      });
      layout
        .into_iter()
        .map(|(window, offset)| (window, LandedAt { pack, len, offset }))
        .collect()
    });
  record_landed_here(state, &records)
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
/// one until something holds it, and is freed once every record is replayed, `release_unheld`). How many were held, and
/// the ones it could not take back (the store lacks the window, refused its hold, or the set was borrowed), never skipped
/// unseen: a region hold is the pin that keeps a landed window in RAM (goal 93 item 2b, main 01a20182's ruling of
/// 2026-11-22 07:33), so one lost is counted and logged by the recovery. The store's hold is taken only once the set can
/// name it, so the two always agree: before 2026-11-23 a hold was taken first and given back where the set was
/// borrowed, its release's refusal dropped (w-tsan's sweep, main 01a20182's commit-2 list).
pub(crate) fn restore_region_holds(
  content: &mut slates_vcs::ContentStore,
  holds: &[(ContentId, Vec<HomeOwner>)],
) -> (u64, Vec<ContentId>) {
  let mut held = 0u64;
  let mut refused = Vec::new();
  for (identity, owners) in holds {
    let kept = REGION_HELD.with(|region| {
      let Ok(mut region) = region.try_borrow_mut() else {
        return false;
      };
      if owners.is_empty() || !content.contains(identity) || content.hold(identity).is_err() {
        return false;
      }
      region.insert(*identity, owners.iter().copied().collect());
      true
    });
    if kept {
      held = held.saturating_add(1);
      region_windows_changed(content, identity, true);
    } else {
      refused.push(*identity);
    }
  }
  (held, refused)
}

/// Keeps the object `bytes` on this shard for the region, for `owners` (module docs, "From the region"; A-727 (d1a)): an
/// object held for the region already is held no more, its owners joined (a keep is idempotent per object and owner);
/// its identity and ticket.
pub(crate) fn keep_for_region(
  state: &mut ShardState,
  bytes: &[u8],
  owners: &[HomeOwner],
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  let identity = ContentId::of(bytes);
  if owners.is_empty() {
    return Err(HomeRefused::NotHeld);
  }
  let held = REGION_HELD.with(|held| {
    held
      .try_borrow()
      .is_ok_and(|held| held.contains_key(&identity))
  });
  if held
    && state
      .vcs_stores
      .content
      .as_ref()
      .is_some_and(|content| content.contains(&identity))
  {
    join_owners(&identity, owners);
    return Ok((identity, ticket_here(state.shard)));
  }
  let kept = keep_here(state, bytes)?;
  let inserted = REGION_HELD.with(|held| {
    held.try_borrow_mut().is_ok_and(|mut held| {
      region_before(&held, &kept.0);
      held
        .insert(kept.0, owners.iter().copied().collect())
        .is_none()
    })
  });
  if inserted && let Some(content) = state.vcs_stores.content() {
    region_windows_changed(content, &kept.0, true);
  }
  region_changed(kept.0, true);
  Ok(kept)
}

/// Joins `owners` to the region hold of `identity` this shard keeps, each once, the change marked for the shard image.
fn join_owners(identity: &ContentId, owners: &[HomeOwner]) {
  let joined = REGION_HELD.with(|held| {
    held.try_borrow_mut().is_ok_and(|mut held| {
      if owners.iter().any(|owner| {
        held
          .get(identity)
          .is_some_and(|holding| !holding.contains(owner))
      }) {
        region_before(&held, identity);
      }
      held.get_mut(identity).is_some_and(|held| {
        // Every owner is inserted, so no short circuit: `any` would stop at the first new one.
        let mut joined = false;
        for owner in owners {
          joined |= held.insert(*owner);
        }
        joined
      })
    })
  });
  if joined {
    region_changed(*identity, true);
  }
}

/// Moves the region hold of `identity` this shard keeps from owner `from` to owner `to` (A-727 (d1a): an attempt's entry
/// committed, its records now naming the object): `NotHeld` where `from` holds none of it here.
pub(crate) fn move_region_owner(
  identity: &ContentId,
  (from, to): (HomeOwner, HomeOwner),
) -> Result<(), HomeRefused> {
  let moved = REGION_HELD.with(|held| {
    held.try_borrow_mut().is_ok_and(|mut held| {
      if held
        .get(identity)
        .is_some_and(|holding| holding.contains(&from))
      {
        region_before(&held, identity);
      }
      held.get_mut(identity).is_some_and(|held| {
        let had = held.remove(&from);
        if had {
          held.insert(to);
        }
        had
      })
    })
  });
  if !moved {
    return Err(HomeRefused::NotHeld);
  }
  region_changed(*identity, true);
  Ok(())
}

/// The owners of the region hold of `identity` this shard keeps (A-727 (d1a)).
pub(crate) fn region_owners(identity: &ContentId) -> Vec<HomeOwner> {
  REGION_HELD.with(|held| {
    held
      .try_borrow()
      .ok()
      .and_then(|held| {
        held
          .get(identity)
          .map(|owners| owners.iter().copied().collect())
      })
      .unwrap_or_default()
  })
}

/// Counts the windows of the object `identity` in `content` into ([`REGION_WINDOWS`]), as its region hold is taken
/// (`held`), or out of it, as it is given back.
fn region_windows_changed(content: &slates_vcs::ContentStore, identity: &ContentId, held: bool) {
  let windows = content.object_layout(identity);
  REGION_WINDOWS.with(|counted| {
    let Ok(mut counted) = counted.try_borrow_mut() else {
      return;
    };
    for (window, _) in windows {
      if held {
        let count = counted.entry(window).or_insert(0);
        *count = count.saturating_add(1);
      } else if let Some(count) = counted.get_mut(&window) {
        *count = count.saturating_sub(1);
        if *count == 0 {
          counted.remove(&window);
        }
      }
    }
  });
}

/// Whether the window `identity` lies in an object this shard holds for the region ([`REGION_WINDOWS`]): a landed
/// window's pin (`vcs_store::landed_pin`).
pub(crate) fn region_holds_window(identity: &ContentId) -> bool {
  REGION_WINDOWS.with(|counted| {
    counted
      .try_borrow()
      .is_ok_and(|counted| counted.contains_key(identity))
  })
}

/// Lets `owner`'s region hold of the object `identity` go on this shard, or every owner's (`None`: a holder that is no
/// home, the healer's), and the object's hold with its last owner (module docs, "From the region"; A-727 (d1a)):
/// `NotHeld` where that owner, or any, holds none of it here.
pub(crate) fn let_go_for_region(
  state: &mut ShardState,
  identity: &ContentId,
  owner: Option<HomeOwner>,
) -> Result<(), HomeRefused> {
  let (released, last) = REGION_HELD.with(|held| {
    let Ok(mut held) = held.try_borrow_mut() else {
      return (false, false);
    };
    let releases = match (held.get(identity), owner) {
      (Some(owners), Some(owner)) => owners.contains(&owner),
      (Some(_), None) => true,
      (None, _) => false,
    };
    if releases {
      region_before(&held, identity);
    }
    let released = match (held.get_mut(identity), owner) {
      (Some(owners), Some(owner)) => owners.remove(&owner),
      (Some(owners), None) => {
        owners.clear();
        true
      }
      (None, _) => false,
    };
    let last = released
      && held
        .get(identity)
        .is_some_and(std::collections::BTreeSet::is_empty);
    if last {
      held.remove(identity);
    }
    (released, last)
  });
  if !released {
    return Err(HomeRefused::NotHeld);
  }
  if !last {
    region_changed(*identity, true);
    return Ok(());
  }
  if let Some(content) = state.vcs_stores.content() {
    region_windows_changed(content, identity, false);
  }
  region_changed(*identity, false);
  let_go_here(state, identity)
}

/// The holds this shard keeps of `identity` for the region: 1 where it is the identity's home and keeps it, else 0. A
/// replay counts it apart from its record's own hold (`super::replay_push`): a window restored with its region hold is
/// held by the region, never yet by its record.
pub(crate) fn region_holds_here(identity: &ContentId) -> u64 {
  REGION_HELD.with(|held| {
    u64::from(
      held
        .try_borrow()
        .is_ok_and(|held| held.contains_key(identity)),
    )
  })
}

/// The identities whose first eight bytes, read big-endian, lie in `lo..=hi`: one page of the shard image (A-870).
fn identities_in(lo: u64, hi: u64) -> core::ops::RangeInclusive<ContentId> {
  let mut first = [0u8; slates_vcs::content::IDENTITY_BYTES];
  let mut last = [u8::MAX; slates_vcs::content::IDENTITY_BYTES];
  first[..8].copy_from_slice(&lo.to_be_bytes());
  last[..8].copy_from_slice(&hi.to_be_bytes());
  ContentId(first)..=ContentId(last)
}

/// The region holds whose identities' first eight bytes lie in `lo..=hi`, each with its owners (A-870; A-727 (d1a)).
pub(crate) fn region_held_in(lo: u64, hi: u64) -> Vec<(ContentId, Vec<HomeOwner>)> {
  REGION_HELD.with(|held| {
    held
      .try_borrow()
      .map(|held| {
        held
          .range(identities_in(lo, hi))
          .map(|(identity, owners)| (*identity, owners.iter().copied().collect()))
          .collect()
      })
      .unwrap_or_default()
  })
}

/// [`landed_here`] whose identities' first eight bytes lie in `lo..=hi` (A-870).
pub(crate) fn landed_in(lo: u64, hi: u64) -> Vec<(ContentId, Landed)> {
  LANDED.with(|landed| {
    landed
      .try_borrow()
      .map(|landed| {
        landed
          .range(identities_in(lo, hi))
          .map(|(identity, landed)| (*identity, landed.clone()))
          .collect()
      })
      .unwrap_or_default()
  })
}

/// The windows this shard holds for the region, in identity order: what the healer walks (module docs, "The healer").
pub(crate) fn region_held_here() -> Vec<ContentId> {
  REGION_HELD.with(|held| {
    held
      .try_borrow()
      .map(|held| held.keys().copied().collect())
      .unwrap_or_default()
  })
}

/// Counter: a home message of another version this member refused at once, typed (A-727 (d1a): no mixed-version
/// homing). Format: a counter name.
pub(crate) const VERSION_REFUSED: &str = "vcs_store.homes.version_refused";
/// Format: the status count of a put (or a put's first part) this node answered `Behind` at once, behind the
/// configuration its sender ranked by (A-912; A-902's wait for its own install, answered rather than held).
pub(crate) const BEHIND_ANSWERED: &str = "vcs_store.homes.behind_answered";
/// Format: the status count of a home a region exchange found behind it (its answer `Behind`), asked again once a
/// council round had passed (A-912).
pub(crate) const BEHIND_ASKED_AGAIN: &str = "vcs_store.homes.behind_asked_again";
/// Format: the status count of such a home that kept the put when asked again: A-912's path, run and accepted.
pub(crate) const BEHIND_KEPT_AFTER: &str = "vcs_store.homes.behind_kept_after";
/// Counter: a home's reply refusing this member's version, or one it could not read, its exchange failed at once
/// (A-727 (d1a)). Format: a counter name.
pub(crate) const REPLY_OTHER_VERSION: &str = "vcs_store.homes.reply_other_version";

/// Counter: home exchanges a member served from the region (module docs, "From the region"). Format: a counter name.
pub(crate) const REMOTE_SERVED: &str = "vcs_store.homes.remote_served";

/// A whole put from the region kept at its home ([`serve_remote`]): only where the configuration ranks this node among
/// the object's homes and its bytes hash to their name, answered `Kept` once the home's image holding it is published.
async fn keep_put(
  origin: u16,
  shards: &[u16],
  (local, members, f): (slates_db::HostId, &[slates_db::HostId], usize),
  (identity, owners, bytes): ([u8; 32], Vec<HomeOwner>, Vec<u8>),
  budget_ns: u64,
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::{HomeOutcome, HomeRefusal, admits};
  if let Err(refusal) = admits(&identity, &bytes, (local, members, f)) {
    return HomeOutcome::Refused(refusal);
  }
  match keep_region_at_home(origin, shards, (bytes, owners), budget_ns).await {
    Ok((_, ticket)) if published(origin, ticket, budget_ns).await => HomeOutcome::Kept {
      generation: ticket.generation,
    },
    Ok(_) | Err(HomeRefused::Unanswered) => HomeOutcome::Refused(HomeRefusal::Unanswered),
    Err(HomeRefused::PastWindow) => HomeOutcome::Refused(HomeRefusal::Misnamed),
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
    let bytes = match read(origin, shards, identity, budget_ns).await {
      Ok(bytes) => bytes,
      Err(refused) => return HomeOutcome::Refused(refused.wire()),
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

/// Whether this node is behind the configuration a put's `sender` ranked the window's homes by, `carried` (A-902's rule):
/// its own configuration does not rank it among the window's homes, and either `carried` is newer than its own installed
/// version or `sender` is not among its members (a joiner's configuration before the region's comes from its own
/// council, whose versions are not the region's). Its installed version where it is behind; `None` where it is not (a
/// node ranked a home, or at the sender's version with the sender among its members, answers as it stands: a true
/// `NotAHome` or a stale sender at once), and off a shard.
fn behind_its_sender(
  identity: &[u8; 32],
  (sender, carried): (slates_db::HostId, u64),
) -> Option<u64> {
  crate::state::with_state(|state| {
    let local = state.fleet.host();
    let members = state.fleet.members();
    let installed = state.fleet.configuration().version;
    let f = usize::try_from(state.fleet.configuration().quorum.f).unwrap_or(usize::MAX);
    let ranked = slates_db::identity_homes::homes(identity, members, f).contains(&local);
    (!ranked && (carried > installed || !members.contains(&sender))).then_some(installed)
  })
  .flatten()
}

/// The answer a put (or a put's first part) from `sender` gets from a node behind the configuration it was ranked by
/// (A-912, main 01a20182's ruling of 2026-11-24): `Behind` with this node's installed version, at once, counted
/// [`BEHIND_ANSWERED`]. A-902 had the node hold the put until its own install, but that install comes with the council's
/// next round, whose append to this node rides the record session the sender's put held: neither moved until the put's
/// budget ran out. The sender asks again once a council round has passed (`exchange`). `None` for any other message, or
/// a node not behind.
fn answered_behind(
  message: &slates_cluster::homes::HomeMessage,
  sender: slates_db::HostId,
) -> Option<Vec<u8>> {
  use slates_cluster::homes::{HomeMessage, HomeOutcome};
  let (identity, carried) = match message {
    HomeMessage::Put {
      identity,
      configuration,
      ..
    }
    | HomeMessage::PutPart {
      identity,
      configuration,
      offset: 0,
      ..
    } => (identity, *configuration),
    _ => return None,
  };
  let installed = behind_its_sender(identity, (sender, carried))?;
  let _ = crate::state::with_state(|state| state.count(BEHIND_ANSWERED, 1));
  Some(
    HomeMessage::Answer {
      identity: *identity,
      outcome: HomeOutcome::Behind { installed },
    }
    .encode(),
  )
}

/// Serves one home exchange from another member of the region on shard `origin` (module docs, "From the region";
/// `slates_cluster::homes`): a put kept only where the configuration this node knows ranks it among the window's homes
/// and its bytes hash to their name, on the node's own home shard for the window, and answered `Kept` only once that
/// shard's image holding it is published ([`published`], A-727's barrier); a read answered with the bytes; a let-go
/// answered once its holder is let go. Each wait is judged in its target's own time within `budget_ns`. Malformed bytes
/// answer nothing, as the session's other streams do. A put (or a put's first part) from `sender` that this node is
/// behind is answered `Behind` at once ([`answered_behind`], A-912), never held for this node's install.
pub(crate) async fn serve_remote(
  (origin, sender): (u16, slates_db::HostId),
  request: &[u8],
  budget_ns: u64,
) -> Vec<u8> {
  let delay_ns = ANSWER_DELAY_NS.with(std::cell::Cell::get);
  if delay_ns > 0 {
    let _ = slates_rt::futures::sleep(delay_ns).await;
  }
  let message = match decoded(request) {
    Ok(message) => message,
    Err(answer) => return answer,
  };
  if let Some(behind) = answered_behind(&message, sender) {
    return behind;
  }
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
  outcome_of(message, (origin, &shards), (local, &members, f), budget_ns)
    .await
    .map_or_else(Vec::new, |(identity, outcome)| {
      slates_cluster::homes::HomeMessage::Answer { identity, outcome }.encode()
    })
}

/// [`serve_remote`]'s outcome for a decoded `message` and the identity it answers for, served on shard `origin` with what
/// this node knows of the region; `None` for an answer, which nothing answers.
async fn outcome_of(
  message: slates_cluster::homes::HomeMessage,
  (origin, shards): (u16, &[u16]),
  (local, members, f): (slates_db::HostId, &[slates_db::HostId], usize),
  budget_ns: u64,
) -> Option<([u8; 32], slates_cluster::homes::HomeOutcome)> {
  use slates_cluster::homes::{HomeMessage, HomeOutcome};
  let answered = match message {
    HomeMessage::Put {
      identity,
      owners,
      bytes,
      ..
    } => (
      identity,
      keep_put(
        origin,
        shards,
        (local, members, f),
        (identity, owners, bytes),
        budget_ns,
      )
      .await,
    ),
    HomeMessage::PutPart {
      identity,
      owners,
      total,
      offset,
      bytes,
      ..
    } => (
      identity,
      part_answer(
        (origin, shards),
        (local, members, f),
        (identity, owners, (total, offset)),
        (bytes, budget_ns),
      )
      .await,
    ),
    HomeMessage::ReadPart { identity, offset } => (
      identity,
      read_part(origin, shards, ContentId(identity), offset, budget_ns).await,
    ),
    HomeMessage::Read { identity } => (
      identity,
      read_answer(origin, shards, identity, budget_ns).await,
    ),
    HomeMessage::LetGo { identity, owner } => (
      identity,
      let_go_answer(origin, shards, (identity, owner), budget_ns).await,
    ),
    HomeMessage::Moved { identity, from, to } => (
      identity,
      moved_answer(origin, shards, (identity, (from, to)), budget_ns).await,
    ),
    // Answered by the shard holding the attempt's repository, from its records (A-727 (d1a-3)).
    HomeMessage::Resolve { identity, attempt } => (
      identity,
      resolve_answer(origin, shards, (identity, attempt), budget_ns).await,
    ),
    // A repository rebuilt on its successor (A-727 (d1a-3), main 01a20182's ruling of 2026-11-24 01:36): the answer a
    // resolution asks for exists now, so a pass is owed here, on this control shard; owing it twice is harmless.
    HomeMessage::Rebuilt { .. } => {
      resolve_owed();
      let _ = crate::state::with_state(|state| state.count(REBUILT_HEARD, 1));
      ([0; 32], HomeOutcome::Owed)
    }
    HomeMessage::Answer { .. } => return None,
  };
  Some(answered)
}

/// [`serve_remote`]'s answer to a read: the bytes this node's home holds, or the refusal typed for the wire.
async fn read_answer(
  origin: u16,
  shards: &[u16],
  identity: [u8; 32],
  budget_ns: u64,
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::HomeOutcome;
  match read(origin, shards, ContentId(identity), budget_ns).await {
    Ok(bytes) => HomeOutcome::Bytes(bytes),
    Err(refused) => HomeOutcome::Refused(refused.wire()),
  }
}

/// A home message read from `request` ([`serve_remote`]), or what to answer at once instead: nothing for bytes no message
/// reads as, or for any message while this member plays one of another version (a test's); another version's message (a
/// peer of another build) refused at once and typed, so its sender's homing fails at once rather than at its deadline
/// (A-727 (d1a); a version-1 peer reads no version-2 answer, and fails as at once).
fn decoded(request: &[u8]) -> Result<slates_cluster::homes::HomeMessage, Vec<u8>> {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal, HomeWireError};
  if OTHER_VERSION.with(std::cell::Cell::get) {
    return Err(Vec::new());
  }
  match HomeMessage::decode(request) {
    Ok(message) => Ok(message),
    Err(HomeWireError::Version { .. }) => {
      let _ = crate::state::with_state(|state| state.count(VERSION_REFUSED, 1));
      Err(
        HomeMessage::Answer {
          identity: [0; 32],
          outcome: HomeOutcome::Refused(HomeRefusal::Version),
        }
        .encode(),
      )
    }
    Err(_) => Err(Vec::new()),
  }
}

/// [`serve_remote`]'s answer to a put's part (A-724 piece 8c): staged where it may be ([`stage_part`]), refused by kind
/// where it may not, `Taken` with what is staged until the object is whole, then kept as a whole put is ([`keep_put`]).
async fn part_answer(
  (origin, shards): (u16, &[u16]),
  (local, members, f): (slates_db::HostId, &[slates_db::HostId], usize),
  (identity, owners, (total, offset)): ([u8; 32], Vec<HomeOwner>, (u64, u64)),
  (bytes, budget_ns): (Vec<u8>, u64),
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::HomeOutcome;
  match stage_part((local, members, f), (identity, total, offset), bytes) {
    Staged::Refused(refusal) => HomeOutcome::Refused(refusal),
    Staged::Taken(received) => HomeOutcome::Taken { received },
    Staged::Whole(bytes) => {
      keep_put(
        origin,
        shards,
        (local, members, f),
        (identity, owners, bytes),
        budget_ns,
      )
      .await
    }
  }
}

/// [`serve_remote`]'s answer to a resolution of `attempt`'s hold of `identity` (A-727 (d1a-3)): whether a record of its
/// repository names it, from the shard holding the repository here ([`answer_resolve`]); `NotHeld` where none does,
/// `Unanswered` where the attempt is still in flight or the shard did not answer.
async fn resolve_answer(
  origin: u16,
  shards: &[u16],
  (identity, attempt): ([u8; 32], HomeOwner),
  budget_ns: u64,
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::{HomeOutcome, HomeRefusal};
  match answer_resolve(origin, shards, (ContentId(identity), attempt), budget_ns).await {
    Resolution::Named(named) => HomeOutcome::Named { named },
    Resolution::NotHeld => HomeOutcome::Refused(HomeRefusal::NotHeld),
    Resolution::Unresolved => HomeOutcome::Refused(HomeRefusal::Unanswered),
  }
}

/// [`serve_remote`]'s answer to a let-go of `identity` for `owner` (every owner where none), from its home among
/// `shards`, asked from shard `origin`: `LetGo` once its holder is let go.
async fn let_go_answer(
  origin: u16,
  shards: &[u16],
  (identity, owner): ([u8; 32], Option<HomeOwner>),
  budget_ns: u64,
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::{HomeOutcome, HomeRefusal};
  match let_go_region_at_home(origin, shards, ContentId(identity), owner, budget_ns).await {
    Ok(()) => HomeOutcome::LetGo,
    Err(HomeRefused::NotHeld) => HomeOutcome::Refused(HomeRefusal::NotHeld),
    Err(_) => HomeOutcome::Refused(HomeRefusal::Unanswered),
  }
}

/// [`serve_remote`]'s answer to a move of `identity`'s hold between `owners` (from, to), from its home among `shards`,
/// asked from shard `origin`: `Moved` only once that home's image holding the move is published ([`published`], A-727's
/// barrier).
async fn moved_answer(
  origin: u16,
  shards: &[u16],
  (identity, owners): ([u8; 32], (HomeOwner, HomeOwner)),
  budget_ns: u64,
) -> slates_cluster::homes::HomeOutcome {
  use slates_cluster::homes::{HomeOutcome, HomeRefusal};
  match move_region_at_home(origin, shards, ContentId(identity), owners, budget_ns).await {
    Ok(ticket) if published(origin, ticket, budget_ns).await => HomeOutcome::Moved,
    Ok(_) => HomeOutcome::Refused(HomeRefusal::Unanswered),
    Err(HomeRefused::NotHeld) => HomeOutcome::Refused(HomeRefusal::NotHeld),
    Err(_) => HomeOutcome::Refused(HomeRefusal::Unanswered),
  }
}

/// [`keep_for_region`] on the home shard among `shards` of the window `bytes`, asked from shard `origin`.
async fn keep_region_at_home(
  origin: u16,
  shards: &[u16],
  (bytes, owners): (Vec<u8>, Vec<HomeOwner>),
  budget_ns: u64,
) -> Result<(ContentId, HomeTicket), HomeRefused> {
  let identity = ContentId::of(&bytes);
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| keep_for_region(state, &bytes, &owners),
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
  owner: Option<HomeOwner>,
  budget_ns: u64,
) -> Result<(), HomeRefused> {
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| let_go_for_region(state, &identity, owner),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)?
}

/// [`move_region_owner`] on the home shard among `shards` of the object `identity`, asked from shard `origin`: the ticket
/// of the image the move is first durable in.
async fn move_region_at_home(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  owners: (HomeOwner, HomeOwner),
  budget_ns: u64,
) -> Result<HomeTicket, HomeRefused> {
  let target = home_of(shards, &identity).ok_or(HomeRefused::Unanswered)?;
  crate::xshard::call_in_shard_time(
    origin,
    target,
    move |state| move_region_owner(&identity, owners).map(|()| ticket_here(state.shard)),
    budget_ns,
  )
  .await
  .ok_or(HomeRefused::Unanswered)?
}

std::thread_local! {
  /// This shard's attempts numbered so far ([`attempt_owner`]).
  static ATTEMPTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
  /// The owners' moves and let-gos this shard owes the region (A-727 (d1a)): an attempt's entry committed (its holds
  /// moved to its records) or refused (its holds let go), each made where the step is and settled off it by one task
  /// ([`settle_owed`]) a turn at a time. Bounded by the holds its attempts in flight took.
  static SETTLES: std::cell::RefCell<std::collections::VecDeque<Settle>> =
    const { std::cell::RefCell::new(std::collections::VecDeque::new()) };
  /// Whether the task settling [`SETTLES`] runs.
  static SETTLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// The attempts of this process in flight on this shard, by number ([`attempt_owner`], [`attempt_ended`]): a resolution
  /// of one of them is answered unresolved, never released under a live attempt (A-727 (d1a-3)). Bounded by the attempts
  /// in flight, one a repository's landing turn.
  static LIVE_ATTEMPTS: std::cell::RefCell<std::collections::BTreeSet<u64>> =
    const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
  /// Whether this node owes a pass resolving the attempts whose process ended ([`resolve_ended`]): set by a member's
  /// death, a member's new process, and this node's own start (A-727 (d1a-3)); cleared as a pass begins.
  static RESOLVE_OWED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
  /// Test support (a fleet test's kill point, A-727 (d1a-3)): whether this shard's settles wait unsent, so a stop leaves
  /// them unsent, as a crash between an attempt's end and its homes' messages does ([`hold_settles`]).
  static SETTLES_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// Test support (A-727 (d1a)): whether this node answers every home message as a node of another home-wire version
  /// does, nothing it can read ([`answer_as_another_version`]).
  static OTHER_VERSION: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// Test support (A-727 (d1a-3)): whether this node answers every resolution unresolved, as a repository's holder this
  /// node cannot reach does ([`hold_resolutions`]).
  static RESOLUTIONS_HELD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
  /// Test support (A-907): how long this node waits before it serves each home message from the region
  /// ([`delay_answers`]): a home slower than its sender's round, as one waiting for its own install is (A-902).
  static ANSWER_DELAY_NS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Test support (A-907): this node waits `delay_ns` before it serves each home message from the region while `delay_ns`
/// is not 0, so it answers after its sender's round has stalled, as a home waiting for its own install does (A-902).
pub(crate) fn delay_answers(delay_ns: u64) {
  ANSWER_DELAY_NS.with(|delay| delay.set(delay_ns));
}

/// Test support (A-727 (d1a-3)): this node's resolutions all answer unresolved while `held`, as when the repository's
/// holder is unreachable (a takeover under way): the holds stay, counted kept, for the next pass.
pub(crate) fn hold_resolutions(held: bool) {
  RESOLUTIONS_HELD.with(|resolutions| resolutions.set(held));
}

/// Test support, a fleet test's kill point (A-727 (d1a-3)): this shard's settles wait unsent while `held`, so the
/// daemon's stop leaves an attempt's moves and let-gos unsent, as a crash after its end and before its homes heard does.
/// Released, what waited is settled.
pub(crate) fn hold_settles(held: bool) {
  SETTLES_HELD.with(|settles| settles.set(held));
  if !held
    && SETTLES.with(|settles| {
      settles
        .try_borrow()
        .is_ok_and(|settles| !settles.is_empty())
    })
  {
    let owed = SETTLES.with(|settles| settles.try_borrow_mut().ok()?.pop_front());
    if let Some(settle) = owed {
      settle_later(settle);
    }
  }
}

/// Test support (A-727 (d1a)): this node answers every home message empty while `on`, as a node of the previous
/// home-wire version answers one of this version's (its decode refuses it, and every record-session request is
/// answered): the version mismatch a mixed region meets.
pub(crate) fn answer_as_another_version(on: bool) {
  OTHER_VERSION.with(|other| other.set(on));
}

/// Test support (A-727 (d1a-3)): every hold this shard keeps for the region, with its owners.
pub(crate) fn region_holds_owned() -> Vec<([u8; 32], Vec<HomeOwner>)> {
  REGION_HELD.with(|held| {
    held
      .try_borrow()
      .map(|held| {
        held
          .iter()
          .map(|(identity, owners)| (identity.0, owners.iter().copied().collect()))
          .collect()
      })
      .unwrap_or_default()
  })
}

/// Format: the bits an attempt's number keeps for its shard's count, below the shard's own number (a shard numbers its
/// own attempts, so two shards of one boot never make the same one).
const ATTEMPT_SHARD_SHIFT: u32 = 48;

/// A fresh owner for an attempt of repository `repository` on this shard (A-727 (d1a)): this member, its process (the
/// process's nonce, fresh at every start: a member's SWIM incarnation starts over at each start, and a warm restart keeps
/// its boot nonce) and a number no other attempt of this process takes.
pub(crate) fn attempt_owner(state: &ShardState, repository: [u8; 16]) -> HomeOwner {
  let count = ATTEMPTS.with(|attempts| {
    let count = attempts.get();
    attempts.set(count.wrapping_add(1));
    count
  });
  let shard = u64::from(state.shard)
    .checked_shl(ATTEMPT_SHARD_SHIFT)
    .unwrap_or(0);
  let attempt = shard | (count & ((1u64 << ATTEMPT_SHARD_SHIFT) - 1));
  LIVE_ATTEMPTS.with(|live| {
    if let Ok(mut live) = live.try_borrow_mut() {
      live.insert(attempt);
    }
  });
  HomeOwner::Attempt {
    repository,
    node: state.fleet.host(),
    process: state.member_process_nonce,
    attempt,
  }
}

/// The attempt `owner` ended on this shard (A-727 (d1a-3)): committed or refused, what it owes settled
/// ([`settle_later`]); a resolution of it is answered from the records from now on.
pub(crate) fn attempt_ended(owner: &HomeOwner) {
  if let HomeOwner::Attempt { attempt, .. } = owner {
    LIVE_ATTEMPTS.with(|live| {
      if let Ok(mut live) = live.try_borrow_mut() {
        live.remove(attempt);
      }
    });
  }
}

/// Marks a resolution pass owed ([`resolve_ended`]): a member died or announced a new process (A-727 (d1a-3)).
pub(crate) fn resolve_owed() {
  RESOLVE_OWED.with(|owed| owed.set(true));
}

/// An attempt's holds in the region (A-727 (d1a), (d1a-3)): the owner they are for, made at the attempt's first homing,
/// and the objects homed for it. Ended once, by its record's commit ([`AttemptHolds::committed`]: each moved to the
/// records) or its refusal ([`AttemptHolds::released`]: each let go); dropped before either (its push's task gone), it
/// is released as a refusal would be, so no hold outlives its attempt unowed. Bounded by the objects one attempt homes
/// (a push's names and pack).
#[derive(Debug, Default)]
pub(crate) struct AttemptHolds {
  /// The attempt, once its homing began.
  owner: Option<HomeOwner>,
  /// The objects homed for it.
  homed: Vec<ContentId>,
}

impl AttemptHolds {
  /// The attempt's owner, made at the first ask ([`attempt_owner`]).
  pub(crate) fn owner(&mut self, state: &ShardState, repository: [u8; 16]) -> HomeOwner {
    *self
      .owner
      .get_or_insert_with(|| attempt_owner(state, repository))
  }

  /// The attempt's owner, once made.
  pub(crate) fn owner_now(&self) -> Option<HomeOwner> {
    self.owner
  }

  /// `identity` homed for the attempt.
  pub(crate) fn homed(&mut self, identity: ContentId) {
    if !self.homed.contains(&identity) {
      self.homed.push(identity);
    }
  }

  /// The attempt's hold of `identity` let go at every home that kept it: its homing fell short.
  pub(crate) fn release_one(&self, identity: ContentId) {
    if let Some(owner) = self.owner {
      settle_later(Settle::Release { identity, owner });
    }
  }

  /// The attempt's record committed: each of its holds moved to repository `repository`'s records, the attempt ended.
  pub(crate) fn committed(&mut self, repository: [u8; 16]) {
    let Some(from) = self.owner.take() else {
      return;
    };
    let to = HomeOwner::Record { repository };
    for identity in core::mem::take(&mut self.homed) {
      settle_later(Settle::Move { identity, from, to });
    }
    attempt_ended(&from);
  }

  /// The attempt refused: each of its holds let go, the attempt ended.
  pub(crate) fn released(&mut self) {
    let Some(owner) = self.owner.take() else {
      return;
    };
    for identity in core::mem::take(&mut self.homed) {
      settle_later(Settle::Release { identity, owner });
    }
    attempt_ended(&owner);
  }
}

impl Drop for AttemptHolds {
  fn drop(&mut self) {
    self.released();
  }
}

/// What an attempt owes its holds' homes once it ends (A-727 (d1a)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Settle {
  /// Its entry committed: the hold of `identity` moved from the attempt to the records naming it.
  Move {
    /// The object.
    identity: ContentId,
    /// The attempt.
    from: HomeOwner,
    /// The records.
    to: HomeOwner,
  },
  /// It was refused: the attempt's hold of `identity` let go.
  Release {
    /// The object.
    identity: ContentId,
    /// The attempt.
    owner: HomeOwner,
  },
}

/// Counter: an attempt's move or let-go every home of its object answered (A-727 (d1a)). Format: a counter name.
pub(crate) const SETTLED: &str = "vcs_store.homes.settled";
/// Counter: one some home did not answer, its hold left to the resolution of the attempt (A-727 (d1a-3)). Format: a
/// counter name.
pub(crate) const SETTLE_UNANSWERED: &str = "vcs_store.homes.settle_unanswered";
/// Counter: the task settling what this shard owes the region not started, or started and not detached
/// ([`settle_later`]): its settles wait for the next settle to start one. Format: a counter name.
pub(crate) const SETTLE_TASK_REFUSED: &str = "vcs_store.homes.settle_task_refused";

/// Owes `settle` to the region (A-727 (d1a)), and starts the task that settles what this shard owes where none runs.
pub(crate) fn settle_later(settle: Settle) {
  SETTLES.with(|settles| {
    if let Ok(mut settles) = settles.try_borrow_mut() {
      settles.push_back(settle);
    }
  });
  if SETTLING.with(|settling| settling.replace(true)) {
    return;
  }
  // Detached as it is spawned, since nobody joins it: a joinable task holds its slot once it ends. Refused (no task to
  // spare, or the detach refused): counted, and what is owed is settled by the next settle that starts one.
  if slates_rt::futures::spawn_detached(settle_owed()).is_err() {
    SETTLING.with(|settling| settling.set(false));
    crate::state::with_state(|state| state.count(SETTLE_TASK_REFUSED, 1));
  }
}

/// The task settling what this shard owes the region ([`SETTLES`]), one a turn, counted; it ends once nothing is owed or
/// the shard's state is gone.
async fn settle_owed() {
  loop {
    if SETTLES_HELD.with(std::cell::Cell::get) {
      break;
    }
    let next = SETTLES.with(|settles| settles.try_borrow_mut().ok()?.pop_front());
    let Some(settle) = next else {
      break;
    };
    let Some((origin, shards)) =
      crate::state::with_state(|state| (state.shard, state.shards.clone()))
    else {
      break;
    };
    let settled = settle_in_region((origin, &shards), settle, crate::fleet::live_budget()).await;
    let _ = crate::state::with_state(|state| {
      state.count(if settled { SETTLED } else { SETTLE_UNANSWERED }, 1);
    });
    slates_rt::futures::yield_now().await;
  }
  SETTLING.with(|settling| settling.set(false));
}

/// Settles `settle` at every home of its object (A-727 (d1a)): this node's home shard where this node is one, and each
/// other home by its own message ([`HomeMessage::Moved`](slates_cluster::homes::HomeMessage::Moved),
/// [`HomeMessage::LetGo`](slates_cluster::homes::HomeMessage::LetGo)). Whether every home answered it (a home that
/// holds nothing for the owner answers too: there is nothing to settle there).
async fn settle_in_region(
  (origin, shards): (u16, &[u16]),
  settle: Settle,
  budget: slates_cluster::CommitBudget,
) -> bool {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal};
  let Some((local, members, f)) = crate::state::with_state(|state| {
    (
      state.fleet.host(),
      state.fleet.members().to_vec(),
      usize::try_from(state.fleet.configuration().quorum.f).unwrap_or(usize::MAX),
    )
  }) else {
    return false;
  };
  let budget_ns = budget.max_deadline_ns();
  let identity = match settle {
    Settle::Move { identity, .. } | Settle::Release { identity, .. } => identity,
  };
  let homes = slates_db::identity_homes::homes(&identity.0, &members, f);
  let mut answered = true;
  if homes.contains(&local) {
    let here = match settle {
      Settle::Move { from, to, .. } => {
        move_region_at_home(origin, shards, identity, (from, to), budget_ns)
          .await
          .map(drop)
      }
      Settle::Release { owner, .. } => {
        let_go_region_at_home(origin, shards, identity, Some(owner), budget_ns).await
      }
    };
    answered &= matches!(here, Ok(()) | Err(HomeRefused::NotHeld));
  }
  let others: Vec<slates_db::HostId> = homes.into_iter().filter(|home| *home != local).collect();
  if others.is_empty() {
    return answered;
  }
  let request = match settle {
    Settle::Move { identity, from, to } => HomeMessage::Moved {
      identity: identity.0,
      from,
      to,
    },
    Settle::Release { identity, owner } => HomeMessage::LetGo {
      identity: identity.0,
      owner: Some(owner),
    },
  }
  .encode();
  let control = shards.first().copied().unwrap_or(origin);
  let count = others.len();
  let replies = crate::xshard::task_in_shard_time(
    origin,
    control,
    move || async move { exchange(&others, &request, budget).await },
    budget_ns,
  )
  .await
  .unwrap_or_default();
  let settled = replies
    .iter()
    .filter(|(_, reply)| {
      matches!(
        HomeMessage::decode(reply),
        Ok(HomeMessage::Answer {
          identity: answered,
          outcome: HomeOutcome::Moved | HomeOutcome::LetGo | HomeOutcome::Refused(HomeRefusal::NotHeld),
        }) if answered == identity.0
      )
    })
    .count();
  answered && settled == count
}

/// Test support (A-727 (d1a-3)): `owner` joined to the region hold of `identity` this shard keeps, as a keep for it would.
#[cfg(test)]
pub(crate) fn join_owner(identity: &ContentId, owner: HomeOwner) {
  join_owners(identity, &[owner]);
}

/// Test support: the owner a test's region hold is kept for where no attempt or record of its own is about (A-727 (d1a)):
/// a repository's records, the repository all zeros.
#[cfg(test)]
pub(crate) const TEST_OWNER: HomeOwner = HomeOwner::Record {
  repository: [0; 16],
};

/// What a resolution of an attempt's hold found (A-727 (d1a-3), main 01a20182's rulings of 2026-11-23, (u)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
  /// The shard holding the attempt's repository answered: whether a record of it names the object.
  Named(bool),
  /// No shard of this node holds the repository (its placement moved: the asker routes again at its next pass).
  NotHeld,
  /// Not answerable now: the attempt is still in flight here, or the shard did not answer in time.
  Unresolved,
}

/// Answers a resolution of `attempt`'s hold of `identity` on this node (A-727 (d1a-3)): on the shard the attempt's
/// repository's id routes to (its partition, `slates_db::catalog::RepositoryId::partition`), from the repository's
/// records, a slice of them a turn ([`records_name`]). Asked from shard `origin` of `shards`.
async fn answer_resolve(
  origin: u16,
  shards: &[u16],
  (identity, attempt): (ContentId, HomeOwner),
  budget_ns: u64,
) -> Resolution {
  let repository = slates_db::catalog::RepositoryId {
    bytes: attempt.repository(),
  };
  let Some(shard) = shards.get(usize::from(repository.partition())).copied() else {
    return Resolution::NotHeld;
  };
  let mut from = 0usize;
  loop {
    let step = crate::xshard::call_in_shard_time(
      origin,
      shard,
      move |state| resolve_step(state, (repository, identity, attempt), from),
      budget_ns,
    )
    .await;
    match step {
      Some(Walked::Done(resolution)) => return resolution,
      Some(Walked::More(next)) => from = next,
      None => return Resolution::Unresolved,
    }
    slates_rt::futures::yield_now().await;
  }
}

/// How far a slice of a resolution's walk of a repository's records came ([`resolve_step`]).
enum Walked {
  /// The answer.
  Done(Resolution),
  /// The walk goes on from this entry.
  More(usize),
}

/// One slice of a resolution on the shard holding `repository` (A-727 (d1a-3)): unresolved while the attempt is still
/// in flight here (this member, this process, one of this shard's live attempts); else whether a record of the
/// repository names `identity`, walked from entry `from` until the shard's step quantum is spent.
fn resolve_step(
  state: &mut ShardState,
  (repository, identity, attempt): (slates_db::catalog::RepositoryId, ContentId, HomeOwner),
  from: usize,
) -> Walked {
  if let HomeOwner::Attempt {
    node,
    process,
    attempt: number,
    ..
  } = attempt
    && node == state.fleet.host()
    && process == state.member_process_nonce
    && LIVE_ATTEMPTS.with(|live| {
      live
        .try_borrow()
        .map_or(true, |live| live.contains(&number))
    })
  {
    return Walked::Done(Resolution::Unresolved);
  }
  let partition = state.db.partition();
  let repositories = partition.repositories();
  let Some(record) = repositories.repository(repository) else {
    return Walked::Done(Resolution::NotHeld);
  };
  match record.tool {
    slates_db::catalog::VcsTool::Git => {
      let Some(format) = super::git_format(record.format) else {
        return Walked::Done(Resolution::Named(false));
      };
      let quantum_ns = state.config.step_quantum_ns();
      let began = slates_rt::futures::now_ns();
      for (at, bytes) in repositories.objects(repository).enumerate().skip(from) {
        if let Ok(record) = super::record::Record::decode(bytes, format)
          && [record.named, record.packed]
            .into_iter()
            .flatten()
            .any(|(named, _)| named == identity)
        {
          return Walked::Done(Resolution::Named(true));
        }
        if slates_rt::futures::now_ns().saturating_sub(began) >= quantum_ns {
          return Walked::More(at.saturating_add(1));
        }
      }
      Walked::Done(Resolution::Named(false))
    }
    // Only git homes in the region so far (A-727 (d1b) brings Subversion's texts): no other tool's record names a hold.
    _ => Walked::Done(Resolution::Named(false)),
  }
}

std::thread_local! {
  /// The members owed word of a repository rebuilt here (A-727 (d1a-3), main 01a20182's ruling of 2026-11-24 01:36): each
  /// (repository, member) until the member answers that it owes itself a pass ([`announce_rebuilt`]). Bound: the
  /// configuration's members a rebuild, each entry dropped once answered or once its member leaves the configuration.
  static REBUILT_OWED: std::cell::RefCell<std::collections::BTreeSet<([u8; 16], slates_db::HostId)>> =
    const { std::cell::RefCell::new(std::collections::BTreeSet::new()) };
}

/// A repository taken over was rebuilt here (A-727 (d1a-3), main 01a20182's ruling of 2026-11-24 01:36), called on the
/// control shard: the answer a resolution of its attempts' holds asks for exists from now on, so a resolution pass is
/// owed here, and each other member of the configuration is owed word of it ([`announce_rebuilt`]), since the homes
/// holding its attempts' objects are on any of them. Before, nothing owed a pass after the rebuild: a pass the death or
/// the configuration owed ran before it and kept every hold, and the holds stayed an attempt's.
pub(crate) fn rebuilt(
  repository: [u8; 16],
  (local, members): (slates_db::HostId, &[slates_db::HostId]),
) {
  resolve_owed();
  REBUILT_OWED.with(|owed| {
    if let Ok(mut owed) = owed.try_borrow_mut() {
      owed.extend(
        members
          .iter()
          .filter(|member| **member != local)
          .map(|member| (repository, *member)),
      );
    }
  });
}

/// Tells each member owed word of a rebuild here ([`rebuilt`]) that the repository was rebuilt, from the control shard's
/// period (`fleet`), never on a timer: an entry its member answers is dropped (counted [`REBUILT_TOLD`]); one it does not is
/// kept for the next period and counted ([`REBUILT_UNANSWERED`]), so a lost message is asked again, never dropped; one
/// whose member is no member of the configuration any more is dropped.
pub(crate) async fn announce_rebuilt(
  origin: u16,
  shards: &[u16],
  budget: slates_cluster::CommitBudget,
) {
  use slates_cluster::homes::{HomeMessage, HomeOutcome};
  let owed: Vec<([u8; 16], slates_db::HostId)> = REBUILT_OWED.with(|owed| {
    owed
      .try_borrow()
      .map(|owed| owed.iter().copied().collect())
      .unwrap_or_default()
  });
  if owed.is_empty() {
    return;
  }
  let members =
    crate::state::with_state(|state| state.fleet.members().to_vec()).unwrap_or_default();
  for (repository, member) in owed {
    let answered = if members.contains(&member) {
      let request = HomeMessage::Rebuilt { repository }.encode();
      let replies = crate::xshard::task_in_shard_time(
        origin,
        shards.first().copied().unwrap_or(origin),
        move || async move { exchange(&[member], &request, budget).await },
        budget.max_deadline_ns(),
      )
      .await
      .unwrap_or_default();
      let told = replies.iter().any(|(_, reply)| {
        matches!(
          HomeMessage::decode(reply),
          Ok(HomeMessage::Answer {
            outcome: HomeOutcome::Owed,
            ..
          })
        )
      });
      let _ = crate::state::with_state(|state| {
        state.count(
          if told {
            REBUILT_TOLD
          } else {
            REBUILT_UNANSWERED
          },
          1,
        );
      });
      told
    } else {
      true
    };
    if answered {
      REBUILT_OWED.with(|owed| {
        if let Ok(mut owed) = owed.try_borrow_mut() {
          owed.remove(&(repository, member));
        }
      });
    }
  }
}

/// Counter: a member told that a repository was rebuilt here, its answer heard (A-727 (d1a-3)). Format: a counter name.
pub(crate) const REBUILT_TOLD: &str = "vcs_store.homes.rebuilt_told";
/// Counter: a member owed word of a rebuild here that did not answer: asked again next period. Format: a counter name.
pub(crate) const REBUILT_UNANSWERED: &str = "vcs_store.homes.rebuilt_unanswered";
/// Counter: a rebuild elsewhere heard here, a resolution pass owed by it. Format: a counter name.
pub(crate) const REBUILT_HEARD: &str = "vcs_store.homes.rebuilt_heard";

/// Counter: an attempt's hold a resolution moved to its repository's records (A-727 (d1a-3)). Format: a counter name.
pub(crate) const RESOLVED_RECORD: &str = "vcs_store.homes.resolved_record";
/// Counter: an attempt's hold a resolution let go, no record naming its object. Format: a counter name.
pub(crate) const RESOLVED_LET_GO: &str = "vcs_store.homes.resolved_let_go";
/// Counter: an attempt's hold a resolution left held, unanswered (its repository's holder unknown or unreachable, or the
/// attempt still in flight): asked again at the next pass. Format: a counter name.
pub(crate) const RESOLVE_KEPT: &str = "vcs_store.homes.resolve_kept";

/// The control shard's resolution of the attempts whose process ended (A-727 (d1a-3), main's ruling (u)): where one is
/// owed ([`resolve_owed`]: a member's death or new process, this node's start) or the healer begins a pass at a new
/// configuration ([`heal_region`] marks one owed), every hold this node's home shards keep for an attempt whose member is dead or runs another
/// process now is resolved by its repository's current holder ([`resolve_one`]), never on a timer. Driven from the
/// region's period turn (`fleet`), within `budget`.
pub(crate) async fn resolve_ended(
  origin: u16,
  shards: &[u16],
  budget: slates_cluster::CommitBudget,
) {
  if !RESOLVE_OWED.with(|owed| owed.replace(false)) {
    return;
  }
  let budget_ns = budget.max_deadline_ns();
  for shard in shards.iter().copied() {
    let attempts =
      crate::xshard::call_in_shard_time(origin, shard, |_| attempt_holds_here(), budget_ns)
        .await
        .unwrap_or_default();
    for (identity, attempt) in attempts {
      let ended = crate::state::with_state(|state| process_ended(state, &attempt)).unwrap_or(false);
      if ended {
        resolve_one((origin, shards, shard), (identity, attempt), budget).await;
      }
    }
  }
}

/// Every hold this shard keeps for an attempt, with that attempt.
fn attempt_holds_here() -> Vec<(ContentId, HomeOwner)> {
  REGION_HELD.with(|held| {
    held
      .try_borrow()
      .map(|held| {
        held
          .iter()
          .flat_map(|(identity, owners)| {
            owners
              .iter()
              .filter(|owner| matches!(owner, HomeOwner::Attempt { .. }))
              .map(move |owner| (*identity, *owner))
          })
          .collect()
      })
      .unwrap_or_default()
  })
}

/// Whether the process an attempt ran in has ended, as this control shard knows (A-727 (d1a-3)): this member's own
/// earlier process; another member dead, or announcing another process (`member_task`'s announcements). A member this
/// node has heard nothing of is not judged ended.
fn process_ended(state: &ShardState, attempt: &HomeOwner) -> bool {
  let HomeOwner::Attempt { node, process, .. } = attempt else {
    return false;
  };
  if *node == state.fleet.host() {
    return *process != state.member_process_nonce;
  }
  let dead = state
    .fleet
    .membership()
    .dead()
    .iter()
    .any(|(member, _)| member == node);
  dead
    || state
      .plane
      .announced_from
      .get(node)
      .is_some_and(|current| current != process)
}

/// Resolves `attempt`'s hold of `identity` on home shard `home` (A-727 (d1a-3)): asks the node holding the attempt's
/// repository now (this node's routing for it, else a location round, as a forwarded verb finds an owner) whether a
/// record names the object; named, the hold moves to the repository's records; not named, it is let go; unanswered, it
/// stays, counted, for the next pass.
async fn resolve_one(
  (origin, shards, home): (u16, &[u16], u16),
  (identity, attempt): (ContentId, HomeOwner),
  budget: slates_cluster::CommitBudget,
) {
  let budget_ns = budget.max_deadline_ns();
  let repository = attempt.repository();
  // This node first: a node knows what its own shards hold (at `N = 1` the holder is always this node). Elsewhere, the
  // node holding it now: this node's routing for the repository, else a location round, as a forwarded verb finds one.
  if RESOLUTIONS_HELD.with(std::cell::Cell::get) {
    let _ = crate::state::with_state(|state| state.count(RESOLVE_KEPT, 1));
    return;
  }
  let mut resolution = answer_resolve(origin, shards, (identity, attempt), budget_ns).await;
  if resolution == Resolution::NotHeld {
    resolution = resolve_elsewhere((origin, shards), (identity, attempt), budget).await;
  }
  let records = HomeOwner::Record { repository };
  let applied = match resolution {
    Resolution::Named(true) => crate::xshard::call_in_shard_time(
      origin,
      home,
      move |_| move_region_owner(&identity, (attempt, records)).is_ok(),
      budget_ns,
    )
    .await
    .map(|moved| (moved, RESOLVED_RECORD)),
    Resolution::Named(false) => crate::xshard::call_in_shard_time(
      origin,
      home,
      move |state| let_go_for_region(state, &identity, Some(attempt)).is_ok(),
      budget_ns,
    )
    .await
    .map(|released| (released, RESOLVED_LET_GO)),
    Resolution::NotHeld | Resolution::Unresolved => None,
  };
  let counter = match applied {
    Some((true, counter)) => counter,
    _ => RESOLVE_KEPT,
  };
  let _ = crate::state::with_state(|state| state.count(counter, 1));
}

/// [`resolve_one`] asked of the node that holds the attempt's repository now, where this node does not: this node's
/// routing for the repository's register, else the committed configuration's lineage for it (its member, or the
/// successor its retirement names), else a location round.
async fn resolve_elsewhere(
  (origin, shards): (u16, &[u16]),
  (identity, attempt): (ContentId, HomeOwner),
  budget: slates_cluster::CommitBudget,
) -> Resolution {
  use slates_cluster::homes::{HomeMessage, HomeOutcome};
  let object = slates_db::register::ObjectId(attempt.repository());
  let HomeOwner::Attempt { node, .. } = attempt else {
    return Resolution::Unresolved;
  };
  // Where the repository's register belongs now, from the committed configuration alone (every node names the same
  // one, `RegionalConfiguration::lineage`): the attempt's member while it is one (a process that only restarted), else the
  // successor its retirement names.
  let Some((local, routed, configured, query)) = crate::state::with_state(|state| {
    let regional = state.council.configuration();
    let configured = match regional.lineage(node, object) {
      slates_db::register::Lineage::Successor { successor, .. } => Some(successor),
      _ if state.fleet.members().contains(&node) => Some(node),
      _ => None,
    };
    (
      state.fleet.host(),
      state.fleet.object_owner(object),
      configured,
      crate::owner_location::Query::new(state, object, 0),
    )
  }) else {
    return Resolution::Unresolved;
  };
  let holder = match routed.or(configured) {
    Some(holder) => Some(holder),
    None => crate::owner_location::locate(query).await.ok(),
  };
  let Some(holder) = holder.filter(|holder| *holder != local) else {
    return Resolution::Unresolved;
  };
  let request = HomeMessage::Resolve {
    identity: identity.0,
    attempt,
  }
  .encode();
  let replies = crate::xshard::task_in_shard_time(
    origin,
    shards.first().copied().unwrap_or(origin),
    move || async move { exchange(&[holder], &request, budget).await },
    budget.max_deadline_ns(),
  )
  .await
  .unwrap_or_default();
  replies
    .iter()
    .find_map(|(_, reply)| match HomeMessage::decode(reply) {
      Ok(HomeMessage::Answer {
        identity: answered,
        outcome: HomeOutcome::Named { named },
      }) if answered == identity.0 => Some(Resolution::Named(named)),
      _ => None,
    })
    .unwrap_or(Resolution::Unresolved)
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
/// else 0 with why it was not (for [`home_in_region`]'s log of a short homing).
async fn kept_here(
  origin: u16,
  shards: &[u16],
  (bytes, owner): (&[u8], HomeOwner),
  budget_ns: u64,
) -> Result<(usize, Option<super::durable::BarrierRefused>), HomeRefused> {
  let (_, ticket) =
    keep_region_at_home(origin, shards, (bytes.to_vec(), vec![owner]), budget_ns).await?;
  Ok(
    match super::durable::content_durable(origin, ticket, budget_ns).await {
      Ok(()) => (1, None),
      Err(refused) => (0, Some(refused)),
    },
  )
}

pub(crate) async fn home_in_region(
  origin: u16,
  shards: &[u16],
  (bytes, owner): (Vec<u8>, HomeOwner),
  budget: slates_cluster::CommitBudget,
) -> Homed {
  let identity = ContentId::of(&bytes);
  let budget_ns = budget.max_deadline_ns();
  let began = slates_rt::futures::now_ns();
  let mut homing = Homing {
    kept_by: Vec::new(),
    bytes,
    ranked: Vec::new(),
    reranked: false,
    other_version: false,
  };
  loop {
    let Some((local, members, f, installs)) = crate::state::with_state(|state| {
      (
        state.fleet.host(),
        state.fleet.members().to_vec(),
        usize::try_from(state.fleet.configuration().quorum.f).unwrap_or(usize::MAX),
        state.fleet.installs(),
      )
    }) else {
      return homing.ended(Err(HomeRefused::Unanswered));
    };
    let homes = slates_db::identity_homes::homes(&identity.0, &members, f);
    if !homing.ranked.is_empty() && homing.ranked != homes {
      homing.reranked = true;
    }
    homing.ranked.clone_from(&homes);
    let wanted: Vec<slates_db::HostId> = homes
      .iter()
      .copied()
      .filter(|home| !homing.kept_by.contains(home))
      .collect();
    if wanted.is_empty() && !homes.is_empty() {
      if homing.reranked {
        let _ = crate::state::with_state(|state| state.count(HOMING_RERANKED_KEPT, 1));
      }
      crate::state::with_state(|state| state.count(HOMED_IN_REGION, 1));
      return homing.ended(Ok(identity));
    }
    let left_ns = budget_ns.saturating_sub(slates_rt::futures::now_ns().saturating_sub(began));
    // No home ranked (no members known) is never homed, and never a loop.
    if left_ns == 0 || homes.is_empty() {
      return homing.short(identity, (&homes, budget_ns));
    }
    // A round of puts within the live commit's deadline, or what is left: a home that died unseen costs one round, not
    // the whole window (A-912).
    let round = slates_cluster::CommitBudget::hard(
      left_ns.min(crate::fleet::live_budget().max_deadline_ns()),
      budget.poll_interval_ns,
    );
    if let Err(refused) = homing
      .round(origin, shards, (identity, owner), (local, &wanted), round)
      .await
    {
      return homing.ended(Err(refused));
    }
    if homes.iter().all(|home| homing.kept_by.contains(home)) {
      continue;
    }
    // A member of another version: refused at once, never waited for (A-727 (d1a): no mixed-version homing).
    if homing.other_version {
      return homing.short(identity, (&homes, budget_ns));
    }
    // A home did not keep it: ranked again once this node installs a configuration (a death dropped, A-912), never on a
    // timer, within what is left.
    let left_ns = budget_ns.saturating_sub(slates_rt::futures::now_ns().saturating_sub(began));
    if left_ns == 0 || !install_after(installs, left_ns).await {
      return homing.short(identity, (&homes, budget_ns));
    }
  }
}

/// One homing in the region as it goes ([`home_in_region`], A-912): the homes that kept the window so far, its bytes, the
/// homes it was last ranked at, and whether a configuration installed meanwhile ranked them differently.
struct Homing {
  kept_by: Vec<slates_db::HostId>,
  bytes: Vec<u8>,
  ranked: Vec<slates_db::HostId>,
  reranked: bool,
  /// A home answered as a member of another home wire version: no install changes that (A-727 (d1a)).
  other_version: bool,
}

impl Homing {
  /// One round of puts to `wanted` within `round`: kept here where this node is one of them (once durable), the rest
  /// sent from the control shard, which holds the record sessions (`crate::fleet::take_sessions`). Refused where this
  /// node's own keep was.
  async fn round(
    &mut self,
    origin: u16,
    shards: &[u16],
    (identity, owner): (ContentId, HomeOwner),
    (local, wanted): (slates_db::HostId, &[slates_db::HostId]),
    round: slates_cluster::CommitBudget,
  ) -> Result<(), HomeRefused> {
    let round_ns = round.max_deadline_ns();
    let began = slates_rt::futures::now_ns();
    if wanted.contains(&local) {
      let (durable, refused) = kept_here(origin, shards, (&self.bytes, owner), round_ns)
        .await
        .inspect_err(|refused| {
          slates_machine::log_line!(
            "slates-server: a homing of {identity:?} fell short at once: this node, one of its homes, refused its keep \
             in {} ns: {refused:?}",
            slates_rt::futures::now_ns().saturating_sub(began)
          );
        })?;
      if durable == 1 {
        self.kept_by.push(local);
      } else {
        slates_machine::log_line!(
          "slates-server: a homing of {identity:?}: this node kept it, not durable: {refused:?}"
        );
      }
    }
    let local_ns = slates_rt::futures::now_ns().saturating_sub(began);
    let others: Vec<slates_db::HostId> = wanted
      .iter()
      .copied()
      .filter(|home| *home != local)
      .collect();
    let began = slates_rt::futures::now_ns();
    if !others.is_empty() {
      let control = shards.first().copied().unwrap_or(origin);
      let bytes = std::mem::take(&mut self.bytes);
      // The bytes go to the control shard's task and come back with its answer, so a caller that moves them on (a push's
      // pack into the changeset store, A-767 C5) makes no copy; a task that never answered took them with it.
      let answered = crate::xshard::task_in_shard_time(
        origin,
        control,
        move || put_remote(others, identity, (vec![owner], bytes), round),
        round_ns,
      )
      .await;
      if let Some((remote, back)) = answered {
        self.kept_by.extend(remote.kept);
        self.other_version |= remote.other_version;
        self.bytes = back;
      }
    }
    let region_ns = slates_rt::futures::now_ns().saturating_sub(began);
    crate::state::with_state(|state| {
      state.count(HOMING_LOCAL_NS, local_ns);
      state.count(HOMING_REGION_NS, region_ns);
      state.count_max(HOMING_LONGEST_REGION_NS, region_ns);
    });
    Ok(())
  }

  /// The homing ended `outcome`, its bytes given back.
  fn ended(self, outcome: Result<ContentId, HomeRefused>) -> Homed {
    Homed {
      outcome,
      bytes: self.bytes,
    }
  }

  /// The homing fell short of `homes` within `budget_ns`: logged (main 01a20182's gate 7: which homes kept it, and
  /// for how long it ran), counted, and refused `Unanswered`, the typed `retry` its push answers.
  fn short(self, identity: ContentId, (homes, budget_ns): (&[slates_db::HostId], u64)) -> Homed {
    slates_machine::log_line!(
      "slates-server: a homing of {identity:?} fell short: {} of {} homes kept it ({:?} of {homes:?}); budget {budget_ns} \
       ns",
      self
        .kept_by
        .iter()
        .filter(|kept| homes.contains(kept))
        .count(),
      homes.len(),
      self.kept_by
    );
    crate::state::with_state(|state| state.count(HOMED_SHORT, 1));
    self.ended(Err(HomeRefused::Unanswered))
  }
}

/// Waits, within `left_ns`, for this node to install a configuration after the `seen` installs (A-902's install wake,
/// `slates_cluster::fleet::FleetNode::await_install`), for a homing to rank its homes again (A-912): whether one came.
/// An event, never a timer; off a shard, none.
async fn install_after(seen: u64, left_ns: u64) -> bool {
  let change = InstallChange { seen, token: None };
  matches!(
    slates_rt::futures::within(left_ns, change).await,
    Ok(Some(true))
  )
}

/// The wait of [`install_after`]: ready once this shard's fleet node has made a changing install since `seen` (`true`),
/// its waker registered with the node under its token meanwhile; `false` off a shard. Dropped, it takes its registration
/// away, so none outlives it.
struct InstallChange {
  seen: u64,
  token: Option<u64>,
}

impl std::future::Future for InstallChange {
  type Output = bool;

  fn poll(
    self: std::pin::Pin<&mut Self>,
    cx: &mut std::task::Context<'_>,
  ) -> std::task::Poll<bool> {
    let this = self.get_mut();
    let polled = crate::state::with_state(|state| {
      if state.fleet.installs() != this.seen {
        if let Some(token) = this.token.take() {
          state.fleet.forget_install_waiter(token);
        }
        return std::task::Poll::Ready(true);
      }
      this.token = Some(state.fleet.await_install(this.token, cx.waker()));
      std::task::Poll::Pending
    });
    polled.unwrap_or(std::task::Poll::Ready(false))
  }
}

impl Drop for InstallChange {
  fn drop(&mut self) {
    if let Some(token) = self.token.take() {
      let _ = crate::state::with_state(|state| state.fleet.forget_install_waiter(token));
    }
  }
}

/// Counter: a homing that ranked its homes again after a configuration installed meanwhile (a death dropped) and was
/// then kept at every home ranked (A-912): the path past a dead home, run and accepted. Format: a counter name.
pub(crate) const HOMING_RERANKED_KEPT: &str = "vcs_store.homes.homing_reranked_kept";

/// What [`home_in_region`] answers: its outcome, and the bytes it was handed, given back so the caller can move them on
/// (a push's pack into the node's changeset store, A-767 C5) with no copy; empty where a home's task took them and
/// never answered.
#[derive(Debug)]
pub(crate) struct Homed {
  /// The object's identity once `f + 1` homes kept it; refused otherwise.
  pub(crate) outcome: Result<ContentId, HomeRefused>,
  /// The bytes homed.
  pub(crate) bytes: Vec<u8>,
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
  // A refusal is counted by its kind, since this answer can only say absent; the region's other homes are asked next.
  if let Some(bytes) = read_else_counted(origin, shards, identity, budget_ns).await {
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
  let began = slates_rt::futures::now_ns();
  let spend = budget.max_deadline_ns();
  let mut left: Vec<slates_db::HostId> = hosts.to_vec();
  let mut answers = Vec::new();
  let mut behind: Vec<slates_db::HostId> = Vec::new();
  while !left.is_empty() {
    // Counted before the round asks, so a round that passes between a `Behind` answer and the wait is not missed.
    let seen = crate::state::with_state(|state| state.council_rounds.count()).unwrap_or(0);
    let answered_behind =
      exchange_round(&mut left, request, budget, (&mut answers, &mut behind)).await;
    let spent = slates_rt::futures::now_ns().saturating_sub(began);
    if left.is_empty() || spent >= spend {
      break;
    }
    let waited = if answered_behind {
      // A home behind this node's configuration (A-912): asked again once a council round has passed here, the round
      // whose append brings its install, its record session given back meanwhile.
      council_round_after(seen, spend.saturating_sub(spent), &behind).await
    } else {
      // A session out on another exchange's loan: wait for its return (or its link's change), then look again.
      crate::fleet::session_returned(&left, spend.saturating_sub(spent)).await
    };
    if waited != Some(true) {
      log_unanswered(&left, began, waited);
      return answers;
    }
  }
  if !left.is_empty() {
    log_unanswered(&left, began, Some(false));
  }
  answers
}

/// One round of [`exchange`]: `request` sent to each host of `left` whose record session is free now, within `budget`;
/// each that replied leaves `left`, its reply kept in `answers` where this build can read it, and its session returned.
/// A reply this build cannot read (a peer of another version answers none it can) fails its exchange at once, counted:
/// never a wait for the deadline (A-727 (d1a)).
async fn exchange_round(
  left: &mut Vec<slates_db::HostId>,
  request: &[u8],
  budget: slates_cluster::CommitBudget,
  (answers, behind): (
    &mut Vec<(slates_db::HostId, Vec<u8>)>,
    &mut Vec<slates_db::HostId>,
  ),
) -> bool {
  use slates_cluster::homes::{HOME_STREAM, HomeMessage, HomeOutcome, HomeRefusal};
  let sessions =
    crate::fleet::take_sessions("vcs_store.homes.exchange", |host| left.contains(&host));
  if sessions.is_empty() {
    return false;
  }
  let mut answered_behind = false;
  let outgoing: Vec<_> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.to_vec(), endpoint))
    .collect();
  let (replied, stragglers) = slates_cluster::broadcast(
    outgoing,
    HOME_STREAM,
    slates_transport::connection::Priority::Bulk,
    budget,
  )
  .await;
  let mut take = |host: slates_db::HostId, bytes: Vec<u8>| {
    let decoded = HomeMessage::decode(&bytes);
    // A home behind this node's configuration stays left, asked again after a council round (A-912).
    if matches!(
      decoded,
      Ok(HomeMessage::Answer {
        outcome: HomeOutcome::Behind { .. },
        ..
      })
    ) {
      answered_behind = true;
      if !behind.contains(&host) {
        behind.push(host);
      }
      return;
    }
    left.retain(|waiting| *waiting != host);
    let readable = !matches!(
      decoded,
      Ok(HomeMessage::Answer {
        outcome: HomeOutcome::Refused(HomeRefusal::Version),
        ..
      }) | Err(_)
    );
    if !readable {
      // Kept with the answers, so a caller that must end at once on another version (a homing, A-727 (d1a)) sees it.
      let _ = crate::state::with_state(|state| state.count(REPLY_OTHER_VERSION, 1));
      answers.push((host, bytes));
      return;
    }
    if behind.contains(&host)
      && matches!(
        decoded,
        Ok(HomeMessage::Answer {
          outcome: HomeOutcome::Kept { .. },
          ..
        })
      )
    {
      let _ = crate::state::with_state(|state| state.count(BEHIND_KEPT_AFTER, 1));
    }
    answers.push((host, bytes));
  };
  let mut returned = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    take(host, reply.bytes);
    returned.push((host, endpoint));
  }
  crate::fleet::return_sessions(returned);
  // A home slower than the round's stall (one waiting for its own install, A-902) answers late: its answer counts, and
  // its session goes back on its link (A-907). An unanswered one's link is dialed afresh and the host stays left.
  for (host, bytes) in crate::fleet::settled_stragglers(stragglers).await {
    take(host, bytes);
  }
  answered_behind
}

/// Waits, within `left_ns`, for a council round to pass through this node after the `seen` counted
/// (`crate::fleet::CouncilRound`, A-912), then counts each home of `behind` as asked again: `Some(true)` once one has,
/// `Some(false)` at the deadline, `None` when no wait could be made (off a shard). An event, never a timer.
async fn council_round_after(
  seen: u64,
  left_ns: u64,
  behind: &[slates_db::HostId],
) -> Option<bool> {
  let round = crate::fleet::CouncilRound { seen, token: None };
  let waited = match slates_rt::futures::within(left_ns, round).await {
    Ok(Some(true)) => Some(true),
    // No wait could be made (off a shard): the exchange ends as it stands, never a spin.
    Ok(Some(false)) | Err(_) => None,
    Ok(None) => Some(false),
  };
  if waited == Some(true) {
    let asked = u64::try_from(behind.len()).unwrap_or(u64::MAX);
    let _ = crate::state::with_state(|state| state.count(BEHIND_ASKED_AGAIN, asked));
  }
  waited
}

/// Logs the hosts of an exchange that never answered, how long it ran, and why it stopped (gate 8: a homing short in about
/// 1 ms told no answer from a refusal; w-shared agreed this line in writing, 2026-11-24): `waited` the session wait's
/// answer, none where no wait could be made, false at the deadline.
fn log_unanswered(left: &[slates_db::HostId], began: u64, waited: Option<bool>) {
  slates_machine::log_line!(
    "slates-server: a region exchange ended with {left:?} unanswered after {} ns: {}",
    slates_rt::futures::now_ns().saturating_sub(began),
    match waited {
      None => "no session to them, and no wait for one could be made",
      Some(false) => "its budget spent, no session or no reply from them",
      Some(true) => "a session came back",
    }
  );
}

/// The configuration version this node ranks an object's homes by now (A-902), carried on a put so that a home behind it
/// waits for its own install rather than refusing: zero off a shard, which no member's install is behind.
fn installed_configuration() -> u64 {
  crate::state::with_state(|state| state.fleet.configuration().version).unwrap_or(0)
}

/// The object `bytes` named `identity` sent to the other homes `homes` (A-724 piece 8c), whole within one message or a
/// part at a time past it ([`put_in_parts`]): the homes that answered kept. Run on the control shard.
async fn put_remote(
  homes: Vec<slates_db::HostId>,
  identity: ContentId,
  (owners, bytes): (Vec<HomeOwner>, Vec<u8>),
  budget: slates_cluster::CommitBudget,
) -> (RemoteKept, Vec<u8>) {
  use slates_cluster::homes::{HomeMessage, HomeOutcome, HomeRefusal, PART_BYTES};
  if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > PART_BYTES {
    let kept = put_in_parts(&homes, (identity, &owners), &bytes, budget).await;
    return (
      RemoteKept {
        kept,
        other_version: false,
      },
      bytes,
    );
  }
  let message = HomeMessage::Put {
    identity: identity.0,
    configuration: installed_configuration(),
    owners,
    bytes,
  };
  let request = message.encode();
  let bytes = match message {
    HomeMessage::Put { bytes, .. } => bytes,
    _ => Vec::new(),
  };
  let began = slates_rt::futures::now_ns();
  let answers = exchange(&homes, &request, budget).await;
  let other_version = answers.iter().any(|(_, reply)| {
    matches!(
      HomeMessage::decode(reply),
      Ok(HomeMessage::Answer {
        outcome: HomeOutcome::Refused(HomeRefusal::Version),
        ..
      }) | Err(_)
    )
  });
  let kept = answers
    .into_iter()
    .filter(|(host, reply)| {
      let decoded = HomeMessage::decode(reply);
      let kept = matches!(
        decoded,
        Ok(HomeMessage::Answer { identity: answered, outcome: HomeOutcome::Kept { .. } }) if answered == identity.0
      );
      if !kept {
        // What a home answered other than kept, and when (gate 8; w-shared agreed this line in writing, 2026-11-24).
        slates_machine::log_line!(
          "slates-server: home {host:?} answered a put of {identity:?} other than kept after {} ns: {}",
          slates_rt::futures::now_ns().saturating_sub(began),
          match &decoded {
            Ok(HomeMessage::Answer { outcome, .. }) => format!("{outcome:?}"),
            Ok(_) => "another message than an answer".to_owned(),
            Err(refused) => format!("unreadable: {refused:?}"),
          }
        );
      }
      kept
    })
    .map(|(host, _)| host)
    .collect();
  (
    RemoteKept {
      kept,
      other_version,
    },
    bytes,
  )
}

/// What [`put_remote`] answers of its homes: those that kept the window, and whether one answered as a member of another
/// home wire version, which no install changes, so its homing ends short at once (A-727 (d1a)).
#[derive(Debug, Default)]
struct RemoteKept {
  kept: Vec<slates_db::HostId>,
  other_version: bool,
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
  (identity, owners): (ContentId, &[HomeOwner]),
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
      configuration: installed_configuration(),
      owners: owners.to_vec(),
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

/// Counter: nanoseconds homings spent on their local home (the keep and its shard's publication), summed: with
/// [`HOMING_REGION_NS`], where a homing's time goes (the three-node A/B, 2026-11-20: about 10 to 16 ms a homing). Format:
/// a counter name.
pub(crate) const HOMING_LOCAL_NS: &str = "vcs_store.homes.homing_local_ns";
/// Counter: nanoseconds homings spent on their other homes (the exchange, their keep and publication), summed. Format: a
/// counter name.
pub(crate) const HOMING_REGION_NS: &str = "vcs_store.homes.homing_region_ns";
/// Counter: the most nanoseconds one homing spent on its other homes: a homing that waited out its deadline shows it
/// (A-727 (d1a): one refused by another home-wire version is refused at once, far under it). Format: a counter name.
pub(crate) const HOMING_LONGEST_REGION_NS: &str = "vcs_store.homes.longest_homing_region_ns";

/// Counter: identities the healer offered to their region homes. Format: a counter name.
pub(crate) const HEAL_OFFERED: &str = "vcs_store.homes.heal_offered";
/// Counter: an object the healer offered without an ended attempt among its owners, or held back while every owner was
/// one (A-727 (d1a-3)): an ended attempt is resolved, never spread. Format: a counter name.
pub(crate) const HEAL_ENDED_WITHHELD: &str = "vcs_store.homes.heal_ended_withheld";

/// Test support (A-727 (d1a-3)): whether this control shard's heal walk of the configuration `version` has run through.
pub(crate) fn heal_walk_done(version: u64) -> bool {
  HEALING.with(|healing| {
    healing
      .try_borrow()
      .is_ok_and(|healing| healing.0 == Some(version) && healing.1.is_empty())
  })
}

/// Test support (A-727 (d1a-3)): owes this control shard a heal walk of every hold, as a new configuration does.
pub(crate) fn owe_heal_walk() {
  HEALING.with(|healing| {
    if let Ok(mut healing) = healing.try_borrow_mut() {
      *healing = (None, std::collections::VecDeque::new());
    }
  });
}
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
    // A new configuration: the holds an attempt a crash left are resolved again too (A-727 (d1a-3)), so one whose
    // repository's holder was unreachable before is asked of the holder this configuration names.
    resolve_owed();
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
    && let_go_region_at_home(origin, shards, identity, None, budget.max_deadline_ns())
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
  let homes = slates_db::identity_homes::homes(&identity.0, members, f);
  let others: Vec<slates_db::HostId> = homes
    .iter()
    .copied()
    .filter(|home| *home != local)
    .collect();
  let mut held: std::collections::BTreeSet<slates_db::HostId> = std::collections::BTreeSet::new();
  held.insert(local);
  if !others.is_empty() {
    let owners = offered_owners(origin, shards, identity, budget).await;
    if owners.is_empty() {
      return;
    }
    // A refusal is counted by its kind, since the healer can only pass it over until its next walk.
    let Some(bytes) = read_else_counted(origin, shards, identity, budget.max_deadline_ns()).await
    else {
      return;
    };
    held.extend(offer(&others, (identity, owners), bytes, budget).await);
    crate::state::with_state(|state| state.count(HEAL_OFFERED, 1));
  }
  release_if_healed(origin, shards, (local, members, f), identity, budget, &held).await;
}

/// The owners [`heal_one`] offers the window `identity` for: every owner its hold has here (A-727 (d1a)), so a home it
/// reaches holds it for the same reasons, except an ended attempt. An ended attempt is never spread, only resolved
/// (A-727 (d1a-3), main 01a20182's ruling of 2026-11-24 04:40): a home not yet resolved offering it to one that had moved
/// it to the records joined it back after the last owed pass (the group runs' flake, 1 in 4), so the resolution's answer
/// is the only way a hold changes owner. Judged here, on the control shard; a live attempt (a push in flight) is offered
/// as before. An object whose every owner is an ended attempt is offered for none, and waits until its resolution moves
/// it to the records or lets it go.
async fn offered_owners(
  origin: u16,
  shards: &[u16],
  identity: ContentId,
  budget: slates_cluster::CommitBudget,
) -> Vec<HomeOwner> {
  let owners = match home_of(shards, &identity) {
    Some(home) => crate::xshard::call_in_shard_time(
      origin,
      home,
      move |_| region_owners(&identity),
      budget.max_deadline_ns(),
    )
    .await
    .unwrap_or_default(),
    None => Vec::new(),
  };
  let offered: Vec<HomeOwner> = crate::state::with_state(|state| {
    owners
      .iter()
      .copied()
      .filter(|owner| !process_ended(state, owner))
      .collect()
  })
  .unwrap_or_default();
  if offered.len() < owners.len() {
    crate::state::with_state(|state| state.count(HEAL_ENDED_WITHHELD, 1));
  }
  offered
}

/// Offers the window `identity`'s `bytes` to `others` for `owners` ([`heal_one`]): whole within one message, or a part at
/// a time past it (A-724 piece 8c), as `home_in_region` sends it. The homes that kept it.
async fn offer(
  others: &[slates_db::HostId],
  (identity, owners): (ContentId, Vec<HomeOwner>),
  bytes: Vec<u8>,
  budget: slates_cluster::CommitBudget,
) -> Vec<slates_db::HostId> {
  if u64::try_from(bytes.len()).unwrap_or(u64::MAX) > slates_cluster::homes::PART_BYTES {
    put_in_parts(others, (identity, &owners), &bytes, budget).await
  } else {
    put_whole(others, (identity, owners), bytes, budget).await
  }
}

/// Offers the window `identity` whole to `others` for `owners`, one message each over the sessions the fleet has free,
/// within `budget` ([`heal_one`]): the homes that answered `Kept`.
async fn put_whole(
  others: &[slates_db::HostId],
  (identity, owners): (ContentId, Vec<HomeOwner>),
  bytes: Vec<u8>,
  budget: slates_cluster::CommitBudget,
) -> Vec<slates_db::HostId> {
  use slates_cluster::homes::{HOME_STREAM, HomeMessage, HomeOutcome};
  let request = HomeMessage::Put {
    identity: identity.0,
    configuration: installed_configuration(),
    owners,
    bytes,
  }
  .encode();
  let sessions = crate::fleet::take_sessions("vcs_store.homes.heal", |host| others.contains(&host));
  let outgoing: Vec<(
    slates_db::HostId,
    Vec<u8>,
    slates_transport::endpoint::Endpoint,
  )> = sessions
    .into_iter()
    .map(|(host, endpoint)| (host, request.clone(), endpoint))
    .collect();
  let (replied, stragglers) = slates_cluster::broadcast(
    outgoing,
    HOME_STREAM,
    slates_transport::connection::Priority::Bulk,
    budget,
  )
  .await;
  let mut kept = Vec::new();
  let mut take = |host: slates_db::HostId, bytes: &[u8]| {
    if let Ok(HomeMessage::Answer {
      identity: answered,
      outcome: HomeOutcome::Kept { .. },
    }) = HomeMessage::decode(bytes)
      && answered == identity.0
    {
      kept.push(host);
    }
  };
  let mut returned = Vec::with_capacity(replied.len());
  for (host, reply, endpoint) in replied {
    take(host, &reply.bytes);
    returned.push((host, endpoint));
  }
  crate::fleet::return_sessions(returned);
  // A late home's answer counts and its session goes back on its link (A-907).
  for (host, bytes) in crate::fleet::settled_stragglers(stragglers).await {
    take(host, &bytes);
  }
  kept
}

#[cfg(test)]
mod tests;
