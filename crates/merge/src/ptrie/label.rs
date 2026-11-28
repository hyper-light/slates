//! A radix tree node's label (A-920 unit 0, main's ruling of 2026-11-28 00:38): up to [`INLINE`] bytes kept in the node
//! itself, a longer one a run of the arena's label store ([`super::store`]).
//!
//! **Why.** With every label a run, a lookup read each node's label through its store's chunk list and then the chunk:
//! one more hop than a label of its own, and `get` at 9,000 keys ran at about 1.16 times the median of the tree before
//! the arena (w-svn's warmed 20-run table of 2026-11-28: 52.9 against 45.6 ns). Most labels of a repository's paths are
//! short (a path segment, a file's tail), so a short label in its node reads no store at all.
//!
//! **What.** [`Label`] is a short label's bytes in place, or a run. [`Labels`] makes and changes labels as the store
//! does runs (alloc, duplicate, prefix, suffix, joined, prepended, free), choosing the form by length alone, so the
//! same bytes always take the same form, and copying through a stack buffer where an inline label meets a run, so no
//! label is ever a heap allocation.

use super::store::{Run, Store};

/// The most bytes a label keeps in its node.
///
/// Derived: a run's own size, 16 bytes (its chunk, first item and length, three `u32`s, and its class, a byte, padded),
/// less the byte that holds an inline label's length: the label's bytes take no more room in the node than a run does.
pub(super) const INLINE: usize = size_of::<Run>() - 1;

/// A node's label: its bytes in place when it is short, else a run of the arena's label store.
#[derive(Clone, Copy, Debug)]
pub(super) enum Label {
  /// Up to [`INLINE`] bytes, the first `len` of `bytes`.
  Inline { len: u8, bytes: [u8; INLINE] },
  /// A run of the label store.
  Run(Run),
}

impl Label {
  /// The empty label.
  pub(super) const EMPTY: Label = Label::Inline {
    len: 0,
    bytes: [0; INLINE],
  };

  /// Its length.
  pub(super) fn len(&self) -> usize {
    match self {
      Label::Inline { len, .. } => usize::from(*len),
      Label::Run(run) => run.len(),
    }
  }

  /// Whether it holds no byte.
  #[cfg(test)]
  pub(super) fn is_empty(&self) -> bool {
    self.len() == 0
  }
}

/// `bytes` as an inline label, where they fit.
fn inline(bytes: &[u8]) -> Option<Label> {
  let len = u8::try_from(bytes.len())
    .ok()
    .filter(|len| usize::from(*len) <= INLINE)?;
  let mut kept = [0u8; INLINE];
  kept.get_mut(..bytes.len())?.copy_from_slice(bytes);
  Some(Label::Inline { len, bytes: kept })
}

/// The labels of an arena: its label store, and the labels made from it (module docs).
#[derive(Debug)]
pub(super) struct Labels {
  store: Store<u8>,
}

impl Labels {
  /// An empty label store whose chunks hold `chunk` bytes.
  pub(super) fn new(chunk: usize) -> Labels {
    Labels {
      store: Store::new(chunk),
    }
  }

  /// The store's runs reused, bumped and chunks added.
  pub(super) fn counts(&self) -> (u64, u64, u64) {
    self.store.counts()
  }

  /// The store's bytes, exactly.
  pub(super) fn bytes(&self) -> usize {
    self.store.bytes()
  }

  /// The bytes of `label`.
  pub(super) fn get<'a>(&'a self, label: &'a Label) -> &'a [u8] {
    match label {
      Label::Inline { len, bytes } => bytes.get(..usize::from(*len)).unwrap_or_default(),
      Label::Run(run) => self.store.get(*run),
    }
  }

  /// A new label holding `bytes`.
  pub(super) fn alloc(&mut self, bytes: &[u8]) -> Label {
    inline(bytes).unwrap_or_else(|| Label::Run(self.store.alloc(bytes)))
  }

