//! Names with no content in §4.16: FIFO and socket names (A-26) and device nodes (A-294). Each carries an explicit
//! kind and captured ownership/timestamps, never stream bytes or a kernel endpoint; a device node carries its major
//! and minor numbers too, metadata nothing in the merge opens. The fixed little-endian encoding is shared by origins,
//! checkpoints and declared creations; replay reads no clock or host state. Modes use the engine's existing
//! independent mode dimension. Link counts follow namespace edges and size is always zero, so neither is an
//! independently mutable field here.
//!
//! A device node's encoding is a FIFO or socket name's with its two numbers after it, under kind tags of its own, so
//! every FIFO and socket encoding, and every identity over one, is what it was before device nodes were kept, and no
//! origin, checkpoint or chain format version moved: a reader of those formats meets a device kind only in bytes
//! that hold one.

use crate::ops_doc::{DocDecodeError, Reader};

/// The kinds of name with no content: the two IPC kinds A-26 authorized, and the device nodes A-294 keeps as
/// metadata, each with its number.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialKind {
  /// A kernel-local named pipe.
  Fifo,
  /// A kernel-local socket name, without a listener or connection.
  Socket,
  /// A block device node and its number (A-294).
  BlockDevice {
    /// The major number.
    major: u32,
    /// The minor number.
    minor: u32,
  },
  /// A character device node and its number (A-294); a whiteout is one numbered 0/0.
  CharDevice {
    /// The major number.
    major: u32,
    /// The minor number.
    minor: u32,
  },
}

/// Captured metadata of a name with no content. These values are part of the immutable version's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpecialNode {
  /// The namespace kind, with a device node's number.
  pub kind: SpecialKind,
  /// The owner id.
  pub uid: u32,
  /// The group id.
  pub gid: u32,
  /// Last access, in nanoseconds in the volume's clock domain.
  pub atime: i64,
  /// Last modification in the same clock domain.
  pub mtime: i64,
  /// Last metadata change in the same clock domain.
  pub ctime: i64,
  /// Creation time in the same clock domain.
  pub btime: i64,
}

/// Format: a FIFO or socket name's encoding: one kind byte, two owner words, and four signed nanosecond timestamps.
pub const ENCODED_BYTES: usize = size_of::<u8>() + 2 * size_of::<u32>() + 4 * size_of::<i64>();
/// Format: a device node's encoding: a FIFO or socket name's, then its major and minor words (A-294).
pub const DEVICE_ENCODED_BYTES: usize = ENCODED_BYTES + 2 * size_of::<u32>();

/// Format: the kind byte of a FIFO name.
const KIND_FIFO: u8 = 0;
/// Format: the kind byte of a socket name.
const KIND_SOCKET: u8 = 1;
/// Format: the kind byte of a block device node (A-294).
const KIND_BLOCK_DEVICE: u8 = 2;
/// Format: the kind byte of a character device node (A-294).
const KIND_CHAR_DEVICE: u8 = 3;

impl SpecialNode {
  /// The length of [`SpecialNode::encode`]'s bytes: [`DEVICE_ENCODED_BYTES`] for a device node, [`ENCODED_BYTES`]
  /// for a FIFO or socket name.
  pub fn encoded_len(self) -> usize {
    match self.kind {
      SpecialKind::Fifo | SpecialKind::Socket => ENCODED_BYTES,
      SpecialKind::BlockDevice { .. } | SpecialKind::CharDevice { .. } => DEVICE_ENCODED_BYTES,
    }
  }

