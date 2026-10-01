//! Exporting a snapshot as an archive (§4.10 "Content replication"; §4.11 "Archive"; D-17). A
//! frozen snapshot — its directory tree, each file's bytes and each entry's metadata — is walked
//! into the content-addressed [`Archive`]: a canonical Merkle-hashed manifest whose root identity
//! fingerprints the whole tree, and the distinct chunks the files' bytes are cut into, each named
//! by the BLAKE3 of its bytes. That archive is what the fleet replicates to a snapshot's candidate
//! holders (the design's "the same container is the replication transfer unit and the
//! clone-from-archive source") and what a successor restores after a takeover.
//!
//! The walk is **resumable in bounded slices** (§4.3, "every loop over user-scaled data is chunked
//! into cooperative slices under the shard's per-iteration budget"): [`SnapshotArchiver::advance`]
//! does about `byte_budget` bytes of work — bytes hashed, with a directory entry charged one chunk
//! as the upper bound on its cost — then returns [`Progress::More`] with its cursor kept, so a large
//! volume is archived across many shard steps without starving the clients the shard serves. The
//! caller derives the budget from the machine's measured hash throughput and the shard's step
//! budget (R3); a budget below one unit of work still makes progress, so the walk always terminates.
//!
//! Chunking is fixed-size at the volume's chunk size (the copy-on-write unit, §4.5), so the same
//! bytes at the same offsets cut into the same chunks and deduplicate across snapshots by identity —
//! the design's rule for large files ("page-multiple fixed chunks", research §2.4). Only the chunk
//! windows that hold data are cut (AUD-29-57, A-55): the body's data ranges at the snapshot decide them,
//! each data range becomes an extent naming its slice of its window's chunk, and every gap is a hole
//! extent, so a sparse file costs its data and its exact sparse map reaches the archive. Each chunk is
//! stored under the **compress-or-not cost model** (D-17, §4.11; [`slates_archive::codec`]): the
//! [`CodecPolicy`] the caller derives from the boot profile's measured codec points decides raw, LZ4 or
//! a zstd level per chunk; the manifest identity is the BLAKE3 of the raw bytes and never depends on it.
//! Content-defined chunking (FastCDC) is the design's *measured, per-volume* gate ("only for the class
//! of files whose measured size exceeds a threshold and whose observed dedup gain … exceeds the
//! measured hashing cost", research §2.4): the walk records the measurements that gate needs — the
//! file-size distribution it walked and the bytes deduplication saved per byte hashed
//! ([`Walked`]) — and the chunker itself is derived from them once they have data (owed until then; a
//! fixed FastCDC regime would be a magic policy, R3). Every node's extended attributes are carried with
//! its metadata (format minor 3, AUD-29-56): each value is an attribute inode's body (§4.5), so it is cut
//! into chunks by the same sliced cutter as a file's bytes, and the node is placed only once all its
//! values are cut. A file's bytes are read **at the snapshot**
//! ([`Volume::read_in`]), never at the head. A symlink is carried as a file whose bytes are its
//! target and whose mode carries the link type bits — the manifest has directory and file nodes
//! only, and the mode's type bits ([`kind_of_mode`]) are what tell a restore the kind. A base-backed
//! entry (an overlay over a host directory, §4.15) is refused [`VfsError::RecoveryIncomplete`]: its
//! bytes are on disk, not in RAM, and an archive that silently covered them with zeros would place a
//! lie (§4.4: a snapshot over a live base has coverage `DeltaWithLiveBase`, and a placement never
//! silently upgrades it).
//!
//! Deterministic: the same snapshot walks to the same archive bytes and the same manifest identity
//! (the manifest sorts entries; the chunk list follows the walk, which follows the directory order
//! the snapshot froze), so two hosts archive one snapshot identically — the property the fleet's
//! holder-side verification and the design's "holders recompute the identity" rest on. Proven by
//! use in the tests below: an export restores byte-identical, twice to the same identity.

use std::collections::{BTreeSet, VecDeque};

use slates_archive::format::Chunk as ArchiveChunk;
use slates_archive::{Archive, CodecPolicy, Entry, Extent, Node, NodeMeta, Xattr};
use slates_mem::Handle;

use crate::dir::{Child, DirNode};
use crate::error::VfsError;
use crate::ids::{InodeNo, SnapshotId};
use crate::inode::{Attrs, Body, Kind};
use crate::names::NameEquivalence;
use crate::volume::{Store, Volume, snapshot_handle};

/// Format: POSIX `S_IFMT`, the file-type mask of a mode.
pub const MODE_TYPE_MASK: u32 = 0o170_000;
/// Format: POSIX `S_IFDIR`, the type bits a directory's manifest mode carries.
pub const MODE_DIRECTORY: u32 = 0o040_000;
/// Format: POSIX `S_IFREG`, the type bits a regular file's manifest mode carries.
pub const MODE_FILE: u32 = 0o100_000;
/// Format: POSIX `S_IFLNK`, the type bits a symlink's manifest mode carries (its bytes are the
/// target).
pub const MODE_SYMLINK: u32 = 0o120_000;
/// Format: POSIX S_IFIFO, a metadata-only pipe name.
pub const MODE_FIFO: u32 = 0o010_000;
/// Format: POSIX S_IFSOCK, a metadata-only socket name.
pub const MODE_SOCKET: u32 = 0o140_000;

/// Format: the archive header's name-policy id for byte-exact names.
const POLICY_EXACT: u32 = 0;
/// Format: the archive header's name-policy id for normalization- and case-insensitive names.
const POLICY_FOLD: u32 = 1;
/// Format: the archive header's Unicode version field when the exporter carries none — the volume
/// core's names module fixes its own table and does not yet surface a version number (owed).
const UNICODE_VERSION_UNSPECIFIED: u32 = 0;

/// The kind a manifest mode's type bits name, or `None` for bits the exporter never writes.
pub fn kind_of_mode(mode: u32) -> Option<Kind> {
  match mode & MODE_TYPE_MASK {
    MODE_DIRECTORY => Some(Kind::Dir),
    MODE_FILE => Some(Kind::File),
    MODE_SYMLINK => Some(Kind::Symlink),
    MODE_FIFO => Some(Kind::Fifo),
    MODE_SOCKET => Some(Kind::Socket),
    _ => None,
  }
}

/// The permission bits of a manifest mode, without its type bits.
pub fn permissions_of_mode(mode: u32) -> u32 {
  mode & !MODE_TYPE_MASK
}

