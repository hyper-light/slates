//! Each repository's memory share on its owner shard, divided by marginal utility at the learning cadence and kept as
//! an immutable value between divisions (A-724 piece 2d; §4.17.5 "Shares": "published ... as an immutable snapshot at
//! the learning cadence (D-8), never asked for per request").
//!
//! **What it holds.** [`Grants`]: for each repository with a usage row on this shard, the bytes of the shard's room
//! its curve wins (`slates_vcs::partition`: each first guaranteed the lesser of A-188's floor and what its curve can
//! use, the rest by lookahead), and the room no curve would use (the spare, lent). A division reads every row's curve,
//! aged first to now at its repository's read horizon (`Ledger::age_curves`), each repository's misses turned into
//! misses a second by that horizon (a curve's counts are its reads over about one horizon).
//!
//! **When it is divided** ([`current_at`]). At the learning cadence: once the shortest read horizon among the
//! repositories served has passed since the last division, or at once when the set of repositories served changed
//! (one came or went, so every floor moved). With no horizon learned yet, only such a change divides again. So a
//! division costs `O(n · e · log n)` (n repositories, e ≤ 65 edges a curve) once a cadence, never a request's.
//!
//! **What it is not.** It divides; admission reads it (`crate::vcs_share::room`, A-724 piece 3's side). One value a
//! shard, replaced whole, read where it is kept: nothing shares it across threads.

use std::collections::BTreeMap;

use slates_db::catalog::RepositoryId;
use slates_vcs::{Curve, Want, partition};

use crate::state::ShardState;

/// Counter: divisions of the shard's room by the repositories' curves. Format: a counter name.
pub(crate) const DIVIDED: &str = "vcs_share.grants_divided";

/// Format: nanoseconds in a second: a curve's counts over its horizon in nanoseconds, read as misses a second.
const NANOS_PER_SECOND: f64 = 1e9;

/// One division of the shard's room (module docs).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Grants {
  /// The divisions made on this shard: one more each time.
  pub(crate) generation: u64,
  /// The shard clock's nanoseconds it was divided at.
  pub(crate) at_ns: u64,
  /// The room divided, in bytes.
  pub(crate) room: u64,
  /// The repositories it was divided among, in id order.
  served: Vec<RepositoryId>,
  /// The cadence it is good for: the shortest read horizon among them, in nanoseconds; `None` before any is learned.
  pub(crate) cadence_ns: Option<u64>,
  /// Each repository's bytes.
  given: BTreeMap<RepositoryId, u64>,
  /// The room no curve would use.
  pub(crate) spare: u64,
}

impl Grants {
  /// Repository `id`'s bytes, when it was among those divided.
  pub(crate) fn of(&self, id: RepositoryId) -> Option<u64> {
    self.given.get(&id).copied()
  }
}

/// The shard's grants as of `now_ns`, divided again first when they are due (module docs, "When it is divided"), and
/// once divided, every repository's read-backs past its new grant let go (`read_backs`, §4.17.5 "The cold trigger").
pub(crate) fn current_at(state: &mut ShardState, now_ns: u64) -> &Grants {
  if due(state, now_ns) {
    let divided = divide(state, now_ns);
    state.vcs_grants = divided;
    state.count(DIVIDED, 1);
    // The cold trigger (§4.17.5, A-724 piece 4): what reads brought back past each repository's new grant goes now.
    super::read_backs::let_go_past_grants(state);
  }
  &state.vcs_grants
}

/// Whether the grants kept are due to be divided again at `now_ns`.
fn due(state: &ShardState, now_ns: u64) -> bool {
  let kept = &state.vcs_grants;
  if kept.generation == 0 {
    return true;
  }
  let served = state.vcs_usage.rows().map(|(id, _)| *id);
  if !served.eq(kept.served.iter().copied()) {
    return true;
  }
  kept
    .cadence_ns
    .is_some_and(|cadence| now_ns.saturating_sub(kept.at_ns) >= cadence)
}

