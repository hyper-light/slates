//! A tree object (A-590): its entries, each `<mode> SP <name> NUL <id>`, read as git's `decode_tree_entry` reads them
//! (tree-walk.c, with `parse_mode` from object.h), written back byte for byte, ordered as git orders them, and checked
//! as git's `fsck_tree` (fsck.c) checks them.
//!
//! An entry's mode is kept as written as well as read. git reads the octal digits into 32 bits and keeps the low 16, so
//! `1100644` reads as `100644`; and a mode written with a leading zero (`040000`), which old git wrote and fsck now
//! warns of (`zeroPaddedFilemode`), is served as written. A new entry takes one of the five modes git writes
//! ([`Mode`]), in the text git writes for it.

use core::cmp::Ordering;

use super::fsck::{FsckId, Report, Strictness};
use super::{ObjectRefused, Refusal, dotgit};
use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::out::Out;
use crate::{Admission, PastAdmission};

/// Format: a mode's file-type bits (`S_IFMT`).
const S_IFMT: u16 = 0o170000;
/// Format: a regular file's file type (`S_IFREG`).
const S_IFREG: u16 = 0o100000;
/// Format: a directory's file type (`S_IFDIR`), and the mode git writes for a subtree.
const S_IFDIR: u16 = 0o040000;
/// Format: a symbolic link's file type (`S_IFLNK`), and the mode git writes for one.
const S_IFLNK: u16 = 0o120000;
/// Format: a submodule's commit (object.h's `S_IFGITLINK`), and the mode git writes for one.
const S_IFGITLINK: u16 = 0o160000;
/// Format: a regular file git writes as not executable, `S_IFREG | 0644`.
const MODE_FILE: u16 = 0o100644;
/// Format: a regular file git writes as executable, `S_IFREG | 0755`.
const MODE_EXECUTABLE: u16 = 0o100755;
/// Format: the nonstandard mode git wrote "early on when we honored the full set of mode bits" (fsck.c), `S_IFREG |
/// 0664`: standard unless the check is strict.
const MODE_GROUP_WRITABLE: u16 = 0o100664;
/// Format: the owner's execute bit, by which git's checkout reads a regular file as executable or not
/// (`ce_permissions`, read-cache-ll.h).
const OWNER_EXECUTE: u16 = 0o100;
/// Format: the longest name fsck accepts without `largePathname`, in bytes (fsck.c's `max_tree_entry_len`, the default
/// of `fsck.largePathname`).
const LARGE_PATHNAME: usize = 4096;
/// Format: the fewest bytes `decode_tree_entry` reads an entry from besides its object id ("size < hashsz + 3").
const ENTRY_LEAST: usize = 3;
/// Format: the highest octal digit (`parse_mode`: "c < '0' || c > '7'").
const LAST_OCTAL_DIGIT: u8 = b'7';
/// Format: the bits one octal digit carries (`parse_mode`: `mode << 3`).
const OCTAL_DIGIT_BITS: u32 = 3;
/// Format: the byte between a path's components (`find_tree_entry`: "name[entrylen] != '/'").
const PATH_SEPARATOR: &[u8] = b"/";

/// A mode git writes: one of the five `fsck_tree` takes as standard. Each is written as git writes it (`%o`: no
/// leading zero, so a subtree is `40000`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
  /// A regular file, `100644`.
  File,
  /// An executable regular file, `100755`.
  Executable,
  /// A symbolic link, whose blob holds its target, `120000`.
  Symlink,
  /// A subtree, `40000`.
  Directory,
  /// A submodule's commit, `160000`.
  Gitlink,
}

impl Mode {
  /// Format: every mode git writes, the five `fsck_tree` takes as standard.
  pub const ALL: [Mode; 5] = [
    Mode::File,
    Mode::Executable,
    Mode::Symlink,
    Mode::Directory,
    Mode::Gitlink,
  ];

  /// The mode's bits.
  pub const fn bits(self) -> u16 {
    match self {
      Mode::File => MODE_FILE,
      Mode::Executable => MODE_EXECUTABLE,
      Mode::Symlink => S_IFLNK,
      Mode::Directory => S_IFDIR,
      Mode::Gitlink => S_IFGITLINK,
    }
  }

