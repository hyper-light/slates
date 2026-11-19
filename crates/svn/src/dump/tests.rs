//! The dump codec's tests (A-593; §4.17.4 piece 6, its first part). Every test does something with the codec and says
//! what it expects, naming the acceptance it serves:
//! - `golden`: dumps stock `svnadmin` 1.15.0 wrote (`fixtures/dump`, by the recorded `make-fixtures.sh`), each checked
//!   against its recorded SHA-256 first, read and written again byte for byte; format 3's deltas and property changes
//!   applied to their bases rebuild format 2's trees revision by revision; every revision's records are what
//!   `svnlook changed` says it changed; the tightest admission reads each dump and one byte less is refused.
//! - `hostile`: every cut of every vector, every bit flip of a small one and random flips of the rest, lengths past
//!   the stream, a missing `PROPS-END`, and every refusal the reader makes, each by kind and Subversion's number.
//! - `writer`: the writer's own refusals, nothing appended on any of them, and the bytes of the record shapes the
//!   vectors do not hold.
//! - `generated`: generated streams written, read back to the same events in any pieces, and written again to the same
//!   bytes.

mod generated;
mod golden;
mod hostile;
mod support;
mod writer;