/// How far one [`SnapshotArchiver::advance`] got.
#[derive(Debug)]
pub enum Progress {
  /// The slice's budget is spent; more of the snapshot remains, and the cursor is kept.
  More,
  /// The whole snapshot is archived.
  Done(Archive),
}

/// One directory being walked: its node, the entry that names it in its parent, the entries still to
/// visit (fixed when the frame was entered, in the snapshot's directory order), and those built.
struct Frame {
  dir: Handle<DirNode>,
  name: String,
  meta: NodeMeta,
  pending: VecDeque<Pending>,
  built: Vec<Entry>,
}

/// A directory entry still to visit.
struct Pending {
  name: String,
  kind: Kind,
  inode: InodeNo,
}

/// A body — a file's bytes or an extended attribute's value — part-way through being cut into chunks
/// across slices, read at the snapshot. Only its **data** is walked (AUD-29-57): the data ranges still to
/// cut, and the offset the extents tile so far; every gap between data is a hole extent, so a sparse body
/// costs its data, not its logical size, and its exact sparse map reaches the archive.
struct BodyCursor {
  inode: InodeNo,
  size: u64,
  data: VecDeque<Range>,
  tiled: u64,
  extents: Vec<Extent>,
}

/// The zero identity a hole extent names.
const HOLE: [u8; 32] = [0u8; 32];

/// A byte range `[start, end)` of a body.
type Range = (u64, u64);

/// One chunk window to cut and the data ranges inside it.
type Window = (Range, Vec<Range>);

impl BodyCursor {
  /// The cursor over `inode`'s body of `size` bytes at `snapshot`: its data ranges read up front (a body with
  /// none is one hole, or nothing when empty).
  fn new(
    volume: &Volume,
    store: &Store,
    snapshot: SnapshotId,
    inode: InodeNo,
    size: u64,
  ) -> Result<BodyCursor, VfsError> {
    let mut cursor = BodyCursor {
      inode,
      size,
      data: volume
        .data_ranges_in(store, snapshot, inode)?
        .into_iter()
        .collect(),
      tiled: 0,
      extents: Vec::new(),
    };
    cursor.close_if_done();
    Ok(cursor)
  }

  fn finished(&self) -> bool {
    self.data.is_empty()
  }

  /// Tiles `[self.tiled, to)` with one hole extent, when it is not empty.
  fn hole_to(&mut self, to: u64) {
    if to > self.tiled {
      self.extents.push(Extent {
        offset: self.tiled,
        len: to.saturating_sub(self.tiled),
        chunk: HOLE,
        chunk_offset: 0,
      });
      self.tiled = to;
    }
  }

  /// Once every data range is cut, ends the body with the hole up to its size.
  fn close_if_done(&mut self) {
    if self.data.is_empty() {
      self.hole_to(self.size);
    }
  }

  /// The next chunk window to cut — `[start, end)`, aligned to `chunk` bytes and within the size — and the
  /// data ranges inside it, taken off the cursor (a range running past the window keeps its remainder).
  fn next_window(&mut self, chunk: u64) -> Option<Window> {
    let (first, _) = *self.data.front()?;
    let start = first.checked_div(chunk).unwrap_or(0).saturating_mul(chunk);
    let end = start.saturating_add(chunk).min(self.size);
    let mut inside = Vec::new();
    while let Some(&(from, to)) = self.data.front() {
      if from >= end {
        break;
      }
      self.data.pop_front();
      if to > end {
        self.data.push_front((end, to));
        inside.push((from, end));
        break;
      }
      inside.push((from, to));
    }
    Some(((start, end), inside))
  }
}

/// A file part-way through being cut into chunks across slices.
struct FileCursor {
  name: String,
  meta: NodeMeta,
  body: BodyCursor,
}

/// A node's extended attributes still to cut: each name and the attribute inode holding its value.
type Attributes = VecDeque<(Box<[u8]>, InodeNo)>;

/// A node whose metadata is gathering its extended attributes' values (format minor 3): the attributes
/// still to cut, the one being cut, and what to do with the node once its metadata is whole.
struct NodeCursor {
  name: String,
  meta: NodeMeta,
  queued: Attributes,
  value: Option<(Box<[u8]>, BodyCursor)>,
  then: Then,
}

/// What a node becomes once its metadata is whole.
enum Then {
  /// The root's metadata: it replaces the root frame's.
  Root,
  /// A directory: its frame is pushed, with the entries still to visit.
  Directory {
    dir: Handle<DirNode>,
    pending: VecDeque<Pending>,
  },
  /// A file: its bytes are cut next (or, empty, it is placed at once).
  File { inode: InodeNo, size: u64 },
  /// A node already built (a symlink's target, an IPC name): placed as is.
  Leaf(Node),
}

/// The resumable walk of one snapshot into an archive. Create with [`SnapshotArchiver::new`], then
/// call [`advance`](SnapshotArchiver::advance) with a byte budget until it returns
/// [`Progress::Done`].
pub struct SnapshotArchiver {
  snapshot: SnapshotId,
  chunk_bytes: usize,
  base_page_size: u32,
  created_unix: u64,
  volume_id: u64,
  name_policy_id: u32,
  frames: Vec<Frame>,
  node: Option<NodeCursor>,
  file: Option<FileCursor>,
  chunks: Vec<ArchiveChunk>,
  seen: BTreeSet<[u8; 32]>,
  codec: CodecPolicy,
  walked: Walked,
}

/// Format: the file-size histogram's classes are powers of two of bytes; a `u64` size falls in one of
/// these many classes (bit length 0 through 64).
const SIZE_CLASSES: usize = (u64::BITS + 1) as usize;

/// What one walk **measured** about the volume it archived (§4.11, research §2.4): the inputs the
/// content-defined-chunking gate is derived from — "FastCDC only for the class of files whose measured
/// size exceeds a threshold and whose observed dedup gain (bytes saved per byte hashed, tracked per
/// volume) exceeds the measured hashing cost; the FastCDC parameters … re-derived from the measured
/// file-size distribution of the volume". Recorded per walk so the gate reads real numbers, never a
/// fixed regime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Walked {
  /// Every byte read from the snapshot and hashed (the walk's cost).
  pub bytes_hashed: u64,
  /// Bytes a chunk already seen in this walk would have occupied again — what fixed-size chunking
  /// deduplicated within the snapshot (the dedup gain per byte hashed, against `bytes_hashed`).
  pub bytes_saved_by_dedup: u64,
  /// Raw bytes and stored bytes over every distinct chunk: what the cost model saved.
  pub raw_bytes: u64,
  /// The bytes the distinct chunks occupy as stored (compressed where the policy chose to).
  pub stored_bytes: u64,
  /// How many files fell in each power-of-two size class (index = bit length of the size), the
  /// file-size distribution the chunking parameters are derived from.
  pub file_sizes: [u64; SIZE_CLASSES],
}

