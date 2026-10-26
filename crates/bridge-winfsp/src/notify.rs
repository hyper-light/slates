//! What a WinFsp mount tells Windows when the volume changes through another path (§4.6 "Windows": `FileInfoTimeout =
//! -1` with `FspFileSystemNotify` invalidations; the analogue of the FUSE mount's notifications, A-141). Pure and
//! cfg-free, so every rule here is unit-tested on any host; the Windows mount host hands the encoded round to
//! `FspFileSystemNotify`.
//!
//! **Why names, and only open ones.** WinFsp's notification names files by path, and the kernel acts on a name only
//! where it has a file node under it: `FspFileNodeInvalidateCachesAndNotifyChangeByName` (WinFsp `src/sys/file.c`)
//! looks the name up among the open nodes, flushes and purges that node's data section, drops its cached file info,
//! security, directory listing, streams and extended attributes, and tells the directory's watchers; for a name with
//! no node it tells the parent's watchers and drops the parent's listing. A file node lives from the first open of a
//! name until the last close of a file object on it (the Cache Manager's own reference included, so a file whose
//! cached pages outlive its handle keeps its node), and every cache WinFsp keeps hangs off a node. So the names Windows
//! can hold stale are exactly the names of contexts it holds open, which [`OpenNames`] tracks from the open's reply to
//! its close. The table is keyed by the bridge's open handle, one entry per handle the bridge's bounded handle store
//! granted, so it is bounded by that store and refuses nothing of its own.
//!
//! **What a change becomes.** An inode's change ([`Invalidation::Inode`]) is a `FILE_ACTION_MODIFIED` of every name it
//! is open under. A name's change in a directory ([`Invalidation::Entry`]) is a `FILE_ACTION_MODIFIED` of that name
//! under every name the directory is open under: the journal says that the name changed, not whether it was added or
//! removed, and the kernel's invalidation is the same either way; a watcher reads the directory again either way.
//! [`Invalidation::Everything`] (a journal overrun) is every open name. An entry under a directory Windows does not hold
//! open needs no word: nothing of that directory is cached, and a watcher on it would hold it open.
//!
//! **The record.** `FSP_FSCTL_NOTIFY_INFO` (`fsctl.h`, `static_assert` 12 bytes): `UINT16 Size`, two bytes of padding,
//! `UINT32 Filter`, `UINT32 Action`, then the name in UTF-16 with no terminator; `Size` counts the head and the name,
//! and each record starts at the next multiple of `FSP_FSCTL_DEFAULT_ALIGNMENT` (8), as the kernel walks them
//! (`FspVolumeNotifyWork`, `src/sys/volume.c`). WinFsp paths are at most `FSP_FSCTL_TRANSACT_PATH_SIZEMAX` (1024 UTF-16
//! units), so a record always fits its 16-bit size; a longer name than any WinFsp path is refused, counted, and left
//! out rather than truncated into a different name.

use std::collections::BTreeMap;

use slates_bridge_core::Invalidation;

/// Format: `FILE_NOTIFY_CHANGE_FILE_NAME` (winnt.h): a name was added, removed or renamed.
pub const FILE_NOTIFY_CHANGE_FILE_NAME: u32 = 0x0000_0001;
/// Format: `FILE_NOTIFY_CHANGE_DIR_NAME` (winnt.h): a directory's name was added, removed or renamed.
pub const FILE_NOTIFY_CHANGE_DIR_NAME: u32 = 0x0000_0002;
/// Format: `FILE_NOTIFY_CHANGE_ATTRIBUTES` (winnt.h).
pub const FILE_NOTIFY_CHANGE_ATTRIBUTES: u32 = 0x0000_0004;
/// Format: `FILE_NOTIFY_CHANGE_SIZE` (winnt.h).
pub const FILE_NOTIFY_CHANGE_SIZE: u32 = 0x0000_0008;
/// Format: `FILE_NOTIFY_CHANGE_LAST_WRITE` (winnt.h).
pub const FILE_NOTIFY_CHANGE_LAST_WRITE: u32 = 0x0000_0010;
/// Format: `FILE_ACTION_MODIFIED` (winnt.h): the named object changed.
pub const FILE_ACTION_MODIFIED: u32 = 0x0000_0003;

/// Format: the filter of an inode's change: its bytes, its size and its attributes may all have changed.
const INODE_FILTER: u32 =
  FILE_NOTIFY_CHANGE_LAST_WRITE | FILE_NOTIFY_CHANGE_SIZE | FILE_NOTIFY_CHANGE_ATTRIBUTES;
