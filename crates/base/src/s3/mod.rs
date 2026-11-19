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
//! - [`http`]: HTTP/1.1 framing, a request's head written and a response read as its bytes arrive, every length checked
//!   against its cap before it is held, chunked and length bodies alike, every hostile shape refused by its kind.
//! - [`wait`]: how long a request waits and how long it pauses before trying again, learned from the path's round trips
//!   within the bounds the caller states, as A-442's learned wait.
//! - [`tls`]: rustls' client over aws-lc-rs, driven sans-io beneath the framing.
//! - [`client`]: `HeadObject`, ranged `GetObject` bound by `If-Match` or a version id, and `ListObjectsV2` pages,
//!   signed, sent over the runtime's TCP, every answer typed (A-471).
//!
//! The `HostFs` implementation with its simulated oracle is part 3 (GAPS A-464).

pub mod client;
pub mod http;
pub mod keys;
pub mod list;
pub mod sigv4;
pub mod tls;
pub mod wait;