impl Default for Walked {
  fn default() -> Walked {
    Walked {
      bytes_hashed: 0,
      bytes_saved_by_dedup: 0,
      raw_bytes: 0,
      stored_bytes: 0,
      file_sizes: [0; SIZE_CLASSES],
    }
  }
}

impl Walked {
  fn file(&mut self, size: u64) {
    let class = usize::try_from(u64::BITS - size.leading_zeros()).unwrap_or(0);
    self.file_sizes[class.min(SIZE_CLASSES - 1)] =
      self.file_sizes[class.min(SIZE_CLASSES - 1)].saturating_add(1);
  }
}

impl SnapshotArchiver {
  /// Starts the walk of `id` in `volume`, rooted at the snapshot's frozen root. `volume_id` is the
  /// informational volume identity the archive header carries (the caller's routing id) and
  /// `created_unix` its informational creation time (Unix seconds, the caller's clock — the walk
  /// itself reads no clock, so it is pure and deterministic). `codec` is the compress-or-not policy
  /// each chunk is stored under (D-17; the caller derives it from the boot profile's measured codec
  /// points, or [`CodecPolicy::raw_only`] where none were measured).
  pub fn new(
    volume: &Volume,
    store: &Store,
    id: SnapshotId,
    volume_id: u64,
    created_unix: u64,
    codec: CodecPolicy,
  ) -> Result<SnapshotArchiver, VfsError> {
    let snapshot = volume
      .snapshots
      .get(snapshot_handle(id))
      .map_err(|_| VfsError::StaleHandle)?;
    let root = snapshot.root;
    let pending = pending_of(volume, store, root)?;
    // The root has no entry naming it in the tree, so its own metadata (mode, owner, times) rides
    // the root frame to the archive's head (format minor 2).
    let root_no = store.dirs.get(root)?.inode;
    let root_meta = meta_of(volume, store, id, root_no, MODE_DIRECTORY)?;
    let root_attributes = attributes_of(volume, store, id, root_no)?;
    let name_policy_id = match volume.policy {
      NameEquivalence::Exact => POLICY_EXACT,
      NameEquivalence::Fold => POLICY_FOLD,
    };
    let mut archiver = SnapshotArchiver {
      snapshot: id,
      chunk_bytes: store.content.chunk_bytes().max(1),
      base_page_size: u32::try_from(store.content.page()).unwrap_or(u32::MAX),
      created_unix,
      volume_id,
      name_policy_id,
      frames: vec![Frame {
        dir: root,
        name: String::new(),
        meta: NodeMeta::default(),
        pending,
        built: Vec::new(),
      }],
      node: None,
      file: None,
      chunks: Vec::new(),
      seen: BTreeSet::new(),
      codec,
      walked: Walked::default(),
    };
    // The root's attribute values are cut first; its metadata replaces the frame's once whole.
    archiver.start(
      volume,
      store,
      NodeCursor {
        name: String::new(),
        meta: root_meta,
        queued: root_attributes,
        value: None,
        then: Then::Root,
      },
    )?;
    Ok(archiver)
  }

  /// The bytes hashed so far — the non-vacuity counter a test reads to know the walk did real work.
  pub fn bytes_hashed(&self) -> u64 {
    self.walked.bytes_hashed
  }

  /// What the walk has measured so far ([`Walked`]): the dedup gain, the compression saving and the
  /// file-size distribution — complete once [`advance`](SnapshotArchiver::advance) returns
  /// [`Progress::Done`].
  pub fn walked(&self) -> &Walked {
    &self.walked
  }

  /// Does about `byte_budget` bytes of work (at least one unit, so the walk always progresses) and
  /// reports whether the archive is complete. A refusal (a stale snapshot, a base-backed entry the
  /// archive cannot cover) ends the walk; the archiver is then spent.
  pub fn advance(
    &mut self,
    volume: &Volume,
    store: &Store,
    byte_budget: u64,
  ) -> Result<Progress, VfsError> {
    let mut spent: u64 = 0;
    loop {
      if spent >= byte_budget && spent > 0 {
        return Ok(Progress::More);
      }
      let cost = match self.step(volume, store)? {
        Step::Worked(cost) => cost,
        Step::Finished(archive) => return Ok(Progress::Done(archive)),
      };
      spent = spent.saturating_add(cost.max(1));
    }
  }

  /// One unit of the walk: a piece of the file in progress, or the next directory entry, or the close
  /// of a finished directory. Returns the work's cost in bytes.
  fn step(&mut self, volume: &Volume, store: &Store) -> Result<Step, VfsError> {
    if self.node.is_some() {
      return self.step_node(volume, store).map(Step::Worked);
    }
    if self.file.is_some() {
      return self.step_file(volume, store).map(Step::Worked);
    }
    let Some(frame) = self.frames.last_mut() else {
      return Err(VfsError::StaleHandle); // A spent archiver: nothing left to walk.
    };
    let Some(next) = frame.pending.pop_front() else {
      return self.close_directory();
    };
    let dir = frame.dir;
    let cost = match next.kind {
      Kind::Dir => self.enter_directory(volume, store, dir, next)?,
      Kind::File => self.begin_file(volume, store, next)?,
      Kind::Symlink => self.take_symlink(volume, store, next)?,
      Kind::Fifo | Kind::Socket => self.take_special(volume, store, next)?,
    };
    Ok(Step::Worked(cost))
  }

