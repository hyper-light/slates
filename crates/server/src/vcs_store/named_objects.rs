//! A green's named content read from the shard's content plane (A-760; §4.17.1 "A push's intake"; A-761: Subversion's
//! intake writes each text once into the plane, durable before its revision's entry).
//!
//! **What.** [`PlaneObjects`] is the engine's [`Objects`] over vcs_store's content store: an object found by its
//! identity's hash (`ContentStore::root_of`: the node-local root it is kept under), and read straight into the run's
//! buffer a window's range at a time (`ContentStore::window_range_in`), so a read copies its own bytes and nothing more.
//! A recovery's replay holds the shard read-only, so its chunk store, which a step lends the content store, is given in
//! its place (`kept_in`: the windows are read from it where the store holds none lent).

use slates_merge::named::{Identity, ObjectMissing, Objects};
use slates_vcs::{ContentId, ContentStore};

/// The engine's named objects over the content plane (module docs).
pub(crate) struct PlaneObjects<'a> {
  /// The content store.
  pub(crate) store: &'a ContentStore,
  /// What its windows are read from where none is lent to it now: the shard's chunk store, or nothing (`&()`).
  pub(crate) kept_in: &'a dyn std::any::Any,
}

impl Objects for PlaneObjects<'_> {
  fn read(&self, identity: &Identity, at: u64, into: &mut [u8]) -> Result<(), ObjectMissing> {
    let missing = ObjectMissing {
      identity: *identity,
    };
    let wanted = u64::try_from(into.len()).map_err(|_| missing)?;
    if at.checked_add(wanted).is_none_or(|end| end > identity.len) {
      return Err(missing);
    }
    let root = self.store.root_of(&ContentId(identity.hash));
    let window = u64::try_from(self.store.window())
      .ok()
      .filter(|window| *window > 0)
      .ok_or(missing)?;
    let mut done = 0usize;
    while done < into.len() {
      let offset = at.saturating_add(u64::try_from(done).map_err(|_| missing)?);
      let index = offset.checked_div(window).ok_or(missing)?;
      let skip = offset.checked_rem(window).ok_or(missing)?;
      // The rest of this window, at most what is left to read: read straight into the run's buffer.
      let in_window = usize::try_from(window.saturating_sub(skip)).map_err(|_| missing)?;
      let room = into.get_mut(done..).ok_or(missing)?;
      let take = in_window.min(room.len());
      let piece = room.get_mut(..take).ok_or(missing)?;
      let skip = usize::try_from(skip).map_err(|_| missing)?;
      self
        .store
        .window_range_in((&root, identity.len, index), (skip, piece), self.kept_in)
        .ok_or(missing)?;
      done = done.saturating_add(take);
    }
    Ok(())
  }
}

#[cfg(test)]
mod tests {
  #![allow(clippy::unwrap_used)]

  use slates_merge::engine::{Green, Increment, Judgment};
  use slates_merge::increment::{Base, VolumeOp, compose_volume_planned};
  use slates_merge::named::{Identity, NamedRun};

  use super::PlaneObjects;
  use crate::state::ShardState;

  /// Shape: the text's length: the 500 MB commit's shape scaled to a focused test, many copy slices long.
  const TEXT: usize = 8 << 20;

  /// The text, put into the shard's content plane as an intake streams it (`ContentStore::put_window`), and its identity.
  fn put_text(state: &mut ShardState, bytes: &[u8]) -> Identity {
    let ShardState {
      store, vcs_stores, ..
    } = state;
    // Lent the room the text brings, as a step's puts are (`vcs_store::lend`), then settled.
    crate::vcs_store::lend(store, vcs_stores, bytes.len() as u64 * 2);
    let id = {
      let lent = crate::vcs_store::Lent::new(&mut *store, vcs_stores);
      let content = lent.stores.content_mut().unwrap();
      let mut writing = content.begin_object();
      for piece in bytes.chunks(97 * 1024) {
        content.put_window(&mut writing, piece).unwrap();
      }
      content.finish_object(&mut writing).unwrap()
    };
    crate::vcs_store::settle_in(store, vcs_stores);
    Identity {
      hash: id.0,
      len: bytes.len() as u64,
    }
  }

