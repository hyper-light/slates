//! git's hooks as the server runs them (A-665 part 2; §4.17.2 "Hooks, in git's terms" and
//! "Pieces (git)" piece 10): the read-only bare repository a hook runs in, opened on the
//! repository's owner shard from the store A-662's map holds, sealed there in slices, and handed
//! to the hook driver with the variables receive-pack sets for it.
//!
//! **The view on the owner shard** ([`GitHookView`]). Part 1's view
//! (`slates_git::store::hook_view`) is a pure function of the repository's git store, the refs a
//! push read and the push. It is opened here over the repository's entry in the shard's
//! `vcs_stores` (`crate::vcs_store`), with the push where receive-pack keeps it at each point: in
//! git's quarantine for `pre-receive`; among the repository's packs for `update`, receive-pack
//! having moved the objects in between (`tmp_objdir_migrate`); and nowhere for `post-receive`,
//! whose refs are the ones the push left, nor for a push of deletes alone, for which receive-pack
//! unpacks nothing. The store keeps no ref names (A-640), so the caller names each ref by its full
//! name, its green and the version of the green it read ([`ReadRef`]), and the view shows the
//! store's version of that green the green's version names (`GitStore::versions`). A ref at a
//! version no push made is refused ([`GitHookRefused::NotInStore`]), since the store holds what
//! pushes brought and nothing else (A-662). Opening holds every content the view reads, and
//! charges the view's own tables to the content store, so the shard's budget carries them as
//! retention while the view lives, as it carries the store's (`vcs_store::within_budget`).
//! [`GitHookView::close`] gives both back, through the shard's one content store, whether or not
//! the repository's entry is still there.
//!
//! **Sealed in the owner's slices.** git reads a pack's trailer when it opens the pack, and its
//! index whole (part 1's `seal` module), so every pack the view shows is sealed before a hook runs
//! in it, a turn at a time ([`GitHookView::seal_turn`]): each turn the pass runs for the shard's
//! step quantum (`DaemonConfig::step_quantum_ns`, §4.3), at least a unit of work, and the caller's
//! task yields between turns as every sliced operation does. A seal's index is kept as content in
//! the store, the room for it lent first (`HookView::seal_len`), and given back with the view.
//!
//! **Handed to the driver** ([`run`]). [`GitHookView::presented_at`] answers what a hook running in
//! the view, at the root it is presented at, is handed for it: `GIT_DIR=.`, which receive-pack
//! exports to every hook once it has entered the repository, and for `pre-receive` the three
//! variables git's quarantine sets, each as stock git 2.50.1 sets it (part 1's `environment`
//! module, held to A-665's `fixtures/view`). [`run`] finds the hook the repository's policy
//! configures at the input's point (git keeps one hook a point, A-622), adds the view's variables
//! to the point's own (the push options, A-597), and runs it in the view's root through
//! `crate::hook_driver`. A view handed to a point that would not see it so (a quarantine to
//! `update`, a push to `post-receive`), or an input at another tool's point, is refused before any
//! process starts.
//!
//! **Owed** (GAPS, the §4.17 entry): part 3, the view presented at a path, as a read-only
//! attachment the mount edges serve a read at a time from [`GitHookView::read`] (on Linux inside
//! the launcher's namespace, §4.6), where only its tests present it today, by writing it into a
//! scratch directory; its seals kept as derived data across views, since a pack's name hashes what
//! it is made from; receive-pack's calls (A-654); and git's words for each outcome.

use std::collections::BTreeMap;
use std::future::Future;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use slates_db::catalog::{HookConfig, RepositoryId, RepositoryPolicy, VolumeId as DbVolumeId};
use slates_git::Admission;
use slates_git::object_name::ObjectId;
use slates_git::store::hook_view::{
  HookView, Listed, Node, Pushed, Quantum, ReadRefused, Seal, SealRefused, Sealing, ShownRef,
  Snapshot, ViewRefused,
};
use slates_git::store::learn::Clock;
use slates_vcs::{ContentRefused, ContentStore};

use crate::hook_driver::Launch;
use crate::state::ShardState;
use crate::vcs_hooks::{EnvValue, HookInput, HookPoint, Lesson, Outcome};
use crate::vcs_store::{GitStore, SealedPush, VcsStores, green_key, settle, within_budget};

