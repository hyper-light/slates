//! A green's version as a base tree (A-774 part 2; §4.16 "Role", D-5; `slates_vfs::host::tree`): what a mounted work's
//! overlay reads where it lies, so a mount copies only what its user changes.
//!
//! A listing is the head's keys beneath the directory, each file kept only if it was there at the version, with every
//! path changed after the version that was a file then put back: so a version the head has moved past lists as it was,
//! at a cost of what changed since, never a walk of an older tree. A file's bytes are lent from the head when the file
//! is the head's (the version is the head), and read a page at a time at the version otherwise. What is owed (GAPS, A-774):
//! a directory made or removed after the version, and a symlink changed after it, list as the head has them; the head's
//! keys beneath a directory are walked to list it (its subtree, not its children alone).

use std::collections::BTreeMap;

use slates_merge::engine::Green;
use slates_vfs::host::HostError;
use slates_vfs::host::HostKind;
use slates_vfs::host::tree::{TreeEntry, TreeSource};

/// Green `green` as it was at `version`, as a base tree.
pub(crate) struct GreenTree<'a> {
  green: &'a Green,
  version: u64,
  /// The shard's content plane, where a file the green holds by its object's name alone is read (A-760 (b) 4d); none
  /// for a reader without one, which refuses such a file.
  plane: Option<crate::vcs_store::named_objects::PlaneObjects<'a>>,
}

impl<'a> GreenTree<'a> {
  /// `green` at `version`, reading no content plane.
  pub(crate) fn at(green: &'a Green, version: u64) -> GreenTree<'a> {
    GreenTree {
      green,
      version,
      plane: None,
    }
  }

  /// [`GreenTree::at`], a named file read through `plane`, the shard's content plane.
  pub(crate) fn reading(
    green: &'a Green,
    version: u64,
    plane: Option<crate::vcs_store::named_objects::PlaneObjects<'a>>,
  ) -> GreenTree<'a> {
    GreenTree {
      green,
      version,
      plane,
    }
  }

  /// At most `into`'s length of the object `identity`'s bytes from `offset`, read through the plane: how many. A read
  /// past its end reads none; one the plane cannot give now (its windows let go, or no plane) is the try-again answer.
  fn read_named(
    &self,
    identity: slates_merge::named::Identity,
    offset: u64,
    into: &mut [u8],
  ) -> Result<usize, HostError> {
    use slates_merge::named::Objects;
    let left = identity.len.saturating_sub(offset);
    let take = usize::try_from(left).unwrap_or(usize::MAX).min(into.len());
    if take == 0 {
      return Ok(0);
    }
    let objects = self.plane.as_ref().ok_or_else(named_unread)?;
    let into = into.get_mut(..take).ok_or_else(named_unread)?;
    if objects.read(&identity, offset, into).is_err() {
      // Its windows let go (main's ruling (B)): noted for the call's async frame, which brings them back and serves
      // the read again (`named_objects::away_scope`); the try-again answer stays inside the bridge.
      crate::vcs_store::named_objects::note_if_away(
        objects,
        identity,
        (offset, u64::try_from(take).unwrap_or(u64::MAX)),
      );
      return Err(named_unread());
    }
    Ok(take)
  }

  /// Whether the version is the head (nothing changed since).
  fn at_head(&self) -> bool {
    self.version >= self.green.head()
  }

  /// The length of the file at `path` at the version.
  fn file_len(&self, path: &str) -> Option<u64> {
    self
      .green
      .content_len_at(path, self.version)
      .and_then(|len| u64::try_from(len).ok())
  }

  /// Whether `path` is a directory: one the green records, or one some file lies beneath.
  fn is_dir(&self, path: &str) -> bool {
    path.is_empty() || self.green.is_dir(path) || !self.green.beneath(path).is_empty()
  }
}

/// `path` relative to the directory whose prefix (`dir/`, or nothing for the root) is `prefix`.
fn beneath<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
  path.strip_prefix(prefix).filter(|rest| !rest.is_empty())
}

/// A read of a file the green holds by its content-plane object alone (A-760 (b) 4b), refused as a host's try-again
/// answer ([`TRY_AGAIN`]): the mount's reader tries again, as piece 4d's bring-back will have it.
fn named_unread() -> HostError {
  HostError::Unavailable(TRY_AGAIN)
}