  /// Pops the finished directory and attaches it to its parent, or finishes the archive at the root.
  fn close_directory(&mut self) -> Result<Step, VfsError> {
    let Some(frame) = self.frames.pop() else {
      return Err(VfsError::StaleHandle);
    };
    let node = Node::Directory(frame.built);
    let cost = u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX);
    match self.frames.last_mut() {
      Some(parent) => {
        parent.built.push(Entry {
          name: frame.name,
          meta: frame.meta,
          node,
        });
        Ok(Step::Worked(cost))
      }
      None => Ok(Step::Finished(self.finish(node, frame.meta))),
    }
  }

  /// Enters a subdirectory: resolves its node at the snapshot, records its metadata, and pushes a frame
  /// with its entries.
  fn enter_directory(
    &mut self,
    volume: &Volume,
    store: &Store,
    parent: Handle<DirNode>,
    next: Pending,
  ) -> Result<u64, VfsError> {
    let located = volume.lookup_in(store, parent, &next.name)?;
    let Child::Dir(dir) = located.child else {
      return Err(VfsError::NotDirectory);
    };
    let meta = meta_of(volume, store, self.snapshot, next.inode, MODE_DIRECTORY)?;
    let queued = attributes_of(volume, store, self.snapshot, next.inode)?;
    let pending = pending_of(volume, store, dir)?;
    // An entry inside this directory has one path component per frame, the root's included, once this
    // one is pushed: past the manifest's bound the archive could not be read back, so the export refuses
    // it typed here (AUD-29-13) rather than emit what every reader refuses.
    let limit = slates_archive::manifest::MAX_DEPTH;
    if !pending.is_empty() && self.frames.len() >= limit {
      return Err(VfsError::TreeTooDeep { limit });
    }
    self.start(
      volume,
      store,
      NodeCursor {
        name: next.name,
        meta,
        queued,
        value: None,
        then: Then::Directory { dir, pending },
      },
    )?;
    Ok(u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX))
  }

  /// Begins a file: refuses a base-backed body (its bytes are not in RAM), records the metadata, and
  /// leaves a node cursor that cuts the file's attribute values and then its bytes.
  fn begin_file(&mut self, volume: &Volume, store: &Store, next: Pending) -> Result<u64, VfsError> {
    let inode = volume.inode_in(store, self.snapshot, next.inode)?;
    if matches!(inode.body, Body::Base(_)) {
      return Err(VfsError::RecoveryIncomplete);
    }
    let size = inode.attrs.size;
    self.walked.file(size);
    let meta = node_meta(next.inode, &inode.attrs, MODE_FILE);
    let queued = attributes_of(volume, store, self.snapshot, next.inode)?;
    self.start(
      volume,
      store,
      NodeCursor {
        name: next.name,
        meta,
        queued,
        value: None,
        then: Then::File {
          inode: next.inode,
          size,
        },
      },
    )?;
    Ok(u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX))
  }

  /// One unit of a node's metadata: a piece of the attribute value being cut, the start of the next
  /// value, or — every value cut — the node's placement. Returns the work's cost in bytes.
  fn step_node(&mut self, volume: &Volume, store: &Store) -> Result<u64, VfsError> {
    let Some(mut node) = self.node.take() else {
      return Ok(0);
    };
    let unit = u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX);
    if let Some((name, body)) = node.value.take() {
      let (body, cost) = self.cut(volume, store, body)?;
      if body.finished() {
        node.meta.xattrs.push(Xattr {
          name: name.into_vec(),
          extents: body.extents,
        });
      } else {
        node.value = Some((name, body));
      }
      self.node = Some(node);
      return Ok(cost);
    }
    if let Some((name, attribute)) = node.queued.pop_front() {
      let value = volume.inode_in(store, self.snapshot, attribute)?;
      if matches!(value.body, Body::Base(_)) {
        return Err(VfsError::RecoveryIncomplete);
      }
      let body = BodyCursor::new(volume, store, self.snapshot, attribute, value.attrs.size)?;
      if body.finished() {
        // A value without data — empty, or all hole — is its extents as they stand: an empty chunk is not
        // canonical (no extent can name one).
        node.meta.xattrs.push(Xattr {
          name: name.into_vec(),
          extents: body.extents,
        });
      } else {
        node.value = Some((name, body));
      }
      self.node = Some(node);
      return Ok(unit);
    }
    self.place(volume, store, node)?;
    Ok(unit)
  }

  /// Starts a node: one with no attributes is placed in the step that began it, so a tree without attributes
  /// walks in exactly the steps it did before format minor 3; one with attributes waits in the cursor
  /// while its values are cut.
  fn start(&mut self, volume: &Volume, store: &Store, node: NodeCursor) -> Result<(), VfsError> {
    if node.queued.is_empty() {
      self.place(volume, store, node)
    } else {
      self.node = Some(node);
      Ok(())
    }
  }

  /// Places a node whose metadata is whole, as its [`Then`] says.
  fn place(&mut self, volume: &Volume, store: &Store, node: NodeCursor) -> Result<(), VfsError> {
    let NodeCursor {
      name, meta, then, ..
    } = node;
    match then {
      Then::Root => {
        let root = self.frames.first_mut().ok_or(VfsError::StaleHandle)?;
        root.meta = meta;
        Ok(())
      }
      Then::Directory { dir, pending } => {
        self.frames.push(Frame {
          dir,
          name,
          meta,
          pending,
          built: Vec::new(),
        });
        Ok(())
      }
      Then::File { inode, size } => {
        let body = BodyCursor::new(volume, store, self.snapshot, inode, size)?;
        if body.finished() {
          // Empty, or all hole: the extents are already whole.
          return self.push_entry(Entry {
            name,
            meta,
            node: Node::File(body.extents),
          });
        }
        self.file = Some(FileCursor { name, meta, body });
        Ok(())
      }
      Then::Leaf(built) => self.push_entry(Entry {
        name,
        meta,
        node: built,
      }),
    }
  }

  /// Cuts the next data window off `body`, read at the snapshot — one chunk of the window's bytes, at its
  /// aligned offset so equal bytes at equal offsets deduplicate — and tiles it exactly: a data extent per
  /// data range in the window (naming its slice of the chunk) and a hole extent per gap. Returns the body
  /// advanced and the bytes hashed.
  fn cut(
    &mut self,
    volume: &Volume,
    store: &Store,
    mut body: BodyCursor,
  ) -> Result<(BodyCursor, u64), VfsError> {
    let chunk_len = u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX);
    let Some(((start, end), inside)) = body.next_window(chunk_len) else {
      return Ok((body, 0));
    };
    let want =
      usize::try_from(end.saturating_sub(start)).map_err(|_| VfsError::RecoveryIncomplete)?;
    let mut piece = vec![0u8; want];
    // `read_in_body`, not `read_in`: an attribute value is an attribute inode's body, which the public
    // read refuses so that no client reaches a value through a file read.
    let read = volume.read_in_body(store, self.snapshot, body.inode, start, &mut piece)?;
    // A read short of the size the inode records is a torn snapshot; refuse rather than archive a
    // truncated body under the recorded size.
    if read != want {
      return Err(VfsError::RecoveryIncomplete);
    }
    let len = u64::try_from(read).unwrap_or(u64::MAX);
    let chunk = self.push_chunk(piece);
    for (from, to) in inside {
      body.hole_to(from);
      body.extents.push(Extent {
        offset: from,
        len: to.saturating_sub(from),
        chunk,
        chunk_offset: from.saturating_sub(start),
      });
      body.tiled = to;
    }
    body.close_if_done();
    Ok((body, len))
  }

  /// Cuts the next chunk off the file in progress, finishing the file when its bytes are all cut.
  fn step_file(&mut self, volume: &Volume, store: &Store) -> Result<u64, VfsError> {
    let Some(FileCursor { name, meta, body }) = self.file.take() else {
      return Ok(0);
    };
    let (body, len) = self.cut(volume, store, body)?;
    if body.finished() {
      self.push_entry(Entry {
        name,
        meta,
        node: Node::File(body.extents),
      })?;
    } else {
      self.file = Some(FileCursor { name, meta, body });
    }
    Ok(len)
  }

  /// Records only the type and metadata of an IPC name, never kernel state (A-26).
  fn take_special(
    &mut self,
    volume: &Volume,
    store: &Store,
    next: Pending,
  ) -> Result<u64, VfsError> {
    let type_bits = if next.kind == Kind::Fifo {
      MODE_FIFO
    } else {
      MODE_SOCKET
    };
    let meta = meta_of(volume, store, self.snapshot, next.inode, type_bits)?;
    let queued = attributes_of(volume, store, self.snapshot, next.inode)?;
    self.start(
      volume,
      store,
      NodeCursor {
        name: next.name,
        meta,
        queued,
        value: None,
        then: Then::Leaf(Node::File(Vec::new())),
      },
    )?;
    Ok(u64::try_from(self.chunk_bytes).unwrap_or(u64::MAX))
  }

  /// Carries a symlink as a file of its target bytes under the link type bits.
  fn take_symlink(
    &mut self,
    volume: &Volume,
    store: &Store,
    next: Pending,
  ) -> Result<u64, VfsError> {
    let target = volume.readlink_in(store, self.snapshot, next.inode)?;
    let meta = meta_of(volume, store, self.snapshot, next.inode, MODE_SYMLINK)?;
    let bytes = target.as_bytes().to_vec();
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    let extents = if bytes.is_empty() {
      Vec::new()
    } else {
      let chunk = self.push_chunk(bytes);
      vec![Extent {
        offset: 0,
        len,
        chunk,
        chunk_offset: 0,
      }]
    };
    let queued = attributes_of(volume, store, self.snapshot, next.inode)?;
    self.start(
      volume,
      store,
      NodeCursor {
        name: next.name,
        meta,
        queued,
        value: None,
        then: Then::Leaf(Node::File(extents)),
      },
    )?;
    Ok(
      u64::try_from(self.chunk_bytes)
        .unwrap_or(u64::MAX)
        .saturating_add(len),
    )
  }

  /// Adds a built entry to the directory being walked.
  fn push_entry(&mut self, entry: Entry) -> Result<(), VfsError> {
    match self.frames.last_mut() {
      Some(frame) => {
        frame.built.push(entry);
        Ok(())
      }
      None => Err(VfsError::StaleHandle),
    }
  }

  /// Records a chunk once by identity — stored under the cost model's verdict for its bytes
  /// ([`Archive::chunk_with`]) — and returns that identity; a chunk already seen in this walk is
  /// deduplicated (its bytes counted as saved) and not stored or compressed again.
  fn push_chunk(&mut self, bytes: Vec<u8>) -> [u8; 32] {
    let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    self.walked.bytes_hashed = self.walked.bytes_hashed.saturating_add(len);
    let identity = slates_archive::archive::hash_of(&bytes);
    if !self.seen.insert(identity) {
      self.walked.bytes_saved_by_dedup = self.walked.bytes_saved_by_dedup.saturating_add(len);
      return identity;
    }
    let chunk = Archive::chunk_with(bytes, &self.codec);
    self.walked.raw_bytes = self.walked.raw_bytes.saturating_add(chunk.raw_len);
    self.walked.stored_bytes = self.walked.stored_bytes.saturating_add(chunk.stored_len);
    self.chunks.push(chunk);
    identity
  }

  /// The finished archive: the header fields, the root's own metadata, the manifest and the chunks in
  /// walk order.
  fn finish(&mut self, root: Node, root_meta: NodeMeta) -> Archive {
    let chunk_size = u32::try_from(self.chunk_bytes).unwrap_or(u32::MAX);
    Archive {
      base_page_size: self.base_page_size,
      chunk_min: chunk_size,
      chunk_max: chunk_size,
      created_unix: self.created_unix,
      volume_id: self.volume_id,
      snapshot_id: (u64::from(self.snapshot.index) << u32::BITS)
        | u64::from(self.snapshot.generation),
      name_policy_id: self.name_policy_id,
      unicode_version: UNICODE_VERSION_UNSPECIFIED,
      root_meta,
      manifest: root,
      chunks: std::mem::take(&mut self.chunks),
    }
  }
}

