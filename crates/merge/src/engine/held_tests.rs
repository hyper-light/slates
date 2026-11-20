//! A-716's tests: the green's identity table ([`crate::held`]) holds a text by path, sharing the file's block, or by
//! bytes with no path; the held text survives the file's change and removal; a hold past the room is refused typed;
//! and a release lets the text go.

use super::{Green, Increment, Outcome};
use crate::held::HeldRefused;
use crate::ops_doc::{Op, OpKind, OpsDoc};

/// Builds an increment as the deriver lays one out: paths interned, bytes in the post-state named by offset.
#[derive(Default)]
struct Work {
  doc: OpsDoc,
  post: Vec<u8>,
}

impl Work {
  fn push(&mut self, kind: OpKind, path: &str, at: u64, len: u64, src: u64) -> &mut Work {
    let path = self.doc.paths.intern(path);
    self.doc.ops.push(Op {
      kind,
      flags: 0,
      path,
      at,
      len,
      src,
    });
    self
  }

  fn create(&mut self, path: &str, bytes: &[u8]) -> &mut Work {
    self.push(OpKind::Create, path, 0, 0, u64::MAX);
    let src = u64::try_from(self.post.len()).unwrap();
    self.post.extend_from_slice(bytes);
    self.push(
      OpKind::Insert,
      path,
      0,
      u64::try_from(bytes.len()).unwrap(),
      src,
    )
  }

  fn overwrite(&mut self, path: &str, bytes: &[u8]) -> &mut Work {
    let src = u64::try_from(self.post.len()).unwrap();
    self.post.extend_from_slice(bytes);
    self.push(
      OpKind::Overwrite,
      path,
      0,
      u64::try_from(bytes.len()).unwrap(),
      src,
    )
  }

  fn unlink(&mut self, path: &str) -> &mut Work {
    self.push(OpKind::Unlink, path, 0, 0, u64::MAX)
  }

  fn at(&self, id: u8, base: u64) -> Increment {
    let mut identity = [0u8; 32];
    identity[0] = id;
    Increment {
      id: identity,
      base,
      doc: self.doc.clone(),
      post_state: self.post.clone(),
      evidence: Vec::new(),
      named: Default::default(),
    }
  }
}

/// A content identity a test names.
fn identity(tag: u8) -> [u8; 32] {
  [tag; 32]
}

/// A-716. Do: on a green with room for two texts, create `a` with "one" and hold it by path; overwrite `a` with "two";
/// then remove `a`. Expect: the held text reads "one" after the hold, after the overwrite (the file reads "two") and
/// after the removal; the hold of a path that holds no file is refused `NoFile`; one entry held.
#[test]
fn a_text_held_by_path_outlives_its_files_change_and_removal() {
  let mut green = Green::new();
  green.set_held_room(2);
  let made = green.submit(&Work::default().create("a", b"one").at(1, 0));
  assert_eq!(made, Outcome::Accepted { version: 1 });
  green.hold_path(identity(1), "a").unwrap();
  assert_eq!(green.held(&identity(1)), Some(&b"one"[..]));
  let changed = green.submit(&Work::default().overwrite("a", b"two").at(2, 1));
  assert_eq!(changed, Outcome::Accepted { version: 2 });
  assert_eq!(
    (green.content("a"), green.held(&identity(1))),
    (Some(&b"two"[..]), Some(&b"one"[..]))
  );
  let removed = green.submit(&Work::default().unlink("a").at(3, 2));
  assert_eq!(removed, Outcome::Accepted { version: 3 });
  assert_eq!(green.held(&identity(1)), Some(&b"one"[..]));
  assert_eq!(green.hold_path(identity(2), "a"), Err(HeldRefused::NoFile));
  assert_eq!(green.held_count(), 1);
}

