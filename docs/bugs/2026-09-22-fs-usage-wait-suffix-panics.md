# A malformed fs_usage wait suffix panicked the trace parser

Date: 2026-09-22. Found while auditing the macOS hermeticity failure, independently
of the tracer-startup race. This parser defect does not explain the empty CI trace.

`read_row` accepts four tokens as a possible row. If the penultimate token is `W`,
it subtracts three trailing tokens, then slices from token 2 to token 1. The row
`08:20:01.000001 write W slates.123` therefore panics before the next line can be
judged. A malformed external trace must not crash the conformance gate.

The regression feeds this row and two shorter truncations, each followed by a real
mkdir outside the granted target. It requires that complete violation to survive.

```sh
cargo test --offline -p slates-conformance \
  malformed_fs_usage_wait_suffixes_do_not_hide_a_following_violation -- --nocapture
```

Before the fix: failure in 0.00 s, `slice index starts at 2 but ends at 1`.
Log: `/private/tmp/slates-ci-35615970514-trace-row-red.log`.
The correction uses checked slice access and rejects the incomplete row, as the
parser already does for other malformed rows. It changes neither the allowed
write policy nor the requirement for a complete, non-vacuous landing trace.

After the fix, all 45 conformance unit tests and 4 record tests pass (one deliberate
matrix writer remains ignored), and strict crate Clippy passes. Logs:
`/private/tmp/slates-ci-35615970514-trace-row-{green,clippy}.log`.
