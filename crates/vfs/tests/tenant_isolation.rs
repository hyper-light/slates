//! Cross-tenant isolation as a model-based property (§4.13, AC-4.12; R1): two volumes sharing one shard's arena are
//! fully isolated — one volume's operations never change anything the other observes. The single-angle proofs
//! (counter-confinement, scoped mounts, capability-bound handles, A-99 zero-on-free) each pin one mechanism; this
//! drives a random interleaved history of creates, writes and removals across *two* volumes (distinct inode
//! prefixes, one shared `Store`) and asserts after every step that the second volume's observable state is exactly
//! what its *own* operations produced — the first volume's interleaved operations had zero effect on it. A
//! regression against any future change that let one tenant's write, truncate or reclaim reach another's bytes.

// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::indexing_slicing)]

use proptest::prelude::*;
use slates_vfs::ids::InodeNo;
use slates_vfs::volume::{Store, Volume};

mod common;
use common::{clone_config, store, volume};

/// The three names each volume's history plays over — a small set so collisions and reuse are frequent.
const NAMES: [&str; 3] = ["f", "g", "h"];

/// Sets `name` in `vol` to `bytes` (create if absent, then truncate and write), as a client's overwrite does.
fn set_bytes(store: &mut Store, vol: &mut Volume, root: InodeNo, name: &str, bytes: &[u8]) {
  let ino = match vol.lookup_no(&*store, root, name) {
    Ok(located) => located.inode,
    Err(_) => vol.create_file_no(store, root, name, 0o644).unwrap(),
  };
  vol.truncate(store, ino, 0).unwrap();
  vol.write(store, ino, 0, bytes).unwrap();
}

/// Removes `name` from `vol` if present (a client's unlink); absent is a no-op.
fn remove(store: &mut Store, vol: &mut Volume, root: InodeNo, name: &str) {
  let _ = vol.unlink_no(store, root, name);
}

/// The bytes `name` holds in `vol`, or `None` when it is absent — what a client reads back.
fn read_name(store: &Store, vol: &Volume, root: InodeNo, name: &str) -> Option<Vec<u8>> {
  let located = vol.lookup_no(store, root, name).ok()?;
  let size = usize::try_from(vol.stat(store, located.inode).unwrap().size).unwrap();
  let mut buf = vec![0u8; size];
  let read = vol.read(store, located.inode, 0, &mut buf).unwrap();
  buf.truncate(read);
  Some(buf)
}

proptest! {
  #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]

  /// A random interleaved history over two volumes never crosses: after every step the second volume reads back
  /// exactly what its own operations wrote, so the first volume's creates, writes, truncates and removals — over
  /// the same shared arena — never reach it. Each step is `(on_second, remove, index, bytes)`: `remove` picks an
  /// unlink over a set, so the history mixes creates, overwrites and removals on both volumes. (No `prop_oneof!`,
  /// which expands to a disallowed `Arc` — R2.)
  #[test]
  fn an_interleaved_history_over_two_volumes_never_crosses(
    history in proptest::collection::vec(
      (any::<bool>(), any::<bool>(), 0usize..NAMES.len(), proptest::collection::vec(any::<u8>(), 0..8)),
      1..60,
    )
  ) {
    let mut store = store();
    let mut first = volume(&mut store, 1 << 24);
    let mut second = Volume::create(&mut store, clone_config(9)).unwrap();
    let first_root = first.root_inode(&store).unwrap();
    let second_root = second.root_inode(&store).unwrap();
    // The reference model of the SECOND volume, advanced only by the second volume's own steps.
    let mut model: [Option<Vec<u8>>; NAMES.len()] = [None, None, None];

    for (on_second, remove_it, index, bytes) in history {
      let name = NAMES[index];
      {
        let (vol, root) = if on_second {
          (&mut second, second_root)
        } else {
          (&mut first, first_root)
        };
        if remove_it {
          remove(&mut store, vol, root, name);
        } else {
          set_bytes(&mut store, vol, root, name, &bytes);
        }
      }
      if on_second {
        model[index] = if remove_it { None } else { Some(bytes) };
      }
      // The second volume shows exactly its own history — the first's interleaved steps changed nothing here.
      for (i, nm) in NAMES.iter().enumerate() {
        prop_assert_eq!(
          read_name(&store, &second, second_root, nm),
          model[i].clone(),
          "the second volume's {} diverged from its own history after a step on the {} volume",
          nm,
          if on_second { "second" } else { "first" }
        );
      }
    }
  }
}
