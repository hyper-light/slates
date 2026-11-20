import sys
p=sys.argv[1]
s=open(p).read()
def rep(old,new):
    global s
    assert s.count(old)==1, old[:80]
    s=s.replace(old,new)
rep('''use std::collections::BTreeMap;

use crate::engine::Increment;''','''use std::collections::{BTreeMap, BTreeSet};

use crate::engine::Increment;''')
rep('''    /// Whether the copy replaces what the destination holds ([`crate::ops_doc::REPLACE`]).
    replace: bool,
  },
}''','''    /// Whether the copy replaces what the destination holds ([`crate::ops_doc::REPLACE`]).
    replace: bool,
    /// What the copy changes in the head's tree it grafts, when its source changed after `version` (A-626 part 2):
    /// recorded whole, so a replay reads no history.
    fix: Option<Box<CopyFix>>,
  },
}

/// What a copy from a version its source changed after does to the head's tree it grafts, so the copy brings the
/// source's tree as it stood at that version (A-626 part 2: FSFS copies from any committed revision). Recorded in the
/// entry whole, so a replay reads no history, whatever its holder folded or restored. Paths are relative to the source,
/// which is what they are relative to at the destination too.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CopyFix {
  /// A copy made after the version at or above the source replaced its tree: nothing of the head's is grafted.
  pub start_empty: bool,
  /// The trees of copies made inside the source after the version, cut from what is grafted.
  pub cuts: BTreeSet<String>,
  /// Each file's bytes at the version, or none, where they differ from what is grafted.
  pub files: BTreeMap<String, Option<Vec<u8>>>,
  /// Each IPC node at the version, or none, where it differs from what is grafted.
  pub specials: BTreeMap<String, Option<SpecialNode>>,
  /// Whether each directory stood at the version, where that differs from what is grafted.
  pub dirs: BTreeMap<String, bool>,
  /// Each mode at the version, or none, where it differs from what is grafted.
  pub modes: BTreeMap<String, Option<u32>>,
  /// Each symbolic link's target at the version, or none, where it differs from what is grafted.
  pub symlinks: BTreeMap<String, Option<String>>,
  /// Each attribute's value at the version, or none, where it differs from what is grafted: by `(path, name)`.
  pub xattrs: BTreeMap<(String, String), Option<Vec<u8>>>,
}

/// Format: the least bytes one record of a [`CopyFix`] table holds, a path's length word and a presence byte: what a
/// decoder checks a table's count against before it allocates.
const FIX_RECORD_MIN_BYTES: usize = size_of::<u32>() + size_of::<u8>();

/// Format: the bytes a length or count word takes.
const WORD_BYTES: usize = size_of::<u32>();

/// The bytes [`put_fix`] writes for `fix`, without writing them: what a fixed copy's record adds to its entry, which the
/// service secures before the verdict (`Green::copy_entry_room`).
pub fn fix_bytes(fix: &CopyFix) -> usize {
  let string = |text: &str| WORD_BYTES.saturating_add(text.len());
  let maybe = |len: Option<usize>| len.map_or(1, |len| len.saturating_add(1));
  let sum = |sizes: &mut dyn Iterator<Item = usize>| sizes.fold(0usize, usize::saturating_add);
  let tables = [
    sum(&mut fix.cuts.iter().map(|cut| string(cut))),
    sum(&mut fix.files.iter().map(|(path, bytes)| {
      string(path).saturating_add(maybe(bytes.as_ref().map(|bytes| string_len(bytes))))
    })),
    sum(&mut fix.specials.iter().map(|(path, node)| {
      string(path).saturating_add(maybe(node.map(|node| string_len(&node.encode()))))
    })),
    sum(&mut fix.dirs.keys().map(|path| string(path).saturating_add(1))),
    sum(&mut fix.modes.iter().map(|(path, mode)| {
      string(path).saturating_add(maybe(mode.map(|_| WORD_BYTES)))
    })),
    sum(&mut fix.symlinks.iter().map(|(path, target)| {
      string(path).saturating_add(maybe(target.as_deref().map(string)))
    })),
    sum(&mut fix.xattrs.iter().map(|((path, name), value)| {
      string(path)
        .saturating_add(string(name))
        .saturating_add(maybe(value.as_ref().map(|value| string_len(value))))
    })),
  ];
  let counts = WORD_BYTES.saturating_mul(tables.len());
  sum(&mut tables.into_iter()).saturating_add(counts).saturating_add(1)
}

/// The bytes a length word and `bytes` take.
fn string_len(bytes: &[u8]) -> usize {
  WORD_BYTES.saturating_add(bytes.len())
}

/// Writes `value`'s presence byte, then the value through `put` when present.
fn put_maybe<T: ?Sized>(out: &mut Vec<u8>, value: Option<&T>, put: impl Fn(&mut Vec<u8>, &T)) {
  out.push(u8::from(value.is_some()));
  if let Some(value) = value {
    put(out, value);
  }
}

/// Writes a [`CopyFix`]: whether it starts empty, then its tables in key order, each a count and its records.
fn put_fix(out: &mut Vec<u8>, fix: &CopyFix) {
  out.push(u8::from(fix.start_empty));
  put_count(out, fix.cuts.len());
  for cut in &fix.cuts {
    put_str(out, cut);
  }
  put_count(out, fix.files.len());
  for (path, bytes) in &fix.files {
    put_str(out, path);
    put_maybe(out, bytes.as_deref(), put_bytes);
  }
  put_count(out, fix.specials.len());
  for (path, node) in &fix.specials {
    put_str(out, path);
    put_maybe(out, node.as_ref(), |out, node| put_bytes(out, &node.encode()));
  }
  put_count(out, fix.dirs.len());
  for (path, stood) in &fix.dirs {
    put_str(out, path);
    out.push(u8::from(*stood));
  }
  put_fix_rest(out, fix);
}

/// [`put_fix`]'s modes, symbolic links and attributes.
fn put_fix_rest(out: &mut Vec<u8>, fix: &CopyFix) {
  put_count(out, fix.modes.len());
  for (path, mode) in &fix.modes {
    put_str(out, path);
    put_maybe(out, mode.as_ref(), |out, mode| {
      out.extend_from_slice(&mode.to_le_bytes());
    });
  }
  put_count(out, fix.symlinks.len());
  for (path, target) in &fix.symlinks {
    put_str(out, path);
    put_maybe(out, target.as_deref(), put_str);
  }
  put_count(out, fix.xattrs.len());
  for ((path, name), value) in &fix.xattrs {
    put_str(out, path);
    put_str(out, name);
    put_maybe(out, value.as_deref(), put_bytes);
  }
}

/// A presence or truth byte: 0 or 1, anything else refused.
fn take_flag(reader: &mut Reader<'_>) -> Result<bool, DocDecodeError> {
  match reader.u8()? {
    0 => Ok(false),
    1 => Ok(true),
    _ => Err(DocDecodeError::BadKind),
  }
}

/// A [`CopyFix`] table's count, refused before anything is allocated when the bytes that remain could not hold that
/// many records of `least` bytes each.
fn take_table(reader: &mut Reader<'_>, least: usize) -> Result<usize, DocDecodeError> {
  let count = reader.u32()? as usize;
  if count > reader.remaining() / least.max(1) {
    return Err(DocDecodeError::Truncated);
  }
  Ok(count)
}

/// A present value read by `take`, or none.
fn take_maybe<T>(
  reader: &mut Reader<'_>,
  take: impl Fn(&mut Reader<'_>) -> Result<T, DocDecodeError>,
) -> Result<Option<T>, DocDecodeError> {
  if take_flag(reader)? {
    take(reader).map(Some)
  } else {
    Ok(None)
  }
}

/// Inserts one record of a [`CopyFix`] table; a key written twice is refused (an encoder writes each once, in order).
fn insert_once<K: Ord, V>(table: &mut BTreeMap<K, V>, key: K, value: V) -> Result<(), DocDecodeError> {
  if table.insert(key, value).is_some() {
    return Err(DocDecodeError::DuplicatePath);
  }
  Ok(())
}

/// Reads a [`CopyFix`] as [`put_fix`] writes it.
fn take_fix(reader: &mut Reader<'_>) -> Result<CopyFix, DocDecodeError> {
  let mut fix = CopyFix {
    start_empty: take_flag(reader)?,
    ..CopyFix::default()
  };
  for _ in 0..take_table(reader, WORD_BYTES)? {
    if !fix.cuts.insert(take_str(reader)?) {
      return Err(DocDecodeError::DuplicatePath);
    }
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let bytes = take_maybe(reader, |reader| reader_bytes(reader).map(<[u8]>::to_vec))?;
    insert_once(&mut fix.files, path, bytes)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let node = take_maybe(reader, |reader| SpecialNode::decode(reader_bytes(reader)?))?;
    insert_once(&mut fix.specials, path, node)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let stood = take_flag(reader)?;
    insert_once(&mut fix.dirs, path, stood)?;
  }
  take_fix_rest(reader, &mut fix)?;
  Ok(fix)
}

/// [`take_fix`]'s modes, symbolic links and attributes.
fn take_fix_rest(reader: &mut Reader<'_>, fix: &mut CopyFix) -> Result<(), DocDecodeError> {
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let mode = take_maybe(reader, |reader| reader.u32())?;
    insert_once(&mut fix.modes, path, mode)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let path = take_str(reader)?;
    let target = take_maybe(reader, take_str)?;
    insert_once(&mut fix.symlinks, path, target)?;
  }
  for _ in 0..take_table(reader, FIX_RECORD_MIN_BYTES)? {
    let key = (take_str(reader)?, take_str(reader)?);
    let value = take_maybe(reader, |reader| reader_bytes(reader).map(<[u8]>::to_vec))?;
    insert_once(&mut fix.xattrs, key, value)?;
  }
  Ok(())
}''')
rep('''  /// Format: the tag of a tree copied by reference (A-586), its source's version the increment's `Copy` operation's.
  pub(super) const COPY_TREE: u8 = 18;''','''  /// Format: the tag of a tree copied by reference (A-586), its source's version the increment's `Copy` operation's.
  pub(super) const COPY_TREE: u8 = 18;
  /// Format: the tag of a tree copied from a version its source changed after (A-626 part 2): [`COPY_TREE`]'s record,
  /// then its `CopyFix`.
  pub(super) const COPY_TREE_FIXED: u8 = 19;''')
