//! A green's head checked out into the volume it was created from (§4.16, A-201): the step between a merge and a
//! landing. A green's versions live in its merge engine, which no landing reads, and a green is refused `land`; so the
//! head's difference from the green's origin is written into the green's base volume, and that volume — the server's
//! overlay of the directory it lands into — lands under a grant like any other (§4.15). The landing's verdict is then
//! the judge of the disk: a file the merge changed that an outsider also changed since the base's witness is a
//! conflict, an equal one is accepted by identity, and nothing is inferred (D-26, D-27).
//!
//! **What is checked first.** The green is readable by the caller, the volume and snapshot named are the green's base
//! (the snapshot's origin, captured as `green --base` captures it, encodes byte for byte to the green's recorded
//! origin: an equal path is not evidence of an equal source, §4.10), and the volume is still exactly that snapshot's
//! state ([`slates_vfs::volume::Volume::unchanged_since`]), so nothing the volume gained since is overwritten by the
//! head. A green whose difference from its origin includes a symbolic link, a hard link, an extended attribute or a
//! FIFO/socket is refused `Unsupported` naming the path: those dimensions are owed.
//!
//! **What is written.** The difference of the head from the origin, which is exact at any fold of the green's
//! histories (the origin is durable and never folded; AUD-16 folds the histories): paths the origin held and the head
//! does not are removed, deepest first; then directories the head adds are made and files it adds or changes are
//! written whole, parents first; then each mode the head changed is set. Each step is one of the typed file verbs'
//! (`crate::fs_verbs`), run through the caller's own write attachment of the volume, which is checked again at every
//! step, so a detach or a revocation midway stops the checkout with the attachment's refusal.
//!
//! **Running.** As the image verbs run (A-160): checked at once, then deferred to a task of the volume's owner shard
//! that takes one step a unit in slices of [`crate::config::DaemonConfig::copy_slice_ns`], yielding between; a retry
//! joins it. The volume is published before the completion is recorded, as a file verb publishes before it answers.
//! The steps are not one atom: a refusal midway leaves the steps before it written and names the path it stopped at,
//! as a refused file verb leaves what it wrote; the volume's snapshot the green was made from still holds the state
//! before the checkout.
//!
//! Cost: the origin capture and the difference are one unit, unsliced, proportional to the tree, as `green --base`
//! captures its origin (owed with the extent-backed green); the plan holds a copy of each file it writes.

use std::collections::{BTreeMap, BTreeSet};

use slates_bridge_core::{ObjectId, SetAttr};
use slates_db::catalog::Principal;
use slates_ipc::protocol::{CheckoutReport, GreenBase, Refusal, ReplyBody, VolumeId};
use slates_merge::origin::Origin;
use slates_wire::request::RequestId;

use crate::error::refusal_of_vfs;
use crate::fs_verbs::FsOp;
use crate::state::ShardState;
use crate::verbs::{core_snapshot, find, forbidden, refused, rights_of, to_db_volume};

/// Format: the permission bits of a mode: what a file verb creates with and `chmod` sets, without the type bits.
const PERMISSION_BITS: u32 = 0o7777;
/// Format: the mode a directory the head adds takes when the green records none for it (the bridge's `mkdir`
/// default, before the umask a mount's caller would apply).
const DEFAULT_DIR_MODE: u32 = 0o755;
/// Format: the mode a file the head adds takes when the green records none for it.
const DEFAULT_FILE_MODE: u32 = 0o644;

/// One step of a checkout, in the order [`plan`] gives them.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Step {
  /// Remove the file or empty directory at the path.
  Remove(String),
  /// Make the directory at the path with the mode.
  Mkdir(String, u32),
  /// Write the file at the path whole, creating it with the mode when absent.
  Write(String, Vec<u8>, u32),
  /// Set the mode of the path.
  Mode(String, u32),
}

impl Step {
  fn path(&self) -> &str {
    match self {
      Step::Remove(path) | Step::Mkdir(path, _) | Step::Write(path, _, _) | Step::Mode(path, _) => {
        path
      }
    }
  }
}