#[cfg(test)]
mod tests;

/// Counter: a hold, a seal or a charge a closing view gave back that the store refused; the rest
/// was given back. Zero in every correct use. Format: a counter name in the status report.
pub(crate) const VIEW_RELEASE_REFUSED: &str = "git_hooks.view_release_refused";

/// One ref a hook's view shows, as the push read it (the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ReadRef<'n> {
  /// Its full name, under `refs/`.
  pub(crate) name: &'n [u8],
  /// Its green.
  pub(crate) green: DbVolumeId,
  /// The green's version the push read.
  pub(crate) version: u64,
}

/// Where a hook's view shows a sealed push (part 1's `Pushed`), by the point it is opened for.
#[derive(Clone, Copy, Debug)]
pub(crate) enum ShownPush<'p> {
  /// In git's quarantine: `pre-receive`'s view.
  Quarantined(&'p SealedPush),
  /// Among the repository's packs, receive-pack having moved its objects: `update`'s view.
  Migrated(&'p SealedPush),
}

impl<'p> ShownPush<'p> {
  /// The push.
  fn sealed(self) -> &'p SealedPush {
    match self {
      ShownPush::Quarantined(sealed) | ShownPush::Migrated(sealed) => sealed,
    }
  }

  /// The push as part 1's view takes it.
  fn pushed(self) -> Pushed<'p> {
    match self {
      ShownPush::Quarantined(sealed) => Pushed::Quarantined(sealed.push()),
      ShownPush::Migrated(sealed) => Pushed::Migrated(sealed.push()),
    }
  }

  /// Where it shows the push.
  fn shows(self) -> Shows {
    match self {
      ShownPush::Quarantined(_) => Shows::Quarantined,
      ShownPush::Migrated(_) => Shows::Migrated,
    }
  }
}

/// Where a view shows a push's objects: what decides the points a hook may see it at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Shows {
  /// In git's quarantine, as `pre-receive` sees a push.
  Quarantined,
  /// Among the repository's packs, as `update` sees one.
  Migrated,
  /// No push: `post-receive`'s view, and a push of deletes alone at any point.
  NoPush,
}

/// Why a hook's view could not be opened, sealed or read, or a hook run in it. Each leaves the
/// store and the shard's charge as they were, but for a seal under way, which the view's close
/// gives back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GitHookRefused {
  /// No git store of the repository is held on this shard.
  NoStore,
  /// The push is to another repository.
  ForeignPush,
  /// The ref at this place among the refs read is at a version of its green no push made, which
  /// the store does not hold (the module docs).
  NotInStore {
    /// Its place.
    at: u64,
  },
  /// Part 1's view refused to open.
  View(ViewRefused),
  /// The store would not take the charge of the view's tables.
  Charge(ContentRefused),
  /// A seal refused: `Changed` when a base pack was made again under the view, which is closed and
  /// opened again.
  Seal(SealRefused),
  /// A read refused.
  Read(ReadRefused),
  /// The input is at another tool's point.
  NotGits {
    /// The point.
    point: HookPoint,
  },
  /// The view shows the push otherwise than a hook at the point sees it (the module docs).
  ViewForAnotherPoint {
    /// The point.
    point: HookPoint,
    /// Where the view shows the push.
    shows: Shows,
  },
}

/// The shard's clock as the store's passes read it: the runtime's monotonic nanoseconds (§4.3).
struct ShardClock;

impl Clock for ShardClock {
  fn now_ns(&mut self) -> u64 {
    slates_rt::futures::now_ns()
  }
}

/// A hook's read-only bare repository on the repository's owner shard (the module docs).
pub(crate) struct GitHookView {
  /// The repository.
  repository: RepositoryId,
  /// Part 1's view.
  view: HookView,
  /// Where it shows the push.
  shows: Shows,
  /// The seals made, by pack name.
  seals: BTreeMap<ObjectId, Seal>,
  /// The packs not yet sealed, by name, the next last.
  unsealed: Vec<ObjectId>,
  /// The seal under way.
  sealing: Option<Sealing>,
  /// What the view's tables hold of the content store's budget (`ContentStore::charge`), given back
  /// with it.
  charged: u64,
}

