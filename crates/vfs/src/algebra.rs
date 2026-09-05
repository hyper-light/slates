//! The interval algebra of declared operations (§4.16 "Composition at seal", D-27, Phase 1
//! task 14): a pure module that composes a file's declared operations since its base version
//! into a net edit relative to the base content, without ever reading the bytes.
//!
//! The state is a content map: the post-state as a sequence of segments, each either a run of
//! base bytes (by base offset) or a run of new bytes. Every declared operation is a splice on
//! that sequence: an overwrite replaces a range by new bytes, an extend appends them, a
//! truncate cuts, an insert splices new bytes in, a delete removes a range. Base offsets in the
//! map only ever increase along the post-state (no operation copies base bytes), so the map
//! reads as an edit script: the runs of base bytes that survive, and between them the
//! maximal runs of deleted base bytes and new bytes, each such run one [`Hunk`]. The hunks are
//! the unique minimal edit relative to the surviving base bytes, so the same declared
//! operations give the same hunks whatever their order where the algebra says they commute
//! (disjoint ranges), and a whole-file rewrite by any route is one hunk of the base length and
//! the new length (T-1.19).
//!
//! Evidence: the design's clause that composition is arithmetic on facts and comparison of file
//! states is inference (hecate `MERGE.md` §5, `research/merge-engine.md` §2); the reference
//! applier in the tests replays the same operations on bytes and the hunks must reproduce its
//! result byte for byte on every generated history (T-1.18).

/// Where a run of post-state bytes comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Src {
  /// Bytes of the base content starting at this base offset.
  Base(u64),
  /// Bytes the work wrote (their values live in the post-state).
  New,
}

/// A run of post-state bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Segment {
  /// Length in bytes.
  pub len: u64,
  /// Source.
  pub src: Src,
}

/// A declared content operation on one file, positions in the file as it was when declared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentOp {
  /// Bytes overwritten at `at` for `len` (past the end, the file grows).
  Overwrite {
    /// Offset.
    at: u64,
    /// Length.
    len: u64,
  },
  /// Bytes appended: `at` is the old length, `len` the bytes added (a gap is zero bytes).
  Extend {
    /// The old length.
    at: u64,
    /// Length.
    len: u64,
  },
  /// Truncated to `len` (past the end, zero bytes are added).
  Truncate {
    /// The new length.
    len: u64,
  },
  /// `len` new bytes inserted at `at`, shifting the rest.
  Insert {
    /// Offset.
    at: u64,
    /// Length.
    len: u64,
  },
  /// `len` bytes removed at `at`, shifting the rest.
  Delete {
    /// Offset.
    at: u64,
    /// Length.
    len: u64,
  },
}

/// One run of the net edit: `base_len` bytes at `base_at` of the base are replaced by
/// `new_len` bytes at `post_at` of the post-state. A delete has `new_len == 0`, an insert
/// `base_len == 0`, an overwrite equal lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hunk {
  /// Base offset of the replaced bytes.
  pub base_at: u64,
  /// Replaced base bytes.
  pub base_len: u64,
  /// Post-state offset of the new bytes.
  pub post_at: u64,
  /// New bytes.
  pub new_len: u64,
}

/// The post-state of one file as runs of base and new bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContentMap {
  segments: Vec<Segment>,
}

impl ContentMap {
  /// A file identical to its base of `base_len` bytes.
  pub fn identity(base_len: u64) -> Self {
    let mut map = Self {
      segments: Vec::new(),
    };
    map.push(Segment {
      len: base_len,
      src: Src::Base(0),
    });
    map
  }

  /// A file with no base (created by the work), empty.
  pub fn created() -> Self {
    Self {
      segments: Vec::new(),
    }
  }

  /// The post-state length.
  pub fn len(&self) -> u64 {
    self.segments.iter().map(|s| s.len).sum()
  }

  /// Whether the post-state is empty.
  pub fn is_empty(&self) -> bool {
    self.len() == 0
  }

  /// The segments.
  pub fn segments(&self) -> &[Segment] {
    &self.segments
  }

