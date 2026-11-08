//! A work volume's files as a delta over its base (§4.16 "Role": each agent works on a clone of the green; D-5
//! clones share what they did not change). A work holds only the files it changed since its base version, each as
//! its bytes or as removed; every other file is the green's at that version, read from the green's history, which
//! keeps that version while any work is based there (`merge_service::reachable_floor`).
//!
//! Before 2026-11-04 a work held a whole copy of its green: every accept copied every file of the green back into
//! the work and recounted its charge, so a submit's cost followed the green's size (a 10,000-file green of 4 KiB
//! files: 5.46 ms at the median, two thirds of it that copy, `crates/client/examples/edit_merge_bench.rs`). Now an
//! accept or a rebase moves the base and clears the delta, and a work is charged for what it holds itself.
//!
//! A work whose green is gone has no base to read: a file it did not change answers `UnknownBase`, as its submit
//! does (§4.16 failure matrix), and the files it changed stay readable.

use std::borrow::Cow;
use std::collections::BTreeMap;

use slates_merge::engine::Green;

/// What a work changed since its base: per path, the file's bytes, or `None` for a file of the base it removed.
#[derive(Debug, Default)]
pub struct WorkFiles {
  changed: BTreeMap<String, Option<Vec<u8>>>,
}

/// A work's base: its green, when the green still exists, and the version the work is based on.
#[derive(Clone, Copy)]
pub struct Base<'a> {
  /// The green, `None` once it is destroyed.
  pub green: Option<&'a Green>,
  /// The base version.
  pub version: u64,
}

impl<'a> Base<'a> {
  /// The base's bytes of `path`: `Ok(None)` when the base has no such file, `Err(BaseGone)` when the green is gone
  /// and the base cannot be read.
  fn file(self, path: &str) -> Result<Option<Cow<'a, [u8]>>, BaseGone> {
    match self.green {
      Some(green) => Ok(green.content_ref_at(path, self.version)),
      None => Err(BaseGone),
    }
  }
}

/// The base of a work cannot be read: its green is gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BaseGone;

/// The footprint a work's delta holds at one key: the path and the bytes, the path alone for a removal.
fn held(path: &str, entry: Option<&Option<Vec<u8>>>) -> u64 {
  match entry {
    None => 0,
    Some(None) => crate::work_charge::file_footprint(path, 0),
    Some(Some(bytes)) => crate::work_charge::file_footprint(path, bytes.len()),
  }
}

impl WorkFiles {
  /// The file at `path` as the work sees it: its own bytes, nothing where it removed one, else the base's.
  pub fn get<'a>(&'a self, base: Base<'a>, path: &str) -> Result<Option<Cow<'a, [u8]>>, BaseGone> {
    match self.changed.get(path) {
      Some(entry) => Ok(entry.as_deref().map(Cow::Borrowed)),
      None => base.file(path),
    }
  }