/// The outcome of one walk step.
enum Step {
  Worked(u64),
  Finished(Archive),
}

/// The entries of `dir` at the snapshot, in the directory's order, whiteouts skipped (a whiteout
/// hides a base name and has no content of its own).
fn pending_of(
  volume: &Volume,
  store: &Store,
  dir: Handle<DirNode>,
) -> Result<VecDeque<Pending>, VfsError> {
  Ok(
    volume
      .readdir_in(store, dir)?
      .into_iter()
      .map(|row| Pending {
        name: row.name.to_owned(),
        kind: row.kind,
        inode: row.inode,
      })
      .collect(),
  )
}

/// The manifest metadata of `no` at the snapshot, its mode carrying `type_bits`.
fn meta_of(
  volume: &Volume,
  store: &Store,
  snapshot: SnapshotId,
  no: InodeNo,
  type_bits: u32,
) -> Result<NodeMeta, VfsError> {
  let attrs = volume.stat_in(store, snapshot, no)?;
  Ok(node_meta(no, &attrs, type_bits))
}

/// The extended attributes of `no` at the snapshot: each name (ascending, as the owner's table holds them)
/// and the attribute inode holding its value.
fn attributes_of(
  volume: &Volume,
  store: &Store,
  snapshot: SnapshotId,
  no: InodeNo,
) -> Result<Attributes, VfsError> {
  volume
    .xattr_names_in(store, snapshot, no)?
    .into_iter()
    .map(|name| {
      let attribute = volume.xattr_inode_in(store, snapshot, no, &name)?;
      Ok((name, attribute))
    })
    .collect()
}

