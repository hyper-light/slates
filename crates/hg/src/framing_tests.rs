//! The framing's tests: a whole changegroup framed alike from a slice and from a window filled a piece at a time.

use super::*;
use crate::changegroup::Changegroup;

/// Every golden changegroup stock 7.2 wrote, versions 01 to 04: its version and its bytes.
fn golden() -> Vec<(Version, Vec<u8>)> {
  let room = || Admission::new(u64::MAX);
  let mut out = vec![(
    Version::V01,
    crate::bundle1::changegroup(crate::fixtures::CG01)
      .unwrap()
      .to_vec(),
  )];
  for stream in [
    crate::fixtures::CG02,
    crate::fixtures::CG03,
    crate::fixtures::CG04,
  ] {
    let decoded = crate::bundle2::Bundle2::decode(stream, &mut room()).unwrap();
    let (index, part) = decoded
      .top_level()
      .find(|(_, part)| part.is(b"changegroup"))
      .unwrap();
    let version = Version::from_part_param(part.param(b"version")).unwrap();
    out.push((version, decoded.payload_bytes(index, &mut room()).unwrap()));
  }
  out
}

/// Frames `source` to its end, putting the wanted bytes in from `whole` a `piece` at a time where it is a window: the
/// framing, the wants met, and the most bytes the window held at once.
fn framed_through(version: Version, whole: &[u8], piece: usize) -> (Framed, usize, u64) {
  let mut window = Window::new(to_u64(whole.len()));
  let mut framing = Framing::new(version, false);
  let (mut wants, mut most) = (0, 0);
  let mut room = Admission::new(u64::MAX);
  let mut framed = Framed::default();
  loop {
    let mut sink = Collect {
      framed: &mut framed,
      admission: &mut room,
    };
    match framing.step(&window, &mut sink) {
      Ok(true) => {}
      Ok(false) => break,
      Err(Halt::Wanted(wanted)) => {
        wants += 1;
        window.keep_from(wanted.range.start);
        while window.end() < wanted.range.end {
          let from = usize::try_from(window.end()).unwrap();
          let to = (from + piece).min(whole.len());
          window.put(&whole[from..to], &mut room).unwrap();
        }
        most = most.max(window.held());
      }
      Err(Halt::Refused(refusal)) => panic!("{refusal:?}"),
    }
  }
  assert!(framing.done());
  (framed, wants, most)
}

/// A-767 C1 (the piece-reading intake). Do: frame each golden changegroup from its slice, and from a window filled 64
/// bytes at a time. Expect the same framing; the window wanted bytes, and never held more than a chunk past a piece.
#[test]
fn a_changegroup_frames_alike_from_a_slice_and_from_pieces() {
  for (version, whole) in golden() {
    let whole = whole.as_slice();
    let mut framing = Framing::new(version, false);
    let mut room = Admission::new(u64::MAX);
    let mut from_slice = Framed::default();
    while framing
      .step(
        whole,
        &mut Collect {
          framed: &mut from_slice,
          admission: &mut room,
        },
      )
      .unwrap()
    {}
    let (from_pieces, wants, most) = framed_through(version, whole, 64);
    assert_eq!(from_pieces, from_slice, "{version:?}");
    assert!(wants > 1, "the window wanted bytes {wants} times");
    let longest = from_slice
      .revisions
      .iter()
      .map(|placed| placed.data.end - placed.data.start)
      .max()
      .unwrap_or(0);
    let header = to_u64(version.header_len() + 2 * LENGTH_LEN);
    assert!(
      most <= longest + header + 64,
      "{version:?}: held {most} at once"
    );
  }
}

/// A-767 C1. Do: decode the fixtures' push whole, and frame it; read each placed revision back from the slice. Expect
/// every revision the decoding's, field for field and byte for byte.
#[test]
fn a_framed_revision_reads_back_as_the_whole_decoding_has_it() {
  for (version, whole) in golden() {
    let whole = whole.as_slice();
    let decoded = Changegroup::decode(whole, version, &mut Admission::new(u64::MAX)).unwrap();
    let (framed, _, _) = framed_through(version, whole, 4096);
    let read: Vec<_> = framed
      .revisions
      .iter()
      .map(|placed| placed.revision(whole).unwrap())
      .collect();
    assert_eq!(read, decoded.revisions, "{version:?}");
    assert_eq!(framed.changelog, decoded.changelog);
    assert_eq!(framed.manifests, decoded.manifests);
    assert_eq!(framed.files.len(), decoded.files.len());
  }
}

/// Hostile input (CLAUDE.md §4). Do: frame a golden changegroup cut short at every length, and with a chunk length of
/// 2. Expect each refused, never a want past the end.
#[test]
fn a_cut_or_damaged_changegroup_is_refused_never_framed_past_its_end() {
  let (version, whole) = golden().swap_remove(1);
  let whole = whole.as_slice();
  for cut in 0..whole.len() {
    let bytes = &whole[..cut];
    let mut framing = Framing::new(version, true);
    let mut room = Admission::new(u64::MAX);
    let mut framed = Framed::default();
    let ended = loop {
      let mut sink = Collect {
        framed: &mut framed,
        admission: &mut room,
      };
      match framing.step(bytes, &mut sink) {
        Ok(true) => {}
        Ok(false) => break Ok(()),
        Err(halt) => break Err(halt),
      }
    };
    assert!(
      matches!(ended, Err(Halt::Refused(_))),
      "cut at {cut}: {ended:?}"
    );
  }
  let mut damaged = whole.to_vec();
  damaged[..4].copy_from_slice(&2i32.to_be_bytes());
  let mut framing = Framing::new(version, true);
  let (mut framed, mut room) = (Framed::default(), Admission::new(u64::MAX));
  assert_eq!(
    framing.step(
      damaged.as_slice(),
      &mut Collect {
        framed: &mut framed,
        admission: &mut room,
      }
    ),
    Err(Halt::Refused(Refusal::ChunkLength { value: 2 }))
  );
}