impl std::fmt::Debug for GitHookView {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("GitHookView")
      .field("repository", &self.repository)
      .field("shows", &self.shows)
      .field("sealed", &self.seals.len())
      .field("unsealed", &self.unsealed.len())
      .finish()
  }
}

/// Why opening failed inside the store's step, and whether what it held went back.
#[derive(Debug)]
struct Unopened {
  /// Why.
  refused: GitHookRefused,
  /// Whether every hold taken was given back.
  given_back: bool,
}

impl From<GitHookRefused> for Unopened {
  fn from(refused: GitHookRefused) -> Self {
    Unopened {
      refused,
      given_back: true,
    }
  }
}

impl GitHookView {
  /// Opens the view of `repository` on this shard at the refs the push read and `head`, `HEAD`'s
  /// target (the default branch's full name, whether or not it exists), with the push where
  /// `pushed` says (the module docs). Refused, nothing is held or charged.
  pub(crate) fn open(
    state: &mut ShardState,
    repository: RepositoryId,
    (refs, head): (&[ReadRef<'_>], &[u8]),
    pushed: Option<ShownPush<'_>>,
  ) -> Result<GitHookView, GitHookRefused> {
    if pushed.is_some_and(|pushed| pushed.sealed().repository() != repository) {
      return Err(GitHookRefused::ForeignPush);
    }
    let opened = within_budget(state, 0, |stores| {
      open_over(
        stores,
        repository,
        (refs, head),
        pushed.map(ShownPush::pushed),
      )
    });
    let (view, charged) = match opened {
      Ok(opened) => opened,
      Err(unopened) => {
        if !unopened.given_back {
          state.count(VIEW_RELEASE_REFUSED, 1);
        }
        return Err(unopened.refused);
      }
    };
    let mut unsealed: Vec<ObjectId> = view.packs().map(|(name, _)| name).collect();
    unsealed.reverse();
    Ok(GitHookView {
      repository,
      view,
      shows: pushed.map_or(Shows::NoPush, ShownPush::shows),
      seals: BTreeMap::new(),
      unsealed,
      sealing: None,
      charged,
    })
  }

  /// One turn of sealing on this shard (the module docs): the pass of the pack under way for the
  /// shard's step quantum, at least a unit of work, and once its pass is done its seal made, with
  /// room lent for what the seal keeps. Whether every pack the view shows is sealed. Refused, the
  /// view is closed (and, at `Changed`, opened again).
  pub(crate) fn seal_turn(&mut self, state: &mut ShardState) -> Result<bool, GitHookRefused> {
    let ns = state.config.step_quantum_ns();
    self.seal_within(state, ns)
  }

  /// [`GitHookView::seal_turn`] with the pass's turn `ns` nanoseconds long.
  fn seal_within(&mut self, state: &mut ShardState, ns: u64) -> Result<bool, GitHookRefused> {
    let mut sealing = match self.sealing.take() {
      Some(sealing) => sealing,
      None => {
        let Some(pack) = self.unsealed.pop() else {
          return Ok(true);
        };
        self
          .view
          .sealing(&pack)
          .ok_or(GitHookRefused::Seal(SealRefused::NoPack))?
      }
    };
    let (content, _) = state
      .vcs_stores
      .git_ref(self.repository)
      .ok_or(GitHookRefused::NoStore)?;
    let mut clock = ShardClock;
    let quantum = Quantum {
      clock: &mut clock,
      ns,
    };
    let passed = self
      .view
      .seal_step(&mut sealing, content, quantum)
      .map_err(GitHookRefused::Seal)?;
    if !passed {
      self.sealing = Some(sealing);
      return Ok(false);
    }
    let room = self.view.seal_len(&sealing.pack()).unwrap_or(0);
    let (view, repository) = (&self.view, self.repository);
    let seal = within_budget(state, room, |stores| {
      let (content, git) = stores.git(repository).ok_or(GitHookRefused::NoStore)?;
      let mut admission = Admission::new(content.room());
      view
        .seal(sealing, &git.repository, content, &mut admission)
        .map_err(GitHookRefused::Seal)
    })?;
    self.seals.insert(seal.pack(), seal);
    Ok(self.unsealed.is_empty())
  }

  /// What `path` names (`/`-separated from the view's root, the root itself empty); `None` for
  /// nothing: what a presentation of the view serves.
  pub(crate) fn node(&self, path: &[u8]) -> Option<Node> {
    self.view.node(path)
  }

  /// The entries of the directory `path` names, sorted by name; `None` when it names no
  /// directory: what a presentation of the view serves.
  pub(crate) fn list(&self, path: &[u8]) -> Option<Vec<Listed>> {
    self.view.list(path)
  }

  /// Appends the bytes `range` (an offset and a length, clipped to the file) of the file `path`
  /// names to `out`, made from the repository's store on this shard as the read reaches them, with
  /// the view's seals: what a presentation of the view serves.
  pub(crate) fn read(
    &self,
    stores: &VcsStores,
    path: &[u8],
    range: (u64, u64),
    out: &mut Vec<u8>,
  ) -> Result<(), GitHookRefused> {
    let (content, _) = stores
      .git_ref(self.repository)
      .ok_or(GitHookRefused::NoStore)?;
    self
      .view
      .read(content, &self.seals, path, range, out)
      .map_err(GitHookRefused::Read)
  }

  /// The view as a hook sees it presented at `root`, the absolute path the hook runs in: where it
  /// shows the push, and the variables receive-pack sets for it (the module docs).
  pub(crate) fn presented_at(&self, root: &Path) -> Presented {
    let env = self
      .view
      .environment(root.as_os_str().as_bytes())
      .into_iter()
      .map(|(name, value)| (name.to_owned(), EnvValue::Bytes(value)))
      .collect();
    Presented {
      root: root.to_path_buf(),
      shows: self.shows,
      env,
    }
  }

  /// Gives back everything the view holds (its seals, its contents' holds and its tables' charge)
  /// through the shard's one content store, then settles the shard's charge. A refusal there is
  /// counted ([`VIEW_RELEASE_REFUSED`]) and the rest given back.
  pub(crate) fn close(self, state: &mut ShardState) {
    let given_back = state
      .vcs_stores
      .content_mut()
      .is_some_and(|content| self.give_back(content));
    if !given_back {
      state.count(VIEW_RELEASE_REFUSED, 1);
    }
    settle(state);
  }

  /// Gives back the view's seals, holds and charge to `content`: whether every one went back.
  fn give_back(self, content: &mut ContentStore) -> bool {
    let mut whole = true;
    for seal in self.seals.into_values() {
      whole &= seal.release(content).is_ok();
    }
    whole &= self.view.close(content).is_ok();
    whole & content.discharge(self.charged).is_ok()
  }
}

/// Opens part 1's view over `stores` (the module docs), its tables charged to the content store:
/// the view and that charge.
fn open_over(
  stores: &mut VcsStores,
  repository: RepositoryId,
  (refs, head): (&[ReadRef<'_>], &[u8]),
  pushed: Option<Pushed<'_>>,
) -> Result<(HookView, u64), Unopened> {
  let (content, git) = stores.git(repository).ok_or(GitHookRefused::NoStore)?;
  let mut admission = Admission::new(content.room());
  let shown = shown_refs(git, refs, &mut admission)?;
  let before = admission.left();
  let snapshot = Snapshot { refs: &shown, head };
  let view = HookView::open(&git.repository, content, &snapshot, pushed, &mut admission)
    .map_err(GitHookRefused::View)?;
  let charged = before.saturating_sub(admission.left());
  if let Err(refused) = content.charge(charged) {
    return Err(Unopened {
      refused: GitHookRefused::Charge(refused),
      given_back: view.close(content).is_ok(),
    });
  }
  Ok((view, charged))
}

/// Each ref as the store names it (the module docs): its green's key, and the newest of the store's
/// versions of that green at the green's version read; its table charged to `admission` first.
/// Refused at the first ref the store holds no such version of.
fn shown_refs<'n>(
  git: &GitStore,
  refs: &[ReadRef<'n>],
  admission: &mut Admission,
) -> Result<Vec<ShownRef<'n>>, GitHookRefused> {
  let each = u64::try_from(std::mem::size_of::<ShownRef<'n>>()).unwrap_or(u64::MAX);
  let count = u64::try_from(refs.len()).unwrap_or(u64::MAX);
  admission
    .charge(each.saturating_mul(count))
    .map_err(|past| GitHookRefused::View(ViewRefused::PastAdmission(past)))?;
  refs
    .iter()
    .zip(0_u64..)
    .map(|(read, at)| {
      let green = green_key(read.green);
      let version = git
        .versions(green)
        .iter()
        .rposition(|version| *version == read.version)
        .ok_or(GitHookRefused::NotInStore { at })?;
      Ok(ShownRef {
        name: read.name,
        green,
        version,
      })
    })
    .collect()
}

/// A view as a hook sees it (the module docs): the root it is presented at, where it shows the
/// push, and the variables receive-pack sets for it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Presented {
  /// The absolute path the hook runs in.
  root: PathBuf,
  /// Where the view shows the push.
  shows: Shows,
  /// `GIT_DIR`, and for a quarantine its three variables, sorted by name.
  env: Vec<(String, EnvValue)>,
}

/// What a git hook's run needs from its caller beside the hook's input.
#[derive(Clone, Copy, Debug)]
pub(crate) struct GitHookSite<'a> {
  /// The repository's policy: the hook configured at each of git's points.
  pub(crate) policy: &'a RepositoryPolicy,
  /// The view the hook runs in, as presented.
  pub(crate) view: &'a Presented,
  /// How many bytes of the hook's words the caller keeps: its connection's share.
  pub(crate) words_cap: usize,
}

/// What a git hook's run answers its caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitHookRan {
  /// How the hook ended; `None` where no hook is configured at the point, which lets the push
  /// through (A-597).
  pub(crate) outcome: Option<Outcome>,
  /// What the run teaches its hook's times; `None` where no hook ran.
  pub(crate) lesson: Option<Lesson>,
}

/// Runs the hook `site`'s policy configures at `input`'s point in `site`'s view, held to
/// `bound_ns` (its hook's learned bound, none before its first finished run) and ended when
/// `client_gone` resolves (the module docs). A point with no hook answers at once, running nothing;
/// a view the point would not see, or another tool's point, is refused before anything runs.
pub(crate) async fn run(
  site: &GitHookSite<'_>,
  input: &HookInput,
  bound_ns: Option<u64>,
  client_gone: impl Future<Output = ()>,
) -> Result<GitHookRan, GitHookRefused> {
  sees(input.point, site.view.shows)?;
  let Some(hook) = configured(site.policy, input.point) else {
    return Ok(GitHookRan {
      outcome: None,
      lesson: None,
    });
  };
  let mut handed = input.clone();
  handed.env.extend(site.view.env.iter().cloned());
  let launch = Launch {
    program: Path::new(&hook.program),
    input: &handed,
    view_root: &site.view.root,
    words_cap: site.words_cap,
  };
  let ended = crate::hook_driver::run(launch, bound_ns, client_gone).await;
  Ok(GitHookRan {
    outcome: Some(ended.outcome),
    lesson: Some(ended.lesson),
  })
}

/// Whether a hook at `point` sees a view showing the push as `shows` (the module docs):
/// `pre-receive` the push in git's quarantine or none, `update` the push among the packs or none,
/// `post-receive` none; refused at another tool's point.
fn sees(point: HookPoint, shows: Shows) -> Result<(), GitHookRefused> {
  let seen = match point {
    HookPoint::GitPreReceive => matches!(shows, Shows::Quarantined | Shows::NoPush),
    HookPoint::GitUpdate => matches!(shows, Shows::Migrated | Shows::NoPush),
    HookPoint::GitPostReceive => shows == Shows::NoPush,
    // Mercurial's and Subversion's points, whose servers run their own hooks.
    _ => return Err(GitHookRefused::NotGits { point }),
  };
  if seen {
    Ok(())
  } else {
    Err(GitHookRefused::ViewForAnotherPoint { point, shows })
  }
}

/// The hook `policy` configures at `point`, if any: git keeps one hook a point, named for it
/// (A-622's guard).
fn configured(policy: &RepositoryPolicy, point: HookPoint) -> Option<&HookConfig> {
  policy
    .hooks
    .iter()
    .find(|hook| HookPoint::of(hook.point) == point)
}