/// The head of a green, as the checkout reads it: its files with their bytes, its directories and modes, and the
/// dimensions a checkout does not write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Head {
  files: BTreeMap<String, Vec<u8>>,
  dirs: BTreeSet<String>,
  modes: BTreeMap<String, u32>,
  /// Every symbolic link, hard link, extended attribute and FIFO/socket, keyed by path, as comparable values.
  unwritten: BTreeMap<String, Vec<String>>,
}

/// The dimensions a checkout does not write, keyed by path, each as a comparable description.
fn unwritten_of(
  symlinks: &[(String, String)],
  hardlinks: &[(String, String)],
  xattrs: &[(String, String, Vec<u8>)],
  specials: &[(String, slates_merge::special::SpecialNode)],
) -> BTreeMap<String, Vec<String>> {
  let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
  for (path, target) in symlinks {
    out
      .entry(path.clone())
      .or_default()
      .push(format!("symlink {target}"));
  }
  for (path, target) in hardlinks {
    out
      .entry(path.clone())
      .or_default()
      .push(format!("hardlink {target}"));
  }
  for (path, name, value) in xattrs {
    out
      .entry(path.clone())
      .or_default()
      .push(format!("xattr {name} {value:?}"));
  }
  for (path, node) in specials {
    out
      .entry(path.clone())
      .or_default()
      .push(format!("special {node:?}"));
  }
  for values in out.values_mut() {
    values.sort();
  }
  out
}

impl Head {
  fn of_origin(origin: &Origin) -> Head {
    Head {
      files: origin.files.iter().cloned().collect(),
      dirs: origin.dirs.iter().cloned().collect(),
      modes: origin.modes.iter().cloned().collect(),
      unwritten: unwritten_of(
        &origin.symlinks,
        &origin.hardlinks,
        &origin.xattrs,
        &origin.specials,
      ),
    }
  }

  fn of_green(green: &slates_merge::engine::Green) -> Head {
    let base = green.current_base();
    Head {
      files: green
        .files()
        .map(|(path, bytes)| (path.to_owned(), bytes.to_vec()))
        .collect(),
      dirs: base.dirs.iter().cloned().collect(),
      modes: base.modes.iter().cloned().collect(),
      unwritten: unwritten_of(
        &base.symlinks,
        &base.hardlinks,
        &base.xattrs,
        &base.specials,
      ),
    }
  }
}

/// The steps that turn `origin` into `head`, with what they count as, or the path of a difference a checkout does not
/// write (the module doc). Removals deepest first, then directories and files parents first, then modes.
fn plan(origin: &Head, head: &Head) -> Result<(Vec<Step>, CheckoutReport), String> {
  if let Some(path) = unwritten_difference(origin, head) {
    return Err(path);
  }
  let mut report = CheckoutReport::default();
  let removed = removals(origin, head);
  let mut steps: Vec<Step> = removed
    .iter()
    .rev()
    .map(|path| Step::Remove((*path).clone()))
    .collect();
  report.removed = u64::try_from(steps.len()).unwrap_or(u64::MAX);
  let made = creations(origin, head, &removed, &mut steps, &mut report);
  mode_steps(origin, head, &made, &mut steps, &mut report);
  Ok((steps, report))
}

/// The first path whose symbolic links, hard links, extended attributes or FIFO/socket differ between `origin` and
/// `head`: a difference a checkout does not write.
fn unwritten_difference(origin: &Head, head: &Head) -> Option<String> {
  let mut paths: BTreeSet<&String> = origin.unwritten.keys().collect();
  paths.extend(head.unwritten.keys());
  paths
    .into_iter()
    .find(|path| origin.unwritten.get(*path) != head.unwritten.get(*path))
    .cloned()
}

/// The paths a checkout removes: those the origin held and the head does not, and those whose kind changed between a
/// file and a directory, which are made again.
fn removals<'a>(origin: &'a Head, head: &Head) -> BTreeSet<&'a String> {
  let gone_files = origin
    .files
    .keys()
    .filter(|path| !head.files.contains_key(*path) || head.dirs.contains(*path));
  let gone_dirs = origin
    .dirs
    .iter()
    .filter(|path| !head.dirs.contains(*path) || head.files.contains_key(*path));
  gone_files.chain(gone_dirs).collect()
}