  /// Every file the work sees, sorted by path: the base's files it did not change and the files it holds. Without
  /// the base (its green gone) only the files the work holds are known.
  pub fn files<'a>(&'a self, base: Base<'a>) -> Vec<(&'a str, Cow<'a, [u8]>)> {
    let mut files: BTreeMap<&str, Cow<'a, [u8]>> = base
      .green
      .map(|green| green.files_at(base.version).into_iter().collect())
      .unwrap_or_default();
    for (path, entry) in &self.changed {
      match entry {
        Some(bytes) => {
          files.insert(path.as_str(), Cow::Borrowed(bytes.as_slice()));
        }
        None => {
          files.remove(path.as_str());
        }
      }
    }
    files.into_iter().collect()
  }

  /// A page of the file at `path` as the work sees it: its bytes in `[offset, offset + want)` and its length, from its
  /// own bytes or the base's ([`Green::content_page_at`], which never copies the whole file).
  pub fn page(
    &self,
    base: Base<'_>,
    path: &str,
    (offset, want): (u64, u64),
  ) -> Result<Option<(Vec<u8>, u64)>, BaseGone> {
    match self.changed.get(path) {
      Some(Some(bytes)) => {
        let total = bytes.len();
        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
        let end = usize::try_from(offset.saturating_add(want))
          .unwrap_or(usize::MAX)
          .min(total);
        let page = bytes.get(start..end).unwrap_or_default().to_vec();
        Ok(Some((page, u64::try_from(total).unwrap_or(u64::MAX))))
      }
      Some(None) => Ok(None),
      None => match base.green {
        Some(green) => Ok(green.content_page_at(path, base.version, (offset, want))),
        None => Err(BaseGone),
      },
    }
  }

  /// Every file the work sees with its length, sorted by path: [`WorkFiles::files`] without the bytes.
  pub fn sizes<'a>(&'a self, base: Base<'a>) -> Vec<(&'a str, u64)> {
    let mut sizes: BTreeMap<&str, u64> = base
      .green
      .map(|green| {
        green
          .file_lens_at(base.version)
          .into_iter()
          .map(|(path, len)| (path, u64::try_from(len).unwrap_or(u64::MAX)))
          .collect()
      })
      .unwrap_or_default();
    for (path, entry) in &self.changed {
      match entry {
        Some(bytes) => {
          sizes.insert(
            path.as_str(),
            u64::try_from(bytes.len()).unwrap_or(u64::MAX),
          );
        }
        None => {
          sizes.remove(path.as_str());
        }
      }
    }
    sizes.into_iter().collect()
  }

  /// The footprint of everything the delta holds (the work's charge less its journal's).
  pub fn footprint(&self) -> u64 {
    self
      .changed
      .iter()
      .map(|(path, entry)| held(path, Some(entry)))
      .fold(0, u64::saturating_add)
  }

  /// What splicing `[at, at + delete_len)` of `path` for `inserted` bytes changes in the delta's footprint, before
  /// it is applied: `(added, removed)`, the file copied from the base included. `old` is the file as the work sees
  /// it now (`None` when it is new).
  pub fn splice_delta(
    &self,
    path: &str,
    old: Option<&[u8]>,
    (at, delete_len, inserted): (u64, u64, usize),
  ) -> (u64, u64) {
    let before = held(path, self.changed.get(path));
    let old_len = old.map_or(0, <[u8]>::len);
    let start = usize::try_from(at).unwrap_or(usize::MAX).min(old_len);
    let removed = usize::try_from(delete_len)
      .unwrap_or(usize::MAX)
      .min(old_len.saturating_sub(start));
    let after = crate::work_charge::file_footprint(
      path,
      old_len.saturating_sub(removed).saturating_add(inserted),
    );
    (after.saturating_sub(before), before.saturating_sub(after))
  }

  /// Splices `[at, at + delete_len)` of `path` for `bytes`, copying the file from the base first when the work has
  /// not changed it (the file a new path starts empty). The base must be readable for a file the work has not
  /// changed.
  pub fn splice(
    &mut self,
    base: Base<'_>,
    path: &str,
    (at, delete_len): (u64, u64),
    bytes: &[u8],
  ) -> Result<(), BaseGone> {
    if !self.changed.contains_key(path) {
      let from_base = base.file(path)?.map(Cow::into_owned);
      self.changed.insert(path.to_owned(), from_base);
    }
    let entry = self.changed.entry(path.to_owned()).or_default();
    let content = entry.get_or_insert_with(Vec::new);
    let start = usize::try_from(at).unwrap_or(usize::MAX).min(content.len());
    let del = usize::try_from(delete_len)
      .unwrap_or(usize::MAX)
      .min(content.len().saturating_sub(start));
    content.splice(start..start.saturating_add(del), bytes.iter().copied());
    Ok(())
  }

  /// What removing `path` from the work's view leaves at that key: `Some(true)` a removal (the base has the file,
  /// or cannot be read, so it must stay hidden), `Some(false)` no entry (the file was the work's own), `None` when
  /// the work sees no such file and nothing changes.
  fn removal(&self, base: Base<'_>, path: &str) -> Result<Option<bool>, BaseGone> {
    if self.get(base, path)?.is_none() {
      return Ok(None);
    }
    Ok(Some(base.file(path).map_or(true, |file| file.is_some())))
  }

  /// The footprint `removal` leaves at `path`.
  fn held_after_removal(path: &str, removal: Option<bool>, before: u64) -> u64 {
    match removal {
      None => before,
      Some(true) => crate::work_charge::file_footprint(path, 0),
      Some(false) => 0,
    }
  }

  /// Applies `removal` at `path`.
  fn remove_as(&mut self, path: &str, removal: Option<bool>) {
    match removal {
      None => {}
      Some(true) => {
        self.changed.insert(path.to_owned(), None);
      }
      Some(false) => {
        self.changed.remove(path);
      }
    }
  }

  /// What a namespace declaration `op` changes in the delta's footprint, before it is applied: `(added, removed)`.
  pub fn declare_delta(
    &self,
    base: Base<'_>,
    op: &slates_merge::increment::VolumeOp,
  ) -> Result<(u64, u64), BaseGone> {
    use slates_merge::increment::VolumeOp;
    let (before, after) = match op {
      VolumeOp::Unlink { path } => {
        let before = held(path, self.changed.get(path));
        let removal = self.removal(base, path)?;
        (before, Self::held_after_removal(path, removal, before))
      }
      VolumeOp::Rename { from, to } if from != to => {
        let Some(bytes) = self.get(base, from)? else {
          return Ok((0, 0));
        };
        let held_from = held(from, self.changed.get(from));
        let before = held_from.saturating_add(held(to, self.changed.get(to)));
        let removal = self.removal(base, from)?;
        let after = Self::held_after_removal(from, removal, held_from)
          .saturating_add(crate::work_charge::file_footprint(to, bytes.len()));
        (before, after)
      }
      _ => (0, 0),
    };
    Ok((after.saturating_sub(before), before.saturating_sub(after)))
  }

  /// Applies a namespace declaration to the work's view: an unlink removes the file, a rename moves it (replacing
  /// what `to` named). Other declarations change no file.
  pub fn declare(
    &mut self,
    base: Base<'_>,
    op: &slates_merge::increment::VolumeOp,
  ) -> Result<(), BaseGone> {
    use slates_merge::increment::VolumeOp;
    match op {
      VolumeOp::Unlink { path } => {
        let removal = self.removal(base, path)?;
        self.remove_as(path, removal);
      }
      VolumeOp::Rename { from, to } if from != to => {
        let Some(bytes) = self.get(base, from)?.map(Cow::into_owned) else {
          return Ok(());
        };
        let removal = self.removal(base, from)?;
        self.remove_as(from, removal);
        self.changed.insert(to.clone(), Some(bytes));
      }
      _ => {}
    }
    Ok(())
  }

  /// A work's delta as a clean rebase leaves it (`Rebased::changed`), or as a restatement in its log replays it
  /// (A-207): per path its bytes where they differ from the base, `None` where it removes the base's file.
  pub fn from_changed(changed: BTreeMap<String, Option<Vec<u8>>>) -> WorkFiles {
    WorkFiles { changed }
  }

  /// What the work holds of its own, in path order (A-207: what a restatement in its log carries).
  pub fn changed_entries(&self) -> impl Iterator<Item = (&str, Option<&[u8]>)> {
    self
      .changed
      .iter()
      .map(|(path, entry)| (path.as_str(), entry.as_deref()))
  }

  /// Forgets every change: the work now equals its (new) base.
  pub fn clear(&mut self) {
    self.changed.clear();
  }
}
