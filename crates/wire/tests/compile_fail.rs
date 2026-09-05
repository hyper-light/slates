//! The derive's refusals are compile errors with the reason spelled out (trybuild).

#[test]
fn the_derive_refuses_what_has_no_canonical_encoding() {
  let t = trybuild::TestCases::new();
  t.compile_fail("tests/ui/*.rs");
}
