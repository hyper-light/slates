//! A store's file index, version 1, read as stock Mercurial 7.2 reads one (A-630 part 3; §4.17.3 piece 3, the import;
//! `helptext/internals/file-index.txt` \[B\]; `store_utils/file_index.py` and `store_utils/file_index_util.py` \[C\]):
//! the list of every tracked path whose file log a store holds, which takes the fncache's place in a store that requires
//! `fileindex-v1` (every revlog version 2 store 7.2 makes, and a version 1 store made with `format.use-fileindex-v1`).
//!
//! **The docket** (`fileindex`, at the store's root) names three files by an 8-byte id each and says how many bytes of
//! each are the index's; a file may be longer, while another process appends to it, and the rest is not read. Its fixed
//! part is 60 bytes: the marker `fileindex-v1`, the three used sizes, the three ids, the tree's root (an offset into the
//! tree file), the tree file's bytes no node reaches, and four flag bytes 7.2 ignores. Then the garbage list: a count and
//! a buffer's size, 12-byte entries and the buffer of the old files' paths that are deleted later. The files are the
//! list file (`fileindex-list.<id>`: every path, each followed by a NUL by convention), the meta file
//! (`fileindex-meta.<id>`: an 8-byte record a token, the path's offset in the list file, its length and its directory's
//! length, token 0 the root's) and the tree file (`fileindex-tree.<id>`: a prefix tree from paths to tokens). An id of
//! eight `0`s names no file, and an absent docket is an empty index, as 7.2 reads one (a store has none before its first
//! file).
//!
//! **The paths** are the meta file's records from token 1 in order, each read from the list file, as 7.2's
//! `FileIndexView.items` reads them and its store's `data_entries` walks them. The tree serves lookups by path, which the
//! walk never makes, so only its root is read, held to 7.2's two checks when it opens an index (`FileIndexView`): token 0
//! with an empty label, and children whenever the tree file is not empty. An empty meta file reads as the root's record
//! alone and an empty tree file as a root without children (7.2's `EMPTY_META_BYTES`, `EMPTY_TREE_BYTES`).
//!
//! **Stricter than 7.2, on purpose.** 7.2 slices a garbage list, a root's children and a path up to wherever the bytes
//! end, and drops a meta file's last partial record; here a docket whose garbage list is cut short, a used size past its
//! file's end, a root whose children are cut short, a meta file that is not whole records, and a path past the list
//! file's used bytes, empty, or holding a NUL, a line feed or a carriage return (bytes the format forbids in a path) are
//! refused by kind. What is read is charged to the caller's admission before it is held.

use crate::read::{Cursor, to_u64};
use crate::{Admission, PastAdmission, Truncated};

/// Format: the docket's file, at the store's root.
pub const DOCKET: &[u8] = b"fileindex";

/// Format: the docket's marker, its first twelve bytes (`V1_FORMAT_MARKER`).
pub const MARKER: &[u8] = b"fileindex-v1";

/// Format: a file's id in the docket (`docket.UID_SIZE`).
pub const ID_BYTES: usize = 8;

/// Format: the id that names no file (`docket.UNSET_UID`, eight `0`s).
const UNSET_ID: &[u8; ID_BYTES] = b"00000000";

/// Format: the list file's name, before its id.
const LIST_FILE: &[u8] = b"fileindex-list.";

/// Format: the meta file's name, before its id.
const META_FILE: &[u8] = b"fileindex-meta.";

/// Format: the tree file's name, before its id.
const TREE_FILE: &[u8] = b"fileindex-tree.";

/// Format: a garbage list entry's bytes (`GarbageIndexEntry`, `>HIIH`: its life in transactions, its time, its path's
/// offset and length).
const GARBAGE_ENTRY: usize = 12;

/// Format: a meta record's bytes (`Metadata`, `>IHH`: the path's offset in the list file, its length, its directory's
/// length).
const META_RECORD: usize = 8;

/// Format: a tree node's header bytes (`TreeNodeHeader`, `>IBB`: its token, its label's length, its children's count).
const NODE_HEADER: usize = 6;