  /// The text git writes for the mode in a tree.
  pub const fn text(self) -> &'static [u8] {
    match self {
      Mode::File => b"100644",
      Mode::Executable => b"100755",
      Mode::Symlink => b"120000",
      Mode::Directory => b"40000",
      Mode::Gitlink => b"160000",
    }
  }

  /// The mode git writes with these bits, if any.
  pub fn from_bits(bits: u16) -> Option<Mode> {
    Mode::ALL.into_iter().find(|mode| mode.bits() == bits)
  }
}

/// One entry of a tree, as written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry<'a> {
  /// The mode's octal digits as written (`100644`, or a zero-padded `040000`).
  pub mode_text: &'a [u8],
  /// The mode git reads from them (`parse_mode`: the low 16 bits).
  pub mode: u16,
  /// The name.
  pub name: &'a [u8],
  /// The object id: raw bytes, the repository's hash length of them.
  pub id: &'a [u8],
}

impl<'a> Entry<'a> {
  /// A new entry as git writes one: `mode` in git's text, `name`, and `id`'s raw bytes.
  pub fn new(mode: Mode, name: &'a [u8], id: &'a [u8]) -> Self {
    Self {
      mode_text: mode.text(),
      mode: mode.bits(),
      name,
      id,
    }
  }

  /// Whether the entry is a subtree (`S_ISDIR`): it orders as if its name ended with `/`.
  pub fn is_dir(&self) -> bool {
    self.mode & S_IFMT == S_IFDIR
  }

  /// Whether the entry is a symbolic link (`S_ISLNK`).
  pub fn is_symlink(&self) -> bool {
    self.mode & S_IFMT == S_IFLNK
  }

  /// The type of object the entry names, by its mode (object.h's `object_type`): a subtree a tree, a submodule's link
  /// a commit, and anything else a blob.
  pub fn kind(&self) -> Kind {
    match self.mode & S_IFMT {
      S_IFDIR => Kind::Tree,
      S_IFGITLINK => Kind::Commit,
      _ => Kind::Blob,
    }
  }

  /// The mode, when it is one git writes.
  pub fn standard_mode(&self) -> Option<Mode> {
    Mode::from_bits(self.mode)
  }

  /// The mode git's tree walk reads the entry with, which a checkout writes: `decode_tree_entry` applies `canon_mode`
  /// to every mode it reads \[C: tree-walk.c, read-cache-ll.h, git v2.50.1\]. A regular file is executable when its
  /// owner's execute bit is set and a plain file otherwise, so a legacy `100664` reads as `100644`; a symbolic link and
  /// a directory read as themselves; any other mode, one of no known type included, reads as a submodule's link.
  /// Stock git 2.50.1 reads `100600` and `100664` as `100644`, `100700` and `100775` as `100755`, and `644` as
  /// `160000` (`git ls-tree`, and `git ls-files -s` after `git read-tree`, 2026-11-19).
  pub fn canonical_mode(&self) -> Mode {
    match self.mode & S_IFMT {
      S_IFREG if self.mode & OWNER_EXECUTE != 0 => Mode::Executable,
      S_IFREG => Mode::File,
      S_IFLNK => Mode::Symlink,
      S_IFDIR => Mode::Directory,
      _ => Mode::Gitlink,
    }
  }

  /// The entry's object id in `format`; none when its bytes are not that format's length.
  pub fn object_id(&self, format: ObjectFormat) -> Option<ObjectId> {
    ObjectId::from_bytes(format, self.id)
  }

  /// Writes the entry as it was read.
  pub fn write(&self, out: &mut impl Out) {
    out.put(self.mode_text);
    out.put(b" ");
    out.put(self.name);
    out.put(b"\0");
    out.put(self.id);
  }
}

/// git's order of two entries (`base_name_compare`, tree.c): their names byte by byte, a subtree's read as if it ended
/// with `/`. A new tree is written in this order.
pub fn compare(a: &Entry<'_>, b: &Entry<'_>) -> Ordering {
  let shared = a.name.len().min(b.name.len());
  let a_head = a.name.get(..shared).unwrap_or_default();
  let b_head = b.name.get(..shared).unwrap_or_default();
  a_head
    .cmp(b_head)
    .then_with(|| after_shared(a, shared).cmp(&after_shared(b, shared)))
}