  /// Applies one declared operation.
  pub fn apply(&mut self, op: ContentOp) {
    match op {
      ContentOp::Overwrite { at, len } => self.replace(at, len, len),
      ContentOp::Extend { at, len } => {
        // The declared old length is where the bytes went; a gap is zero bytes, new too.
        self.replace(at, 0, len);
      }
      ContentOp::Truncate { len } => {
        let size = self.len();
        if len < size {
          self.remove(len, size - len);
        } else {
          self.replace(size, 0, len - size);
        }
      }
      ContentOp::Insert { at, len } => self.replace(at, 0, len),
      ContentOp::Delete { at, len } => self.remove(at, len),
    }
  }

  /// Replaces `old_len` bytes at `at` (growing past the end with new bytes) by `new_len` new
  /// bytes.
  fn replace(&mut self, at: u64, old_len: u64, new_len: u64) {
    let size = self.len();
    if at > size {
      // A hole is new bytes (zeros the post-state holds).
      self.push(Segment {
        len: at - size,
        src: Src::New,
      });
    }
    let size = self.len();
    let end = at.saturating_add(old_len).min(size);
    let tail = self.split_off(end);
    let _ = self.split_off(at);
    self.push(Segment {
      len: new_len,
      src: Src::New,
    });
    for s in tail {
      self.push(s);
    }
  }

  /// Removes `len` bytes at `at` (clipped to the end).
  fn remove(&mut self, at: u64, len: u64) {
    let size = self.len();
    if at >= size {
      return;
    }
    let end = at.saturating_add(len).min(size);
    let tail = self.split_off(end);
    let _ = self.split_off(at);
    for s in tail {
      self.push(s);
    }
  }

  /// Cuts the map at `pos`, returning the segments past it.
  fn split_off(&mut self, pos: u64) -> Vec<Segment> {
    let mut cursor = 0u64;
    let mut index = self.segments.len();
    let mut carry = None;
    for (i, s) in self.segments.iter().enumerate() {
      if cursor + s.len <= pos {
        cursor += s.len;
        continue;
      }
      if cursor < pos {
        // Split this segment.
        let keep = pos - cursor;
        let rest = s.len - keep;
        let rest_src = match s.src {
          Src::Base(off) => Src::Base(off + keep),
          Src::New => Src::New,
        };
        carry = Some((
          i,
          Segment {
            len: keep,
            src: s.src,
          },
          Segment {
            len: rest,
            src: rest_src,
          },
        ));
        index = i + 1;
      } else {
        index = i;
      }
      break;
    }
    let mut tail = self.segments.split_off(index);
    if let Some((i, head, rest)) = carry {
      self.segments[i] = head;
      tail.insert(0, rest);
    }
    tail
  }

  /// Appends a segment, merging with the previous when they continue each other.
  fn push(&mut self, s: Segment) {
    if s.len == 0 {
      return;
    }
    if let Some(last) = self.segments.last_mut() {
      let merge = match (last.src, s.src) {
        (Src::New, Src::New) => true,
        (Src::Base(a), Src::Base(b)) => a + last.len == b,
        _ => false,
      };
      if merge {
        last.len += s.len;
        return;
      }
    }
    self.segments.push(s);
  }

  /// The net edit relative to a base of `base_len` bytes: maximal runs of replaced base
  /// bytes and new bytes between the surviving base runs, in post-state order.
  pub fn hunks(&self, base_len: u64) -> Vec<Hunk> {
    let mut out = Vec::new();
    let mut base_cursor = 0u64;
    let mut post_cursor = 0u64;
    let mut pending: Option<Hunk> = None;
    for s in &self.segments {
      match s.src {
        Src::Base(off) => {
          let deleted = off.saturating_sub(base_cursor);
          if deleted > 0 || pending.is_some() {
            let h = pending.get_or_insert(Hunk {
              base_at: base_cursor,
              base_len: 0,
              post_at: post_cursor,
              new_len: 0,
            });
            h.base_len += deleted;
          }
          if let Some(h) = pending.take() {
            out.push(h);
          }
          base_cursor = off + s.len;
        }
        Src::New => {
          let h = pending.get_or_insert(Hunk {
            base_at: base_cursor,
            base_len: 0,
            post_at: post_cursor,
            new_len: 0,
          });
          h.new_len += s.len;
        }
      }
      post_cursor += s.len;
    }
    let trailing = base_len.saturating_sub(base_cursor);
    if trailing > 0 || pending.is_some() {
      let h = pending.get_or_insert(Hunk {
        base_at: base_cursor,
        base_len: 0,
        post_at: post_cursor,
        new_len: 0,
      });
      h.base_len += trailing;
      out.push(*h);
    }
    out
  }
}