/// Divides the shard's room among its repositories' curves at `now_ns` (module docs).
fn divide(state: &mut ShardState, now_ns: u64) -> Grants {
  state.vcs_usage.age_curves(now_ns);
  let room = state.store.admittable();
  let empty = Curve::new(1, 1);
  let rows: Vec<(RepositoryId, &Curve, f64, Option<f64>)> = state
    .vcs_usage
    .rows()
    .map(|(id, row)| {
      let horizon = row.read_horizon_ns();
      let per_s = horizon.map_or(0.0, |tau| NANOS_PER_SECOND / tau);
      (*id, row.curve().unwrap_or(&empty), per_s, horizon)
    })
    .collect();
  let wants: Vec<Want<'_>> = rows
    .iter()
    .map(|(_, curve, per_s, _)| Want {
      curve,
      per_s: *per_s,
    })
    .collect();
  let division = partition(room, &wants);
  let cadence_ns = rows
    .iter()
    .filter_map(|(_, _, _, horizon)| *horizon)
    .fold(None, |least: Option<f64>, tau| {
      Some(least.map_or(tau, |least| least.min(tau)))
    })
    .map(slates_vcs::whole_units);
  Grants {
    generation: state.vcs_grants.generation.saturating_add(1),
    at_ns: now_ns,
    room,
    served: rows.iter().map(|(id, ..)| *id).collect(),
    cadence_ns,
    given: rows
      .iter()
      .map(|(id, ..)| *id)
      .zip(division.given)
      .collect(),
    spare: division.spare,
  }
}

/// Counter: windows let go to the node's store past their repository's grant (A-724 piece 4). Format: a counter name.
pub(crate) const LET_GO_WINDOWS: &str = "vcs_share.let_go_windows";
/// Counter: the bytes those windows freed. Format: a counter name.
pub(crate) const LET_GO_BYTES: &str = "vcs_share.let_go_bytes";

/// Lets go repository `id`'s coldest windows past its published grant (A-724 piece 4, §4.17.5 "The cold trigger";
/// main 01a20182's ruling of 2026-11-21): `candidates` are the windows its tool names as its own; their bytes still held
/// here less its grant is what it holds past its share, and the windows read least recently go first
/// (`ContentStore::last_read`) until it holds no more than its grant. A window the store refuses to let go (another
/// holder keeps it, no landed changeset holds it yet, or it is gone already) is passed over, so another repository's
/// grant never loses a window here. It runs as a step of its own (`vcs_store::within_budget`: the chunk store lent, the
/// shard's charge trued up after). The bytes freed; none without a grant.
///
/// Its callers are the tools' read-through (w-git's and w-hg's, after their correctness items): until a read that
/// misses falls through to the node's store, a daemon build lets nothing go, and a test is the reader.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn let_go_past_grant(
  state: &mut ShardState,
  id: RepositoryId,
  candidates: &[slates_vcs::ContentId],
) -> u64 {
  let Some(grant) = state.vcs_grants.of(id) else {
    return 0;
  };
  let Some(content) = state.vcs_stores.content() else {
    return 0;
  };
  let mut held: Vec<(u64, slates_vcs::ContentId, u64)> = candidates
    .iter()
    .filter(|identity| !content.is_away(identity))
    .filter_map(|identity| {
      Some((
        content.last_read(identity)?,
        *identity,
        content.len_of(identity)?,
      ))
    })
    .collect();
  let in_memory = held
    .iter()
    .fold(0u64, |sum, (_, _, len)| sum.saturating_add(*len));
  let mut over = in_memory.saturating_sub(grant);
  held.sort_unstable_by_key(|(read, ..)| *read);
  // Within a step of its own: the shard's chunk store lent to the content store, its charge trued up after.
  let (freed, windows) = crate::vcs_store::within_budget(state, 0, |stores| {
    let (mut freed, mut windows) = (0u64, 0u64);
    if let Some(content) = stores.content_mut() {
      for (_, identity, _) in held {
        if over == 0 {
          break;
        }
        if let Ok(bytes) = content.let_go(&identity) {
          freed = freed.saturating_add(bytes);
          windows = windows.saturating_add(1);
          over = over.saturating_sub(bytes);
        }
      }
    }
    (freed, windows)
  });
  if windows > 0 {
    state.count(LET_GO_WINDOWS, windows);
    state.count(LET_GO_BYTES, freed);
  }
  freed
}

#[cfg(test)]
mod tests {
  use slates_db::catalog::RepositoryId;
  use slates_vcs::Work;

  use super::{Grants, current_at};
  use crate::state::ShardState;

  /// Shape: a millisecond in the shard clock's nanoseconds, the tests' rhythm.
  const MILLISECOND: u64 = 1_000_000;