/// Format: the filter of a name's change in a directory: a file's or a directory's name.
const ENTRY_FILTER: u32 = FILE_NOTIFY_CHANGE_FILE_NAME | FILE_NOTIFY_CHANGE_DIR_NAME;
/// Format: the filter of a journal overrun: anything about the name.
const EVERYTHING_FILTER: u32 = INODE_FILTER | ENTRY_FILTER;

/// Format: `sizeof(FSP_FSCTL_NOTIFY_INFO)`, the record's fixed head (`fsctl.h` `static_assert`).
pub const NOTIFY_INFO_HEAD: usize = 12;
/// Format: `FSP_FSCTL_DEFAULT_ALIGNMENT` (`fsctl.h`): each record starts at a multiple of it.
pub const NOTIFY_INFO_ALIGNMENT: usize = 8;
/// Format: `FSP_FSCTL_TRANSACT_PATH_SIZEMAX / sizeof(WCHAR)` (`fsctl.h`): the longest path WinFsp hands a file system,
/// in UTF-16 units. A name longer than this was never handed out, so it is never told.
pub const MAX_PATH_UNITS: usize = 1024;

/// The names Windows holds open on a WinFsp mount, by the bridge's open handle: each the inode it names and the path it
/// was opened by, as the kernel's file node is named. See the module docs for why these are what Windows can hold stale.
#[derive(Debug, Default)]
pub struct OpenNames {
  /// Open handle → (inode, path from the volume's root, `\`-separated, as WinFsp names it).
  by_handle: BTreeMap<u64, (u64, String)>,
}

impl OpenNames {
  /// An empty table: a mount Windows holds nothing of yet.
  pub fn new() -> OpenNames {
    OpenNames::default()
  }

  /// Handle `handle` was opened on inode `ino` by `path`.
  pub fn opened(&mut self, handle: u64, ino: u64, path: &str) {
    self.by_handle.insert(handle, (ino, path.to_owned()));
  }

  /// Handle `handle` was closed: Windows no longer holds its node through it.
  pub fn closed(&mut self, handle: u64) {
    self.by_handle.remove(&handle);
  }

  /// A rename through the mount moved `from` to `to`: WinFsp renames the open node `from` and every node beneath it, so
  /// the names Windows holds under the old path are now under the new.
  pub fn renamed(&mut self, from: &str, to: &str) {
    for (_, path) in self.by_handle.values_mut() {
      if let Some(rest) = beneath(path, from) {
        *path = format!("{to}{rest}");
      }
    }
  }

  /// How many handles Windows holds open.
  pub fn len(&self) -> usize {
    self.by_handle.len()
  }

  /// Whether Windows holds nothing open.
  pub fn is_empty(&self) -> bool {
    self.by_handle.is_empty()
  }

  /// Every distinct name inode `ino` is open under.
  fn names_of(&self, ino: u64) -> Vec<&str> {
    let mut names: Vec<&str> = self
      .by_handle
      .values()
      .filter(|(open, _)| *open == ino)
      .map(|(_, path)| path.as_str())
      .collect();
    names.sort_unstable();
    names.dedup();
    names
  }

  /// Every distinct name open.
  fn every_name(&self) -> Vec<&str> {
    let mut names: Vec<&str> = self
      .by_handle
      .values()
      .map(|(_, path)| path.as_str())
      .collect();
    names.sort_unstable();
    names.dedup();
    names
  }
}

/// The rest of `path` after `prefix` when `path` is `prefix` or lies beneath it (`prefix\…`), else `None`. The root
/// (`\`) has every path beneath it.
fn beneath<'p>(path: &'p str, prefix: &str) -> Option<&'p str> {
  let rest = path.strip_prefix(prefix)?;
  if rest.is_empty() || rest.starts_with('\\') || prefix.ends_with('\\') {
    Some(rest)
  } else {
    None
  }
}

/// The path of `name` in the directory named `parent` (the root is `\`).
fn child_path(parent: &str, name: &str) -> String {
  if parent.ends_with('\\') {
    format!("{parent}{name}")
  } else {
    format!("{parent}\\{name}")
  }
}

/// One round of notifications for `FspFileSystemNotify`: the records' bytes, back to back, each aligned.
#[derive(Debug, Default)]
pub struct NotifyRound {
  bytes: Vec<u8>,
  /// Records in the round.
  records: usize,
  /// Names left out because they were longer than any WinFsp path (see the module docs).
  too_long: u64,
}

impl NotifyRound {
  /// An empty round.
  pub fn new() -> NotifyRound {
    NotifyRound::default()
  }

