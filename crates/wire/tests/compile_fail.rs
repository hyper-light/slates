//! The derive's refusals are compile errors with the reason spelled out (trybuild).

/// Format: this crate's directory, fixed when the test is compiled — the same value cargo exports as
/// `CARGO_MANIFEST_DIR` when it runs the test.
const MANIFEST_DIR: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn the_derive_refuses_what_has_no_canonical_encoding() {
  // trybuild locates the crate under test through the `CARGO_MANIFEST_DIR` variable, and without it walks
  // up from the working directory to the first `Cargo.toml` — the workspace root when the test binary is
  // run directly rather than through `cargo test`, which builds its scratch project against the wrong
  // manifest and fails. Naming the crate explicitly makes the test independent of how it is launched.
  // SAFETY: this binary holds this one test, and nothing else in the process reads or writes the
  // environment concurrently; the value is the one cargo itself would have exported.
  unsafe { std::env::set_var("CARGO_MANIFEST_DIR", MANIFEST_DIR) };
  // trybuild also asks `cargo metadata` about the working directory, so run from the crate (as `cargo
  // test` does), whatever directory the binary was started in.
  std::env::set_current_dir(MANIFEST_DIR)
    .expect("this crate's directory exists: it was compiled from it");
  let t = trybuild::TestCases::new();
  t.compile_fail(format!("{MANIFEST_DIR}/tests/ui/*.rs"));
}
