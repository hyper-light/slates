# The SDKs sliced a volume id through a character

Date: 2026-09-29. Scope: the volume-id parsers of the Node and Python SDKs (`parse_volume`,
`crates/sdk-node/src/lib.rs` and `crates/sdk-python/src/lib.rs`). Found by the no-panic sweep (GAPS
2026-09-29): `clippy::string_slice` flagged both.

## Symptom

A volume id that is 32 bytes long but not 32 ASCII characters crashed the SDK. Given `"a€" + "0" × 28` (32
bytes; `€` is three), both parsers passed the length check, then sliced the string at byte 2, inside the
`€`:

- **Python:** `pyo3_runtime.PanicException: end byte index 2 is not a char boundary; it is inside '€' (bytes
  1..4 of string)`, a `BaseException`, not the SDK's `SlatesError`.
- **Node:** the Rust panic took the test process down before its cleanup ran, so the test's own daemon was
  left running.

In a release build (`panic = "abort"`) either would abort the whole host process: the user's Python
interpreter or Node application.

## Root cause

`&hex[start..start + 2]` indexes a `&str` by byte offsets. The comment above it held that "the length is
checked above, so the slice is always in bounds". That covers the bounds, not character boundaries: a
multi-byte character makes a byte offset fall inside a character, and string slicing panics there.

## Fix

Both parsers read the id as bytes, two at a time (`as_bytes().chunks(2)`). A pair that is not UTF-8, or not
two hex digits, is refused with the SDK's typed error, `not a hex byte`, like any other malformed id. Both
crates now carry the no-panic attribute, which denies string slicing outside tests.

## Tests

**Failing first,** by use against a real daemon: each SDK's lifecycle test now passes the hostile id to
`status` and expects the typed refusal.
- `crates/sdk-python/tests/test_sdk.py`: unfixed, the `PanicException` above; fixed, `SlatesError` with
  `not a hex byte`. The suite passes 5 of 5.
- `crates/sdk-node/tests/sdk.test.mjs`: unfixed, the panic ended the process; fixed, a JS error matching
  `not a hex byte`. The suite passes 3 of 3.

Both were built and run on this machine against `target/debug/slates`: the Node addon from `cargo build -p
slates-sdk-node`, copied to a `.node`; the Python wheel from `maturin build`, installed in a venv.

## Siblings

The same byte-pair pattern was already safe, or is now, in:
- the CLI's id parser (`crates/cli/src/format.rs`), which used `str::get`, so it refused rather than
  panicked;
- the MCP server's (`crates/mcp/src/lib.rs`), likewise;
- the manifest parser (`crates/cli/src/args.rs`), which indexed an array.

All three now iterate the pairs, with no index arithmetic.