/// The byte of `entry`'s name after its first `shared`, or, past the name's end, `/` for a subtree and the NUL that
/// ends a C string for anything else.
fn after_shared(entry: &Entry<'_>, shared: usize) -> u8 {
  match entry.name.get(shared) {
    Some(byte) => *byte,
    None if entry.is_dir() => b'/',
    None => b'\0',
  }
}

/// Writes `entries` in the order given: a tree's content. A new tree's entries are sorted by [`compare`] first.
pub fn write_entries(entries: &[Entry<'_>], out: &mut impl Out) {
  for entry in entries {
    entry.write(out);
  }
}

/// A tree: its entries in the order written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tree<'a> {
  /// The entries.
  entries: Vec<Entry<'a>>,
}

impl<'a> Tree<'a> {
  /// Parses `bytes`, a tree's content in a repository of `format`, as git's tree walk reads it: refused at the first
  /// entry git cannot read, as `fsck` reports `badTree` of it; otherwise every entry, counted and charged to
  /// `admission` before any is kept. A tree whose names or order fsck finds fault with is still read: judging it is
  /// [`fsck`]'s.
  pub fn parse(
    bytes: &'a [u8],
    format: ObjectFormat,
    admission: &mut Admission,
  ) -> Result<Self, ObjectRefused> {
    let mut count = 0_usize;
    for entry in entries(bytes, format) {
      entry?;
      count = count.saturating_add(1);
    }
    admission
      .charge_items::<Entry<'a>>(count)
      .map_err(|past| ObjectRefused {
        at: 0,
        refusal: Refusal::PastAdmission(past),
      })?;
    let mut kept = Vec::with_capacity(count);
    for entry in entries(bytes, format) {
      kept.push(entry?);
    }
    Ok(Self { entries: kept })
  }

  /// The entries in the order written.
  pub fn entries(&self) -> &[Entry<'a>] {
    &self.entries
  }

  /// The entry git's tree walk finds for `path`, and the path left below it (`tree-walk.c` `find_tree_entry`, git
  /// v2.50.1; A-640 part 4, a `sparse:oid` filter's `<rev>:<path>`): the entry named `path`, nothing left; or the
  /// directory named `path`'s first component, what follows its `/` left (nothing for a trailing `/`). The entries are
  /// searched in the order written, as git searches them: one longer than `path` or before its start is passed over,
  /// and one after its start ends the search, as does a file or a submodule where a directory is wanted, so in a tree
  /// whose entries are out of order this finds what git finds.
  pub fn find<'p>(&self, path: &'p [u8]) -> Option<(&Entry<'a>, &'p [u8])> {
    let _ = path;
    None
  }

  /// Writes the tree as it was read.
  pub fn write(&self, out: &mut impl Out) {
    write_entries(&self.entries, out);
  }
}

/// Decodes the entry at `from` as `decode_tree_entry` does: the entry and where the next begins, or the refusal.
fn entry_at(
  bytes: &[u8],
  from: usize,
  format: ObjectFormat,
) -> Result<(Entry<'_>, usize), ObjectRefused> {
  let refused = |at: usize, refusal: Refusal| ObjectRefused { at, refusal };
  let rest = bytes.get(from..).unwrap_or_default();
  let raw = format.raw_len();
  // git first asks that the bytes left end in a NUL and an id ("too-short tree object"): every name it then reads
  // ends at a NUL with an id after it, inside the tree.
  let id_from_end = raw.saturating_add(1);
  let ends_well = rest
    .len()
    .checked_sub(id_from_end)
    .and_then(|nul| rest.get(nul))
    == Some(&b'\0');
  if rest.len() < raw.saturating_add(ENTRY_LEAST) || !ends_well {
    return Err(refused(from, Refusal::TooShortTree));
  }
  let space = rest
    .iter()
    .position(|byte| !(b'0'..=LAST_OCTAL_DIGIT).contains(byte))
    .filter(|at| *at > 0 && rest.get(*at) == Some(&b' '))
    .ok_or_else(|| refused(from, Refusal::MalformedMode))?;
  let mode_text = rest.get(..space).unwrap_or_default();
  let name_from = space.saturating_add(1);
  // The NUL the check above found ends the name at the latest.
  let name_len = rest
    .get(name_from..)
    .and_then(|tail| tail.iter().position(|byte| *byte == b'\0'))
    .ok_or_else(|| refused(from, Refusal::TooShortTree))?;
  if name_len == 0 {
    return Err(refused(from.saturating_add(name_from), Refusal::EmptyName));
  }
  let id_from = name_from.saturating_add(name_len).saturating_add(1);
  let next = id_from.saturating_add(raw);
  // The check above makes both present; a tree that somehow lacks them is the same too-short tree, never a panic.
  let (Some(name), Some(id)) = (
    rest.get(name_from..id_from.saturating_sub(1)),
    rest.get(id_from..next),
  ) else {
    return Err(refused(from, Refusal::TooShortTree));
  };
  let entry = Entry {
    mode_text,
    mode: parse_mode(mode_text),
    name,
    id,
  };
  Ok((entry, from.saturating_add(next)))
}