  /// The canonical metadata bytes, without padding or an endpoint payload.
  pub fn encode(self) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(self.encoded_len());
    let (kind, device) = match self.kind {
      SpecialKind::Fifo => (KIND_FIFO, None),
      SpecialKind::Socket => (KIND_SOCKET, None),
      SpecialKind::BlockDevice { major, minor } => (KIND_BLOCK_DEVICE, Some((major, minor))),
      SpecialKind::CharDevice { major, minor } => (KIND_CHAR_DEVICE, Some((major, minor))),
    };
    bytes.push(kind);
    bytes.extend_from_slice(&self.uid.to_le_bytes());
    bytes.extend_from_slice(&self.gid.to_le_bytes());
    for time in [self.atime, self.mtime, self.ctime, self.btime] {
      bytes.extend_from_slice(&time.to_le_bytes());
    }
    if let Some((major, minor)) = device {
      bytes.extend_from_slice(&major.to_le_bytes());
      bytes.extend_from_slice(&minor.to_le_bytes());
    }
    bytes
  }

  /// Decode exactly one node, refusing unknown kinds, truncation and trailing bytes.
  pub fn decode(bytes: &[u8]) -> Result<Self, DocDecodeError> {
    let mut reader = Reader::new(bytes);
    let node = Self::read(&mut reader)?;
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(node)
  }

  /// Reads one node from the front of `reader`, its length decided by its kind byte, refusing an unknown kind and
  /// truncation: what a table of nodes (an origin's, a checkpoint's) reads each entry with.
  pub(crate) fn read(reader: &mut Reader<'_>) -> Result<Self, DocDecodeError> {
    let tag = reader.u8()?;
    if !matches!(
      tag,
      KIND_FIFO | KIND_SOCKET | KIND_BLOCK_DEVICE | KIND_CHAR_DEVICE
    ) {
      return Err(DocDecodeError::BadKind);
    }
    let uid = reader.u32()?;
    let gid = reader.u32()?;
    let mut time = || -> Result<i64, DocDecodeError> {
      let mut word = [0; size_of::<i64>()];
      word.copy_from_slice(reader.bytes(size_of::<i64>())?);
      Ok(i64::from_le_bytes(word))
    };
    let (atime, mtime, ctime, btime) = (time()?, time()?, time()?, time()?);
    let kind = match tag {
      KIND_FIFO => SpecialKind::Fifo,
      KIND_SOCKET => SpecialKind::Socket,
      KIND_BLOCK_DEVICE => SpecialKind::BlockDevice {
        major: reader.u32()?,
        minor: reader.u32()?,
      },
      _ => SpecialKind::CharDevice {
        major: reader.u32()?,
        minor: reader.u32()?,
      },
    };
    Ok(Self {
      kind,
      uid,
      gid,
      atime,
      mtime,
      ctime,
      btime,
    })
  }
}

/// The IPC portion of a composed journal. Its size is bounded by the base and declared names.
pub(crate) struct Composition {
  pub(crate) paths: std::collections::BTreeSet<String>,
  pub(crate) survivors: std::collections::BTreeSet<String>,
  changes: std::collections::BTreeMap<String, Option<SpecialNode>>,
}

impl Composition {
  /// Compose creations and removals. Cross-kind transitions and rename composition have the
  /// same explicit refusal as the existing symlink/hard-link deriver; never invent a file edit.
  pub(crate) fn of(
    base: &crate::increment::Base,
    journal: &[crate::increment::VolumeOp],
  ) -> Result<Self, crate::increment::DeriveError> {
    use crate::increment::{DeriveError, VolumeOp};
    use std::collections::{BTreeMap, BTreeSet};
    let initial: BTreeMap<_, _> = base.specials.iter().cloned().collect();
    let mut present = initial.clone();
    let mut paths: BTreeSet<_> = initial.keys().cloned().collect();
    paths.extend(journal.iter().filter_map(|op| match op {
      VolumeOp::Mknod { path, .. } => Some(path.clone()),
      _ => None,
    }));
    for op in journal {
      match op {
        VolumeOp::Mknod { path, node, .. } => {
          if present.insert(path.clone(), *node).is_some() {
            return Err(DeriveError::CreateOverExisting(path.clone()));
          }
        }
        VolumeOp::Unlink { path } if paths.contains(path) => {
          // The current path-based link model names its inode through one primary path. Removing that path
          // while aliases survive needs an inode-aware namespace journal (GAPS §8f).
          if base.hardlinks.iter().any(|(_, target)| target == path) {
            return Err(DeriveError::Unsupported(path.clone()));
          }
          if present.remove(path).is_none() {
            return Err(DeriveError::UnlinkMissing(path.clone()));
          }
        }
        _ => {}
      }
    }
    let survivors = present.keys().cloned().collect();
    let changes = paths
      .iter()
      .filter_map(|path| {
        let value = present.get(path).copied();
        (value != initial.get(path).copied()).then(|| (path.clone(), value))
      })
      .collect();
    Ok(Self {
      paths,
      survivors,
      changes,
    })
  }

  /// Append metadata after file/xattr bytes. It is an explicitly typed payload, not a content
  /// operation. Canonicalization also remaps the existing link and xattr path references.
  pub(crate) fn emit(self, doc: &mut crate::ops_doc::OpsDoc) {
    use crate::ops_doc::{Op, OpKind};
    let mut offset = doc
      .ops
      .iter()
      .filter(|op| {
        matches!(
          op.kind,
          OpKind::Overwrite | OpKind::Insert | OpKind::Extend | OpKind::SetXattr
        )
      })
      .map(|op| op.src.saturating_add(op.len))
      .max()
      .unwrap_or(0);
    for (path, node) in self.changes {
      let path = doc.paths.intern(&path);
      let (kind, len, src) = if let Some(node) = node {
        let start = offset;
        let len = u64::try_from(node.encoded_len()).unwrap_or(u64::MAX);
        offset = offset.saturating_add(len);
        (OpKind::Mknod, len, start)
      } else {
        (OpKind::Unlink, 0, u64::MAX)
      };
      doc.ops.push(Op {
        kind,
        flags: 0,
        path,
        at: 0,
        len,
        src,
      });
    }
    doc.canonicalize();
  }
}