/// The reference applier: the post-state from the base bytes and the hunks, taking new bytes
/// from `post` (the sealed post-state) at the hunk's post offset. Returns `None` when a hunk
/// lies outside its source, which the property tests treat as a failure.
pub fn apply_hunks(base: &[u8], post: &[u8], hunks: &[Hunk]) -> Option<Vec<u8>> {
  let mut out = Vec::with_capacity(post.len());
  let mut base_cursor = 0usize;
  for h in hunks {
    let base_at = usize::try_from(h.base_at).ok()?;
    let base_len = usize::try_from(h.base_len).ok()?;
    let post_at = usize::try_from(h.post_at).ok()?;
    let new_len = usize::try_from(h.new_len).ok()?;
    if base_at < base_cursor || base_at + base_len > base.len() {
      return None;
    }
    out.extend_from_slice(&base[base_cursor..base_at]);
    if post_at != out.len() || post_at + new_len > post.len() {
      return None;
    }
    out.extend_from_slice(&post[post_at..post_at + new_len]);
    base_cursor = base_at + base_len;
  }
  out.extend_from_slice(&base[base_cursor..]);
  Some(out)
}

/// The reference byte applier of a declared operation, for tests and the applier of §4.16:
/// the same splice on real bytes (new bytes are taken from `fresh`, zeros when it is short).
pub fn apply_to_bytes(bytes: &mut Vec<u8>, op: ContentOp, fresh: &[u8]) {
  let take = |len: u64| -> Vec<u8> {
    let len = usize::try_from(len).unwrap_or(0);
    let mut v = fresh.to_vec();
    v.resize(len, 0);
    v.truncate(len);
    v
  };
  let size = bytes.len();
  match op {
    ContentOp::Overwrite { at, len } => {
      let at = usize::try_from(at).unwrap_or(0);
      let new = take(len);
      if bytes.len() < at {
        bytes.resize(at, 0);
      }
      let end = (at + new.len()).min(bytes.len());
      bytes.splice(at..end, new);
    }
    ContentOp::Extend { at, len } => {
      let at = usize::try_from(at).unwrap_or(0).max(size);
      bytes.resize(at, 0);
      bytes.extend(take(len));
    }
    ContentOp::Truncate { len } => bytes.resize(usize::try_from(len).unwrap_or(0), 0),
    ContentOp::Insert { at, len } => {
      // Past the end the file grows with zeros first, as an overwrite past the end does.
      let at = usize::try_from(at).unwrap_or(0);
      if bytes.len() < at {
        bytes.resize(at, 0);
      }
      let new = take(len);
      bytes.splice(at..at, new);
    }
    ContentOp::Delete { at, len } => {
      let at = usize::try_from(at).unwrap_or(0).min(bytes.len());
      let end = at
        .saturating_add(usize::try_from(len).unwrap_or(0))
        .min(bytes.len());
      bytes.drain(at..end);
    }
  }
}

#[cfg(test)]
mod tests {
  // proptest's strategy types carry `Arc` (D-8's harness exception).
  #![allow(clippy::disallowed_types)]

  use proptest::prelude::*;

  use super::*;

  fn op() -> impl Strategy<Value = ContentOp> {
    prop_oneof![
      (0..64u64, 1..32u64).prop_map(|(at, len)| ContentOp::Overwrite { at, len }),
      (0..64u64, 1..32u64).prop_map(|(at, len)| ContentOp::Extend { at, len }),
      (0..80u64).prop_map(|len| ContentOp::Truncate { len }),
      (0..64u64, 1..32u64).prop_map(|(at, len)| ContentOp::Insert { at, len }),
      (0..64u64, 1..32u64).prop_map(|(at, len)| ContentOp::Delete { at, len }),
    ]
  }

  /// `Extend` is declared with the true old length; the generator's `at` is replaced by it.
  fn normalize(op: ContentOp, size: u64) -> ContentOp {
    match op {
      ContentOp::Extend { len, .. } => ContentOp::Extend { at: size, len },
      other => other,
    }
  }

  proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, failure_persistence: None, .. ProptestConfig::default() })]

    /// T-1.18: net-apply equals raw replay, every hunk lies within its sources.
    #[test]
    fn hunks_reproduce_the_replayed_bytes(
      base in prop::collection::vec(any::<u8>(), 0..48),
      ops in prop::collection::vec(op(), 0..12),
      fresh in prop::collection::vec(1..=255u8, 40),
    ) {
      let base_len = u64::try_from(base.len()).unwrap();
      let mut map = ContentMap::identity(base_len);
      let mut post = base.clone();
      for (i, op) in ops.iter().enumerate() {
        let op = normalize(*op, u64::try_from(post.len()).unwrap());
        map.apply(op);
        // Distinct fresh bytes per operation, so a wrong hunk cannot pass by luck.
        let salt: Vec<u8> = fresh.iter().map(|b| b.wrapping_add(u8::try_from(i).unwrap())).collect();
        apply_to_bytes(&mut post, op, &salt);
        prop_assert_eq!(map.len(), u64::try_from(post.len()).unwrap(), "size after {:?}", op);
      }
      let hunks = map.hunks(base_len);
      let rebuilt = apply_hunks(&base, &post, &hunks);
      prop_assert_eq!(rebuilt.as_deref(), Some(post.as_slice()), "hunks {:?}", hunks);
      // Hunks are sorted, disjoint in the base and never empty.
      for w in hunks.windows(2) {
        prop_assert!(w[0].base_at + w[0].base_len <= w[1].base_at);
        prop_assert!(w[0].post_at + w[0].new_len <= w[1].post_at);
      }
      for h in &hunks {
        prop_assert!(h.base_len + h.new_len > 0);
      }
    }

    /// Disjoint overwrites commute: the hunks do not depend on their order.
    #[test]
    fn disjoint_overwrites_commute(
      base_len in 64..128u64,
      a in (0..24u64, 1..8u64),
      b in (32..56u64, 1..8u64),
    ) {
      let ops = [
        ContentOp::Overwrite { at: a.0, len: a.1 },
        ContentOp::Overwrite { at: b.0, len: b.1 },
      ];
      let mut forward = ContentMap::identity(base_len);
      let mut backward = ContentMap::identity(base_len);
      forward.apply(ops[0]);
      forward.apply(ops[1]);
      backward.apply(ops[1]);
      backward.apply(ops[0]);
      prop_assert_eq!(forward.hunks(base_len), backward.hunks(base_len));
    }
  }

  /// T-1.19: a whole-file rewrite by truncate-and-write is one hunk of the base length and
  /// the new length; so is a file with no surviving base bytes by any other route.
  #[test]
  fn a_whole_file_rewrite_is_one_hunk() {
    let mut map = ContentMap::identity(100);
    map.apply(ContentOp::Truncate { len: 0 });
    map.apply(ContentOp::Extend { at: 0, len: 42 });
    assert_eq!(
      map.hunks(100),
      vec![Hunk {
        base_at: 0,
        base_len: 100,
        post_at: 0,
        new_len: 42
      }]
    );
    let mut replaced = ContentMap::created();
    replaced.apply(ContentOp::Extend { at: 0, len: 42 });
    assert_eq!(replaced.hunks(100), map.hunks(100));
    // Overlapping overwrites merge into one hunk.
    let mut over = ContentMap::identity(100);
    over.apply(ContentOp::Overwrite { at: 10, len: 20 });
    over.apply(ContentOp::Overwrite { at: 20, len: 20 });
    assert_eq!(
      over.hunks(100),
      vec![Hunk {
        base_at: 10,
        base_len: 30,
        post_at: 10,
        new_len: 30
      }]
    );
    // An insert followed by a delete covering it cancels.
    let mut cancel = ContentMap::identity(100);
    cancel.apply(ContentOp::Insert { at: 50, len: 5 });
    cancel.apply(ContentOp::Delete { at: 50, len: 5 });
    assert!(cancel.hunks(100).is_empty());
    // A truncate cancels operations beyond the new length.
    let mut cut = ContentMap::identity(100);
    cut.apply(ContentOp::Overwrite { at: 90, len: 5 });
    cut.apply(ContentOp::Truncate { len: 80 });
    assert_eq!(
      cut.hunks(100),
      vec![Hunk {
        base_at: 80,
        base_len: 20,
        post_at: 80,
        new_len: 0
      }]
    );
  }
}
