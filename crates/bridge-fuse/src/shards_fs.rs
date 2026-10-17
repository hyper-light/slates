//! Serving a slates volume to a `shards` microVM's virtio-fs device (§4.6, the shards integration; see
//! `docs/wip/shards/`). shards' VMM forwards each guest FUSE request to a backend over a Unix socket, framed as a
//! little-endian `u32` length then the FUSE request bytes, and reads the reply framed the same way, one request at a
//! time (`shards crates/vmm/src/devices/virtio/fs`: `write_frame`/`read_frame`/`Slot::ask`). This module is the
//! **slates backend** for that socket: it reads a frame, dispatches the FUSE request through the very bridge the
//! kernel mount uses ([`crate::bridge::dispatch`], the one FUSE seam), and writes the reply frame. So a shards guest
//! whose virtio-fs device is pointed at this socket mounts a slates volume with no kernel NFS or 9p in between.
//!
//! This is the **non-DAX** path: shards' frame carries a FUSE request, not a `FUSE_SETUPMAPPING`, and shards' fs
//! device has no guest-physical DAX cache window (verified against its VMM, `docs/wip/shards/`), so the guest reaches
//! slates through the mediated FUSE path, not a mapped window. DAX in a shards microVM needs the window shards does
//! not yet have; this serves the volume meanwhile, the achievable integration on a host with no DAX-capable VMM.
//!
//! The core is **transport-agnostic** — generic over [`std::io::Read`] + [`std::io::Write`], so it holds no socket
//! type and stays inside the lint wall's RAM-only rule (R1); the caller owns the `UnixStream` shards connected (the
//! daemon's device-attach path, or a test's `UnixStream::pair`). One request is read, dispatched and answered before
//! the next, exactly as shards' `Slot::ask` drives it. Every frame's length is bounded by the caller's reply buffer
//! before a byte is allocated (CLAUDE.md #8: no unbounded allocation from an external length), and every failure is a
//! typed [`std::io::Error`], never a panic (R6).

use std::io::{self, Read, Write};

use slates_bridge_core::{Bridge, OpContext};

/// Reads one shards fs frame: a little-endian `u32` length, then that many FUSE bytes. `Ok(None)` at a clean EOF
/// (the guest's fs device closed the socket). The length is checked against `max` — the caller's reply-buffer size,
/// the largest request it can answer — before any allocation, so a hostile or corrupt length never allocates past
/// the bound (it is refused `InvalidData` instead). `max` is the bound, derived by the caller from its buffer.
pub fn read_shards_frame<S: Read>(conn: &mut S, max: usize) -> io::Result<Option<Vec<u8>>> {
  let mut len = [0u8; 4];
  match conn.read_exact(&mut len) {
    Ok(()) => {}
    Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
    Err(error) => return Err(error),
  }
  // u32 -> usize is a widening on every supported target; the fallback keeps it panic-free for the lint.
  let len = usize::try_from(u32::from_le_bytes(len)).unwrap_or(usize::MAX);
  if len > max {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "a shards fs frame longer than the reply buffer bounds",
    ));
  }
  let mut bytes = vec![0u8; len];
  conn.read_exact(&mut bytes)?;
  Ok(Some(bytes))
}

/// Writes one shards fs frame: a little-endian `u32` length, then `bytes`. Refused `InvalidInput` if a reply somehow
/// exceeds `u32` (it cannot — a reply is bounded by the buffer, itself a `usize` the caller keeps under `u32`).
pub fn write_shards_frame<S: Write>(conn: &mut S, bytes: &[u8]) -> io::Result<()> {
  let len = u32::try_from(bytes.len()).map_err(|_| {
    io::Error::new(
      io::ErrorKind::InvalidInput,
      "a shards fs reply larger than a u32 frame",
    )
  })?;
  conn.write_all(&len.to_le_bytes())?;
  conn.write_all(bytes)
}

/// Serves one guest request on `conn`: reads a frame, dispatches it through the slates FUSE bridge, writes the reply
/// frame. `reply` is the caller's answer buffer (its length bounds a request frame too). `Ok(true)` after a served
/// request, `Ok(false)` at EOF (the guest's fs device closed — the caller ends the connection and sweeps it).
pub fn serve_one<S: Read + Write>(
  conn: &mut S,
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  reply: &mut [u8],
) -> io::Result<bool> {
  let Some(request) = read_shards_frame(conn, reply.len())? else {
    return Ok(false);
  };
  let n = crate::bridge::dispatch(&request, bridge, cx, reply);
  write_shards_frame(conn, reply.get(..n).unwrap_or(&[]))?;
  Ok(true)
}

/// Serves `conn` until the guest's fs device closes it (EOF): one request read, dispatched and answered before the
/// next, as shards' `Slot::ask` drives it. The caller owns `conn` (the socket shards connected) and sizes `reply`.
pub fn serve<S: Read + Write>(
  conn: &mut S,
  bridge: &mut dyn Bridge,
  cx: &OpContext,
  reply: &mut [u8],
) -> io::Result<()> {
  while serve_one(conn, bridge, cx, reply)? {}
  Ok(())
}
