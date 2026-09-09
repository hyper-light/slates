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
use crate::portmap::{PMAPPROC_GETPORT, PMAPPROC_NULL, PORTMAP_PROGRAM, getport_reply};
use crate::procedures::{Export, NFS_PROGRAM};
use crate::rpc::{AcceptStatus, RpcError, parse_call};
use crate::xdr::{XdrReader, XdrWriter};
use crate::{read_record, reply_bytes, write_record};

/// Dispatches one decoded RPC call onto `export`, returning the accept status and encoded results. The
/// `port` answers a portmap `GETPORT` (this server serves every program on one port).
fn dispatch(
  export: &mut Export<'_>,
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
        let reply = export.mnt(&path);
        let mut writer = XdrWriter::new();
        reply.encode(&mut writer);
        (AcceptStatus::Success, writer.as_slice().to_vec())
      }
      MOUNTPROC3_UMNT => (AcceptStatus::Success, Vec::new()),
      _ => (AcceptStatus::ProcUnavail, Vec::new()),
    },
    NFS_PROGRAM => match export.serve_nfs(procedure, args) {
      Some(results) => (AcceptStatus::Success, results),
      None => (AcceptStatus::ProcUnavail, Vec::new()),
    },
    _ => (AcceptStatus::ProgUnavail, Vec::new()),
  }
}

/// Serves NFS/MOUNT/portmap RPC over one connected `stream` against `export`, until the client closes
/// it. `port` is the port the server listens on (answered to a portmap `GETPORT`). Returns when the
/// stream reaches end of file or a read/write fails; a malformed record ends the connection rather than
/// risking a desynchronised stream.
pub fn serve_connection<S: Read + Write>(stream: &mut S, export: &mut Export<'_>, port: u16) {
  let mut buffer: Vec<u8> = Vec::new();
  let mut chunk = [0u8; 1 << 16];
  loop {
    match read_record(&buffer) {
      Ok((body, consumed)) => {
        let reply = match parse_call(&body) {
          Ok((call, mut args)) => {
            let (status, results) = dispatch(export, call.program, call.procedure, &mut args, port);
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
