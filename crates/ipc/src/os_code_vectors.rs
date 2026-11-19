//! The wire form of a host's refusal code (A-539): an errno rides in the errno variants (`HostAnswer::Errno`,
//! `LandingDegradation::Crashed`, `TargetRefusal::Host`, `Refusal::BaseUnavailable`) and a Win32 error code in the Win32
//! variants appended beside them, so a reader knows the numbering of every number it is given. The bytes of each are
//! pinned here against `os_code_vectors.txt`, one line a value, for a number both numberings name; a layout change fails
//! `the_os_code_wire_forms_are_their_golden_vectors` until it is republished deliberately (`--ignored
//! publish_the_os_code_vectors`); a normal run never touches the tree.

use slates_wire::Wire;

use crate::protocol::{HostAnswer, LandingDegradation, Refusal, TargetRefusal};

/// Format: the published vectors.
const PUBLISHED: &str = include_str!("os_code_vectors.txt");

/// Format: the published file's preamble, which the writer writes and the test compares too.
const PREAMBLE: &str = "\
# The wire form of a host's refusal code (A-539): an errno in the errno variants, a Win32 error code in the Win32
# variants, for numbers both numberings name. A line is `<value> <hex>`, the value as the test builds it. Republish
# deliberately after a layout change:
#   cargo test -p slates-ipc --lib -- --ignored publish_the_os_code_vectors
";

/// Shape: 17, `EEXIST` as an errno and `ERROR_NOT_SAME_DEVICE` as a Win32 code.
const SEVENTEEN: i32 = 17;
/// Shape: see `SEVENTEEN`.
const SEVENTEEN_WIN32: u32 = 17;
/// Shape: 5, `EIO` as an errno and `ERROR_ACCESS_DENIED` as a Win32 code.
const FIVE: i32 = 5;
/// Shape: see `FIVE`.
const FIVE_WIN32: u32 = 5;
/// Shape: 2, `ENOENT` as an errno and `ERROR_FILE_NOT_FOUND` as a Win32 code.
const TWO: i32 = 2;
/// Shape: see `TWO`.
const TWO_WIN32: u32 = 2;
/// Shape: a base directory's path.
const BASE: &str = "/base";

/// Each value with its bytes, in a fixed order.
fn vectors() -> Vec<(&'static str, Vec<u8>)> {
  vec![
    (
      "HostAnswer::Errno{errno:17}",
      HostAnswer::Errno { errno: SEVENTEEN }.to_bytes(),
    ),
    (
      "HostAnswer::Win32{code:17}",
      HostAnswer::Win32 {
        code: SEVENTEEN_WIN32,
      }
      .to_bytes(),
    ),
    (
      "LandingDegradation::Crashed{errno:5}",
      LandingDegradation::Crashed { errno: FIVE }.to_bytes(),
    ),
    (
      "LandingDegradation::CrashedWin32{code:5}",
      LandingDegradation::CrashedWin32 { code: FIVE_WIN32 }.to_bytes(),
    ),
    (
      "TargetRefusal::Host{errno:5}",
      TargetRefusal::Host { errno: FIVE }.to_bytes(),
    ),
    (
      "TargetRefusal::HostWin32{code:5}",
      TargetRefusal::HostWin32 { code: FIVE_WIN32 }.to_bytes(),
    ),
    (
      "Refusal::BaseUnavailable{path:/base,errno:2}",
      Refusal::BaseUnavailable {
        path: BASE.to_owned(),
        errno: TWO,
      }
      .to_bytes(),
    ),
    (
      "Refusal::BaseUnavailableWin32{path:/base,code:2}",
      Refusal::BaseUnavailableWin32 {
        path: BASE.to_owned(),
        code: TWO_WIN32,
      }
      .to_bytes(),
    ),
  ]
}

fn hex(bytes: &[u8]) -> String {
  bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// The vectors' text as the encoder writes them now.
fn vector_text() -> String {
  let mut text = String::from(PREAMBLE);
  for (value, bytes) in vectors() {
    text.push_str(&format!("{value} {}\n", hex(&bytes)));
  }
  text
}

/// A-539. Do: encode each errno variant and each Win32 variant beside it for a number both numberings name, and compare
/// the bytes with the published vectors, line by line. Expect every line equal: each kind has its own tag, so the same
/// number in the two numberings never encodes the same; a change to the encoding fails here until it is republished.
#[test]
fn the_os_code_wire_forms_are_their_golden_vectors() {
  let now = vector_text();
  for (number, (published, encoded)) in PUBLISHED.lines().zip(now.lines()).enumerate() {
    assert_eq!(
      published,
      encoded,
      "line {} of os_code_vectors.txt moved; if the layout changed on purpose, republish: cargo test -p slates-ipc --lib -- --ignored publish_the_os_code_vectors",
      number + 1
    );
  }
  assert_eq!(
    PUBLISHED.lines().count(),
    now.lines().count(),
    "the vectors hold another number of lines than the encoder writes"
  );
}

/// The deliberate writer: rewrites `os_code_vectors.txt` from the encoder. Ignored, so a normal run never touches the
/// tree; run it with `--ignored` after a layout change that is meant, and review the lines it moves.
#[test]
#[ignore = "rewrites crates/ipc/src/os_code_vectors.txt from the encoder; run deliberately with --ignored"]
fn publish_the_os_code_vectors() {
  let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/os_code_vectors.txt");
  // The design's `--ignored` writer rewriting a tracked file in the repository (CLAUDE.md §4 "Doc-truth tests");
  // shipped code never reaches this call.
  #[allow(clippy::disallowed_methods)]
  std::fs::write(path, vector_text()).unwrap();
}

/// A-539 (hostile input). Do: decode each vector's bytes, and each cut short by one byte. Expect each whole one back as
/// the value it was, the errno and the Win32 code of one number two values, and every cut one refused.
#[test]
fn each_wire_form_decodes_to_itself_and_a_cut_one_is_refused() {
  fn round<T: Wire + PartialEq + std::fmt::Debug>(value: &T) {
    let bytes = value.to_bytes();
    assert_eq!(&T::decode(&mut bytes.as_slice()).unwrap(), value);
    let cut = &bytes[..bytes.len() - 1];
    assert!(T::decode(&mut &cut[..]).is_err(), "{value:?} cut short");
  }
  round(&HostAnswer::Errno { errno: SEVENTEEN });
  round(&HostAnswer::Win32 {
    code: SEVENTEEN_WIN32,
  });
  round(&LandingDegradation::Crashed { errno: FIVE });
  round(&LandingDegradation::CrashedWin32 { code: FIVE_WIN32 });
  round(&TargetRefusal::Host { errno: FIVE });
  round(&TargetRefusal::HostWin32 { code: FIVE_WIN32 });
  round(&Refusal::BaseUnavailable {
    path: BASE.to_owned(),
    errno: TWO,
  });
  round(&Refusal::BaseUnavailableWin32 {
    path: BASE.to_owned(),
    code: TWO_WIN32,
  });
  assert_ne!(
    HostAnswer::Errno { errno: SEVENTEEN }.to_bytes(),
    HostAnswer::Win32 {
      code: SEVENTEEN_WIN32
    }
    .to_bytes()
  );
}