  fn id(byte: u8) -> RepositoryId {
    RepositoryId { bytes: [byte; 16] }
  }

  /// Repository `id` given `fetches` answered fetches a millisecond apart from `from`, and its curve `reads` reads of
  /// `objects` one-window objects in turn: its usage row, its read horizon and its working set.
  fn used(
    state: &mut ShardState,
    repository: RepositoryId,
    (fetches, from): (u64, u64),
    (objects, reads): (u64, u64),
  ) {
    for fetch in 0..fetches {
      state
        .vcs_usage
        .answered(&repository, Work::Fetch, 100, from + fetch * MILLISECOND);
    }
    let window = 1 << 16;
    let mut curve = state
      .vcs_usage
      .take_curve(&repository, (1 << 16, window), from);
    for read in 0..reads {
      let object = (u64::from(repository.bytes[0]) << 32) | (read % objects);
      let key = u64::from_le_bytes(
        blake3::hash(&object.to_le_bytes()).as_bytes()[..8]
          .try_into()
          .unwrap_or_default(),
      );
      curve.read(key, window);
    }
    state.vcs_usage.put_curve(&repository, curve);
  }

  /// A-724 piece 2d (§4.17.5 "Shares ... published as an immutable snapshot at the learning cadence"). Do: give two
  /// repositories fetches a millisecond apart and curves of 100 and 300 one-window objects read again and again; ask
  /// the grants, again a nanosecond later, after their shortest read horizon has passed, and after a third repository
  /// appears. Expect: the first division gives each its working set (all of the room is far past both) and the rest
  /// spare; the second answer is the same division, not divided again; past the cadence a new division; a new
  /// repository divides again at once.
  #[test]
  fn t_a724_the_grants_are_divided_at_the_learning_cadence_and_kept_between() {
    crate::test_store::audit_on_stored_shard("vcs_share-grants", |state| {
      let (first, second) = (id(1), id(2));
      used(state, first, (50, 1), (100, 2_000));
      used(state, second, (50, 1), (300, 6_000));
      let now = 1 + 50 * MILLISECOND;
      let window = 1u64 << 16;
      let grants = current_at(state, now).clone();
      assert_eq!(grants.generation, 1);
      assert!(
        grants.of(first).is_some_and(|bytes| bytes >= 100 * window),
        "{grants:?}"
      );
      assert!(
        grants.of(second).is_some_and(|bytes| bytes >= 300 * window),
        "{grants:?}"
      );
      assert!(grants.spare > 0, "{grants:?}");
      let cadence = grants.cadence_ns.expect("a learned cadence");
      assert_eq!(
        current_at(state, now + 1).generation,
        1,
        "divided again within its cadence"
      );
      assert_eq!(
        current_at(state, now + cadence).generation,
        2,
        "not divided at its cadence"
      );
      used(state, id(3), (1, now + cadence), (10, 10));
      assert_eq!(
        current_at(state, now + cadence + 1).generation,
        3,
        "a new repository did not divide again"
      );
    });
  }

  /// A-724 piece 2d (§4.17.5 "Admission": a work admitted against its repository's share, else the room nobody's share
  /// is using). Do: give two repositories curves (100 and 300 one-window objects) and fetch horizons, make both live,
  /// and ask each one's room. Expect: each room read from the published grants (counted `vcs_share.by_grants`), at most
  /// its grant plus the spare (the snapshot slot, divided as before, may bind sooner), so the hot one's room leaves out
  /// the small one's grant.
  #[test]
  fn t_a724_a_contested_room_reads_the_published_grants() {
    crate::test_store::audit_on_stored_shard("vcs_share-grants", |state| {
      let (small, hot) = (id(5), id(6));
      used(state, small, (50, 1), (100, 2_000));
      used(state, hot, (50, 1), (300, 6_000));
      let _live = (
        crate::vcs_share::Live::begin(small),
        crate::vcs_share::Live::begin(hot),
      );
      let before = state
        .refusals
        .get(crate::vcs_share::BY_GRANTS)
        .copied()
        .unwrap_or(0);
      let small_room = crate::vcs_share::room(state, small);
      let hot_room = crate::vcs_share::room(state, hot);
      assert_eq!(
        state
          .refusals
          .get(crate::vcs_share::BY_GRANTS)
          .copied()
          .unwrap_or(0),
        before + 2
      );
      let grants = state.vcs_grants.clone();
      let (small_grant, hot_grant) = (grants.of(small).unwrap_or(0), grants.of(hot).unwrap_or(0));
      // Memory binds at most at the grant and the spare; the snapshot slot, divided as before, may bind sooner.
      assert!(
        small_room.bytes <= small_grant + grants.spare,
        "{small_room:?} {grants:?}"
      );
      assert!(
        hot_room.bytes <= hot_grant + grants.spare,
        "{hot_room:?} {grants:?}"
      );
      assert!(
        hot_room.bytes + small_grant <= grants.room,
        "the hot repository took the small one's grant"
      );
    });
  }

