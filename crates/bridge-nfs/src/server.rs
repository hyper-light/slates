//! The loopback server's stream loop and RPC dispatch (§4.6, the socket half the codec sits behind).
//!
//! [`serve_connection`] reads ONC RPC records off one connected stream, dispatches portmap `GETPORT`,
//! MOUNT `MNT`, and the NFSv3 procedures onto an [`Export`] (which wraps the volume's [`Bridge`]), and
//! writes the framed replies, until the client closes the connection. It is transport-generic (any
//! `Read + Write`), so a test drives it over a socket pair and the `nfs_loopback` example serves a real
//! `mount_nfs` client over TCP. This is a blocking, one-connection-at-a-time serve — enough for a mount
//! and file access; the production server multiplexes it on slates's runtime.

use std::io::{Read, Write};

use crate::mount::{
  MOUNT_PROGRAM, MOUNTPROC3_MNT, MOUNTPROC3_NULL, MOUNTPROC3_UMNT, parse_mount_path,
};
use crate::multi::NfsService;
use crate::portmap::{PMAPPROC_GETPORT, PMAPPROC_NULL, PORTMAP_PROGRAM, getport_reply};
use crate::procedures::NFS_PROGRAM;
use crate::rpc::{AcceptStatus, RpcError, parse_call};
use crate::xdr::{XdrReader, XdrWriter};
use crate::{read_record, reply_bytes, write_record};

use slates_rt::RtError;
use slates_rt::tcp::TcpStream;

/// Shape: bytes read from the stream per `read` when more of an RPC record is needed. A large NFS
/// transfer fits in one read, and the record assembler stitches any split across reads, so this
/// bounds the number of syscalls per record, never correctness.
const RECORD_READ_CHUNK: usize = 1 << 16;

/// Dispatches one decoded RPC call onto `service`, returning the accept status and encoded results.
/// The `service` serves one volume ([`Export`](crate::procedures::Export)) or many
/// ([`MultiExport`](crate::multi::MultiExport)) behind the same trait. The `port` answers a portmap
/// `GETPORT` (this server serves every program on one port).
fn dispatch(
  service: &mut dyn NfsService,
  program: u32,
  procedure: u32,
  args: &mut XdrReader<'_>,
  port: u16,
) -> (AcceptStatus, Vec<u8>) {
  match program {
    PORTMAP_PROGRAM => match procedure {
      PMAPPROC_NULL => (AcceptStatus::Success, Vec::new()),
      PMAPPROC_GETPORT => (AcceptStatus::Success, getport_reply(u32::from(port))),
      _ => (AcceptStatus::ProcUnavail, Vec::new()),
    },
    MOUNT_PROGRAM => match procedure {
      MOUNTPROC3_NULL => (AcceptStatus::Success, Vec::new()),
      MOUNTPROC3_MNT => {
        let path = parse_mount_path(args).unwrap_or("/").to_owned();
        let reply = service.serve_mount(&path);
        let mut writer = XdrWriter::new();
        reply.encode(&mut writer);
        (AcceptStatus::Success, writer.as_slice().to_vec())
      }
      MOUNTPROC3_UMNT => (AcceptStatus::Success, Vec::new()),
      _ => (AcceptStatus::ProcUnavail, Vec::new()),
    },
    NFS_PROGRAM => match service.serve_procedure(procedure, args) {
      Some(results) => (AcceptStatus::Success, results),
      None => (AcceptStatus::ProcUnavail, Vec::new()),
    },
    _ => (AcceptStatus::ProgUnavail, Vec::new()),
  }
}

/// Serves NFS/MOUNT/portmap RPC over one connected `stream` against `service`, until the client closes
/// it. `port` is the port the server listens on (answered to a portmap `GETPORT`). Returns when the
/// stream reaches end of file or a read/write fails; a malformed record ends the connection rather than
/// risking a desynchronised stream.
pub fn serve_connection<S: Read + Write>(stream: &mut S, service: &mut dyn NfsService, port: u16) {
  let mut buffer: Vec<u8> = Vec::new();
  let mut chunk = [0u8; RECORD_READ_CHUNK];
  loop {
    match read_record(&buffer) {
      Ok((body, consumed)) => {
        let reply = match parse_call(&body) {
          Ok((call, mut args)) => {
            let (status, results) =
              dispatch(service, call.program, call.procedure, &mut args, port);
            reply_bytes(call.xid, status, &results)
          }
          Err(_) => reply_bytes(0, AcceptStatus::GarbageArgs, &[]),
        };
        if stream.write_all(&write_record(&reply)).is_err() {
          return;
        }
        buffer.drain(..consumed);
      }
      Err(RpcError::Incomplete) => match stream.read(&mut chunk) {
        Ok(0) | Err(_) => return,
        Ok(n) => buffer.extend_from_slice(&chunk[..n]),
      },
      Err(_) => return,
    }
  }
}

/// Serves NFS/MOUNT/portmap RPC over one connected runtime `stream` against `service`, until the client
/// closes it — the async analogue of [`serve_connection`] for the production server, which multiplexes
/// connections on slates's own runtime (§4.6). It shares the RPC engine ([`dispatch`]) and the record
/// codec with the blocking form; only the transport differs. Reads and writes await the shard's driver
/// through the runtime's [`TcpStream`], so a write to a stalled client (a soft-mounted NFS client that
/// stopped reading, filling the send buffer) yields the shard rather than blocking it — the reason the
/// production server runs here and the blocking form stays the example-and-test driver of the same
/// engine. Returns `Ok(())` when the stream reaches end of file; a malformed record ends the
/// connection rather than risk a desynchronised stream; a transport error is returned for the caller
/// to log and drop the connection.
pub async fn serve_connection_async(
  stream: &mut TcpStream,
  service: &mut dyn NfsService,
  port: u16,
) -> Result<(), RtError> {
  let mut buffer: Vec<u8> = Vec::new();
  let mut chunk = [0u8; RECORD_READ_CHUNK];
  loop {
    match read_record(&buffer) {
      Ok((body, consumed)) => {
        let reply = match parse_call(&body) {
          Ok((call, mut args)) => {
            let (status, results) =
              dispatch(service, call.program, call.procedure, &mut args, port);
            reply_bytes(call.xid, status, &results)
          }
          Err(_) => reply_bytes(0, AcceptStatus::GarbageArgs, &[]),
        };
        stream.write_all(&write_record(&reply)).await?;
        buffer.drain(..consumed);
      }
      Err(RpcError::Incomplete) => {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
          return Ok(());
        }
        buffer.extend_from_slice(&chunk[..read]);
      }
      Err(_) => return Ok(()),
    }
  }
}