/// Manifest metadata from a snapshot's attributes: the inode number (identity across renames), the
/// permission bits under `type_bits`, all four times as the volume holds them (signed nanoseconds,
/// format minor 3), the size, the link count and the owner (uid, gid — format minor 2, so a clone or a
/// takeover successor rebuilds ownership). The extended attributes are added as their values are cut.
fn node_meta(no: InodeNo, attrs: &Attrs, type_bits: u32) -> NodeMeta {
  NodeMeta {
    ino: no.0,
    mode: permissions_of_mode(attrs.mode) | type_bits,
    atime_ns: attrs.atime,
    mtime_ns: attrs.mtime,
    ctime_ns: attrs.ctime,
    btime_ns: attrs.btime,
    size: attrs.size,
    nlink: attrs.nlink,
    uid: attrs.uid,
    gid: attrs.gid,
    xattrs: Vec::new(),
  }
}

#[cfg(test)]
mod tests {
  use slates_archive::restore;
  use slates_mem::arena::ChunkArena;
  use slates_mem::region::Region;

  use super::*;
  use crate::clock::StepClock;
  use crate::quota::Quota;
  use crate::volume::{StoreConfig, VolumeConfig};

  /// Shape: the test store's page — the machine's usual page, so the chunk is sixteen of them.
  const PAGE: usize = 4096;
  /// Shape: table caps ample for the trees below.
  const CAP: usize = 4096;
  /// Shape: the arena's pages — room for a few multi-chunk files.
  const ARENA_PAGES: usize = 256;
  /// Shape: the test volume's quota — well above the bytes written.
  const QUOTA: u64 = 1 << 20;
  /// Shape: the informational volume id and creation time the archive header carries.
  const VOLUME_ID: u64 = 7;
  const CREATED_UNIX: u64 = 1_700_000_000;

  fn store() -> Store {
    let mut arena = ChunkArena::new(PAGE);
    arena
      .add_region(Region::map(PAGE * ARENA_PAGES, PAGE, false).unwrap())
      .unwrap();
    Store::new(
      &StoreConfig {
        page: PAGE,
        cache_line: 64,
        max_dirs: CAP,
        max_inodes: CAP,
        max_chunks: CAP,
        max_dir_blocks: CAP,
        dir_cutover: 4,
      },
      arena,
      0,
    )
  }

  fn volume(store: &mut Store) -> Volume {
    Volume::create(
      store,
      VolumeConfig {
        prefix: 1,
        names: NameEquivalence::Exact,
        quota: Quota::Bounded { limit: QUOTA },
        journal_bytes: 1 << 16,
        clock: Box::new(StepClock::new(0, 1)),
      },
    )
    .unwrap()
  }

  /// Archives `id` to completion in `budget`-byte slices, counting the slices.
  fn archive_in_slices(
    volume: &Volume,
    store: &Store,
    id: SnapshotId,
    budget: u64,
  ) -> (Archive, u32) {
    let mut archiver = SnapshotArchiver::new(
      volume,
      store,
      id,
      VOLUME_ID,
      CREATED_UNIX,
      CodecPolicy::raw_only(),
    )
    .unwrap();
    let mut slices = 0;
    loop {
      slices += 1;
      match archiver.advance(volume, store, budget).unwrap() {
        Progress::More => continue,
        Progress::Done(archive) => return (archive, slices),
      }
    }
  }

  /// A tree with a nested directory, a multi-chunk file, an empty file and a symlink, so every
  /// node kind and the chunk cut are exercised.
  fn populate(volume: &mut Volume, store: &mut Store) -> Vec<u8> {
    let root = volume.root();
    let src = volume.mkdir(store, root, "src", 0o755).unwrap();
    let main = volume.create_file(store, src, "main.rs", 0o644).unwrap();
    let body: Vec<u8> = (0..(PAGE * 16 * 3 + 5))
      .map(|i| u8::try_from(i % 251).unwrap_or(0))
      .collect();
    volume.write(store, main, 0, &body).unwrap();
    volume.create_file(store, root, "empty", 0o600).unwrap();
    volume.symlink(store, root, "link", "src/main.rs").unwrap();
    // Ownership travels too (format minor 2): a file owned by another user, and the root itself — which
    // no entry names — given its own mode and owner.
    volume.chown(store, main, MAIN_UID, MAIN_GID).unwrap();
    let root_no = volume.root_inode(store).unwrap();
    volume.chmod(store, root_no, ROOT_MODE).unwrap();
    volume.chown(store, root_no, ROOT_UID, ROOT_GID).unwrap();
    body
  }

  /// Shape: the owner [`populate`] gives `src/main.rs`, any uid and gid other than a fresh inode's `0:0`.
  const MAIN_UID: u32 = 1234;
  /// Shape: see [`MAIN_UID`].
  const MAIN_GID: u32 = 4321;
  /// Shape: the mode [`populate`] gives the root, not the `0755` a fresh volume's root has.
  const ROOT_MODE: u32 = 0o750;
  /// Shape: the owner [`populate`] gives the root.
  const ROOT_UID: u32 = 1000;
  /// Shape: see [`ROOT_UID`].
  const ROOT_GID: u32 = 2000;

  /// AC-7.3 / §4.10: an export restores byte-identical — every file's bytes, the directories, the
  /// symlink (as a file under the link type bits) and each entry's mode round-trip through the
  /// archive — and the multi-chunk file cut into more than one distinct chunk (non-vacuous).
  #[test]
  fn an_export_restores_byte_identical_with_modes_and_kinds() {
    let mut store = store();
    let mut volume = volume(&mut store);
    let body = populate(&mut volume, &mut store);
    let id = volume.snapshot(&mut store).unwrap();

    let (archive, _) = archive_in_slices(&volume, &store, id, u64::MAX);
    assert!(
      archive.chunks.len() >= 3,
      "a file of three chunks and more cuts into several distinct chunks: {}",
      archive.chunks.len()
    );
    let restored = restore(&archive, u64::MAX).unwrap();
    assert_restored_tree(&restored, &body);
  }

