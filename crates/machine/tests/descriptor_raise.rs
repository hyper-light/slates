//! A-837 (the descriptor backstop, platform half): a daemon raises its soft `RLIMIT_NOFILE` at start to the most the
//! platform allows (`slates_machine::facts::raise_descriptor_limit`). The test re-invokes its own binary as a child
//! that lowers its soft limit and then raises it, so the parent's own limit is never lowered under the other tests.
// Test harness code: an unwrap here is a failed test, which is what it should be.
#![allow(
  clippy::unwrap_used,
  clippy::expect_used,
  clippy::panic,
  clippy::indexing_slicing,
  clippy::string_slice
)]

/// Format: the variable that makes the re-invoked binary the child.
#[cfg(unix)]
const CHILD: &str = "SLATES_MACHINE_DESCRIPTOR_RAISE_CHILD";

/// Format: the tag the child's report line carries, found anywhere in a line (libtest's banner precedes it).
#[cfg(unix)]
const TAG: &str = "A837-RAISED";

/// Shape: the soft limit the child starts from, far under any hard limit a host gives a process.
#[cfg(unix)]
const LOWERED: u64 = 64;

/// A-837. Do: in a child process, lower the soft descriptor limit to 64 and call the start's raise. Expect: the kernel
/// accepted the raise, and the soft limit after it is the lower of the hard limit and the platform's cap (never the
/// 64 it began at), as `raised_descriptor_soft` decides; where neither binds, it stays.
#[cfg(unix)]
#[test]
fn a837_a_lowered_soft_descriptor_limit_is_raised_to_what_the_platform_allows() {
  if std::env::var_os(CHILD).is_some() {
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let hard = getrlimit(Resource::Nofile).maximum;
    setrlimit(
      Resource::Nofile,
      Rlimit {
        current: Some(LOWERED),
        maximum: hard,
      },
    )
    .unwrap();
    let raise = slates_machine::facts::raise_descriptor_limit();
    println!(
      "{TAG} {:?} {:?} {:?} {:?} {}",
      raise.before, raise.hard, raise.cap, raise.after, raise.refused
    );
    return;
  }
  let output = std::process::Command::new(std::env::current_exe().unwrap())
    .args([
      "--exact",
      "a837_a_lowered_soft_descriptor_limit_is_raised_to_what_the_platform_allows",
      "--nocapture",
    ])
    .env(CHILD, "1")
    .output()
    .unwrap();
  let stdout = String::from_utf8_lossy(&output.stdout);
  let line = stdout
    .lines()
    .find_map(|line| line.find(TAG).map(|at| &line[at..]))
    .unwrap_or_else(|| panic!("the child reported nothing: {stdout}"));
  let fields: Vec<&str> = line.split_whitespace().collect();
  let [_, before, hard, cap, after, refused] = fields[..] else {
    panic!("{line}")
  };
  let number = |field: &str| -> Option<u64> {
    field
      .strip_prefix("Some(")
      .and_then(|rest| rest.strip_suffix(')'))
      .map(|digits| digits.parse().unwrap())
  };
  assert_eq!(number(before), Some(LOWERED), "{line}");
  assert_eq!(refused, "false", "the kernel refused the raise: {line}");
  let expected =
    slates_machine::facts::raised_descriptor_soft(Some(LOWERED), number(hard), number(cap))
      .or(Some(LOWERED));
  assert_eq!(number(after), expected, "{line}");
  assert!(number(after) > Some(LOWERED), "nothing was raised: {line}");
}