  /// Adds the records `invalidation` owes, naming what Windows holds open (`names`).
  pub fn add(&mut self, invalidation: &Invalidation, names: &OpenNames) {
    match invalidation {
      Invalidation::Inode { ino, .. } => {
        for name in names.names_of(*ino) {
          self.record(name, INODE_FILTER);
        }
      }
      Invalidation::Entry { parent, name, .. } => {
        for directory in names.names_of(*parent) {
          let path = child_path(directory, name);
          self.record(&path, ENTRY_FILTER);
        }
      }
      Invalidation::Everything => {
        for name in names.every_name() {
          self.record(name, EVERYTHING_FILTER);
        }
      }
    }
  }

  /// Appends one `FSP_FSCTL_NOTIFY_INFO` record naming `path`, at the next aligned offset.
  fn record(&mut self, path: &str, filter: u32) {
    let units: Vec<u16> = path.encode_utf16().collect();
    let Some(size) = units
      .len()
      .checked_mul(std::mem::size_of::<u16>())
      .and_then(|name| name.checked_add(NOTIFY_INFO_HEAD))
      .filter(|_| units.len() <= MAX_PATH_UNITS)
      .and_then(|size| u16::try_from(size).ok())
    else {
      self.too_long = self.too_long.saturating_add(1);
      return;
    };
    let start = aligned(self.bytes.len());
    self.bytes.resize(start, 0);
    self.bytes.extend_from_slice(&size.to_le_bytes());
    self.bytes.extend_from_slice(&[0, 0]);
    self.bytes.extend_from_slice(&filter.to_le_bytes());
    self
      .bytes
      .extend_from_slice(&FILE_ACTION_MODIFIED.to_le_bytes());
    for unit in units {
      self.bytes.extend_from_slice(&unit.to_le_bytes());
    }
    self.records = self.records.saturating_add(1);
  }

  /// The round's bytes, for `FspFileSystemNotify`.
  pub fn bytes(&self) -> &[u8] {
    &self.bytes
  }

  /// How many records the round holds.
  pub fn records(&self) -> usize {
    self.records
  }

  /// Whether the round tells Windows nothing.
  pub fn is_empty(&self) -> bool {
    self.records == 0
  }

  /// Names left out of the round for being longer than any WinFsp path; the transport counts them.
  pub fn too_long(&self) -> u64 {
    self.too_long
  }
}