/// Format: what a node holds for each child beyond its header: the child's first byte, and its pointer or token
/// (`POINTER_STRUCT`, `>I`).
const CHILD_BYTES: usize = 1 + 4;

/// Format: the root's token (`ROOT_TOKEN`).
const ROOT_TOKEN: u32 = 0;

/// Format: an empty meta file as 7.2 reads one (`EMPTY_META_BYTES`): the root's record alone, every byte 0.
const EMPTY_META: [u8; META_RECORD] = [0; META_RECORD];

/// Format: an empty tree file as 7.2 reads one (`EMPTY_TREE_BYTES`): a root without children, every byte 0.
const EMPTY_TREE: [u8; NODE_HEADER] = [0; NODE_HEADER];

/// Format: the bytes no path holds (the format's tree section: "\n", "\r", and "\0" are forbidden in file paths).
const FORBIDDEN: &[u8] = b"\0\n\r";

/// A file index's docket, read in place (7.2's `Docket.parse_from`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Docket<'a> {
  /// The list file's used bytes.
  pub list_size: u32,
  /// The meta file's used bytes.
  pub meta_size: u32,
  /// The tree file's used bytes.
  pub tree_size: u32,
  /// The list file's id.
  pub list_id: &'a [u8; ID_BYTES],
  /// The meta file's id.
  pub meta_id: &'a [u8; ID_BYTES],
  /// The tree file's id.
  pub tree_id: &'a [u8; ID_BYTES],
  /// The tree's root: an offset into the tree file's used bytes.
  pub root: u32,
  /// The tree file's used bytes that no node from the root reaches.
  pub unused: u32,
  /// How many old files the garbage list names.
  pub garbage: u32,
}

/// Why a file index was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
  /// Bytes shorter than what they must hold: the docket, its garbage list, a file's used bytes, or the root.
  Truncated(Truncated),
  /// A docket not beginning with `fileindex-v1` (7.2: "file index docket has wrong marker").
  Marker,
  /// A root whose token or label is not empty (7.2: "invalid file index root node").
  Root,
  /// A tree file that is not empty with a root that has no child (7.2: "invalid file index singleton tree").
  Singleton,
  /// A meta file that is not a whole number of records.
  Records {
    /// Its used bytes.
    len: u64,
  },
  /// A token whose path is past the list file's used bytes, empty, or holds a NUL, a line feed or a carriage return.
  Path {
    /// The token.
    token: u64,
  },
  /// More than the caller's admission allows.
  PastAdmission(PastAdmission),
}

impl From<Truncated> for Refused {
  fn from(truncated: Truncated) -> Self {
    Self::Truncated(truncated)
  }
}

impl From<PastAdmission> for Refused {
  fn from(past: PastAdmission) -> Self {
    Self::PastAdmission(past)
  }
}

/// Reads a file index's docket.
pub fn read_docket(bytes: &[u8]) -> Result<Docket<'_>, Refused> {
  let mut cursor = Cursor::new(bytes);
  if cursor.take(MARKER.len(), "a file index docket's marker")? != MARKER {
    return Err(Refused::Marker);
  }
  let sizes = "a file index docket's sizes";
  let (list_size, meta_size, tree_size) =
    (cursor.u32(sizes)?, cursor.u32(sizes)?, cursor.u32(sizes)?);
  let (list_id, meta_id, tree_id) = (id(&mut cursor)?, id(&mut cursor)?, id(&mut cursor)?);
  let tree = "a file index docket's tree fields";
  let (root, unused) = (cursor.u32(tree)?, cursor.u32(tree)?);
  cursor.take(core::mem::size_of::<u32>(), "a file index docket's flags")?;
  let garbage = cursor.u32("a garbage list's count")?;
  let paths = cursor.u32("a garbage list's buffer size")?;
  let entries = usize::try_from(garbage)
    .ok()
    .and_then(|count| count.checked_mul(GARBAGE_ENTRY))
    .unwrap_or(usize::MAX);
  cursor.take(entries, "a garbage list's entries")?;
  cursor.take(
    usize::try_from(paths).unwrap_or(usize::MAX),
    "a garbage list's paths",
  )?;
  Ok(Docket {
    list_size,
    meta_size,
    tree_size,
    list_id,
    meta_id,
    tree_id,
    root,
    unused,
    garbage,
  })
}

