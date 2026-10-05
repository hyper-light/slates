//! Directory listings at a view (§4.12 `slates.fs.list`; `ReadDir`): a directory's direct entries, one reply chunk at
//! a time. A green is listed at a version from the merge engine's state there (`Green::base_at`), a work from what it
//! keeps (its files and the directories and symlinks it declared), and a plain volume through the volume core's
//! paged listing (an overlay merges in the host directory it sits over).
//!
//! A page holds as many entries as its encoded size leaves room for in one reply chunk. A green's or work's listing
//! is a sorted list and its cursor a position in it, so a page of a view that changed between pages may repeat or
//! skip an entry, as POSIX allows `readdir` across a change. A plain volume's cursor is the volume core's resume hash
//! (§4.5): it never splits a group of entries sharing a cookie, and an entry that was not removed is returned exactly
//! once.

use std::collections::{BTreeMap, BTreeSet};

use slates_db::catalog::Principal;
use slates_db::catalog::VolumeId as DbVolumeId;
use slates_ipc::protocol::{DirEntry, EntryKind, ReadAt, Refusal, ReplyBody, VolumeId, framed_len};
use slates_merge::increment::VolumeOp;
use slates_vfs::dir::Child;
use slates_vfs::dir::{dir_cookie, resume_hash};
use slates_vfs::inode::Kind;
use slates_wire::Wire;

use crate::error::refusal_of_vfs;
use crate::merge_service::{canonical_path, find_record};
use crate::state::ShardState;
use crate::verbs::{find, forbidden, refused, rights_of};

/// Format: the fewest encoded bytes one entry takes (a one-byte name, its four-byte length, the kind's tag and the
/// eight-byte size): how many entries a plain volume's page asks the volume core for before sizing it exactly.
const MIN_ENTRY_BYTES: usize = 1 + 4 + 1 + 8;

/// `ReadDir`: one page of `path`'s entries in `volume` at `at`, from `cursor`.
pub(crate) fn read_dir(
  state: &mut ShardState,
  principal: &Principal,
  (volume, path, at): (VolumeId, &str, ReadAt),
  cursor: u64,
) -> ReplyBody {
  let record = match find_record(state, volume) {
    Ok(record) => record,
    Err(refusal) => return refused(refusal),
  };
  if !rights_of(&record, principal).read {
    return forbidden("list");
  }
  let dir = canonical_path(path).trim_end_matches('/');
  let room = page_room();
  match record.policy.role {
    slates_db::catalog::Role::Green { .. } => match green_entries(state, record.id, dir, at) {
      Ok(entries) => page_of_sorted(entries, cursor, room),
      Err(refusal) => refused(refusal),
    },
    slates_db::catalog::Role::Work { .. } => match at {
      ReadAt::Head => match state.works.get(&record.id) {
        Some(work) => match listed(work_paths(work), dir) {
          Some(entries) => page_of_sorted(entries, cursor, room),
          None => refused(Refusal::NotFound),
        },
        None => refused(Refusal::NotFound),
      },
      ReadAt::Version { .. } | ReadAt::Attachment { .. } => refused(Refusal::BadRequest {
        reason: "a work volume has no versions to list at; list its head".to_owned(),
      }),
    },
    slates_db::catalog::Role::Plain => match at {
      ReadAt::Head => plain_page(state, volume, dir, cursor, room),
      ReadAt::Version { .. } | ReadAt::Attachment { .. } => refused(Refusal::BadRequest {
        reason: "a plain volume has no green versions; list its head".to_owned(),
      }),
    },
  }
}

/// The bytes a page's entries may take: one reply chunk less the page's own framing with a cursor.
fn page_room() -> usize {
  let chunk = usize::try_from(crate::config::bulk_chunk_bytes()).unwrap_or(usize::MAX);
  let empty = ReplyBody::DirPage {
    entries: Vec::new(),
    next: Some(u64::MAX),
  };
  chunk.saturating_sub(framed_len(&empty))
}

/// The encoded bytes one entry adds to a page.
fn entry_bytes(entry: &DirEntry) -> usize {
  let mut encoded = Vec::new();
  entry.encode(&mut encoded);
  encoded.len()
}