/// `offset` rounded up to the record alignment (`FSP_FSCTL_DEFAULT_ALIGN_UP`). Saturates at the top of the address
/// space, where no round can reach.
fn aligned(offset: usize) -> usize {
  offset
    .checked_next_multiple_of(NOTIFY_INFO_ALIGNMENT)
    .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
  use super::*;
  use slates_bridge_core::CachedBytes;

  /// The records of a round as the kernel walks them (`FspVolumeNotifyWork`): each record's size, filter, action and
  /// name, the next at the aligned offset past its size.
  fn walk(bytes: &[u8]) -> Vec<(u32, u32, String)> {
    let mut out = Vec::new();
    let mut at = 0usize;
    while at + 2 <= bytes.len() {
      let size = usize::from(u16::from_le_bytes([bytes[at], bytes[at + 1]]));
      assert!(
        size >= NOTIFY_INFO_HEAD,
        "a record at {at} is shorter than its head"
      );
      let filter = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap());
      let action = u32::from_le_bytes(bytes[at + 8..at + 12].try_into().unwrap());
      let units: Vec<u16> = bytes[at + NOTIFY_INFO_HEAD..at + size]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_le_bytes(*pair))
        .collect();
      out.push((filter, action, String::from_utf16(&units).unwrap()));
      at = (at + size).next_multiple_of(NOTIFY_INFO_ALIGNMENT);
    }
    out
  }

  /// §4.6 "Windows". Do: open a file by two names (two handles on one inode, as hard links would) and a third handle on
  /// another file; change the first inode's bytes. Expect: one modified record per distinct name of that inode, with the
  /// write, size and attribute filter, and nothing for the other file.
  #[test]
  fn an_inodes_change_names_every_path_it_is_open_under() {
    let mut names = OpenNames::new();
    names.opened(1, 7, "\\a\\file.bin");
    names.opened(2, 7, "\\b\\link.bin");
    names.opened(3, 7, "\\a\\file.bin");
    names.opened(4, 9, "\\other.txt");
    let mut round = NotifyRound::new();
    round.add(
      &Invalidation::Inode {
        ino: 7,
        data: CachedBytes::Range { at: 0, len: 1 },
      },
      &names,
    );
    assert_eq!(
      walk(round.bytes()),
      vec![
        (
          INODE_FILTER,
          FILE_ACTION_MODIFIED,
          "\\a\\file.bin".to_owned()
        ),
        (
          INODE_FILTER,
          FILE_ACTION_MODIFIED,
          "\\b\\link.bin".to_owned()
        ),
      ]
    );
    assert_eq!(round.records(), 2);
  }

  /// §4.6 "Windows". Do: hold the root and a directory open; a name changes in each, and one in a directory nobody
  /// holds. Expect: the name under the root as `\name` (no doubled separator), the name under the directory as
  /// `\dir\name`, and nothing for the unheld directory, whose listing nothing caches.
  #[test]
  fn an_entrys_change_names_the_entry_under_each_open_name_of_its_directory() {
    let mut names = OpenNames::new();
    names.opened(1, 1, "\\");
    names.opened(2, 5, "\\dir");
    let mut round = NotifyRound::new();
    for (parent, name) in [(1, "new.txt"), (5, "gone.txt"), (6, "unheld.txt")] {
      round.add(
        &Invalidation::Entry {
          parent,
          name: name.to_owned(),
          expire: false,
        },
        &names,
      );
    }
    assert_eq!(
      walk(round.bytes()),
      vec![
        (ENTRY_FILTER, FILE_ACTION_MODIFIED, "\\new.txt".to_owned()),
        (
          ENTRY_FILTER,
          FILE_ACTION_MODIFIED,
          "\\dir\\gone.txt".to_owned()
        ),
      ]
    );
  }

  /// §4.6 "Windows". Do: open three names, close one, then rename a directory through the mount; a journal overrun
  /// follows. Expect: every name still open, under its new path, each once.
  #[test]
  fn an_overrun_names_every_open_name_after_closes_and_renames() {
    let mut names = OpenNames::new();
    names.opened(1, 2, "\\src\\main.rs");
    names.opened(2, 3, "\\src");
    names.opened(3, 4, "\\srcs\\other.rs");
    names.opened(4, 5, "\\gone.txt");
    names.closed(4);
    names.renamed("\\src", "\\lib");
    let mut round = NotifyRound::new();
    round.add(&Invalidation::Everything, &names);
    let told: Vec<String> = walk(round.bytes()).into_iter().map(|(_, _, n)| n).collect();
    assert_eq!(
      told,
      vec![
        "\\lib".to_owned(),
        "\\lib\\main.rs".to_owned(),
        "\\srcs\\other.rs".to_owned(),
      ],
      "a sibling sharing the prefix (`\\srcs`) is not beneath `\\src`"
    );
    assert_eq!(names.len(), 3);
  }

  /// Format: `FSP_FSCTL_NOTIFY_INFO`. Do: encode a name of three units (an 18-byte record) and then one of two. Expect:
  /// each size counts the 12-byte head and two bytes a unit, the padding after the size is zero, and the second record
  /// starts at 24, the first multiple of 8 past the first — the layout the kernel walks.
  #[test]
  fn records_are_laid_out_as_the_kernel_walks_them() {
    let mut names = OpenNames::new();
    names.opened(1, 1, "\\ab");
    names.opened(2, 2, "\\c");
    let mut round = NotifyRound::new();
    round.add(&Invalidation::Everything, &names);
    let bytes = round.bytes();
    assert_eq!(u16::from_le_bytes([bytes[0], bytes[1]]), 12 + 6);
    assert_eq!(&bytes[2..4], &[0, 0], "the padding after the size");
    assert_eq!(
      &bytes[18..24],
      &[0; 6],
      "the alignment padding between records"
    );
    assert_eq!(u16::from_le_bytes([bytes[24], bytes[25]]), 12 + 4);
    assert_eq!(bytes.len(), 24 + 16);
  }

  /// Format: `FSP_FSCTL_TRANSACT_PATH_SIZEMAX`. Do: hold open a name of 1024 units and one of 1025. Expect: the first is
  /// told and the second left out and counted, never truncated into another name.
  #[test]
  fn a_name_longer_than_any_winfsp_path_is_counted_not_truncated() {
    let mut names = OpenNames::new();
    let longest = format!("\\{}", "x".repeat(MAX_PATH_UNITS - 1));
    let longer = format!("\\{}", "y".repeat(MAX_PATH_UNITS));
    names.opened(1, 1, &longest);
    names.opened(2, 2, &longer);
    let mut round = NotifyRound::new();
    round.add(&Invalidation::Everything, &names);
    assert_eq!(round.records(), 1);
    assert_eq!(round.too_long(), 1);
    assert_eq!(walk(round.bytes())[0].2, longest);
  }
}
