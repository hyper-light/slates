//! IPC metadata in §4.16 / A-26. FIFO and socket names carry an explicit kind and captured
//! ownership/timestamps, never stream bytes or a kernel endpoint. The fixed little-endian
//! encoding is shared by origins and declared creations; replay reads no clock or host state.
//! Modes use the engine's existing independent mode dimension. Link counts follow namespace
//! edges and size is always zero, so neither is an independently mutable field here.

use crate::ops_doc::{DocDecodeError, Reader};

/// The two IPC namespace kinds authorized by A-26. Device nodes have no representation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpecialKind {
  /// A kernel-local named pipe.
  Fifo,
  /// A kernel-local socket name, without a listener or connection.
  Socket,
}

/// Captured IPC metadata. These values are part of the immutable version's identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpecialNode {
  /// The namespace kind.
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

/// Format: one kind byte, two owner words, and four signed nanosecond timestamps.
pub const ENCODED_BYTES: usize = size_of::<u8>() + 2 * size_of::<u32>() + 4 * size_of::<i64>();

impl SpecialNode {
  /// The canonical metadata bytes, without padding or an endpoint payload.
  pub fn encode(self) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(ENCODED_BYTES);
    bytes.push(match self.kind {
      SpecialKind::Fifo => 0,
      SpecialKind::Socket => 1,
    });
    bytes.extend_from_slice(&self.uid.to_le_bytes());
    bytes.extend_from_slice(&self.gid.to_le_bytes());
    for time in [self.atime, self.mtime, self.ctime, self.btime] {
      bytes.extend_from_slice(&time.to_le_bytes());
    }
    bytes
  }

  /// Decode exactly one node, refusing unknown kinds, truncation and trailing bytes.
  pub fn decode(bytes: &[u8]) -> Result<Self, DocDecodeError> {
    let mut reader = Reader::new(bytes);
    let kind = match reader.u8()? {
      0 => SpecialKind::Fifo,
      1 => SpecialKind::Socket,
      _ => return Err(DocDecodeError::BadKind),
    };
    let uid = reader.u32()?;
    let gid = reader.u32()?;
    let mut time = || -> Result<i64, DocDecodeError> {
      let mut word = [0; size_of::<i64>()];
      word.copy_from_slice(reader.bytes(size_of::<i64>())?);
      Ok(i64::from_le_bytes(word))
    };
    let node = Self {
      kind,
      uid,
      gid,
      atime: time()?,
      mtime: time()?,
      ctime: time()?,
      btime: time()?,
    };
    if !reader.is_empty() {
      return Err(DocDecodeError::TrailingBytes);
    }
    Ok(node)
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
      let (kind, len, src) = if node.is_some() {
        let start = offset;
        offset = offset.saturating_add(ENCODED_BYTES as u64);
        (OpKind::Mknod, ENCODED_BYTES as u64, start)
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
