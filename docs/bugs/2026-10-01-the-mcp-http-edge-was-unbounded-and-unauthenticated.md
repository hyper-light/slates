# The MCP HTTP edge was unbounded, served one connection at a time, and admitted any caller

**Date:** 2026-10-01. **Area:** `slates-mcp` (`http`), `slates-cli` (`slates mcp`). **Audit:** AUD-29-23 (P1),
AUD-29-24 (P1). **Design:** §4.9 (bounded parsing), §4.12 (the MCP edge), §4.13 (authority), R3, R6, R10.

## Description

- **AUD-29-23, unbounded and serial.**
  - `read_line` grew a `String` without a cap for the request line and every header, with no field count.
  - Connections were served one at a time for their whole keep-alive life, with blocking reads and no
    deadline, so one idle client kept every other from being accepted.
  - A truncated body or a reset ended `serve`, and with it the server.
- **AUD-29-24, any caller.** The parser ignored `Host`, `Origin`, the request target and `Content-Type`, so
  any POST to the loopback port was dispatched as MCP — from a web page in the user's browser, or under a
  DNS-rebound name. A loopback bind identifies no caller.
- **The stdio half.** `slates mcp` over stdio read lines without a bound.

## Root cause

The edge was written as a minimal blocking HTTP/1.1 loop for one well-behaved agent. Neither hostile input
nor a second client was in its model.

## Fix

- **On the runtime (`crates/mcp/src/http.rs`).** `serve` runs on one slates shard: the accept loop and every
  connection are tasks, reads and writes wait on the driver, and a connection's failure ends that connection
  only. `slates mcp --http` builds the shard from the daemon's own derivation for one shard of this machine.
  The CLI drives it with `run()`, and the edge asks its shard to exit when its listener fails.
- **Bounds, each stated.**
  - A request line or field line within RFC 9112 §3's 8000 octets (`414`/`431`).
  - At most 100 fields (`431`).
  - A body within the message bound (`413`), shared with stdio.
  - `Transfer-Encoding` and contradictory `Content-Length` refused (`400`); a POST with no length `411`.
  - Each request, and each kept-alive wait, within the client's derived reply deadline.
  - Connections at once bounded by the daemon's derived `clients_per_shard`, one more refused `503`.
- **Authority before dispatch.**
  - `Host` must be the loopback address and this port (`421`); `Origin`, when sent, the same loopback origin
    (`403`).
  - The target must be `/mcp` (`404`), the method `POST` (`405`), the body `application/json` (`415`).
  - The request must carry the 128-bit bearer token minted from the platform's secure random when the edge
    starts and shown only on its terminal (`401`, compared in constant time).
  - No grant verb exists here (R10).
- **Stdio.** `read_stdio_line` keeps at most the message bound, and a longer line draws a JSON-RPC parse error
  and is discarded without being kept.

## Tests

- `assert_http_transport` (real sockets, the in-process daemon of `the_mcp_surface_serves_the_tools`):
  - an unterminated over-long line is refused `414`, too many headers `431`;
  - with a body that never finishes and an idle connection held open, and one reset mid-head, a valid
    `initialize` is answered `200`, and the slow connection is still open and unanswered afterwards;
  - a volume-creating call from a foreign origin, under a rebound host and without the token is refused
    (`403`, `421`, `401`), and no such volume exists.
- `only_this_edges_own_callers_are_admitted` and `malformed_or_oversized_heads_are_refused` cover each
  authority defect and framing fault, and `an_oversized_stdio_line_is_discarded_and_the_next_read_whole`
  covers stdio.
- The old edge could not pass these tests: its API took a blocking `std::net` listener and no token, and a
  held-open idle connection blocked every later one. That is reasoning, not a run, since the old API is gone.

## Still owed

- **Servable roots** a human enrolls through the CLI (§4.13), so an authorized agent is also scoped.
- **Windows HTTP edge.** The runtime's TCP is macOS/Linux, so `--http` on Windows refuses and names stdio.
  Before this change Windows had the old unbounded blocking edge.