  /// The tree [`populate`] built, as a restore must reproduce it: every file's bytes, the directory, the
  /// symlink as a file under the link type bits, and each entry's kind, permissions and size.
  fn assert_restored_tree(restored: &slates_archive::Restored, body: &[u8]) {
    let dense = |path: &str| {
      restored
        .files
        .get(path)
        .and_then(slates_archive::RestoredFile::dense)
    };
    assert_eq!(dense("src/main.rs").as_deref(), Some(body));
    assert_eq!(dense("empty"), Some(Vec::new()));
    assert_eq!(dense("link").as_deref(), Some(b"src/main.rs".as_slice()));
    assert!(restored.directories.contains("src"));
    let meta = |path: &str| restored.metadata.get(path).cloned().unwrap();
    let expected = [
      ("src", Some(Kind::Dir), 0o755),
      ("src/main.rs", Some(Kind::File), 0o644),
      ("link", Some(Kind::Symlink), 0o777),
      ("empty", Some(Kind::File), 0o600),
    ];
    for (path, kind, permissions) in expected {
      assert_eq!(kind_of_mode(meta(path).mode), kind, "{path}");
      if kind != Some(Kind::Symlink) {
        assert_eq!(permissions_of_mode(meta(path).mode), permissions, "{path}");
      }
    }
    assert_eq!(meta("src/main.rs").size, u64::try_from(body.len()).unwrap());
    assert_restored_ownership(restored);
  }

  /// The ownership half of [`assert_restored_tree`] (format minor 2): the chowned file's owner, the
  /// untouched entries' `0:0`, and the root's own mode and owner, which no entry names and the archive's
  /// head carries.
  fn assert_restored_ownership(restored: &slates_archive::Restored) {
    let meta = |path: &str| restored.metadata.get(path).cloned().unwrap();
    assert_eq!(
      (meta("src/main.rs").uid, meta("src/main.rs").gid),
      (MAIN_UID, MAIN_GID),
      "a chowned file's owner travels"
    );
    assert_eq!((meta("empty").uid, meta("empty").gid), (0, 0));
    assert_eq!(kind_of_mode(restored.root.mode), Some(Kind::Dir));
    assert_eq!(
      (
        permissions_of_mode(restored.root.mode),
        restored.root.uid,
        restored.root.gid
      ),
      (ROOT_MODE, ROOT_UID, ROOT_GID),
      "the root's own mode and owner travel at the archive's head"
    );
  }

  /// Shape: the latest pre-epoch time — the nearest negative stamp to zero, the one the minor-2 export
  /// clamped to zero. The other test times step down from it, so all three are distinct and pre-epoch.
  const PRE_EPOCH_NS: i64 = -1;

  /// Gives the tree [`populate`] built extended attributes on a file (one small value, and one value of
  /// two chunks and a byte, so it cuts into several extents), on the directory and on the root, and
  /// pre-epoch access, modification and birth times on the file. Returns the multi-chunk value.
  fn decorate(volume: &mut Volume, store: &mut Store) -> Vec<u8> {
    let main = volume.resolve(store, "src/main.rs").unwrap().inode;
    let src = volume.resolve(store, "src").unwrap().inode;
    let root = volume.root_inode(store).unwrap();
    let chunk = store.content.chunk_bytes();
    let large: Vec<u8> = (0..chunk * 2 + 1)
      .map(|i| u8::try_from(i % 241).unwrap_or(0))
      .collect();
    let set = slates_archive::manifest::XATTR_NAME_MAX_BYTES;
    let longest_name = vec![b'n'; set];
    for (no, name, value) in [
      (main, b"user.small".as_slice(), b"v".as_slice()),
      (main, b"user.fork".as_slice(), large.as_slice()),
      (main, b"user.empty".as_slice(), b"".as_slice()),
      (main, longest_name.as_slice(), b"longest name".as_slice()),
      (src, b"user.dir".as_slice(), b"on a directory".as_slice()),
      (root, b"user.root".as_slice(), b"on the root".as_slice()),
    ] {
      volume
        .xattr_set(store, no, name, value, crate::xattr::XattrSet::Create)
        .unwrap();
    }
    volume
      .set_times(
        store,
        main,
        Some(PRE_EPOCH_NS),
        Some(PRE_EPOCH_NS - 1),
        None,
        Some(PRE_EPOCH_NS - 2),
      )
      .unwrap();
    large
  }

  /// A node's attribute values by name, in name order.
  type Values = Vec<(Vec<u8>, Vec<u8>)>;

  /// The serial oracle for one node's metadata: what the volume holds for `no` at snapshot `id` — every
  /// field the manifest promises, and each attribute's whole value read back through the volume.
  fn oracle_meta(volume: &Volume, store: &Store, id: SnapshotId, no: InodeNo) -> (Attrs, Values) {
    let attrs = volume.stat_in(store, id, no).unwrap();
    let values = volume
      .xattr_names_in(store, id, no)
      .unwrap()
      .into_iter()
      .map(|name| {
        let len = volume.xattr_len_in(store, id, no, &name).unwrap();
        let mut value = vec![0u8; usize::try_from(len).unwrap()];
        let read = volume
          .xattr_read_in(store, id, no, &name, 0, &mut value)
          .unwrap();
        assert_eq!(read, value.len());
        (name.into_vec(), value)
      })
      .collect();
    (attrs, values)
  }

