//! The release tag grammar (Appendix B.2, A-210; `docs/publish.md`, "The binary release"), shared by the version
//! guard (`version.rs`, in the `xtask` binary) and the release lane's tasks (`release.rs`, in the `release` binary).
//! A tag is a release (`v` + the workspace version) or a test of the release lane (`v` + the version + `-test.N`).
//! Pure, so it builds into both binaries, and the `release` binary builds on Windows, where the rest of `xtask` does
//! not (its conformance and KIND tasks reach Unix-only calls).

/// Format: the suffix that makes a tag a test of the release lane rather than a release: `v0.1.0-test.3`.
const TEST_TAG_INFIX: &str = "-test.";

/// What a tag names.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TagKind {
  /// A release of the workspace version.
  Release,
  /// A test of the release lane at the workspace version: its draft is a pre-release titled as a test.
  Test {
    /// The test's number (`v0.1.0-test.3` is 3); each test run takes a new one.
    number: u64,
  },
}

/// What `tag` names when the workspace version is `version`: `v` + the version is a release, `v` + the version +
/// `-test.N` a test of the lane; anything else is refused with the forms it would accept.
pub(crate) fn tag_kind(tag: &str, version: &str) -> Result<TagKind, String> {
  let release = format!("v{version}");
  if tag == release {
    return Ok(TagKind::Release);
  }
  let number = tag
    .strip_prefix(&release)
    .and_then(|rest| rest.strip_prefix(TEST_TAG_INFIX))
    .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
    .and_then(|digits| digits.parse::<u64>().ok());
  match number {
    Some(number) => Ok(TagKind::Test { number }),
    None => Err(format!(
      "tag `{tag}` does not name the workspace version: a release is `{release}` and a test of the release lane \
       `{release}{TEST_TAG_INFIX}N`; bump `[workspace.package] version`, run `cargo xtask version --write`, commit, \
       then tag"
    )),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// Do name a tag by the workspace version, expect a release; with `-test.N`, expect test N; expect every other
  /// form refused (another version, a test with no number or a signed one, a stray suffix, no `v`).
  #[test]
  fn a_tag_is_a_release_or_a_numbered_test_of_the_workspace_version() {
    assert_eq!(tag_kind("v0.1.0", "0.1.0").ok(), Some(TagKind::Release));
    assert_eq!(
      tag_kind("v0.1.0-test.3", "0.1.0").ok(),
      Some(TagKind::Test { number: 3 })
    );
    assert_eq!(
      tag_kind("v0.2.0-rc.1-test.12", "0.2.0-rc.1").ok(),
      Some(TagKind::Test { number: 12 })
    );
    for refused in [
      "v0.1.1",
      "0.1.0",
      "v0.1.0-test.",
      "v0.1.0-test.-1",
      "v0.1.0-test.1a",
      "v0.1.0-rc.1",
      "v0.1.0-test",
      "v0.1.0.test.1",
    ] {
      assert!(
        tag_kind(refused, "0.1.0").is_err(),
        "{refused} must be refused"
      );
    }
  }
}