  /// A-724 piece 2d (no overcommit at a newcomer, w-balance's condition). Do: give one repository a curve and a
  /// second, live beside it, none (its first work). Expect: the room not read from the grants (`vcs_share.by_grants`
  /// unmoved) but divided per request as before, so the newcomer is not given all the shard admits.
  #[test]
  fn t_a724_a_live_repository_with_no_curve_leaves_the_room_divided_as_before() {
    crate::test_store::audit_on_stored_shard("vcs_share-grants", |state| {
      let (curved, newcomer) = (id(7), id(8));
      used(state, curved, (50, 1), (100, 2_000));
      state.vcs_usage.answered(&newcomer, Work::Intake, 100, 1);
      let _live = (
        crate::vcs_share::Live::begin(curved),
        crate::vcs_share::Live::begin(newcomer),
      );
      let before = state
        .refusals
        .get(crate::vcs_share::BY_GRANTS)
        .copied()
        .unwrap_or(0);
      let room = crate::vcs_share::room(state, newcomer);
      assert_eq!(
        state
          .refusals
          .get(crate::vcs_share::BY_GRANTS)
          .copied()
          .unwrap_or(0),
        before
      );
      assert!(room.bytes < state.store.admittable(), "{room:?}");
    });
  }

  /// A-724 piece 4 (§4.17.5 "The cold trigger"; main's ruling: a repository's cached windows past its published grant
  /// let go, never windows another repository's grant keeps). Do: put ten landed windows of 1,000 bytes, a second holder
  /// keeping the coldest; read the last four recently; publish a grant of 4,000 bytes for the repository and let go past
  /// it. Expect: 6,000 bytes freed, the six coldest windows the shared one does not count among let go (windows 1 to 6),
  /// the shared one and the four hot ones kept, counted.
  #[test]
  fn t_a724_a_repositorys_coldest_windows_past_its_grant_are_let_go() {
    crate::test_store::audit_on_stored_shard("vcs_share-grants", |state| {
      // A repository made so its shard's content store exists.
      let (repository, _) =
        crate::vcs_store::tests::repository(state, "cold", slates_ipc::protocol::VcsTool::Git);
      let windows: Vec<slates_vcs::ContentId> =
        crate::vcs_store::within_budget(state, 1 << 16, |stores| {
          let content = stores.content_mut().expect("a content store");
          let windows: Vec<_> = (0..10u8)
            .map(|at| content.put(&[at; 1_000]).expect("put"))
            .collect();
          content.put(&[0u8; 1_000]).expect("a second holder");
          for window in &windows {
            assert!(content.mark_landed(window));
          }
          windows
        });
      let content = state.vcs_stores.content().expect("a content store");
      for (at, window) in windows.iter().enumerate() {
        let _ = content.bytes_of(window);
        if at >= 6 {
          let _ = content.bytes_of(window);
        }
      }
      state.vcs_grants = Grants {
        generation: 1,
        given: [(repository, 4_000)].into_iter().collect(),
        served: vec![repository],
        ..Grants::default()
      };
      let freed = super::let_go_past_grant(state, repository, &windows);
      assert_eq!(freed, 6_000);
      let content = state.vcs_stores.content().expect("a content store");
      let away: Vec<usize> = (0..10)
        .filter(|at| content.is_away(&windows[*at]))
        .collect();
      assert_eq!(away, vec![1, 2, 3, 4, 5, 6]);
      assert_eq!(state.refusals.get(super::LET_GO_WINDOWS).copied(), Some(6));
    });
  }

  /// Shape: the brought-back test's texts: how many, and each one's bytes, one window each (under the content plane's
  /// window), so a text is resident or let go whole.
  const READ_TEXTS: usize = 16;
  /// Shape: see [`READ_TEXTS`].
  const READ_TEXT_BYTES: usize = 64 << 10;