/// The page of `entries` (sorted by name) from position `cursor`, as many as `room` holds (at least one, so a
/// listing always progresses).
fn page_of_sorted(entries: Vec<DirEntry>, cursor: u64, room: usize) -> ReplyBody {
  let start = usize::try_from(cursor).unwrap_or(usize::MAX);
  let mut used = 0usize;
  let mut page = Vec::new();
  let total = entries.len();
  for entry in entries.into_iter().skip(start) {
    let bytes = entry_bytes(&entry);
    if !page.is_empty() && used.saturating_add(bytes) > room {
      break;
    }
    used = used.saturating_add(bytes);
    page.push(entry);
  }
  let end = start.saturating_add(page.len());
  ReplyBody::DirPage {
    entries: page,
    next: (end < total).then(|| u64::try_from(end).unwrap_or(u64::MAX)),
  }
}

/// The direct entries of `dir` among `paths` (every path the view holds, each with its kind and size), sorted by
/// name: a path one component below `dir` is that entry; a deeper one implies a directory entry for its first
/// component. `None` when `dir` is neither the root nor a directory of the view.
fn listed(paths: Vec<(String, EntryKind, u64)>, dir: &str) -> Option<Vec<DirEntry>> {
  let prefix = if dir.is_empty() {
    String::new()
  } else {
    format!("{dir}/")
  };
  let mut exists = dir.is_empty();
  let mut entries: BTreeMap<String, DirEntry> = BTreeMap::new();
  for (path, kind, size) in paths {
    if path == dir {
      exists |= kind == EntryKind::Dir;
      continue;
    }
    let Some(rest) = path.strip_prefix(&prefix) else {
      continue;
    };
    exists = true;
    let (name, entry) = match rest.split_once('/') {
      Some((first, _)) => (first, (EntryKind::Dir, 0)),
      None => (rest, (kind, size)),
    };
    entries
      .entry(name.to_owned())
      .and_modify(|existing| {
        if entry.0 == EntryKind::Dir {
          existing.kind = EntryKind::Dir;
          existing.size = 0;
        }
      })
      .or_insert(DirEntry {
        name: name.to_owned(),
        kind: entry.0,
        size: entry.1,
      });
  }
  exists.then(|| entries.into_values().collect())
}

/// Every path a work holds: its files, and the directories and symlinks its journal declared and did not later
/// remove.
fn work_paths(work: &crate::state::WorkState) -> Vec<(String, EntryKind, u64)> {
  let mut dirs: BTreeSet<String> = BTreeSet::new();
  let mut symlinks: BTreeSet<String> = BTreeSet::new();
  for op in &work.journal {
    match op {
      VolumeOp::Mkdir { path } => {
        dirs.insert(path.clone());
      }
      VolumeOp::Rmdir { path } => {
        dirs.remove(path);
      }
      VolumeOp::Symlink { path, .. } => {
        symlinks.insert(path.clone());
      }
      VolumeOp::Unlink { path } => {
        symlinks.remove(path);
      }
      VolumeOp::Rename { from, to } => {
        if dirs.remove(from) {
          dirs.insert(to.clone());
        }
        if symlinks.remove(from) {
          symlinks.insert(to.clone());
        }
      }
      _ => {}
    }
  }
  let files = work.content.iter().map(|(path, bytes)| {
    (
      path.clone(),
      EntryKind::File,
      u64::try_from(bytes.len()).unwrap_or(u64::MAX),
    )
  });
  files
    .chain(dirs.into_iter().map(|path| (path, EntryKind::Dir, 0)))
    .chain(
      symlinks
        .into_iter()
        .map(|path| (path, EntryKind::Symlink, 0)),
    )
    .collect()
}

/// A green's entries of `dir` at the version `at` names.
fn green_entries(
  state: &ShardState,
  green: DbVolumeId,
  dir: &str,
  at: ReadAt,
) -> Result<Vec<DirEntry>, Refusal> {
  let engine = state.greens.get(&green).ok_or(Refusal::NotFound)?;
  let version = crate::merge_service::green_version(state, engine, green, at)?;
  let base = engine.base_at(version);
  let paths = base
    .files
    .into_iter()
    .map(|(path, size)| (path, EntryKind::File, size))
    .chain(base.dirs.into_iter().map(|path| (path, EntryKind::Dir, 0)))
    .chain(
      base
        .symlinks
        .into_iter()
        .map(|(path, _)| (path, EntryKind::Symlink, 0)),
    )
    .chain(
      base
        .specials
        .into_iter()
        .map(|(path, _)| (path, EntryKind::Other, 0)),
    )
    .collect();
  listed(paths, dir).ok_or(Refusal::NotFound)
}

