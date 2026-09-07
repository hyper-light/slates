//! The MCP loopback Streamable HTTP transport (§4.12). A minimal HTTP/1.1 server: the agent POSTs a
//! JSON-RPC message and gets the reply as `application/json`; a notification (no reply) gets `202
//! Accepted`. This is the same-machine *edge* — one agent to its local daemon — so it is HTTP/1.1
//! over loopback, not QUIC: QUIC's wide-area properties (migration, 0-RTT, per-stream multiplexing)
//! belong to the fleet transport (daemon↔daemon, §4.10), where the scale is, and are wasted on a
//! loopback edge that must anyway speak what MCP clients speak. The parsing is bounds-checked against
//! a hostile `Content-Length` before any allocation; there is no server-initiated stream, so `GET`
//! (the SSE channel) is answered `405` — the stateless profile the design names.

use std::io::{BufRead, Write};
use std::net::TcpListener;

use serde_json::Value;

use crate::McpServer;

/// Shape: the largest HTTP request body accepted, a bound on a hostile or mistaken `Content-Length`
/// so a wild length never drives an allocation. 64 MiB is far above any real JSON-RPC message and far
/// below memory pressure; a larger body is refused `413` rather than read.
const MAX_BODY: u64 = 64 << 20;

/// One parsed HTTP/1.1 request: the method, the body, and whether the connection is kept alive.
struct HttpRequest {
  method: String,
  body: Vec<u8>,
  keep_alive: bool,
  too_large: bool,
}

/// Serves MCP over one accepted connection until the peer closes it (§4.12). Each request is read,
/// dispatched, and answered in turn; the connection is kept alive unless the peer asks to close.
pub fn serve_connection(
  server: &mut McpServer,
  stream: &mut (impl BufRead + Write),
) -> std::io::Result<()> {
  loop {
    let Some(request) = read_request(stream)? else {
      return Ok(()); // the peer closed the connection
    };
    let keep_alive = request.keep_alive && !request.too_large;
    respond(server, &request, stream)?;
    stream.flush()?;
    if !keep_alive {
      return Ok(());
    }
  }
}

/// Accepts loopback connections and serves each in turn (§4.12). Single-threaded: one agent per MCP
/// server, so connections serialize — the daemon, not this edge, is where many agents fan out (§4.7).
pub fn serve(mut server: McpServer, listener: &TcpListener) -> std::io::Result<()> {
  for stream in listener.incoming() {
    let stream = stream?;
    let mut buffered = std::io::BufReader::new(stream);
    // `serve_connection` needs Write; wrap so reads are buffered and writes reach the same stream.
    let mut duplex = Duplex {
      reader: &mut buffered,
    };
    serve_connection(&mut server, &mut duplex)?;
  }
  Ok(())
}

/// A read+write view over a buffered `TcpStream`: reads come from the buffer, writes go to the inner
/// stream (a `TcpStream` is written through its shared handle).
struct Duplex<'a> {
  reader: &'a mut std::io::BufReader<std::net::TcpStream>,
}

impl std::io::Read for Duplex<'_> {
  fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
    self.reader.read(buf)
  }
}

impl BufRead for Duplex<'_> {
  fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
    self.reader.fill_buf()
  }
  fn consume(&mut self, amt: usize) {
    self.reader.consume(amt);
  }
}

impl Write for Duplex<'_> {
  fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
    self.reader.get_mut().write(buf)
  }
  fn flush(&mut self) -> std::io::Result<()> {
    self.reader.get_mut().flush()
  }
}

/// Reads one HTTP/1.1 request: the request line, the headers, and the body named by `Content-Length`.
/// Returns `None` at a clean end of input. A `Content-Length` beyond [`MAX_BODY`] sets `too_large` and
/// the body is not read (the caller answers `413`).
fn read_request(reader: &mut impl BufRead) -> std::io::Result<Option<HttpRequest>> {
  let mut line = String::new();
  if reader.read_line(&mut line)? == 0 {
    return Ok(None);
  }
  let method = line.split_whitespace().next().unwrap_or("").to_owned();
  let mut content_length: u64 = 0;
  // HTTP/1.1 keeps the connection alive unless the peer says otherwise.
  let mut keep_alive = true;
  loop {
    let mut header = String::new();
    if reader.read_line(&mut header)? == 0 {
      break;
    }
    let header = header.trim_end();
    if header.is_empty() {
      break; // the blank line ends the headers
    }
    if let Some((name, value)) = header.split_once(':') {
      let name = name.trim().to_ascii_lowercase();
      let value = value.trim();
      if name == "content-length" {
        content_length = value.parse().unwrap_or(0);
      } else if name == "connection" && value.eq_ignore_ascii_case("close") {
        keep_alive = false;
      }
    }
  }
  if content_length > MAX_BODY {
    return Ok(Some(HttpRequest {
      method,
      body: Vec::new(),
      keep_alive,
      too_large: true,
    }));
  }
  // `content_length <= MAX_BODY` (64 MiB) fits usize on every supported target.
  let mut body = vec![0u8; usize::try_from(content_length).unwrap_or(0)];
  reader.read_exact(&mut body)?;
  Ok(Some(HttpRequest {
    method,
    body,
    keep_alive,
    too_large: false,
  }))
}