  /// AUD-29-56: do: give a tree attribute values (a multi-chunk one, an empty one, one under the longest
  /// name, on a file, a directory and the root) and pre-epoch times, snapshot, export in one-byte slices
  /// and restore; expect every node's four times, owner and every attribute value equal to what the volume
  /// holds at the snapshot (the oracle), the pre-epoch times unclamped, and the multi-chunk value cut into
  /// more than one extent (non-vacuous).
  #[test]
  fn an_export_restores_every_attribute_value_and_all_four_times() {
    let mut store = store();
    let mut volume = volume(&mut store);
    populate(&mut volume, &mut store);
    let large = decorate(&mut volume, &mut store);
    let id = volume.snapshot(&mut store).unwrap();
    let (archive, _) = archive_in_slices(&volume, &store, id, 1);
    let decoded = Archive::decode(&archive.encode()).unwrap();
    let restored = restore(&decoded, u64::MAX).unwrap();
    let paths = ["", "src", "src/main.rs", "empty", "link"];
    for path in paths {
      let no = volume.resolve(&store, path).unwrap().inode;
      let (attrs, values) = oracle_meta(&volume, &store, id, no);
      let meta = if path.is_empty() {
        restored.root.clone()
      } else {
        restored.metadata.get(path).cloned().unwrap()
      };
      assert_eq!(
        (meta.atime_ns, meta.mtime_ns, meta.ctime_ns, meta.btime_ns),
        (attrs.atime, attrs.mtime, attrs.ctime, attrs.btime),
        "{path:?}: the four times"
      );
      assert_eq!((meta.uid, meta.gid), (attrs.uid, attrs.gid), "{path:?}");
      let carried: Vec<(Vec<u8>, Vec<u8>)> = restored
        .xattrs
        .get(path)
        .map(|by_name| by_name.clone().into_iter().collect())
        .unwrap_or_default();
      assert_eq!(carried, values, "{path:?}: the attribute values");
    }
    let main = restored.metadata.get("src/main.rs").unwrap();
    assert_eq!(
      main.atime_ns, PRE_EPOCH_NS,
      "a pre-epoch time is not clamped"
    );
    let fork = main
      .xattrs
      .iter()
      .find(|xattr| xattr.name == b"user.fork")
      .unwrap();
    assert!(
      fork.extents.len() > 1,
      "the large value cut into several extents"
    );
    assert_eq!(
      restored.xattrs["src/main.rs"].get(b"user.fork".as_slice()),
      Some(&large)
    );
  }

  /// AUD-29-56: do: export a snapshot, then change only one attribute's value, only its name set (a new
  /// empty attribute), and only the access time, snapshotting after each; expect each later snapshot's
  /// manifest identity differs from the one before — an attribute-only or time-only change is a change the
  /// archive's identity sees — and the chunk walker names the attribute value's chunk.
  #[test]
  fn an_attribute_or_time_only_change_changes_the_identity() {
    let mut store = store();
    let mut volume = volume(&mut store);
    populate(&mut volume, &mut store);
    let main = volume.resolve(&store, "src/main.rs").unwrap().inode;
    let identity_now = |volume: &mut Volume, store: &mut Store| {
      let id = volume.snapshot(store).unwrap();
      let (archive, _) = archive_in_slices(volume, store, id, u64::MAX);
      (archive.manifest_identity(), archive)
    };
    let set = crate::xattr::XattrSet::Either;
    volume
      .xattr_set(&mut store, main, b"user.tag", b"one", set)
      .unwrap();
    let (first, archive) = identity_now(&mut volume, &mut store);
    let value_chunk = slates_archive::archive::hash_of(b"one");
    assert!(archive.referenced_chunks().contains(&value_chunk));
    volume
      .xattr_set(&mut store, main, b"user.tag", b"two", set)
      .unwrap();
    let (second, _) = identity_now(&mut volume, &mut store);
    volume
      .xattr_set(&mut store, main, b"user.more", b"", set)
      .unwrap();
    let (third, _) = identity_now(&mut volume, &mut store);
    let atime = volume.stat(&store, main).unwrap().atime;
    volume
      .set_times(&mut store, main, Some(atime - 1), None, None, None)
      .unwrap();
    let (fourth, _) = identity_now(&mut volume, &mut store);
    assert_ne!(first, second, "a changed value is a changed identity");
    assert_ne!(second, third, "an added attribute is a changed identity");
    assert_ne!(third, fourth, "a changed access time is a changed identity");
  }

  /// D-17 determinism gate: the same snapshot exports to the same archive bytes and manifest identity
  /// every time, whatever the slice budget; a later snapshot with a changed file has another identity.
  #[test]
  fn the_same_snapshot_exports_to_the_same_identity_whatever_the_slicing() {
    let mut store = store();
    let mut volume = volume(&mut store);
    populate(&mut volume, &mut store);
    let id = volume.snapshot(&mut store).unwrap();

    let (whole, one_slice) = archive_in_slices(&volume, &store, id, u64::MAX);
    let (sliced, many_slices) = archive_in_slices(&volume, &store, id, 1);
    assert_eq!(one_slice, 1, "an unbounded budget archives in one slice");
    assert!(
      many_slices > one_slice,
      "a one-byte budget takes many slices: {many_slices}"
    );
    assert_eq!(
      whole.encode(),
      sliced.encode(),
      "the archive bytes are the same"
    );
    assert_eq!(whole.manifest.identity(), sliced.manifest.identity());

    let root = volume.root();
    let src = volume.lookup(&store, root, "src").unwrap();
    let Child::Dir(src) = src.child else {
      panic!("src is a directory");
    };
    let main = volume.lookup(&store, src, "main.rs").unwrap().inode;
    volume.write(&mut store, main, 0, b"changed").unwrap();
    let later = volume.snapshot(&mut store).unwrap();
    let (changed, _) = archive_in_slices(&volume, &store, later, u64::MAX);
    assert_ne!(
      changed.manifest.identity(),
      whole.manifest.identity(),
      "a changed file changes the manifest identity"
    );
    let (again, _) = archive_in_slices(&volume, &store, id, u64::MAX);
    assert_eq!(
      again.manifest.identity(),
      whole.manifest.identity(),
      "the earlier snapshot still exports to its own identity after the head moved"
    );
  }

  /// §4.3 bounded slices: a budget bounds each slice's work — a slice of `budget` bytes hashes at most
  /// about that many bytes more than the slice before — and the walk still completes.
  #[test]
  fn a_slice_hashes_about_its_budget_and_the_walk_completes() {
    let mut store = store();
    let mut volume = volume(&mut store);
    populate(&mut volume, &mut store);
    let id = volume.snapshot(&mut store).unwrap();
    let budget = u64::try_from(PAGE * 16).unwrap();
    let mut archiver = SnapshotArchiver::new(
      &volume,
      &store,
      id,
      VOLUME_ID,
      CREATED_UNIX,
      CodecPolicy::raw_only(),
    )
    .unwrap();
    let mut before = 0;
    let mut slices = 0;
    let done = loop {
      slices += 1;
      let progress = archiver.advance(&volume, &store, budget).unwrap();
      let hashed = archiver.bytes_hashed();
      assert!(
        hashed - before <= budget * 2,
        "one slice hashes at most its budget plus one unit of work: {}",
        hashed - before
      );
      before = hashed;
      if let Progress::Done(archive) = progress {
        break archive;
      }
    };
    assert!(slices > 1, "the tree took several slices: {slices}");
    assert!(
      done.chunks.len() >= 3 && archiver.bytes_hashed() > 0,
      "the walk did real work"
    );
  }
}