/// The mode a path the head adds takes: the head's, or `default` where it records none.
fn mode_of(head: &Head, path: &String, default: u32) -> u32 {
  head
    .modes
    .get(path)
    .map_or(default, |mode| mode & PERMISSION_BITS)
}

/// Appends the directories the head adds and the files it adds or changes, parents first; the paths made.
fn creations<'a>(
  origin: &Head,
  head: &'a Head,
  removed: &BTreeSet<&String>,
  steps: &mut Vec<Step>,
  report: &mut CheckoutReport,
) -> BTreeSet<&'a String> {
  let mut made = BTreeSet::new();
  let paths: BTreeSet<&String> = head.dirs.iter().chain(head.files.keys()).collect();
  for path in paths {
    let again = removed.contains(path);
    if head.dirs.contains(path) {
      if again || !origin.dirs.contains(path) {
        steps.push(Step::Mkdir(
          path.clone(),
          mode_of(head, path, DEFAULT_DIR_MODE),
        ));
        report.made = report.made.saturating_add(1);
        made.insert(path);
      }
    } else if let Some(bytes) = head.files.get(path)
      && (again || origin.files.get(path) != Some(bytes))
    {
      steps.push(Step::Write(
        path.clone(),
        bytes.clone(),
        mode_of(head, path, DEFAULT_FILE_MODE),
      ));
      report.written = report.written.saturating_add(1);
      made.insert(path);
    }
  }
  made
}

/// Appends a mode step for every present path whose mode the head changed, and for every path made with a recorded
/// mode (so the mode is the head's whatever the bridge's create applied); counts the first kind alone.
fn mode_steps(
  origin: &Head,
  head: &Head,
  made: &BTreeSet<&String>,
  steps: &mut Vec<Step>,
  report: &mut CheckoutReport,
) {
  for (path, mode) in &head.modes {
    let mode = mode & PERMISSION_BITS;
    let changed = origin.modes.get(path).map(|old| old & PERMISSION_BITS) != Some(mode);
    let present = head.dirs.contains(path) || head.files.contains_key(path);
    let was_made = made.contains(path);
    if present && (changed || was_made) {
      steps.push(Step::Mode(path.clone(), mode));
      if !was_made {
        report.moded = report.moded.saturating_add(1);
      }
    }
  }
}

/// Checks a checkout at once and defers its steps to a task (the module doc). `attachment` is the caller's write
/// attachment of `base.volume`.
pub(crate) fn checkout_verb(
  state: &mut ShardState,
  principal: &Principal,
  green: VolumeId,
  base: GreenBase,
  attachment: u64,
) -> ReplyBody {
  if !state.consensus_ready {
    return refused(Refusal::ConsensusNotInitialized);
  }
  let record = match crate::merge_service::require_green(state, principal, green, "checkout") {
    Ok(record) => record,
    Err(refusal) => return refused(refusal),
  };
  let (handle, base_record) = match find(state, base.volume) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  if !rights_of(&base_record, principal).write {
    return forbidden("checkout");
  }
  let Some(recorded) = state
    .db
    .partition()
    .green_origin(record.id)
    .map(<[u8]>::to_vec)
  else {
    // A scratch green has no base to write into: it is created over none.
    return refused(Refusal::ConsistentBaseUnavailable);
  };
  let unchanged = match state.volumes.get(handle) {
    Ok(slot) => slot.volume.unchanged_since(core_snapshot(base.snapshot)),
    Err(_) => return refused(Refusal::NotFound),
  };
  match unchanged {
    Ok(true) => {}
    Ok(false) => {
      return refused(Refusal::Unsupported {
        feature: "a checkout into a volume changed since the green's base snapshot: name the latest snapshot whose \
                  state the volume still is"
          .to_owned(),
      });
    }
    Err(error) => return refused(refusal_of_vfs(&error)),
  }
  let origin = match crate::merge_service::seed_origin(state, principal, base) {
    Ok(origin) => origin,
    Err(refusal) => return refused(refusal),
  };
  if origin.encode() != recorded {
    return refused(Refusal::ConsistentBaseUnavailable);
  }
  let Some(engine) = state.greens.get(&record.id) else {
    return refused(Refusal::NotFound);
  };
  let version = engine.head();
  let (steps, report) = match plan(&Head::of_origin(&origin), &Head::of_green(engine)) {
    Ok(planned) => planned,
    Err(path) => {
      return refused(Refusal::Unsupported {
        feature: format!(
          "a checkout of a symbolic link, hard link, extended attribute or FIFO/socket change, at {path}"
        ),
      });
    }
  };
  let Some(request) = state.current_request else {
    return refused(Refusal::Unsupported {
      feature: "a checkout outside a recorded request".to_owned(),
    });
  };
  let job = Job {
    request,
    principal: principal.clone(),
    volume: base.volume,
    attachment,
    version,
    report,
    steps: steps.into_iter(),
  };
  crate::image::defer(state, run(job))
}