  /// Text `number` of the brought-back test: [`READ_TEXT_BYTES`] of its own lines, so no two share a window.
  fn read_text(number: usize) -> Vec<u8> {
    let line = format!("{number:07}\n");
    line.repeat(READ_TEXT_BYTES / line.len()).into_bytes()
  }

  /// A format-2 dump whose revisions `1..=texts` each add a file `f<n>` holding [`read_text`]`(n)`.
  fn read_texts_dump(texts: usize) -> Vec<u8> {
    let section =
      "K 10\nsvn:author\nV 5\nalice\nK 8\nsvn:date\nV 27\n2026-11-25T00:00:00.000000Z\nPROPS-END\n";
    let zero = "K 8\nsvn:date\nV 27\n2026-11-25T00:00:00.000000Z\nPROPS-END\n";
    let mut dump = format!(
      "SVN-fs-dump-format-version: 2\n\nUUID: 00000000-0000-4000-8000-000000000724\n\nRevision-number: 0\n\
       Prop-content-length: {0}\nContent-length: {0}\n\n{zero}\n",
      zero.len()
    )
    .into_bytes();
    for number in 1..=texts {
      let text = read_text(number);
      let empty = "PROPS-END\n";
      dump.extend_from_slice(
        format!(
          "Revision-number: {number}\nProp-content-length: {0}\nContent-length: {0}\n\n{section}\n\
           Node-path: f{number}\nNode-kind: file\nNode-action: add\nProp-content-length: {1}\n\
           Text-content-length: {2}\nContent-length: {3}\n\n{empty}",
          section.len(),
          empty.len(),
          text.len(),
          empty.len() + text.len()
        )
        .as_bytes(),
      );
      dump.extend_from_slice(&text);
      dump.extend_from_slice(b"\n\n");
    }
    dump
  }

  /// Text `number`'s identity, as the content store and a named read name it.
  fn read_identity(number: usize) -> slates_merge::named::Identity {
    let text = read_text(number);
    slates_merge::named::Identity {
      hash: *blake3::hash(&text).as_bytes(),
      len: u64::try_from(text.len()).unwrap_or(u64::MAX),
    }
  }

  /// The bytes of the [`READ_TEXTS`] texts the content store holds resident, not let go.
  fn resident_read_texts(state: &ShardState) -> u64 {
    let Some(content) = state.vcs_stores.content() else {
      return 0;
    };
    (1..=READ_TEXTS)
      .map(read_identity)
      .filter(|identity| {
        let root = slates_vcs::ContentId(identity.hash);
        content.contains(&root) && !content.is_away(&root)
      })
      .fold(0u64, |held, identity| held.saturating_add(identity.len))
  }

  /// Imports [`READ_TEXTS`] texts into a new Subversion repository `name`, every one let go once its batch landed, then
  /// reads each back whole through a named read (`named_objects::bring_back_range`, the path FUSE, NFS, virtio-fs and
  /// WinFsp read a let-go object by), dropping its pin, text `pinned` kept pinned instead, and reads text 1 again last:
  /// the repository, whether every text was let go before the reads, and the pin kept.
  async fn read_back(
    name: &str,
    pinned: Option<usize>,
  ) -> (
    RepositoryId,
    bool,
    Option<crate::vcs_store::named_objects::ReadPin>,
  ) {
    use crate::repository::svn_commit::tests::on;
    let (svn, _) = crate::vcs_store::svn_import::tests::subversion(name).await;
    let (loaded, _) =
      crate::vcs_store::svn_import::tests::imported(svn, read_texts_dump(READ_TEXTS)).await;
    assert!(loaded.is_ok(), "imported: {loaded:?}");
    let away_before = on(|state| resident_read_texts(state)) == 0;
    let mut kept = None;
    for number in (1..=READ_TEXTS).chain([1]) {
      let identity = read_identity(number);
      let pin = crate::vcs_store::named_objects::bring_back_range(identity, (0, identity.len))
        .await
        .expect("brought back");
      if pinned == Some(number) {
        kept = Some(pin);
      }
    }
    (svn, away_before, kept)
  }

