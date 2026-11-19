//! The git object store (A-614; §4.17.2 "The repository in git's terms", items 1 to 3, and "Pieces (git)", piece 5,
//! part 1), built on §4.17.1's (`slates-vcs`, A-581).
//!
//! **Objects in.** A push's objects arrive through one trait, [`Entries`], which a pack reader (A-608's `unpack`) or an
//! import (piece 15) implements, so the store depends on no reader's module. Each object is named by git's own rule,
//! the store's parameter ([`GitNaming`]: SHA-1 with collision detection or SHA-256 of `<type> <length>\0<content>`,
//! [`crate::object_name::name_of`]), once, and an id its source claims is checked against that name. An input SHA-1's
//! detection finds to be part of a known collision attack is refused with git's own words ([`NameRefused::words`]). The
//! bytes are content in the shared store, held by the push alone ([`Push`]) until the verdict: git's quarantine.
//!
//! **Rows.** When the verdict commits a push, each of its objects new to the repository gets a position, the next in the
//! repository's arrival order, and two rows in the repository's one `slates_vcs::Index` ([`rows`]): its object row
//! under its id (its kind, length, position and the version that brought it; a commit's row adds what git's
//! commit-graph keeps [B: gitformat-commit-graph]: its parents by position, its root tree's position, its commit date
//! and its generation number), and its position row, which names the id back. The rows are written once, in time
//! proportional to the push's new objects, and nothing rewrites them; a refused push writes none.
//!
//! **What each version brought.** Version `k` of a ref's green records D(k), the objects reachable from its head and not
//! from version `k - 1`'s, as a set of positions ([`Positions`]), and the green keeps the set its current head reaches.
//! Both are computed when the push is admitted ([`Repository::admit`]), before its verdict, by walks that read the push
//! and the index alike: from the new head, stopping at every object the green had already reached, so a tree the green
//! reached is skipped whole and the walk costs the objects new to the branch and the entries of its new trees. A
//! fast-forward (found by a walk back through generation numbers that stops at the old head's) grows the reached set by
//! D(k), so a client holding version `M`'s head needs the union of D(M+1) to D(N) and no walk at all. A rewrite, a
//! create and a ref made at another ref's commit make the set again from the newest commit that is some green's
//! current head, found back through the commit rows by generation, walking only what lies beyond it: a branch made at
//! another's head costs no walk.
//!
//! **A version read as a tree.** A version's head and its root tree are read from its commit row, and a path is
//! resolved one tree a component down ([`Repository::entry`]): no walk of history and none of the tree. Making a
//! green's maps (A-534) from a commit that is no version's head is receive-pack's create (piece 6).
//!
//! **Bounds.** Every byte the store keeps is in the shared store's derived budget: the objects and the index as
//! `slates-vcs` charges them, and the versions' sets and the head map as this store charges them ([`Repository`]); a
//! commit past the budget is refused with nothing changed. Nothing here is a tuning constant: positions are counted,
//! generation numbers computed, and the walks bounded by the repository they read.
//!
//! **Owed (A-614's parts 2 and 3, and later pieces):** each version's D(k) kept encoded as a pack fragment and the base
//! pack, with the learned delta window and depth, zlib level and rebuild point (part 2); the changed-path Bloom filters
//! of the commit rows (part 3); the walks and a commit sliced under the shard's budget, and their transient sets charged;
//! the versions' records kept as content, replicated as the index's tables are.

mod deltas;
mod fragment;
pub mod learn;
mod naming;
mod positions;
mod push;
mod repository;
pub mod rows;
mod upload;

pub use deltas::delta_of;
pub use fragment::{FragmentRefused, Placed, Stored};
pub use naming::{COLLISION_WORDS, GitNaming, NameRefused, kind_of};
pub use positions::Positions;
pub use push::{Entries, Incoming, Push, PushRefused};
pub use repository::{Found, Move, Repository, StoreCounters, StoreRefused};
pub use upload::{Answered, Progress, Turn, Upload, UploadCounters, Versions};

#[cfg(test)]
#[path = "../tests/store/mod.rs"]
mod tests;