/// A docket's file id.
fn id<'a>(cursor: &mut Cursor<'a>) -> Result<&'a [u8; ID_BYTES], Truncated> {
  let what = "a file index docket's ids";
  let truncated = Truncated {
    what,
    at: to_u64(cursor.at()),
    needed: to_u64(ID_BYTES),
    left: to_u64(cursor.left()),
  };
  <&[u8; ID_BYTES]>::try_from(cursor.take(ID_BYTES, what)?).map_err(|_| truncated)
}

impl Docket<'_> {
  /// The list file's name in the store, or `None` when the docket names none.
  pub fn list_file(&self) -> Option<Vec<u8>> {
    file_name(LIST_FILE, self.list_id)
  }

  /// The meta file's name in the store, or `None` when the docket names none.
  pub fn meta_file(&self) -> Option<Vec<u8>> {
    file_name(META_FILE, self.meta_id)
  }

  /// The tree file's name in the store, or `None` when the docket names none.
  pub fn tree_file(&self) -> Option<Vec<u8>> {
    file_name(TREE_FILE, self.tree_id)
  }
}

/// A file's name from its prefix and id, or `None` for the id that names none.
fn file_name(prefix: &[u8], id: &[u8; ID_BYTES]) -> Option<Vec<u8>> {
  (id != UNSET_ID).then(|| [prefix, id.as_slice()].concat())
}

/// The paths a file index lists, in its tokens' order from token 1, read from the used bytes of its list, meta and tree
/// files (each empty when the docket names none). Their bytes and entries are charged to `admission` first.
pub fn paths(
  docket: &Docket<'_>,
  list: &[u8],
  meta: &[u8],
  tree: &[u8],
  admission: &mut Admission,
) -> Result<Vec<Vec<u8>>, Refused> {
  let list = used(list, docket.list_size, "a file index's list file")?;
  let meta = used(meta, docket.meta_size, "a file index's meta file")?;
  let tree = used(tree, docket.tree_size, "a file index's tree file")?;
  check_root(
    if tree.is_empty() { &EMPTY_TREE } else { tree },
    docket.root,
    !tree.is_empty(),
  )?;
  let (records, rest) =
    (if meta.is_empty() { &EMPTY_META } else { meta }).as_chunks::<META_RECORD>();
  if !rest.is_empty() {
    return Err(Refused::Records {
      len: to_u64(meta.len()),
    });
  }
  let tokens = records.get(1..).unwrap_or_default();
  let mut bytes: u64 = 0;
  for (token, record) in (1_u64..).zip(tokens) {
    bytes = bytes.saturating_add(to_u64(path_of(list, record, token)?.len()));
  }
  let mut kept = *admission;
  kept.charge(bytes)?;
  kept.charge_items::<Vec<u8>>(tokens.len())?;
  let read = (1_u64..)
    .zip(tokens)
    .map(|(token, record)| path_of(list, record, token).map(<[u8]>::to_vec))
    .collect::<Result<Vec<Vec<u8>>, Refused>>()?;
  *admission = kept;
  Ok(read)
}

/// A file's used bytes: its first `size`, which it must hold.
fn used<'a>(bytes: &'a [u8], size: u32, what: &'static str) -> Result<&'a [u8], Refused> {
  usize::try_from(size)
    .ok()
    .and_then(|size| bytes.get(..size))
    .ok_or_else(|| {
      Refused::Truncated(Truncated {
        what,
        at: 0,
        needed: u64::from(size),
        left: to_u64(bytes.len()),
      })
    })
}

/// Holds the root at `root` to 7.2's checks: token 0 and an empty label, its children whole, and at least one when the
/// tree file is not empty.
fn check_root(tree: &[u8], root: u32, written: bool) -> Result<(), Refused> {
  let at = usize::try_from(root).unwrap_or(usize::MAX);
  let mut cursor = Cursor::new(tree.get(at..).unwrap_or_default());
  let what = "a file index's root";
  let (token, label, children) = (cursor.u32(what)?, cursor.u8(what)?, cursor.u8(what)?);
  if token != ROOT_TOKEN || label != 0 {
    return Err(Refused::Root);
  }
  cursor.take(
    usize::from(children).saturating_mul(CHILD_BYTES),
    "a file index root's children",
  )?;
  if written && children == 0 {
    return Err(Refused::Singleton);
  }
  Ok(())
}

