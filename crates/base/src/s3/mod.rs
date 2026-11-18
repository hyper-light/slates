//! An overlay's base read from S3 objects (§4.15 "An S3 base", A-464; Mantle's CP-T10.5): the pure core, with no I/O
//! and no platform code, so every host tests it alike.
//!
//! - [`sigv4`]: AWS Signature Version 4 for S3 requests, the canonical request, the string to sign, the signing key
//!   and the signature, held to the four worked examples of AWS's S3 signing page (golden vectors).
//! - [`list`]: one `ListObjectsV2` response page read into its objects, its common prefixes and its continuation, its
//!   length checked against the page's cap before a byte is parsed, every hostile shape refused typed.
//! - [`keys`]: the keys under a base prefix mapped to a directory tree, or one directory's entries from a delimited
//!   page, each key that names no POSIX path (an empty or a dot component, a NUL, a name or key too long, a file a
//!   directory shadows) refused by its kind, so no key leaves the base or aliases another.
//!
//! The network client that sends what [`sigv4`] signs over the shard's own runtime (TCP and TLS, rustls over
//! aws-lc-rs) is part 2, and the `HostFs` implementation with its simulated oracle is part 3 (GAPS A-464).

pub mod keys;
pub mod list;
pub mod sigv4;