  /// A work creating `path` from the whole of `object`, named.
  fn named_create(path: &str, object: Identity) -> Increment {
    let journal = vec![
      VolumeOp::Create {
        path: path.to_owned(),
      },
      VolumeOp::Extend {
        path: path.to_owned(),
        at: 0,
        len: object.len,
      },
    ];
    let (doc, _) = compose_volume_planned(&Base::of_files(Vec::new()), &journal).unwrap();
    let mut work = Increment {
      id: [5; 32],
      base: 0,
      doc,
      post_state: Vec::new(),
      evidence: Vec::new(),
      named: Default::default(),
    };
    work.push_named(NamedRun {
      identity: object,
      offset: 0,
      len: object.len,
    });
    work
  }

  /// A-760 part 2 (main's red: the 500 MB Subversion text's landing, scaled; Ada's sizing rule). Do: put an 8 MiB text
  /// into the shard's content plane; land a work creating a file of it, named, its run read a turn at a time (each turn
  /// lent the store, its copy slice at the shard's measured memcpy rate), then judged with `judge_named` and committed.
  /// Expect: more turns than the text has slices less one; no turn read more than its slice, nor ran past its budget by
  /// more than one turn's read; the green's file the text's bytes, made of the very buffer the turns read into (one copy
  /// of the text beside the plane, never two); and the same bytes read with the shard's chunk store given in place of a
  /// lent one (a recovery's replay).
  #[test]
  fn t_a760_a_large_named_text_lands_a_slice_a_turn_held_once() {
    crate::daemon::audit_on_shard(|state| {
      let _repository = crate::git_push::tests::git_repository(state, "texts");
      let bytes: Vec<u8> = (0..TEXT).map(|at| (at % 239) as u8).collect();
      let object = put_text(state, &bytes);
      let slice = state.config.copy_slice_bytes().max(1);
      let budget = state.config.step_quantum_ns() / 2;
      let mut work = named_create("big", object);
      let (mut turns, mut largest, mut longest) = (0u64, 0usize, 0u64);
      loop {
        turns += 1;
        let before = work.read_run(0).map_or(0, <[u8]>::len);
        let began = slates_rt::futures::now_ns();
        let done = crate::vcs_store::lent_state(state, |state| {
          let objects = PlaneObjects {
            store: state.vcs_stores.content().unwrap(),
            kept_in: &(),
          };
          work.read_step(&objects, slice).unwrap()
        });
        longest = longest.max(slates_rt::futures::now_ns().saturating_sub(began));
        largest = largest.max(work.read_run(0).unwrap().len() - before);
        if done {
          break;
        }
      }
      let buffer = work.read_run(0).unwrap().as_ptr();
      let mut green = Green::new();
      let Judgment::Accepted(judged) = green.judge_named(&mut work) else {
        panic!("not accepted");
      };
      green.commit_judged(judged).unwrap();
      let file = green.content("big").unwrap();
      let replayed = {
        let mut again = named_create("big", object);
        let objects = PlaneObjects {
          store: state.vcs_stores.content().unwrap(),
          kept_in: &state.store.content,
        };
        while !again.read_step(&objects, slice).unwrap() {}
        again.read_run(0) == Some(bytes.as_slice())
      };
      eprintln!(
        "A-760 {TEXT} bytes: {turns} turns of a {slice}-byte slice, the largest read {largest}, the longest turn \
         {longest} ns against a {budget} ns budget"
      );
      assert!(turns >= (TEXT as u64).div_ceil(slice), "{turns} turns");
      assert!(largest as u64 <= slice, "a turn read {largest} bytes");
      assert_eq!(file, bytes.as_slice());
      assert_eq!(
        file.as_ptr(),
        buffer,
        "the file is a second copy of the text"
      );
      assert!(replayed, "the replay's read differs");
    });
  }
}
