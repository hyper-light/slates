//! The view's three small files (A-665), each made as git 2.50.1 makes it: `HEAD`, a symbolic ref
//! to the default branch; `config`, the lines `git init --bare` writes for the repository's object
//! format; and `packed-refs`, every ref the view shows, sorted, with the peeled line of each that
//! names an annotated tag. The evidence is `fixtures/view` (`make-view-fixtures.sh`).

use slates_vcs::ContentStore;

use crate::object_name::{Kind, ObjectFormat, ObjectId};
use crate::objects::tag::Tag;
use crate::store::{Repository, StoreRefused};
use crate::{Admission, refname, to_u64};

use super::units::copy;
use super::{ShownRef, ViewRefused};

/// Format: what a symbolic ref's file holds before its target, which a line feed ends
/// (`refs/files-backend.c`, "ref: %s\n"; `fixtures/view/init.HEAD`).
const SYMREF: &[u8] = b"ref: ";

/// Format: the section `initialize_repository_version` opens for an object format other than
/// SHA-1, and its key (`setup.c`; `fixtures/view/init-sha256.config`): set before
/// `core.repositoryformatversion`, so its section comes first.
const EXTENSIONS: &[u8] = b"[extensions]\n\tobjectformat = ";

/// Format: the `[core]` section's first key (`setup.c` `initialize_repository_version`).
const CORE_VERSION: &[u8] = b"[core]\n\trepositoryformatversion = ";

/// Format: the repository format version git writes for SHA-1, `GIT_REPO_VERSION` (`setup.c`).
const VERSION_SHA1: &[u8] = b"0";

/// Format: the version git writes for any other object format, `GIT_REPO_VERSION_READ`, since
/// only a reader of version 1 reads `extensions.objectformat` (`setup.c`).
const VERSION_EXTENDED: &[u8] = b"1";

/// Format: the rest of `[core]` as `create_default_files` writes it for a bare repository on a
/// filesystem whose files' modes change and whose names keep their case and bytes, as the view's
/// do: `fixtures/view/init.config` less the `ignorecase` and `precomposeunicode` lines macOS's
/// probes of its own filesystem add.
const CORE_REST: &[u8] = b"\tfilemode = true\n\tbare = true\n";

/// Format: `packed-refs`' first line, the traits git 2.50.1 writes (`refs/packed-backend.c`
/// `PACKED_REFS_HEADER`; `fixtures/view/bare.packed-refs`).
pub(super) const PACKED_HEADER: &[u8] = b"# pack-refs with: peeled fully-peeled sorted \n";

/// Format: what begins a peeled line (`write_packed_entry`: "^%s\n").
const PEELED: u8 = b'^';

/// Format: what ends each line.
const LINE_END: u8 = b'\n';

/// Format: what lies between a ref's object and its name.
const BETWEEN: u8 = b' ';

/// Format: the bytes a line adds to the object's hexadecimal and the name, a space and a line
/// feed; and a peeled line adds to its object's, a `^` and a line feed (`write_packed_entry`).
const FRAMING: u64 = 2;

/// Format: where every ref the view packs lies; `HEAD` is a file of its own.
pub(super) const REFS: &[u8] = b"refs/";

/// `HEAD`'s bytes, a symbolic ref to `target`.
pub(super) fn head(target: &[u8]) -> Vec<u8> {
  let mut bytes = SYMREF.to_vec();
  bytes.extend_from_slice(target);
  bytes.push(LINE_END);
  bytes
}

/// `config`'s bytes for a repository of `format`.
pub(super) fn config(format: ObjectFormat) -> Vec<u8> {
  let mut bytes = Vec::new();
  let version = if format == ObjectFormat::Sha1 {
    VERSION_SHA1
  } else {
    bytes.extend_from_slice(EXTENSIONS);
    bytes.extend_from_slice(format.name().as_bytes());
    bytes.push(LINE_END);
    VERSION_EXTENDED
  };
  bytes.extend_from_slice(CORE_VERSION);
  bytes.extend_from_slice(version);
  bytes.push(LINE_END);
  bytes.extend_from_slice(CORE_REST);
  bytes
}

/// One ref's line of `packed-refs`: its name, its object, the object it peels to when that is an
/// annotated tag, and where the line begins.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RefLine {
  name: Vec<u8>,
  id: ObjectId,
  peeled: Option<ObjectId>,
  at: u64,
}

impl RefLine {
  /// The line's bytes, and its peeled line's.
  fn bytes(&self) -> Vec<u8> {
    let mut bytes = Vec::new();
    self.id.write_hex(&mut bytes);
    bytes.push(BETWEEN);
    bytes.extend_from_slice(&self.name);
    bytes.push(LINE_END);
    if let Some(peeled) = self.peeled {
      bytes.push(PEELED);
      peeled.write_hex(&mut bytes);
      bytes.push(LINE_END);
    }
    bytes
  }