/// `parse_mode`: octal digits into 32 bits, wrapping as C's unsigned arithmetic does, the low 16 kept.
fn parse_mode(digits: &[u8]) -> u16 {
  let wide = digits.iter().fold(0_u32, |mode, digit| {
    mode
      .checked_shl(OCTAL_DIGIT_BITS)
      .unwrap_or_default()
      .wrapping_add(u32::from(digit.wrapping_sub(b'0')))
  });
  u16::try_from(wide & u32::from(u16::MAX)).unwrap_or(u16::MAX)
}

/// Every entry of `bytes` in order, ending at the first refusal.
fn entries(
  bytes: &[u8],
  format: ObjectFormat,
) -> impl Iterator<Item = Result<Entry<'_>, ObjectRefused>> {
  let mut from = 0;
  let mut stopped = false;
  core::iter::from_fn(move || {
    if stopped || from >= bytes.len() {
      return None;
    }
    match entry_at(bytes, from, format) {
      Ok((entry, next)) => {
        from = next;
        Some(Ok(entry))
      }
      Err(refused) => {
        stopped = true;
        Some(Err(refused))
      }
    }
  })
}

/// What [`fsck`] found in a tree: its report, and the ids of the `.gitmodules` and `.gitattributes` blobs it names,
/// which git checks as those files once it holds them (`gitmodules_found`, `gitattributes_found`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked<'a> {
  /// The findings, in git's order.
  pub report: Report,
  /// The raw ids of the entries named `.gitmodules` (in any spelling HFS+ or NTFS resolves, a part after a backslash
  /// included) that are not symbolic links.
  pub gitmodules: Vec<&'a [u8]>,
  /// The raw ids of the entries named `.gitattributes` that are not symbolic links.
  pub gitattributes: Vec<&'a [u8]>,
}

/// What `verify_ordered` (fsck.c) says of two neighbouring entries.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
  /// In order.
  Ordered,
  /// Out of order (`TREE_UNORDERED`).
  Unordered,
  /// The same name twice, a file's and a subtree's included (`TREE_HAS_DUPS`).
  Duplicate,
}

/// `is_less_than_slash`: a byte that sorts after the NUL and before `/`.
fn below_slash(byte: u8) -> bool {
  byte != b'\0' && byte < b'/'
}

/// Keeps `item` on `kept`, its room charged to `admission` first.
fn keep<T>(kept: &mut Vec<T>, item: T, admission: &mut Admission) -> Result<(), PastAdmission> {
  admission.charge_items::<T>(1)?;
  kept.push(item);
  Ok(())
}

