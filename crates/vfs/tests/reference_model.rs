//! An oracle for the per-attachment reference accounting (§3 of the inode-addressed-io design): a
//! tiny serial model of "an inode is alive iff it has a link or any attachment references it, and a
//! teardown sweep releases exactly one attachment's share" is driven through the same generated
//! histories as the real volume, and their aliveness is compared after every step. This exercises
//! the multi-attachment interactions a few hand-written cases cannot: a sweep must never reclaim an
//! inode another attachment holds (a corruption), a forget must drop only its own owner's share, and
//! a reference of a reclaimed inode must be refused. The model states the design's rule, not the
//! implementation's mechanics, so it certifies behaviour rather than mirrors code.

// Test harness code: an unwrap here is a failed test. proptest's strategy macros expand to
// `Arc`-carrying unions; the test-harness exception of D-8 covers it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::disallowed_types)]

use std::collections::BTreeMap;

use proptest::prelude::*;
use slates_vfs::ids::InodeNo;

mod common;
use common::{store, volume};

/// The fixed set of files and attachments the history plays over — small, so a generated sequence
/// densely exercises the reference interactions between them rather than spreading thin.
const FILES: usize = 4;
const ATTACHMENTS: u64 = 3;

/// One step of a generated history over the reference API.
#[derive(Clone, Debug)]
enum Op {
  /// Attachment `att` takes one reference on file `file`.
  Reference { file: usize, att: u64 },
  /// Attachment `att` drops up to `n` of its references on file `file`.
  Forget { file: usize, att: u64, n: u64 },
  /// Attachment `att` tears down: every reference it holds is swept.
  Sweep { att: u64 },
  /// The file's one link is removed from the namespace.
  Unlink { file: usize },
}

fn op_strategy() -> impl Strategy<Value = Op> {
  prop_oneof![
    (0..FILES, 0..ATTACHMENTS).prop_map(|(file, att)| Op::Reference { file, att }),
    (0..FILES, 0..ATTACHMENTS, 0u64..4).prop_map(|(file, att, n)| Op::Forget { file, att, n }),
    (0..ATTACHMENTS).prop_map(|att| Op::Sweep { att }),
    (0..FILES).prop_map(|file| Op::Unlink { file }),
  ]
}

/// The serial model: each file's link count and each attachment's reference count per file.
struct Model {
  /// Links per file (each created file starts linked; `unlink` drops it to zero).
  nlink: [u32; FILES],
  /// Per-attachment references: `(attachment, file) -> count`.
  refs: BTreeMap<(u64, usize), u32>,
}

impl Model {
  /// The design's aliveness rule: an inode is alive iff it has a link OR some attachment references
  /// it. Content and table entry are reclaimed only when both reach zero.
  fn alive(&self, file: usize) -> bool {
    self.nlink[file] > 0
      || (0..ATTACHMENTS).any(|att| *self.refs.get(&(att, file)).unwrap_or(&0) > 0)
  }
}

proptest! {
  #![proptest_config(ProptestConfig { cases: 400, ..ProptestConfig::default() })]

  /// AC-1.7-style oracle: the volume's per-attachment references and teardown sweep match the serial
  /// model on every generated history — aliveness agrees after every step, a reference succeeds iff
  /// the inode is alive, and an unlink succeeds iff the name is present.
  #[test]
  fn per_attachment_references_and_sweep_match_the_model(
    ops in prop::collection::vec(op_strategy(), 0..80)
  ) {
    let mut store = store();
    let mut vol = volume(&mut store, 1 << 30);
    let root = vol.root_inode(&store).unwrap();
    // Create the fixed files up front; keep their (never-reused, D-4) inode numbers.
    let inodes: Vec<InodeNo> = (0..FILES)
      .map(|i| vol.create_file_no(&mut store, root, &format!("f{i}"), 0o644).unwrap())
      .collect();
    let mut model = Model {
      nlink: [1; FILES],
      refs: BTreeMap::new(),
    };

    for op in ops {
      match op {
        Op::Reference { file, att } => {
          let result = vol.reference_for(&store, inodes[file], att);
          if model.alive(file) {
            prop_assert!(result.is_ok(), "a reference of a live inode must succeed");
            *model.refs.entry((att, file)).or_insert(0) += 1;
          } else {
            prop_assert!(result.is_err(), "a reference of a reclaimed inode must be refused");
          }
        }
        Op::Forget { file, att, n } => {
          // A forget is always accepted (a drop of more than held, or of nothing, is a no-op); it
          // removes at most this attachment's own share.
          vol.forget_for(&mut store, inodes[file], att, n).unwrap();
          let held = model.refs.entry((att, file)).or_insert(0);
          let drop = u32::try_from(n).unwrap_or(u32::MAX).min(*held);
          *held -= drop;
        }
        Op::Sweep { att } => {
          vol.sweep_attachment(&mut store, att).unwrap();
          for file in 0..FILES {
            model.refs.remove(&(att, file));
          }
        }
        Op::Unlink { file } => {
          let result = vol.unlink_no(&mut store, root, &format!("f{file}"));
          if model.nlink[file] > 0 {
            prop_assert!(result.is_ok(), "an unlink of a present name must succeed");
            model.nlink[file] -= 1;
          } else {
            prop_assert!(result.is_err(), "an unlink of an absent name must be refused");
          }
        }
      }

      // The oracle: every file's aliveness in the volume matches the model after every step. A
      // sweep that reclaimed an inode another attachment still holds, or a forget that dropped
      // another owner's share, would show here as a live-in-model / dead-in-volume divergence.
      for (file, &ino) in inodes.iter().enumerate() {
        prop_assert_eq!(
          vol.stat(&store, ino).is_ok(),
          model.alive(file),
          "file {} aliveness diverged from the model",
          file
        );
      }
    }
  }
}