/// A checkout between its slices.
struct Job {
  request: (u64, RequestId),
  principal: Principal,
  volume: VolumeId,
  attachment: u64,
  version: u64,
  report: CheckoutReport,
  steps: std::vec::IntoIter<Step>,
}

impl Job {
  /// Takes the next step through the caller's write attachment; `Some(())` once none is left.
  fn step(&mut self, state: &mut ShardState) -> Result<Option<()>, Refusal> {
    let Some(step) = self.steps.next() else {
      return Ok(Some(()));
    };
    let path = step.path().to_owned();
    let outcome = crate::fs_verbs::with_writer(
      state,
      &self.principal,
      (self.volume, self.attachment),
      "checkout",
      |bridge, cx| match &step {
        Step::Remove(path) => crate::fs_verbs::run(bridge, cx, FsOp::Remove { path }).map(drop),
        Step::Mkdir(path, mode) => {
          crate::fs_verbs::run(bridge, cx, FsOp::Mkdir { path, mode: *mode }).map(drop)
        }
        Step::Write(path, bytes, mode) => crate::fs_verbs::run(
          bridge,
          cx,
          FsOp::Write {
            path,
            bytes,
            mode: *mode,
          },
        )
        .map(drop),
        Step::Mode(path, mode) => {
          let (parent, name) = crate::fs_verbs::parent_of(bridge, cx, path)?;
          let found = bridge.lookup(parent, cx, name)?;
          bridge
            .setattr(
              ObjectId::new(found.ino, found.generation),
              cx,
              SetAttr {
                mode: Some(*mode),
                ..SetAttr::default()
              },
            )
            .map(drop)
        }
      },
    )?;
    outcome.map_err(|error| match refusal_of_vfs(&error) {
      Refusal::Unsupported { feature } => Refusal::Unsupported {
        feature: format!("{feature}, at {path}"),
      },
      other => {
        state.count(CHECKOUT_STEP_REFUSED, 1);
        other
      }
    })?;
    Ok(None)
  }

  /// Publishes the volume so its new state precedes the completion; the reply.
  fn finish(&self, state: &mut ShardState) -> Result<ReplyBody, Refusal> {
    let id = to_db_volume(self.volume);
    match crate::verbs::publish_shard(state) {
      Ok(published) if published.captured(id) => Ok(ReplyBody::CheckedOut {
        version: self.version,
        report: self.report,
      }),
      Ok(_) => Err(refusal_of_vfs(&slates_vfs::VfsError::RecoveryIncomplete)),
      Err(error) => Err(refusal_of_vfs(&error)),
    }
  }
}

/// Counter: a checkout step the volume refused (the refusal names the path in its reply).
const CHECKOUT_STEP_REFUSED: &str = "checkout.step_refused";

/// Takes the steps a slice at a time, then commits the completion and delivers the reply.
async fn run(mut job: Job) {
  let outcome = steps(&mut job).await;
  let request = job.request;
  // With the shard's state unreachable (its shutdown) no completion is recorded, so the client's retry runs the
  // checkout again on the shard that succeeds it, from the volume as it stands.
  // On a live-shipped volume the checkout is answered once `f` candidates hold what it wrote (A-200), as a file verb on
  // that volume is: until then its completion waits with the verbs' (`crate::live_ship::resolve` delivers it).
  let volume = to_db_volume(job.volume);
  let _ = crate::state::with_state(|s| {
    let completed = crate::image::complete_once_durable(s, request, volume, |s| {
      match outcome.and_then(|()| job.finish(s)) {
        Ok(reply) => reply,
        Err(refusal) => refused(refusal),
      }
    });
    if let Some((reply, route)) = completed {
      crate::image::deliver(s, reply, route);
    }
  });
}