  /// A new label holding `label`'s bytes from `skip` on, `label` left as it was.
  pub(super) fn duplicate(&mut self, label: Label, skip: usize) -> Label {
    match label {
      Label::Inline { .. } => {
        inline(self.get(&label).get(skip..).unwrap_or_default()).unwrap_or(Label::EMPTY)
      }
      Label::Run(run) => {
        let rest = self.store.get(run).get(skip..).unwrap_or_default();
        inline(rest).unwrap_or_else(|| Label::Run(self.store.duplicate(run, skip)))
      }
    }
  }

  /// A new label holding `label`'s first `len` bytes, `label` left as it was.
  pub(super) fn prefix(&mut self, label: Label, len: usize) -> Label {
    match label {
      Label::Inline { .. } => {
        inline(self.get(&label).get(..len).unwrap_or_default()).unwrap_or(Label::EMPTY)
      }
      Label::Run(run) => {
        let head = self.store.get(run).get(..len).unwrap_or_default();
        inline(head).unwrap_or_else(|| Label::Run(self.store.prefix(run, len)))
      }
    }
  }

  /// `label` without its first `skip` bytes: a new label, `label` freed.
  pub(super) fn suffix(&mut self, label: Label, skip: usize) -> Label {
    let tail = self.duplicate(label, skip);
    self.free(label);
    tail
  }

  /// A new label holding `first`'s bytes then `second`'s; both freed.
  pub(super) fn joined(&mut self, first: Label, second: Label) -> Label {
    let total = first.len().saturating_add(second.len());
    if total <= INLINE {
      let mut kept = [0u8; INLINE];
      let a = first.len();
      if let Some(head) = kept.get_mut(..a) {
        head.copy_from_slice(self.get(&first));
      }
      if let Some(tail) = kept.get_mut(a..total) {
        tail.copy_from_slice(self.get(&second));
      }
      self.free(first);
      self.free(second);
      return inline(kept.get(..total).unwrap_or_default()).unwrap_or(Label::EMPTY);
    }
    match (first, second) {
      (Label::Run(a), Label::Run(b)) => Label::Run(self.store.joined(a, b)),
      (Label::Inline { len, bytes }, Label::Run(b)) => Label::Run(
        self
          .store
          .prepended(bytes.get(..usize::from(len)).unwrap_or_default(), b),
      ),
      (Label::Run(a), Label::Inline { len, bytes }) => Label::Run(
        self
          .store
          .appended(a, bytes.get(..usize::from(len)).unwrap_or_default()),
      ),
      (Label::Inline { .. }, Label::Inline { .. }) => {
        let mut both = [0u8; 2 * INLINE];
        let a = first.len();
        if let Some(head) = both.get_mut(..a) {
          head.copy_from_slice(self.get(&first));
        }
        if let Some(tail) = both.get_mut(a..total) {
          tail.copy_from_slice(self.get(&second));
        }
        Label::Run(self.store.alloc(both.get(..total).unwrap_or_default()))
      }
    }
  }

  /// A new label holding `bytes` then `label`'s; `label` freed.
  pub(super) fn prepended(&mut self, bytes: &[u8], label: Label) -> Label {
    let total = bytes.len().saturating_add(label.len());
    if total <= INLINE {
      let mut kept = [0u8; INLINE];
      if let Some(head) = kept.get_mut(..bytes.len()) {
        head.copy_from_slice(bytes);
      }
      if let Some(tail) = kept.get_mut(bytes.len()..total) {
        tail.copy_from_slice(self.get(&label));
      }
      self.free(label);
      return inline(kept.get(..total).unwrap_or_default()).unwrap_or(Label::EMPTY);
    }
    match label {
      Label::Run(run) => Label::Run(self.store.prepended(bytes, run)),
      Label::Inline { len, bytes: tail } => {
        let head = self.store.alloc(bytes);
        Label::Run(
          self
            .store
            .appended(head, tail.get(..usize::from(len)).unwrap_or_default()),
        )
      }
    }
  }

  /// Gives `label`'s run back to the store, where it is one.
  pub(super) fn free(&mut self, label: Label) {
    if let Label::Run(run) = label {
      self.store.free(run);
    }
  }
}