/// One entry the volume core listed, owned.
#[derive(Clone, Debug)]
struct Row {
  name: String,
  hash: u64,
  kind: Kind,
  inode: slates_vfs::ids::InodeNo,
}

/// A plain volume's page of `dir` from resume hash `cursor`: as many whole cookie groups as `room` holds (at least
/// one), each file sized from its attributes.
fn plain_page(
  state: &mut ShardState,
  volume: VolumeId,
  dir: &str,
  cursor: u64,
  room: usize,
) -> ReplyBody {
  let (handle, _) = match find(state, volume) {
    Ok(found) => found,
    Err(reply) => return *reply,
  };
  let ShardState { store, volumes, .. } = state;
  let Ok(slot) = volumes.get_mut(handle) else {
    return refused(Refusal::NotFound);
  };
  let want = room / MIN_ENTRY_BYTES;
  let listed: Result<Vec<(Row, u64)>, slates_vfs::error::VfsError> = match slot.host.as_mut() {
    Some(host) => {
      let mut overlay = slot.volume.with_host(host);
      overlay.resolve(store, dir).and_then(|located| {
        if !matches!(located.child, Child::Dir(_)) {
          return Err(slates_vfs::error::VfsError::NotDirectory);
        }
        let rows: Vec<Row> = overlay
          .readdir_page_no(store, located.inode, cursor, want.max(1))?
          .into_iter()
          .map(|row| Row {
            name: row.name.to_owned(),
            hash: row.hash,
            kind: row.kind,
            inode: row.inode,
          })
          .collect();
        rows
          .into_iter()
          .map(|row| {
            let size = match row.kind {
              Kind::File => overlay.observe(store, row.inode)?.attrs.size,
              _ => 0,
            };
            Ok((row, size))
          })
          .collect()
      })
    }
    None => slot.volume.resolve(store, dir).and_then(|located| {
      let Child::Dir(dir_handle) = located.child else {
        return Err(slates_vfs::error::VfsError::NotDirectory);
      };
      let rows: Vec<Row> = slot
        .volume
        .readdir_page(store, dir_handle, cursor, want.max(1))?
        .into_iter()
        .map(|row| Row {
          name: row.name.to_owned(),
          hash: row.hash,
          kind: row.kind,
          inode: row.inode,
        })
        .collect();
      rows
        .into_iter()
        .map(|row| {
          let size = match row.kind {
            Kind::File => slot.volume.observe(store, row.inode)?.attrs.size,
            _ => 0,
          };
          Ok((row, size))
        })
        .collect()
    }),
  };
  match listed {
    Ok(rows) => plain_page_of(rows, want, room),
    Err(slates_vfs::error::VfsError::NotDirectory) => refused(Refusal::BadRequest {
      reason: "the path is not a directory".to_owned(),
    }),
    Err(e) => refused(refusal_of_vfs(&e)),
  }
}