rep('''    Applied::CopyTree { dst, from, .. } => put_tagged_paths(out, tag::COPY_TREE, &[dst, from]),''','''    Applied::CopyTree {
      dst,
      from,
      fix: None,
      ..
    } => put_tagged_paths(out, tag::COPY_TREE, &[dst, from]),
    Applied::CopyTree {
      dst,
      from,
      fix: Some(fix),
      ..
    } => {
      put_tagged_paths(out, tag::COPY_TREE_FIXED, &[dst, from]);
      put_fix(out, fix);
    }''')
rep('''    tag::COPY_TREE => {
      let dst = take_str(reader)?;
      let from = take_str(reader)?;
      let (version, replace) = declared.copy(&dst, &from).ok_or(DocDecodeError::BadKind)?;
      Applied::CopyTree {
        dst,
        from,
        version,
        replace,
      }
    }''','''    kind @ (tag::COPY_TREE | tag::COPY_TREE_FIXED) => {
      let dst = take_str(reader)?;
      let from = take_str(reader)?;
      let (version, replace) = declared.copy(&dst, &from).ok_or(DocDecodeError::BadKind)?;
      let fix = if kind == tag::COPY_TREE_FIXED {
        Some(Box::new(take_fix(reader)?))
      } else {
        None
      };
      Applied::CopyTree {
        dst,
        from,
        version,
        replace,
        fix,
      }
    }''')
open(p,'w').write(s)
print("ok")