/// A-716 (an intake's object, held before any tree names it). Do: hold "blob" under one identity by bytes, then hold
/// the same identity again by bytes and by path over a file of other bytes; release it once, then again, then a third
/// time. Expect: the text reads "blob" throughout, kept once (one entry); the first release keeps it (another holder
/// remains), the next lets it go, and a release of what is not held is refused `NotHeld`.
#[test]
fn a_text_held_by_bytes_is_kept_once_and_let_go_with_its_last_holder() {
  let mut green = Green::new();
  green.set_held_room(1);
  green.submit(&Work::default().create("f", b"other").at(1, 0));
  green.hold_bytes(identity(7), b"blob").unwrap();
  green.hold_bytes(identity(7), b"blob").unwrap();
  green.hold_path(identity(7), "f").unwrap();
  assert_eq!(
    (green.held(&identity(7)), green.held_count()),
    (Some(&b"blob"[..]), 1)
  );
  green.release_held(&identity(7)).unwrap();
  green.release_held(&identity(7)).unwrap();
  assert_eq!(green.held(&identity(7)), Some(&b"blob"[..]));
  green.release_held(&identity(7)).unwrap();
  assert_eq!(green.held(&identity(7)), None);
  assert_eq!(green.release_held(&identity(7)), Err(HeldRefused::NotHeld));
  assert_eq!(green.content("f"), Some(&b"other"[..]));
}

/// A-716 (the table's bound). Do: hold texts by bytes on a green whose room is never set, then on one with room for
/// two: three identities. Expect: with no room set every hold is refused `AtRoom { room: 0 }`; with room for two the
/// third is refused `AtRoom { room: 2 }` and nothing of it is held, while another hold of a held identity still
/// succeeds.
#[test]
fn a_hold_past_the_tables_room_is_refused_and_holds_nothing() {
  let mut green = Green::new();
  assert_eq!(
    green.hold_bytes(identity(1), b"x"),
    Err(HeldRefused::AtRoom { room: 0 })
  );
  green.set_held_room(2);
  green.hold_bytes(identity(1), b"x").unwrap();
  green.hold_bytes(identity(2), b"y").unwrap();
  assert_eq!(
    green.hold_bytes(identity(3), b"z"),
    Err(HeldRefused::AtRoom { room: 2 })
  );
  assert_eq!(green.held(&identity(3)), None);
  green.hold_bytes(identity(1), b"x").unwrap();
  assert_eq!(green.held_count(), 2);
}

/// A-716 (a text staged a piece at a time: §4.17.1 "A push's intake", rule 1). Do: stage "abc" in three pieces and seal
/// it; stage "abc" again in two and seal it under the same identity; stage a text and discard it; stage one past the
/// table's room and seal it. Expect: the first held whole, three bytes staged; the second adds a holder and gives its
/// block back (the blocks alive as after the first); the discarded one leaves no block; the one past the room is
/// refused `AtRoom` and leaves no block either.
#[test]
fn a_text_staged_a_piece_at_a_time_is_held_once_or_given_back() {
  let mut green = Green::new();
  green.set_held_room(1);
  let mut staged = green.stage();
  for piece in [&b"a"[..], b"b", b"c"] {
    green.append_staged(&mut staged, piece);
  }
  assert_eq!(staged.length(), 3);
  green.seal_staged(staged, identity(1)).unwrap();
  assert_eq!(green.held(&identity(1)), Some(&b"abc"[..]));
  let alive = green.content.blocks().live();
  let mut again = green.stage();
  green.append_staged(&mut again, b"ab");
  green.append_staged(&mut again, b"c");
  green.seal_staged(again, identity(1)).unwrap();
  let dropped = green.stage();
  green.discard_staged(dropped);
  assert_eq!(green.content.blocks().live(), alive);
  let mut past = green.stage();
  green.append_staged(&mut past, b"z");
  assert_eq!(
    green.seal_staged(past, identity(2)),
    Err(HeldRefused::AtRoom { room: 1 })
  );
  assert_eq!(
    (green.content.blocks().live(), green.held(&identity(2))),
    (alive, None)
  );
  green.release_held(&identity(1)).unwrap();
  assert_eq!(green.held(&identity(1)), Some(&b"abc"[..]));
}
