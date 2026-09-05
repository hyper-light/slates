//! `slates-archive`: the archive format (D-17, §2.6 of `research/compression-archive-dedup.md`;
//! Phase 7). An archive is one streamable, content-addressed, self-verifying byte sequence of a
//! snapshot — a header, the chunk records in manifest order, the manifest tree, a seek table, and
//! a trailer. The same container is the replication and clone-from-archive format.
//!
//! Every chunk is addressed by the BLAKE3 of its bytes and verified against that identity before
//! use, so a single flipped bit is detected and named (AC-7.3); the trailer's whole-archive
//! BLAKE3 detects truncation or any alteration; the seek table locates a chunk without scanning.
//! Readers reject an unknown major and unknown required flags, and refuse a malformed stream with
//! a typed [`format::ArchiveError`], never a panic (the hostile-input rule, §4.9).
//!
//! slates never writes an archive to disk (R1): it is built in RAM and handed to a caller, or used
//! as the replication/clone container. This crate is pure — byte buffers only, no I/O, no
//! `std::fs`, no `std::net`, no `unsafe`.
//!
//! Scope: raw-encoded chunks and an opaque manifest blob. The codec pass (LZ4, zstd, dictionaries,
//! CDC) and the manifest tree's structure are later Phase 7 work (owed); the format reserves their
//! fields, so an archive written then is readable now.

pub mod archive;
pub mod format;
pub mod wire;

pub use archive::Archive;
pub use format::{ArchiveError, Chunk, Encoding};