/// Format: the host's try-again answer, typed by its numbering (A-539): `EAGAIN`, an errno.
#[cfg(unix)]
const TRY_AGAIN: slates_vfs::host::OsCode =
  slates_vfs::host::OsCode::Errno(rustix::io::Errno::AGAIN.raw_os_error());
/// Format: see the Unix arm; Windows' own, `ERROR_RETRY`, a Win32 error code.
#[cfg(windows)]
const TRY_AGAIN: slates_vfs::host::OsCode =
  slates_vfs::host::OsCode::Win32(windows_sys::Win32::Foundation::ERROR_RETRY);

impl TreeSource for GreenTree<'_> {
  fn entries(&self, dir: &str) -> Result<Vec<TreeEntry>, HostError> {
    if !self.is_dir(dir) {
      return Err(match self.file_len(dir) {
        Some(_) => HostError::NotDirectory,
        None => HostError::NotFound,
      });
    }
    let prefix = if dir.is_empty() {
      String::new()
    } else {
      format!("{dir}/")
    };
    let mut found: BTreeMap<String, (HostKind, u64)> = BTreeMap::new();
    let note = |path: &str, found: &mut BTreeMap<String, (HostKind, u64)>| {
      let Some(rest) = beneath(path, &prefix) else {
        return;
      };
      match rest.split_once('/') {
        Some((first, _)) => {
          found.insert(first.to_owned(), (HostKind::Dir, 0));
        }
        None => {
          if let Some(len) = self.file_len(path) {
            found.insert(rest.to_owned(), (HostKind::File, len));
          }
        }
      }
    };
    for (path, is_dir) in self.green.beneath(dir) {
      if is_dir {
        if let Some(rest) = beneath(&path, &prefix) {
          let first = rest.split_once('/').map_or(rest, |(first, _)| first);
          found.insert(first.to_owned(), (HostKind::Dir, 0));
        }
      } else {
        note(&path, &mut found);
      }
    }
    if !self.at_head() {
      for path in self.green.changed_between(self.version, self.green.head()) {
        note(&path, &mut found);
      }
    }
    for (path, _) in self.green.symlinks() {
      if let Some(rest) = beneath(&path, &prefix)
        && !rest.contains('/')
      {
        found.insert(rest.to_owned(), (HostKind::Symlink, 0));
      }
    }
    Ok(
      found
        .into_iter()
        .map(|(name, (kind, len))| TreeEntry {
          name: name.into_boxed_str(),
          kind,
          len,
        })
        .collect(),
    )
  }

  fn entry(&self, path: &str) -> Option<(HostKind, u64)> {
    if let Some(len) = self.file_len(path) {
      return Some((HostKind::File, len));
    }
    if self.green.symlink(path).is_some() {
      return Some((HostKind::Symlink, 0));
    }
    self.is_dir(path).then_some((HostKind::Dir, 0))
  }

  fn read_at(&self, path: &str, offset: u64, into: &mut [u8]) -> Result<usize, HostError> {
    let want = u64::try_from(into.len()).unwrap_or(u64::MAX);
    // Lent from the head at the head; a page of the file at the version otherwise.
    if self.at_head() {
      let bytes = match self.green.content(path).ok_or(HostError::NotFound)? {
        slates_merge::engine::Content::Bytes(bytes) => bytes,
        // Held by its object's name alone (A-760 (b) 4d): read through the plane.
        slates_merge::engine::Content::Named(identity) => {
          return self.read_named(identity, offset, into);
        }
      };
      let start = usize::try_from(offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
      let run = bytes.get(start..).unwrap_or_default();
      let take = run.len().min(into.len());
      into
        .get_mut(..take)
        .unwrap_or_default()
        .copy_from_slice(run.get(..take).unwrap_or_default());
      return Ok(take);
    }
    let (page, _) = self
      .green
      .content_page_at(path, self.version, (offset, want))
      .ok_or(HostError::NotFound)?;
    let page = match page {
      slates_merge::engine::ContentAt::Bytes(page) => page,
      slates_merge::engine::ContentAt::Named(identity) => {
        return self.read_named(identity, offset, into);
      }
    };
    let take = page.len().min(into.len());
    into
      .get_mut(..take)
      .unwrap_or_default()
      .copy_from_slice(page.get(..take).unwrap_or_default());
    Ok(take)
  }

  fn link_target(&self, path: &str) -> Result<Box<str>, HostError> {
    self
      .green
      .symlink(path)
      .map(Box::from)
      .ok_or(HostError::NotFound)
  }
}