  /// Gives repository `svn` a usage row whose curve reads two one-window objects (`used`) and divides the shard's room
  /// at the learning cadence (`current_at`), as admission asks it: the cold trigger runs. Its grant.
  fn divided_for(state: &mut ShardState, svn: RepositoryId) -> u64 {
    let from = slates_rt::futures::now_ns();
    used(state, svn, (50, from), (2, 2_000));
    let _ = current_at(state, from.saturating_add(50 * MILLISECOND));
    state.vcs_grants.of(svn).unwrap_or(0)
  }

  /// Whether text `number` is resident.
  fn read_text_resident(state: &ShardState, number: usize) -> bool {
    let root = slates_vcs::ContentId(read_identity(number).hash);
    state
      .vcs_stores
      .content()
      .is_some_and(|content| content.contains(&root) && !content.is_away(&root))
  }

  /// Main 01a20182's question and ruling (b) of 2026-11-25 (w-hg's sweep: read paths bring a landed window back for a
  /// reader and drop only its pin, and nothing let the window go): what a read brings back is a cache the memory
  /// division bounds (§4.17.5 "The cold trigger", A-724 piece 4). Do: import a dump of [`READ_TEXTS`] revisions each
  /// adding a text of [`READ_TEXT_BYTES`] (no window shared) into a Subversion repository, every text let go once its
  /// batch landed (P3); read each back whole through a named read and drop its pin, text 1 read again last; give the
  /// repository a usage row of two one-window objects and divide the shard's room at its cadence (`current_at`).
  /// Expect: every text let go before the reads; after the division, the texts resident within the repository's grant;
  /// text 1, read last, resident (hot), text 2 let go (cold); counted. Failing first: 1,048,576 B of the sixteen texts
  /// resident against a grant of 131,072 B, the division's let-go having no caller outside tests.
  #[test]
  fn what_a_read_brings_back_is_let_go_past_its_repositorys_grant() {
    use crate::repository::svn_commit::tests::{on, stored};
    let (away_before, grant, resident, hot_cold, let_go) = stored(|| async move {
      let (svn, away_before, _) = read_back("read-back", None).await;
      let grant = on(|state| divided_for(state, svn));
      on(|state| {
        (
          away_before,
          grant,
          resident_read_texts(state),
          (read_text_resident(state, 1), read_text_resident(state, 2)),
          state
            .refusals
            .get(super::super::read_backs::LET_GO)
            .copied()
            .unwrap_or(0),
        )
      })
    });
    assert!(away_before, "every text let go once landed");
    assert!(grant > 0, "a grant divided");
    assert!(
      resident <= grant,
      "{resident} bytes of read texts resident against a grant of {grant}"
    );
    assert_eq!(
      hot_cold,
      (true, false),
      "the hot text kept, the cold let go"
    );
    assert!(let_go > 0, "counted `vcs_share.read_backs_let_go`");
  }

  /// Main 01a20182's ruling (b), condition 2: a pinned window is never let go, however cold. Do: as
  /// [`what_a_read_brings_back_is_let_go_past_its_repositorys_grant`], text 2 (read once, early: cold) kept pinned by its
  /// reader across the division. Expect: text 2 resident after it; the unpinned texts resident within the grant;
  /// counted `vcs_share.read_backs_pinned_kept`. Then, its pin dropped and the room divided again at the next cadence,
  /// text 2 let go.
  #[test]
  fn a_pinned_read_back_is_never_let_go_however_cold() {
    use crate::repository::svn_commit::tests::{on, stored};
    let (during, kept, after) = stored(|| async move {
      let (svn, _, pin) = read_back("read-pinned", Some(2)).await;
      let grant = on(|state| divided_for(state, svn));
      let during = on(|state| {
        (
          read_text_resident(state, 2),
          resident_read_texts(state).saturating_sub(read_identity(2).len) <= grant,
        )
      });
      let kept = on(|state| {
        state
          .refusals
          .get(super::super::read_backs::PINNED_KEPT)
          .copied()
          .unwrap_or(0)
      });
      drop(pin);
      let after = on(|state| {
        let cadence = state.vcs_grants.cadence_ns.unwrap_or(1);
        let next = state.vcs_grants.at_ns.saturating_add(cadence);
        let _ = current_at(state, next);
        read_text_resident(state, 2)
      });
      (during, kept, after)
    });
    assert_eq!(
      during,
      (true, true),
      "the pinned text kept, the rest within the grant"
    );
    assert!(kept > 0, "counted `vcs_share.read_backs_pinned_kept`");
    assert!(!after, "let go once its pin dropped, at the next division");
  }
}