/// `verify_ordered` (fsck.c): orders entry `b` after `a`. `candidates` keeps the files whose name a later subtree could
/// repeat with only other entries between them (`foo`, then `foo.bar`, then `foo/`), each charged to `admission`.
fn verify_ordered<'a>(
  a: &Entry<'a>,
  b: &Entry<'a>,
  candidates: &mut Vec<&'a [u8]>,
  admission: &mut Admission,
) -> Result<Order, PastAdmission> {
  let shared = a.name.len().min(b.name.len());
  let head = |name: &'a [u8]| name.get(..shared).unwrap_or_default();
  match head(a.name).cmp(head(b.name)) {
    Ordering::Less => return Ok(Order::Ordered),
    Ordering::Greater => return Ok(Order::Unordered),
    Ordering::Equal => {}
  }
  let (c1, c2) = (after_shared(a, shared), after_shared(b, shared));
  if a.name.len() == b.name.len() {
    return Ok(Order::Duplicate);
  }
  if c1 == b'\0' && below_slash(c2) {
    keep(candidates, a.name, admission)?;
  } else if c2 == b'/' && below_slash(c1) && repeats_a_candidate(b.name, candidates) {
    return Ok(Order::Duplicate);
  }
  Ok(if c1 < c2 {
    Order::Ordered
  } else {
    Order::Unordered
  })
}

/// `verify_ordered`'s look back from a subtree named `dir`: pops the candidates `dir` cannot repeat, and says whether
/// it repeats one. A candidate `dir` begins with, followed by a byte below `/`, could still be repeated by a later
/// subtree, so it stays.
fn repeats_a_candidate(dir: &[u8], candidates: &mut Vec<&[u8]>) -> bool {
  while let Some(file) = candidates.pop() {
    let Some(rest) = dir.strip_prefix(file) else {
      continue;
    };
    match rest.first() {
      None => return true,
      Some(next) if below_slash(*next) => {
        candidates.push(file);
        return false;
      }
      Some(_) => {}
    }
  }
  false
}

/// The flags `fsck_tree` raises over a whole tree, reported after its walk.
#[derive(Default)]
struct Flags {
  /// `nullSha1`.
  null_id: bool,
  /// `fullPathname`.
  full_path: bool,
  /// `hasDot`.
  dot: bool,
  /// `hasDotdot`.
  dotdot: bool,
  /// `hasDotgit`.
  dotgit: bool,
  /// `zeroPaddedFilemode`.
  zero_padded: bool,
  /// `badFilemode`.
  bad_mode: bool,
  /// `duplicateEntries`.
  duplicate: bool,
  /// `treeNotSorted`.
  unsorted: bool,
  /// `largePathname`.
  large_name: bool,
}

/// Checks `bytes`, a tree of `format`, as git's `fsck_tree` does under `strictness`. What the check keeps beside the
/// bytes (the names a later subtree could repeat, the ids of the special blobs) is charged to `admission` as it grows.
pub fn fsck<'a>(
  bytes: &'a [u8],
  format: ObjectFormat,
  strictness: Strictness,
  admission: &mut Admission,
) -> Result<Checked<'a>, PastAdmission> {
  let mut checked = Checked {
    report: Report::new(strictness),
    gitmodules: Vec::new(),
    gitattributes: Vec::new(),
  };
  let mut walk = entries(bytes, format);
  // git decodes the first entry before its walk (`init_tree_desc_gently`): a tree whose first entry it cannot read is
  // `badTree` and nothing more.
  let mut current = match walk.next() {
    None => return Ok(checked),
    Some(Err(_)) => {
      checked.report.report(FsckId::BadTree);
      return Ok(checked);
    }
    Some(Ok(entry)) => entry,
  };
  let mut flags = Flags::default();
  let mut previous: Option<Entry<'a>> = None;
  let mut candidates = Vec::new();
  loop {
    check_names(
      &current,
      format,
      &mut flags,
      (&mut checked, &mut *admission),
    )?;
    // git decodes the next entry before it judges this one's mode and order (`update_tree_entry_gently`): one it
    // cannot read is `badTree`, reported now, and the walk ends with this entry's mode and order unjudged.
    let next = match walk.next() {
      Some(Err(_)) => {
        checked.report.report(FsckId::BadTree);
        break;
      }
      next => next.and_then(Result::ok),
    };
    flags.bad_mode |= !standard_mode(current.mode, strictness);
    if let Some(previous) = &previous {
      match verify_ordered(previous, &current, &mut candidates, admission)? {
        Order::Unordered => flags.unsorted = true,
        Order::Duplicate => flags.duplicate = true,
        Order::Ordered => {}
      }
    }
    previous = Some(current);
    match next {
      Some(entry) => current = entry,
      None => break,
    }
  }
  report_flags(&flags, &mut checked.report);
  Ok(checked)
}

