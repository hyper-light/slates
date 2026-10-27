# The MCP edge's refusal was erased by a reset on Windows

- **Date:** found 2026-10-26, in the first Windows run of the MCP test (jupiter), as the HTTP edge was built for
  Windows (A-183). Fixed 2026-10-26.
- **Status:** fixed 2026-10-26. Decided under the standing goal (Ada's every-platform rule), not in plan mode.
- **Subsystem:** the MCP loopback HTTP edge (§4.12): `crates/mcp/src/http.rs` (`serve_connection`, `refuse`, `drain`).
  It also needed the runtime's half-close: `crates/rt/src/tcp.rs` (`TcpStream::close_write`) over
  `crates/rt/src/netsys.rs` (`shutdown_write`).
- **Severity:** a refused client could not learn why it was refused. It read a reset instead of the edge's typed answer
  (`413`, `401`, `403`, `421`, …; AUD-29-23 and AUD-29-24). Such a client takes the refusal for a network fault and may
  retry. Nothing crossed the edge: the request was refused either way.
- **Exposure:**
  - Windows: every run.
  - macOS and Linux: the test delivered the refusal in every run there. RFC 9112 §9.6 says a reset may erase it on any
    stack, so the fix is the same code on all three.

## Symptom

`crates/mcp/tests/mcp.rs`, `the_mcp_surface_serves_the_tools`, on jupiter (Windows 10.0.26200, x86_64). The scenarios
before the HTTP edge all passed. Then a client sent a request whose declared body is one byte past the bound, sending
the whole body before reading, as an ordinary client does:

```
a client that sent a body past the bound reads 413, not a reset: (Err(ConnectionAborted), "")
```

The client's read failed with `WSAECONNABORTED` and returned nothing.

## Root cause

The edge refuses such a request on its head, from the declared `Content-Length`, before reading the body. It wrote
its `413` and closed the socket at once, with the client's body still arriving. A close with received bytes unread
makes the stack reset the connection. RFC 9112 §9.6 ("Tear-down") names the outcome: "the reset packet might erase the
client's unacknowledged input buffers before they can be read and interpreted by the client's HTTP parser". Windows
does exactly that. The client's send fails, and its next receive reports the abort and discards the `413` it had not
read.

**How it was confirmed.** By the one change below, run against the same test on the same host: with the close staged,
the client reads `413`. No other code changed between the two runs. There was no separate logging run: the failing
assertion already printed the client's view.

## Fix

A refusal inside a served connection now ends it in stages, as §9.6 prescribes (`refuse` in `http.rs`):
1. the refusal is written;
2. the edge half-closes its side (`TcpStream::close_write`: `SHUT_WR` on Unix, `SD_SEND` on Windows), so the client
   reads end of stream after the refusal;
3. whatever the client still sends is read and dropped (`drain`), until it closes its side, a read fails, or the
   edge's deadline passes;
4. the connection closes.

The drain keeps no byte. It holds the connection's slot no longer than a request may hold it: the same deadline.

## Verification

- **Red, before the fix (jupiter):** the failure above, with the landing scenarios moved after the HTTP edge for
  that run only, so that landing's known Windows gap could not hide the edge.
- **Green, with the fix (jupiter, same order):**
  - the client reads `413`;
  - every other HTTP assertion passes: the bounds, the authority refusals, the modern protocol, a listen stream;
  - the test then stops where Windows still lacks landing: `slates.land.materialize` answers
    `Unsupported { feature: "landing" }` (G-shards-windows step 5's owner is building it).
- **macOS arm64:** the MCP test passes whole (1.75 s).
- **Linux x86_64 (adacachy):** the MCP test passes whole (0.69 s).
- **Lints:** `cargo clippy -D warnings` is clean for `slates-rt`, `slates-mcp` and `slates-cli` on all three
  platforms.

## Siblings

- **A connection past the bound is still closed at once.** The accept loop answers it `503` and closes it without
  waiting, because the loop must not wait on a connection it does not serve. A client that has already sent its request
  can therefore read a reset there too. Staging that close needs a bounded set of shedding tasks, so it is a design
  choice of its own: GAPS `8a-platforms`.
- **A response that closes after a whole request** (`Connection: close`) is not staged: its request was read whole, so
  nothing is left unread unless the client pipelined past a close.
- **The NFS servers** end a connection without a reply to read, on a malformed or hostile record, so the client has
  no refusal to lose.