/// The path a meta record names: in the list file's used bytes, not empty, and holding no forbidden byte.
fn path_of<'a>(
  list: &'a [u8],
  record: &[u8; META_RECORD],
  token: u64,
) -> Result<&'a [u8], Refused> {
  let [o0, o1, o2, o3, l0, l1, _, _] = *record;
  let start = usize::try_from(u32::from_be_bytes([o0, o1, o2, o3])).unwrap_or(usize::MAX);
  start
    .checked_add(usize::from(u16::from_be_bytes([l0, l1])))
    .and_then(|end| list.get(start..end))
    .filter(|path| !path.is_empty() && !path.iter().any(|byte| FORBIDDEN.contains(byte)))
    .ok_or(Refused::Path { token })
}

#[cfg(test)]
mod tests {
  use super::{Docket, ID_BYTES, MARKER, Refused, paths, read_docket};
  use crate::Admission;

  /// A docket of the given sizes and root, its three ids `ids`, and `garbage` garbage entries over a buffer of `buffer`
  /// bytes, as 7.2's `Docket.serialize` writes one.
  fn docket(sizes: [u32; 3], ids: [&[u8; ID_BYTES]; 3], root: u32, garbage: (u32, u32)) -> Vec<u8> {
    let mut bytes = MARKER.to_vec();
    for size in sizes {
      bytes.extend_from_slice(&size.to_be_bytes());
    }
    for id in ids {
      bytes.extend_from_slice(id);
    }
    for field in [root, 0, 0, garbage.0, garbage.1] {
      bytes.extend_from_slice(&field.to_be_bytes());
    }
    let entries = usize::try_from(garbage.0).unwrap() * 12;
    bytes.resize(
      bytes.len() + entries + usize::try_from(garbage.1).unwrap(),
      0,
    );
    bytes
  }

  /// A meta record: the path's offset and length.
  fn record(offset: u32, length: u16) -> Vec<u8> {
    [&offset.to_be_bytes()[..], &length.to_be_bytes(), &[0, 0]].concat()
  }

  /// A tree file of one root whose single child is a leaf of token 1.
  fn tree() -> Vec<u8> {
    [
      &[0, 0, 0, 0, 0, 1][..],
      b"a",
      &(0x8000_0001_u32).to_be_bytes(),
    ]
    .concat()
  }

  /// The paths of a one-path index whose list file is `list` and whose record is `record`.
  fn read(list: &[u8], record: &[u8]) -> Result<Vec<Vec<u8>>, Refused> {
    let meta = [&[0; 8][..], record].concat();
    let tree = tree();
    let size = |bytes: &[u8]| u32::try_from(bytes.len()).unwrap_or(u32::MAX);
    let bytes = docket(
      [size(list), size(&meta), size(&tree)],
      [b"0000000a", b"0000000b", b"0000000c"],
      0,
      (0, 0),
    );
    let docket = read_docket(&bytes)?;
    paths(&docket, list, &meta, &tree, &mut Admission::new(1 << 20))
  }

  /// A-630 part 3 (7.2's `MutableTree` doctest's layout): do read a docket with two garbage entries and an index of one
  /// path. Expect the docket's fields, its files' names (none for an unset id), and the path.
  #[test]
  fn a_docket_and_its_files_are_read_as_7_2_writes_them() {
    let bytes = docket(
      [4, 16, 11],
      [b"1a2b3c4d", b"00000000", b"5e6f7a8b"],
      0,
      (2, 9),
    );
    let docket = read_docket(&bytes).unwrap();
    assert_eq!(
      (
        docket.list_size,
        docket.meta_size,
        docket.tree_size,
        docket.garbage
      ),
      (4, 16, 11, 2)
    );
    assert_eq!(docket.list_file().unwrap(), b"fileindex-list.1a2b3c4d");
    assert_eq!(docket.meta_file(), None);
    assert_eq!(docket.tree_file().unwrap(), b"fileindex-tree.5e6f7a8b");
    assert_eq!(read(b"abc\0", &record(0, 3)).unwrap(), [b"abc".to_vec()]);
  }