/// Whether `fsck_tree` takes `mode` as standard under `strictness`: a mode git writes, or the old group-writable file
/// unless the check is strict.
fn standard_mode(mode: u16, strictness: Strictness) -> bool {
  Mode::from_bits(mode).is_some()
    || (mode == MODE_GROUP_WRITABLE && strictness == Strictness::Default)
}

/// The checks `fsck_tree` makes of an entry's name as it reaches it, before it decodes the next.
fn check_names<'a>(
  entry: &Entry<'a>,
  format: ObjectFormat,
  flags: &mut Flags,
  (checked, admission): (&mut Checked<'a>, &mut Admission),
) -> Result<(), PastAdmission> {
  let name = entry.name;
  flags.null_id |= entry.id.len() == format.raw_len() && entry.id.iter().all(|byte| *byte == 0);
  flags.full_path |= name.contains(&b'/');
  flags.dot |= name == b".";
  flags.dotdot |= name == b"..";
  flags.dotgit |= dotgit::dotgit(name);
  flags.zero_padded |= entry.mode_text.first() == Some(&b'0');
  flags.large_name |= name.len() > LARGE_PATHNAME;
  if dotgit::dotgitmodules(name) {
    gitmodules(entry, checked, admission)?;
  }
  if dotgit::dotgitattributes(name) {
    if entry.is_symlink() {
      checked.report.report(FsckId::GitattributesSymlink);
    } else {
      keep(&mut checked.gitattributes, entry.id, admission)?;
    }
  }
  if entry.is_symlink() && dotgit::dotgitignore(name) {
    checked.report.report(FsckId::GitignoreSymlink);
  }
  if entry.is_symlink() && dotgit::dotmailmap(name) {
    checked.report.report(FsckId::MailmapSymlink);
  }
  // NTFS reads a backslash as a separator: the rest of the name after each one is checked as a name of its own (the
  // rest, later backslashes included, as git's `strchr` walk hands it on; not the part up to the next backslash).
  let mut rests = name
    .iter()
    .enumerate()
    .filter(|(_, byte)| **byte == b'\\')
    .map(|(at, _)| name.get(at.saturating_add(1)..).unwrap_or_default());
  rests.try_for_each(|rest| {
    flags.dotgit |= dotgit::ntfs_dotgit(rest);
    if dotgit::ntfs_dotgitmodules(rest) {
      gitmodules(entry, checked, admission)
    } else {
      Ok(())
    }
  })
}

/// A `.gitmodules` entry: a symbolic link is `gitmodulesSymlink`, anything else is kept to be checked as the file.
fn gitmodules<'a>(
  entry: &Entry<'a>,
  checked: &mut Checked<'a>,
  admission: &mut Admission,
) -> Result<(), PastAdmission> {
  if entry.is_symlink() {
    checked.report.report(FsckId::GitmodulesSymlink);
    Ok(())
  } else {
    keep(&mut checked.gitmodules, entry.id, admission)
  }
}

/// The whole tree's flags as `fsck_tree` reports them after its walk, in its order. `emptyName` is never raised: an
/// entry with no name is one git cannot read (`badTree`).
fn report_flags(flags: &Flags, report: &mut Report) {
  let raised = [
    (flags.null_id, FsckId::NullSha1),
    (flags.full_path, FsckId::FullPathname),
    (flags.dot, FsckId::HasDot),
    (flags.dotdot, FsckId::HasDotdot),
    (flags.dotgit, FsckId::HasDotgit),
    (flags.zero_padded, FsckId::ZeroPaddedFilemode),
    (flags.bad_mode, FsckId::BadFilemode),
    (flags.duplicate, FsckId::DuplicateEntries),
    (flags.unsorted, FsckId::TreeNotSorted),
    (flags.large_name, FsckId::LargePathname),
  ];
  for (_, id) in raised.iter().filter(|(raised, _)| *raised) {
    report.report(*id);
  }
}
