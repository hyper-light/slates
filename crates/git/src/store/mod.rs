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
//! **D(k) as derived data** (A-647 part 2, [`ReachSets`]). A version's two sets are a type of `slates-vcs`'s derived
//! data: a pure function of the version's record and its parents' values (version `k - 1`'s, and that of the version a
//! remade set came from, which each version records), so any tier derives them with the admission's own walks over the
//! committed index, and the owner keeps the admission's sets under (type, version) with no second walk
//! ([`Repository::reach_value`]). The sets above are unchanged; the type reads them back, identity for identity.
//!
//! **A version read as a tree.** A version's head and its root tree are read from its commit row, and a path is
//! resolved one tree a component down ([`Repository::entry`]): no walk of history and none of the tree. Making a
//! green's maps (A-534) from a commit that is no version's head is receive-pack's create (piece 6).
//!
//! **A protected branch's questions** (A-644, `protect`): how a move changes its ref ([`Repository::change`]: a create,
//! a fast-forward, a rewrite or a delete), judged against the ref's own head with no walk where generation numbers
//! decide it; and which commits a move brings, D(k)'s, with the first merge commit among them
//! ([`Repository::merge_commit`]) and the first no trusted key signed ([`Repository::unsigned_commit`], through a
//! [`CommitSigners`]).
//!
//! **Bounds.** Every byte the store keeps is in the shared store's derived budget: the objects and the index as
//! `slates-vcs` charges them, and the versions' sets and the head map as this store charges them ([`Repository`]); a
//! commit past the budget is refused with nothing changed. Nothing here is a tuning constant: positions are counted,
//! generation numbers computed, and the walks bounded by the repository they read.
//!
//! **upload-pack's pack (A-639, A-653).** A fetch's pack is streamed from what the store keeps ([`Upload`]): the
//! negotiation's runs of versions (A-615, taken through [`Answered`]) from their fragments, then the objects still
//! wanted from the kept parts that hold them (a clone so becomes the base pack and the fragments after it; a part
//! partly wanted, a `blob:limit` fetch's blobs, read entry by entry for its wanted entries alone), then the rest
//! encoded with the fragments' learned rules; each part copied byte for byte when the client reads it as kept, entry
//! by entry otherwise, so no client is sent an offset delta it did not ask for or a base it lacks. A turn does units
//! of work up to the owner's quantum and holds nothing borrowed between turns; every answered pack holds exactly the
//! answer's objects, each once. upload-pack's service writes it through A-640's pack source over it.
//!
//! **Fragments and base packs encoded in turns (A-653 continued (2)).** A version's two fragments and a green's base
//! pack are encoded a unit at a time under the owner's quantum ([`Encoding`]): a pair of trees walked for the bases a
//! reference delta may name, a window's worth of rows read into the encoding's order, or one object encoded, a unit;
//! the parts kept last. The encoding holds no borrow between turns and writes, over any turns, the bytes one pass does.
//!
//! **A hook's repository (A-665, [`hook_view`]).** A git hook runs in a read-only bare repository
//! made from what the store keeps: `HEAD`, `config` and `packed-refs` from the refs a push read,
//! and packs from the base packs and fragments, each whole by itself, with the push's objects in
//! git's quarantine; every file's bytes made when a read reaches them.
//!
//! **Owed (A-614's part 3, and later pieces):** the changed-path Bloom filters of the commit rows (part 3); the walks
//! and a commit sliced under the shard's budget, and their transient sets charged; within an encoding, one object, a
//! pair of trees and a part's put each still a unit whole (`encoding`'s docs); a delta search for an object past a
//! window; the versions' records kept as content, replicated as the index's tables are.

mod deltas;
mod encoding;
mod fragment;
pub mod hook_view;
pub mod learn;
mod naming;
mod positions;
mod push;
mod repository;
pub mod rows;
mod upload;
pub mod views;

pub use deltas::delta_of;
pub use encoding::{Encoding, EncodingCounters};
pub use fragment::{FragmentRefused, Placed, Stored};
pub use naming::{COLLISION_WORDS, GitNaming, NameRefused, kind_of};
pub use positions::Positions;
pub use push::{Entries, Incoming, Push, PushRefused};
pub use repository::graph::{GraphFault, NamedRef, StoreGraph};
pub use repository::protect::{Change, CommitSigners, Unsigned};
pub use repository::reach::{ReachRefused, ReachSets, Sets};
pub use repository::{Found, Move, Repository, StoreCounters, StoreRefused};
pub use upload::{Answered, Progress, Turn, Upload, UploadCounters, Versions};

#[cfg(test)]
#[path = "../tests/store/mod.rs"]
mod tests;