/// Every step of `job`, a slice at a time.
async fn steps(job: &mut Job) -> Result<(), Refusal> {
  loop {
    if crate::image::with(|s| crate::image::sliced(s, |s| job.step(s)))?.is_some() {
      return Ok(());
    }
    slates_rt::futures::yield_now().await;
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  fn head(files: &[(&str, &str)], dirs: &[&str], modes: &[(&str, u32)]) -> Head {
    Head {
      files: files
        .iter()
        .map(|(path, bytes)| ((*path).to_owned(), bytes.as_bytes().to_vec()))
        .collect(),
      dirs: dirs.iter().map(|path| (*path).to_owned()).collect(),
      modes: modes
        .iter()
        .map(|(path, mode)| ((*path).to_owned(), *mode))
        .collect(),
      unwritten: BTreeMap::new(),
    }
  }

  /// A-201: removals come deepest first, then directories and files parents first, then modes; an unchanged file is
  /// not written, a mode-only change is counted as one.
  #[test]
  fn the_plan_removes_deepest_first_then_makes_parents_first_then_sets_modes() {
    let origin = head(
      &[
        ("a/old.txt", "old"),
        ("keep", "same"),
        ("edit", "before"),
        ("x", "x"),
      ],
      &["a"],
      &[("keep", 0o644), ("x", 0o644)],
    );
    let wanted = head(
      &[
        ("keep", "same"),
        ("edit", "after"),
        ("b/c/new", "new"),
        ("x", "x"),
      ],
      &["b", "b/c"],
      &[("keep", 0o644), ("x", 0o600)],
    );
    let (steps, report) = plan(&origin, &wanted).unwrap();
    assert_eq!(
      steps,
      vec![
        Step::Remove("a/old.txt".to_owned()),
        Step::Remove("a".to_owned()),
        Step::Mkdir("b".to_owned(), DEFAULT_DIR_MODE),
        Step::Mkdir("b/c".to_owned(), DEFAULT_DIR_MODE),
        Step::Write("b/c/new".to_owned(), b"new".to_vec(), DEFAULT_FILE_MODE),
        Step::Write("edit".to_owned(), b"after".to_vec(), DEFAULT_FILE_MODE),
        Step::Mode("x".to_owned(), 0o600),
      ]
    );
    assert_eq!(
      report,
      CheckoutReport {
        written: 2,
        made: 2,
        removed: 2,
        moded: 1
      }
    );
  }

  /// A-201: a file that became a directory is removed, then made again as a directory.
  #[test]
  fn a_file_that_became_a_directory_is_removed_then_made() {
    let origin = head(&[("p", "file")], &[], &[]);
    let wanted = head(&[("p/q", "inside")], &["p"], &[]);
    let (steps, _) = plan(&origin, &wanted).unwrap();
    assert_eq!(
      steps,
      vec![
        Step::Remove("p".to_owned()),
        Step::Mkdir("p".to_owned(), DEFAULT_DIR_MODE),
        Step::Write("p/q".to_owned(), b"inside".to_vec(), DEFAULT_FILE_MODE),
      ]
    );
  }

  /// A-201: a difference in a dimension a checkout does not write is refused, naming the path; an equal one is not.
  #[test]
  fn a_symlink_difference_is_refused_by_path() {
    let mut origin = head(&[], &[], &[]);
    let mut wanted = head(&[], &[], &[]);
    origin
      .unwritten
      .insert("l".to_owned(), vec!["symlink a".to_owned()]);
    wanted
      .unwritten
      .insert("l".to_owned(), vec!["symlink a".to_owned()]);
    assert!(plan(&origin, &wanted).is_ok());
    wanted
      .unwritten
      .insert("l".to_owned(), vec!["symlink b".to_owned()]);
    assert_eq!(plan(&origin, &wanted), Err("l".to_owned()));
  }
}
