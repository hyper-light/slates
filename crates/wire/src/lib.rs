//! `slates-wire` — the wire protocol: a 32-byte little-endian header checked before any
//! allocation, CRC32C over control and metadata bodies, canonical derive-generated bodies with a
//! schema hash in the first body word, append-only evolution within a major, per-class frame caps
//! and credit windows derived from measurements, and RIFL request ids for exactly-once
//! provisioning (design §4.9, D-15).
//!
//! The doctrine, in one paragraph. A node is authenticated but not trusted (vorpal's rule
//! [C: survey-vorpal.md §5.4]): every read validates the length against its class's cap before
//! allocating and verifies the checksum before decoding, and a frame it cannot place (bad magic,
//! another major, an unknown kind, a foreign schema) is refused with a typed error, never
//! guessed at. One value, one encoding (hecate's axiom [C: survey-hecate.md §4.6]): integers are
//! fixed-width little-endian, `usize` does not exist on the wire, `bool` and `Option` tags are
//! exactly 0 or 1, lengths are `u32`, and a decoder refuses a non-canonical byte. Evolution is
//! append-only within a major and checked by the golden schema test; there is no decode across
//! majors. Exactly-once follows RIFL [A: Lee et al., SOSP'15]: `(client, sequence)` names a
//! request, the server keeps the completion until the client acknowledges, and a retry returns
//! the original result. Flow control is credit-based with absolute offsets per stream
//! [A: Kung, Blackwell & Chapman, SIGCOMM'94; B: RFC 9113 §5.2], one pool per class, so no
//! class's latency bound contains a term from another class's queue.
//!
//! Modules: [`header`], [`codec`] (the `Wire` trait and the primitive encodings), [`schema`],
//! [`crc32c`], [`frame`], [`credit`], [`request`], [`observe`] (§4.14 spans), [`error`].

// The derive emits `::slates_wire::` paths; this alias makes them resolve inside the crate.
extern crate self as slates_wire;

pub mod codec;
pub mod crc32c;
pub mod credit;
pub mod error;
pub mod frame;
pub mod header;
pub mod observe;
pub mod request;
pub mod schema;

pub use codec::Wire;
pub use error::WireError;
pub use frame::{FrameCaps, Framer};
pub use header::{Class, Flags, Header};
pub use observe::{CausedBy, Chokepoint, SpanContext, SpanId, TraceId};
pub use request::RequestId;
pub use slates_wire_derive::Wire;
