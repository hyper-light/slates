//! `slates-bridge-nfs` — the macOS NFSv3 loopback bridge (design §4.6, D-2; Phase 4). On macOS
//! versions where FSKit is unavailable or disabled, and as the differential oracle against the
//! other bridges, slates presents a volume by running its own NFSv3 server on a TCP loopback socket
//! held by the anchor and mounting it at a user-owned mount point — no kernel extension, no
//! privilege beyond mounting NFS onto a directory the user owns (R10). The design's shape: one TCP
//! loopback listener, ONC RPC record marking, NFSv3 + MOUNT + a minimal portmap responder.
//!
//! This crate is built bottom-up, the wire codec first, because the codec is pure and directly
//! confirmable on every host (golden vectors and hostile-input tests) with no socket and no mount,
//! exactly as the FUSE bridge's ABI codec was. The socket, the mount, and the NFSv3/MOUNT/portmap
//! procedures over the volume core are the rest (owed); those run in the macOS lane against a real
//! `mount_nfs`.
//!
//! Modules: [`xdr`] (the External Data Representation reader and writer, RFC 4506 — big-endian,
//! four-byte aligned, bounds-checked), [`rpc`] (ONC RPC, RFC 1057/5531 — record marking over TCP,
//! the call and reply messages, and the `AUTH_NONE` credential the loopback server uses), and
//! [`nfs`] (the NFSv3 core data types of RFC 1813 — status codes, file types, times, attributes and
//! file handles), and [`handle`] (slates' private encoding of a volume object's durable identity into an opaque file handle: `(volume, inode, gen)`).

pub mod handle;
pub mod nfs;
pub mod rpc;
pub mod xdr;

pub use handle::{FileHandle, FileHandleError};
pub use nfs::{Fattr3, Ftype3, Nfsfh3, Nfsstat3, Nfstime3, PostOpAttr, Specdata3};
pub use rpc::{AcceptStatus, RpcCall, RpcError, read_record, reply_bytes, write_record};
pub use xdr::{XdrError, XdrReader, XdrWriter};