  /// The line's length, with its peeled line's.
  fn len(&self) -> u64 {
    let hex = to_u64(self.id.format().hex_len());
    let line = hex
      .saturating_add(to_u64(self.name.len()))
      .saturating_add(FRAMING);
    let peeled = self.peeled.map_or(0, |_| hex.saturating_add(FRAMING));
    line.saturating_add(peeled)
  }
}

/// `packed-refs` as a table of lines, each made when a read reaches it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct PackedRefs {
  lines: Vec<RefLine>,
  len: u64,
}

impl PackedRefs {
  /// The refs of `refs` that have a head at the version shown, sorted by name, each with the
  /// object it peels to when that is an annotated tag; refused for a name git would not take, a
  /// name given twice, or a version the store does not hold.
  pub(super) fn of(
    repository: &Repository,
    store: &ContentStore,
    refs: &[ShownRef<'_>],
    admission: &mut Admission,
  ) -> Result<PackedRefs, ViewRefused> {
    admission.charge_items::<RefLine>(refs.len())?;
    let mut sorted: Vec<(usize, &ShownRef<'_>)> = refs.iter().enumerate().collect();
    sorted.sort_by(|left, right| left.1.name.cmp(right.1.name));
    let mut table = PackedRefs {
      lines: Vec::with_capacity(refs.len()),
      len: to_u64(PACKED_HEADER.len()),
    };
    let mut before: Option<&[u8]> = None;
    for (at, shown) in sorted {
      let at = to_u64(at);
      if !shown.name.starts_with(REFS) || !refname::is_valid(shown.name) {
        return Err(ViewRefused::RefName { at });
      }
      if before == Some(shown.name) {
        return Err(ViewRefused::Duplicate { at });
      }
      before = Some(shown.name);
      table.push(repository, store, shown, admission)?;
    }
    Ok(table)
  }

  /// Adds `shown`'s line when its green has a head at the version shown.
  fn push(
    &mut self,
    repository: &Repository,
    store: &ContentStore,
    shown: &ShownRef<'_>,
    admission: &mut Admission,
  ) -> Result<(), ViewRefused> {
    if shown.version >= repository.versions(shown.green) {
      return Err(ViewRefused::NoVersion {
        green: shown.green,
        version: to_u64(shown.version),
      });
    }
    let Some(position) = repository.head_position(shown.green, shown.version) else {
      return Ok(());
    };
    let id = repository
      .id_at(store, position)
      .ok_or(ViewRefused::Store(StoreRefused::NoPosition { position }))?;
    admission.charge(to_u64(shown.name.len()))?;
    let line = RefLine {
      name: shown.name.to_vec(),
      id,
      peeled: peeled(repository, store, id)?,
      at: self.len,
    };
    self.len = self.len.saturating_add(line.len());
    self.lines.push(line);
    Ok(())
  }

  /// The file's length.
  pub(super) fn len(&self) -> u64 {
    self.len
  }

  /// Appends the file's bytes `from..to` (clipped to the file) to `out`: the lines the range
  /// reaches, each made as it is reached.
  pub(super) fn fill(&self, (from, to): (u64, u64), out: &mut Vec<u8>) {
    let end = to.min(self.len);
    if from >= end {
      return;
    }
    copy(PACKED_HEADER, (from, end), out);
    let first = self
      .lines
      .partition_point(|line| line.at.saturating_add(line.len()) <= from);
    for line in self.lines.iter().skip(first) {
      if line.at >= end {
        break;
      }
      let within = (from.saturating_sub(line.at), end.saturating_sub(line.at));
      copy(&line.bytes(), within, out);
    }
  }
}

/// The object `id` peels to when it is an annotated tag: each tag's object followed until one is
/// not a tag, as `peel_object` peels a ref git packs. `None` for any other object.
fn peeled(
  repository: &Repository,
  store: &ContentStore,
  id: ObjectId,
) -> Result<Option<ObjectId>, ViewRefused> {
  let mut target = id;
  // No object names itself, a tag's name hashing its target's, so a chain is at most as long as
  // the repository's objects.
  for depth in 0..=repository.next_position() {
    let (row, content) = repository
      .indexed(store, &target)
      .ok_or(ViewRefused::Store(StoreRefused::Missing { id: target }))?;
    if row.kind != Kind::Tag {
      return Ok((depth > 0).then_some(target));
    }
    let bytes = store
      .object_bytes(&content, row.len)
      .ok_or(ViewRefused::Store(StoreRefused::Unreadable { id: target }))?;
    let mut room = Admission::new(store.room());
    target = Tag::parse(&bytes, &mut room)
      .ok()
      .and_then(|tag| tag.object(repository.format()))
      .ok_or(ViewRefused::Tag { id: target })?;
  }
  Err(ViewRefused::Tag { id })
}