/// The page of a plain volume's `rows` (asked for `want` entries): whole cookie groups while they fit `room` (the
/// first always), continued from the resume hash after the last group taken.
fn plain_page_of(rows: Vec<(Row, u64)>, want: usize, room: usize) -> ReplyBody {
  let got_all = rows.len() < want.max(1);
  let mut page: Vec<DirEntry> = Vec::new();
  let mut used = 0usize;
  let mut last_cookie: Option<u64> = None;
  let mut cut = false;
  let mut group: Vec<DirEntry> = Vec::new();
  let mut group_bytes = 0usize;
  let mut group_cookie: Option<u64> = None;
  let flush =
    |page: &mut Vec<DirEntry>, used: &mut usize, group: &mut Vec<DirEntry>, bytes: &mut usize| {
      page.append(group);
      *used = used.saturating_add(*bytes);
      *bytes = 0;
    };
  for (row, size) in rows {
    let cookie = dir_cookie(row.hash);
    let entry = DirEntry {
      name: row.name,
      kind: entry_kind(row.kind),
      size,
    };
    if group_cookie.is_some_and(|current| current != cookie) {
      if !page.is_empty() && used.saturating_add(group_bytes) > room {
        cut = true;
        break;
      }
      flush(&mut page, &mut used, &mut group, &mut group_bytes);
      last_cookie = group_cookie;
    }
    group_cookie = Some(cookie);
    group_bytes = group_bytes.saturating_add(entry_bytes(&entry));
    group.push(entry);
  }
  if !cut && !group.is_empty() {
    if page.is_empty() || used.saturating_add(group_bytes) <= room {
      flush(&mut page, &mut used, &mut group, &mut group_bytes);
      last_cookie = group_cookie;
    } else {
      cut = true;
    }
  }
  let ended = got_all && !cut;
  ReplyBody::DirPage {
    entries: page,
    next: if ended {
      None
    } else {
      last_cookie.and_then(resume_hash)
    },
  }
}

/// The listing kind of a volume-core kind.
fn entry_kind(kind: Kind) -> EntryKind {
  match kind {
    Kind::File => EntryKind::File,
    Kind::Dir => EntryKind::Dir,
    Kind::Symlink => EntryKind::Symlink,
    _ => EntryKind::Other,
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
  use super::*;

  /// A row named `name` whose hash carries cookie `cookie`.
  fn row(name: &str, cookie: u64) -> (Row, u64) {
    let hash = cookie << (u64::BITS - slates_vfs::dir::COOKIE_BITS);
    (
      Row {
        name: name.to_owned(),
        hash,
        kind: Kind::File,
        inode: slates_vfs::ids::InodeNo(1),
      },
      7,
    )
  }

  fn names(reply: &ReplyBody) -> (Vec<String>, Option<u64>) {
    match reply {
      ReplyBody::DirPage { entries, next } => {
        (entries.iter().map(|e| e.name.clone()).collect(), *next)
      }
      other => panic!("{other:?}"),
    }
  }

  /// §4.5, §4.12 (`ReadDir` on a plain volume): do page rows whose last two share a cookie with room for only part
  /// of the group; expect the page to stop before the group (never splitting it) and continue from the resume hash
  /// after the last cookie taken. Given every row the volume core had (fewer than asked), expect the listing to end.
  #[test]
  fn a_plain_page_keeps_cookie_groups_whole_and_resumes_after_them() {
    let rows = vec![row("a", 10), row("b", 11), row("c", 12), row("d", 12)];
    let one = entry_bytes(&DirEntry {
      name: "a".to_owned(),
      kind: EntryKind::File,
      size: 7,
    });
    let (taken, next) = names(&plain_page_of(rows.clone(), 4, one * 3));
    assert_eq!(taken, vec!["a", "b"], "the c/d group does not fit whole");
    assert_eq!(next, resume_hash(11), "continues after b's cookie");
    let (all, ended) = names(&plain_page_of(rows, 8, one * 8));
    assert_eq!(all, vec!["a", "b", "c", "d"]);
    assert_eq!(ended, None, "fewer rows than asked: the listing ended");
  }

  /// §4.12 (`ReadDir` of a work or green): do list a path set; expect a nested path to imply its directory, a file
  /// at the directory itself to make it no directory, and an unknown directory to be `None`.
  #[test]
  fn a_path_set_lists_direct_children_and_implies_directories() {
    let paths = vec![
      ("a.txt".to_owned(), EntryKind::File, 3),
      ("src/lib.rs".to_owned(), EntryKind::File, 9),
      ("src/deep/x.rs".to_owned(), EntryKind::File, 1),
    ];
    let root = listed(paths.clone(), "").unwrap();
    assert_eq!(
      root
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect::<Vec<_>>(),
      vec![("a.txt", EntryKind::File), ("src", EntryKind::Dir)]
    );
    let src = listed(paths.clone(), "src").unwrap();
    assert_eq!(
      src
        .iter()
        .map(|e| (e.name.as_str(), e.kind))
        .collect::<Vec<_>>(),
      vec![("deep", EntryKind::Dir), ("lib.rs", EntryKind::File)]
    );
    assert!(listed(paths, "nope").is_none());
  }
}