  /// AC-HG.9, A-630 part 3: do read a docket of the wrong marker, one cut in its fixed part, and one whose garbage list
  /// is cut short. Expect each refused by kind.
  #[test]
  fn damaged_dockets_are_refused() {
    let mut wrong = docket([0; 3], [b"00000000"; 3], 0, (0, 0));
    wrong[0] = b'F';
    assert_eq!(read_docket(&wrong), Err(Refused::Marker));
    let whole = docket([0; 3], [b"00000000"; 3], 0, (1, 4));
    for cut in [10, 59, whole.len() - 1] {
      assert!(
        matches!(read_docket(&whole[..cut]), Err(Refused::Truncated(_))),
        "cut at {cut}"
      );
    }
  }

  /// AC-HG.9, A-630 part 3: do read indexes whose path is past the list file, empty, or holds a NUL, a line feed or a
  /// carriage return. Expect each refused naming its token.
  #[test]
  fn forbidden_paths_are_refused() {
    for (list, record) in [
      (&b"ab\0"[..], record(1, 4)),
      (b"ab\0", record(0, 0)),
      (b"a\0b\0", record(0, 3)),
      (b"a\nb\0", record(0, 3)),
      (b"a\rb\0", record(0, 3)),
    ] {
      assert_eq!(read(list, &record), Err(Refused::Path { token: 1 }));
    }
  }

  /// AC-HG.9, A-630 part 3: do read an index whose sizes pass its files, whose meta file is not whole records, whose
  /// root is not token 0, and whose written tree's root has no child. Expect each refused by kind.
  #[test]
  fn damaged_files_are_refused() {
    let bytes = docket(
      [4, 16, 11],
      [b"0000000a", b"0000000b", b"0000000c"],
      0,
      (0, 0),
    );
    let docket = read_docket(&bytes).unwrap();
    let meta = [&[0; 8][..], &record(0, 3)].concat();
    let room = || Admission::new(1 << 20);
    let check = |list: &[u8], meta: &[u8], tree: &[u8], docket: &Docket<'_>| {
      paths(docket, list, meta, tree, &mut room())
    };
    assert!(matches!(
      check(b"abc", &meta, &tree(), &docket),
      Err(Refused::Truncated(_))
    ));
    let odd = Docket {
      meta_size: 15,
      ..docket
    };
    assert_eq!(
      check(b"abc\0", &meta, &tree(), &odd),
      Err(Refused::Records { len: 15 })
    );
    let mut rooted = tree();
    rooted[3] = 1;
    assert_eq!(check(b"abc\0", &meta, &rooted, &docket), Err(Refused::Root));
    let lone = [0_u8; 11];
    assert_eq!(
      check(b"abc\0", &meta, &lone, &docket),
      Err(Refused::Singleton)
    );
  }

  /// A-630 part 3: do read an index with no files at all, and one too large for its admission. Expect no path, and the
  /// second refused with the admission untouched.
  #[test]
  fn an_empty_index_lists_nothing_and_the_admission_bounds_the_rest() {
    let bytes = docket([0; 3], [b"00000000"; 3], 0, (0, 0));
    let docket = read_docket(&bytes).unwrap();
    assert_eq!(
      paths(&docket, &[], &[], &[], &mut Admission::new(0)),
      Ok(Vec::new())
    );
    let full = docket_bytes_for_one_path();
    let docket = read_docket(&full).unwrap();
    let meta = [&[0; 8][..], &record(0, 3)].concat();
    let mut small = Admission::new(2);
    assert!(matches!(
      paths(&docket, b"abc\0", &meta, &tree(), &mut small),
      Err(Refused::PastAdmission(_))
    ));
    assert_eq!(small.left(), 2);
  }

  /// A docket for an index of one three-byte path.
  fn docket_bytes_for_one_path() -> Vec<u8> {
    docket(
      [4, 16, 11],
      [b"0000000a", b"0000000b", b"0000000c"],
      0,
      (0, 0),
    )
  }
}