/// Dispatches one request and writes its HTTP response. `POST` carries a JSON-RPC message (a single
/// reply as `application/json`, or `202 Accepted` for a notification, or a JSON-RPC parse error for
/// non-JSON); `GET` (the SSE channel) is `405` — this server initiates no stream; anything else `405`.
fn respond(
  server: &mut McpServer,
  request: &HttpRequest,
  writer: &mut impl Write,
) -> std::io::Result<()> {
  if request.too_large {
    return write_status(writer, "413 Payload Too Large", request.keep_alive);
  }
  if request.method != "POST" {
    return write_status(writer, "405 Method Not Allowed", request.keep_alive);
  }
  match serde_json::from_slice::<Value>(&request.body) {
    Ok(message) => match server.handle(&message) {
      Some(reply) => write_json(writer, &reply, request.keep_alive),
      None => write_status(writer, "202 Accepted", request.keep_alive),
    },
    // A body that is not JSON is a JSON-RPC parse error, carried in a 200 body (a transport success).
    Err(_) => write_json(writer, &crate::parse_error_reply(), request.keep_alive),
  }
}

/// Writes a `200 OK` with a JSON body.
fn write_json(writer: &mut impl Write, value: &Value, keep_alive: bool) -> std::io::Result<()> {
  let body = value.to_string();
  write!(
    writer,
    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: {}\r\n\r\n",
    body.len(),
    connection(keep_alive),
  )?;
  writer.write_all(body.as_bytes())
}

/// Writes a status-only response (no body).
fn write_status(writer: &mut impl Write, status: &str, keep_alive: bool) -> std::io::Result<()> {
  write!(
    writer,
    "HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: {}\r\n\r\n",
    connection(keep_alive),
  )
}

/// The `Connection` header value for whether the connection is kept alive.
fn connection(keep_alive: bool) -> &'static str {
  if keep_alive { "keep-alive" } else { "close" }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::io::Cursor;

  /// A `POST` body round-trips to the parsed request; the method, keep-alive and body are read.
  #[test]
  fn a_post_request_parses() {
    let raw = "POST / HTTP/1.1\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
    let mut reader = Cursor::new(raw.as_bytes());
    let request = read_request(&mut reader).unwrap().unwrap();
    assert_eq!(request.method, "POST");
    assert!(request.keep_alive);
    assert!(!request.too_large);
    assert_eq!(request.body, b"{\"a\":1}");
  }

  /// `Connection: close` is honored; a clean end of input yields `None`.
  #[test]
  fn connection_close_and_eof() {
    let raw = "POST / HTTP/1.1\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    let mut reader = Cursor::new(raw.as_bytes());
    let request = read_request(&mut reader).unwrap().unwrap();
    assert!(!request.keep_alive);
    assert!(
      read_request(&mut Cursor::new(b"".as_slice()))
        .unwrap()
        .is_none(),
      "a clean end of input yields no request"
    );
  }

  /// A `Content-Length` past the cap sets `too_large` and reads no body (no wild allocation).
  #[test]
  fn a_hostile_content_length_is_refused_without_reading() {
    let raw = format!("POST / HTTP/1.1\r\nContent-Length: {}\r\n\r\n", u64::MAX);
    let mut reader = Cursor::new(raw.into_bytes());
    let request = read_request(&mut reader).unwrap().unwrap();
    assert!(request.too_large);
    assert!(request.body.is_empty());
  }

  /// A 200 response carries the JSON body with a matching `Content-Length`; a 202 carries none.
  #[test]
  fn responses_format_correctly() {
    let mut out = Vec::new();
    write_json(&mut out, &serde_json::json!({"ok": true}), true).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(text.contains("Content-Type: application/json\r\n"));
    assert!(text.contains("Content-Length: 11\r\n"));
    assert!(text.ends_with("{\"ok\":true}"));

    let mut out = Vec::new();
    write_status(&mut out, "202 Accepted", false).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.starts_with("HTTP/1.1 202 Accepted\r\n"));
    assert!(text.contains("Connection: close\r\n"));
  }
}
