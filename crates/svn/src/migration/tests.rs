//! The migration verbs' tests (A-628; §4.17.4 piece 6). Every test does something with an import or an export and
//! says what it expects:
//! - `golden`: the dumps stock `svnadmin` 1.15.0 wrote (A-593's `fixtures/dump`), each checked against its recorded
//!   SHA-256 first, imported whole, in pieces, in parts and from format 3, every revision's changed paths what
//!   `svnlook changed` reported of the repository `svnadmin` dumped, and exported again byte for byte.
//! - `rules`: what an import records where a dump's records meet FSFS's rules (a bare change, a section that makes no
//!   call, a deletion and an addition apart, changes beneath a deletion, the mergeinfo flag) and how an export compares.
//! - `refusals`: every refusal an import or an export makes, each by kind and Subversion's number, a refused import
//!   keeping the revisions before it and nothing of the refused one.
//! - `generated`: generated histories written as dumps, imported, held to the model tree they were made from in every
//!   revision, then exported, imported again and exported again to the same history and bytes.

mod generated;
mod golden;
mod refusals;
mod rules;
mod support;
