# A hostile codemode query could crash or hold the MCP server

Date: 2026-10-06. Area: §4.12 codemode, `crates/mcp/src/query.rs` (`slates.query`). Terminating conditions 11 and 13.

## Description

An adversarial MCP session sent `slates.query` text built to hurt the server. It used a release `slates mcp` over a
real anchor, driven by a scratch Python client. Two shapes did damage:

1. **Stack overflow.** A `WHERE` made of a 20,000-term `OR` chain killed the whole MCP process with
   `fatal runtime error: stack overflow, aborting`. The lint wall cannot see this kind of abort. Deep parentheses or
   long `NOT` chains reached the same recursion.
2. **Exponential glob.** `path GLOB` with `*a` forty times then `*b`, against a 200-character file name, got no reply
   within 60 s. So did `**/a` forty times against a deep path. Each one held the server, so it was a denial of service.

## Root cause

1. The parser built `AND` and `OR` as binary trees, one level per operator, and both the parser and the evaluator
   recursed down that tree. Nothing bounded the nesting.
2. `glob_parts` and `glob_component` tried every way to split the input at every star and recursed into each one.
   That is exponential in the number of stars.

## Impact

Any MCP client, an agent included, could crash the server with one request or hold it indefinitely. No data was lost
or written; R1 and R10 held throughout.

## Edits

- `AND` and `OR` became n-ary (`Expr::And(Vec<Expr>)`, `Expr::Or(Vec<Expr>)`). The parser collects their terms in a
  loop, and evaluation iterates over them with short-circuit. A chain of any length now costs no stack.
- The parser counts its depth through parentheses and `NOT`. Past `MAX_EXPR_DEPTH` (200) it refuses with the typed
  `QueryError::Ceiling { name: "expression depth" }`.
  - Measured on a 2 MiB thread (Rust's default spawned-thread stack): a debug build parsed 400 levels and overflowed
    at 600, about 4 KiB a level; a release build parsed 800.
  - So 200 leaves twice that headroom even in debug.
- One iterative wildcard matcher (`wildcard`) now serves both levels: `*` within a path component and `**` across
  components.
  - It backtracks only to the most recent star, so the worst case is O(pattern × input), with no recursion.
  - Reference: Krauss, "Matching Wildcards", Dr. Dobb's 2014, the same shape as `fnmatch` without bracket classes.

## Tests

- `a_query_that_would_overflow_the_stack_is_answered_or_refused_typed`:
  - a 20,000-term `OR` chain answers two rows;
  - 201 nested parentheses and 201 `NOT`s are refused with the typed ceiling.
  - It passes in debug and release.
- `the_glob_matcher_agrees_with_the_reference_and_has_no_exponential_case`:
  - The old recursive matcher is kept in the test module as the oracle.
  - Component level: every pattern over `a`, `b`, `*` up to five characters against every name over `a`, `b` up to
    five characters.
  - Path level: every pattern of up to four components drawn from `a`, `b`, `**`, `a*`, against seven paths.
  - Both levels must give the same answer as the oracle.
  - The adversarial patterns must then answer at once. With the old matcher, those assertions do not finish.
- Re-run against the release server: all 13 adversarial cases reply.
  - The huge query answered in 5.0 ms.
  - The deep shapes are refused typed in 0.1–0.2 ms.
  - The two exponential globs answer `rows=0` in 0.2 ms and 0.1 ms; before the fix they gave no reply in 60 s.
  - No panics.

## Siblings

No other parser in `crates/mcp` recurses on user input. The JSON-RPC body is parsed by `serde_json`, which has its own
recursion limit of 128.
