set -uo pipefail
D=$HOME/.cache/g293-a591-src; T=$HOME/.cache/g293-a591-target; L=$HOME/.cache/g293-a591-logs
rm -rf "$D"; mkdir -p "$L"
date -u '+start %Y-%m-%dT%H:%M:%SZ'; uptime
git clone -q --depth 1 -b agents/g293-a591-git-idx https://github.com/hyper-light/slates.git "$D" || exit 2
cd "$D" || exit 2
git log --oneline -1 | cut -c1-100
cat > "$L/b.patch" <<'PATCHEND'
diff --git a/Cargo.toml b/Cargo.toml
index af8980d36..a012ea944 100644
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -119,11 +119,11 @@ aws-lc-rs = { version = "1.18.1", default-features = false, features = ["aws-lc-
 # `std` nor the OID tables are used.
 sha1-checked = { version = "0.10.0", default-features = false }
 # zlib's CRC32 (the IEEE polynomial, reflected), which git's pack index keeps of each packed object and `git
-# verify-pack` checks (§4.17.2, A-591): the crate flate2 itself computes zlib's CRC32 with, so it costs no code beyond
-# what flate2 brings when deflate enters the workspace (§4.17.2 piece 3; approved by main, 2026-11-19, on that reason).
-# slates' own CRC is CRC32C (`slates_wire::crc32c`), another polynomial. Default features: `std` lets it pick ARMv8's
-# CRC instructions or x86-64's carry-less multiply at run time, with a table for the rest, and slates-git keeps no
-# `unsafe` of its own.
+# verify-pack` checks (§4.17.2, A-591): the crate flate2 itself computes zlib's CRC32 with, and flate2 is the
+# workspace's deflate API (A-607), so it costs no code beyond what flate2 brings (approved by main, 2026-11-19, on that
+# reason). slates' own CRC is CRC32C (`slates_wire::crc32c`), another polynomial. Default features: `std` lets it pick
+# ARMv8's CRC instructions or x86-64's carry-less multiply at run time, with a table for the rest, and slates-git keeps
+# no `unsafe` of its own.
 crc32fast = "1"
 # The shared crates from ../hyper-raft, vendored at the revision in vendor/hyper-raft/SNAPSHOT (A-67).
 hyper-datagram = { path = "vendor/hyper-raft/hyper-datagram" }
diff --git a/crates/git/src/pack.rs b/crates/git/src/pack.rs
index b896b4ae4..7814ec1a2 100644
--- a/crates/git/src/pack.rs
+++ b/crates/git/src/pack.rs
@@ -32,6 +32,7 @@
 
 pub mod idx;
 
+use crate::entry::PACK_HEADER_LEN;
 use crate::object_name::{CollisionAttack, Hasher, ObjectFormat, ObjectId};
 use crate::out::Out;
 use crate::to_u64;
@@ -54,13 +55,11 @@ const WORD: usize = core::mem::size_of::<u32>();
 /// Derived: where the version begins, after the signature.
 const VERSION_AT: usize = SIGNATURE.len();
 
-/// Derived: where the count begins, after the version.
+/// Derived: where the count begins, after the version. The header ends with the count, at A-582's
+/// [`PACK_HEADER_LEN`], one number for the pack's header and for its entries' bases: the first object begins there, so
+/// no object of a pack begins before it.
 const COUNT_AT: usize = VERSION_AT.saturating_add(WORD);
 
-/// Derived: the header's length, the signature, the version and the count. The first object begins here, so no object
-/// of a pack begins before it.
-pub const HEADER_LEN: usize = COUNT_AT.saturating_add(WORD);
-
 /// A pack's version.
 #[derive(Clone, Copy, Debug, PartialEq, Eq)]
 pub enum Version {
@@ -145,9 +144,9 @@ impl<'a> Pack<'a> {
 
   /// Its objects, back to back: every byte between the header and the trailer.
   pub fn objects(&self) -> &'a [u8] {
-    self
-      .bytes
-      .get(HEADER_LEN..self.trailer_at())
+    usize::try_from(PACK_HEADER_LEN)
+      .ok()
+      .and_then(|start| self.bytes.get(start..self.trailer_at()))
       .unwrap_or_default()
   }
 }
@@ -192,13 +191,13 @@ pub fn read_header(bytes: &[u8]) -> Result<Option<Header>, Refusal> {
 /// checksum of the bytes before it, verified before any of them is read, then its header.
 pub fn check(bytes: &[u8], format: ObjectFormat) -> Result<Pack<'_>, Refusal> {
   let truncated = Refusal::Truncated {
-    needed: to_u64(HEADER_LEN.saturating_add(format.raw_len())),
+    needed: PACK_HEADER_LEN.saturating_add(to_u64(format.raw_len())),
     left: to_u64(bytes.len()),
   };
   let trailer_at = bytes
     .len()
     .checked_sub(format.raw_len())
-    .filter(|at| *at >= HEADER_LEN)
+    .filter(|at| to_u64(*at) >= PACK_HEADER_LEN)
     .ok_or(truncated)?;
   let (guarded, trailer) = bytes.split_at_checked(trailer_at).ok_or(truncated)?;
   let checksum = checksum_of(format, guarded).map_err(Refusal::Collision)?;
diff --git a/crates/git/src/pack/idx.rs b/crates/git/src/pack/idx.rs
index 34ad047c9..34b41a5aa 100644
--- a/crates/git/src/pack/idx.rs
+++ b/crates/git/src/pack/idx.rs
@@ -49,7 +49,8 @@
 //! allocates nothing; [`Idx::check_pack`] orders the objects by offset in a table it charges to the caller's
 //! [`Admission`] before it is built; [`write`] sorts the caller's entries in place and allocates nothing.
 
-use super::{Checksummed, HEADER_LEN, Pack, checksum_of};
+use super::{Checksummed, Pack, checksum_of};
+use crate::entry::PACK_HEADER_LEN;
 use crate::object_name::{CollisionAttack, ObjectFormat, ObjectId};
 use crate::out::Out;
 use crate::{Admission, PastAdmission, to_u64};
@@ -457,7 +458,7 @@ impl<'a> Idx<'a> {
         });
       }
       let offset = self.offset(position).unwrap_or_default();
-      if offset < to_u64(HEADER_LEN) {
+      if offset < PACK_HEADER_LEN {
         return Err(Refusal::OffsetInHeader {
           position: to_u64(position),
           offset,
@@ -547,6 +548,12 @@ impl<'a> Idx<'a> {
     None
   }
 
+  /// Where in the pack the object named `id` begins, or `None` when the index does not hold it: [`Idx::find`], then
+  /// [`Idx::offset`]. The lookup from id to offset a pack reader resolves a reference delta's base by (A-608).
+  pub fn offset_of(&self, id: &ObjectId) -> Option<u64> {
+    self.offset(self.find(id)?)
+  }
+
   /// The copy of its pack's trailer.
   pub fn pack_checksum(&self) -> ObjectId {
     self.pack_checksum
@@ -608,9 +615,9 @@ impl<'a> Idx<'a> {
   fn check_spans(&self, pack: &Pack<'_>, by_offset: &[(u64, usize)]) -> Result<(), Mismatch> {
     let end = to_u64(pack.trailer_at());
     let first = by_offset.first().map_or(end, |(offset, _)| *offset);
-    if first != to_u64(HEADER_LEN) {
+    if first != PACK_HEADER_LEN {
       return Err(Mismatch::Unindexed {
-        at: to_u64(HEADER_LEN),
+        at: PACK_HEADER_LEN,
       });
     }
     let mut objects = by_offset.iter().peekable();
@@ -709,7 +716,7 @@ fn check_entries(
   }
   if let Some(position) = entries
     .iter()
-    .position(|entry| entry.offset < to_u64(HEADER_LEN))
+    .position(|entry| entry.offset < PACK_HEADER_LEN)
   {
     return Err(WriteRefusal::OffsetInHeader {
       position: to_u64(position),
diff --git a/crates/git/src/tests/pack.rs b/crates/git/src/tests/pack.rs
index 7ee961535..6f95941ff 100644
--- a/crates/git/src/tests/pack.rs
+++ b/crates/git/src/tests/pack.rs
@@ -1,7 +1,8 @@
 //! A-591: a pack's header and trailer, held to the packs stock git 2.50.1 wrote (`fixtures/make-fixtures.sh` and
 //! `fixtures/make-index-fixtures.sh`, run on the Mac Studio on 2026-11-19) and to hostile bytes.
 
-use super::{HEADER_LEN, Header, Refusal, SIGNATURE, Version, Writer, check, read_header};
+use super::{Header, Refusal, SIGNATURE, Version, Writer, check, read_header};
+use crate::entry::PACK_HEADER_LEN;
 use crate::object_name::{Hasher, ObjectFormat, ObjectId};
 use crate::out::{Measure, Out};
 
@@ -29,6 +30,11 @@ const EMPTY_PACK_HEADER: [u8; 12] = *b"PACK\x00\x00\x00\x02\x00\x00\x00\x00";
 /// That empty pack's trailer, as `xxd` showed it.
 const EMPTY_PACK_TRAILER: &str = "029d08823bd8a8eab510ad6ac75c823cfd3ed31e";
 
+/// A pack header's length, A-582's [`PACK_HEADER_LEN`], as an index into bytes.
+pub(crate) fn header_len() -> usize {
+  usize::try_from(PACK_HEADER_LEN).unwrap()
+}
+
 /// The checksum `format` takes of `bytes`.
 pub(crate) fn checksum(format: ObjectFormat, bytes: &[u8]) -> ObjectId {
   let mut hasher = Hasher::raw(format);
@@ -67,7 +73,7 @@ fn each_pack_git_wrote_is_checked_and_its_header_read() {
       checksum(format, &bytes[..trailer_at]),
       "{name}"
     );
-    assert_eq!(pack.objects(), &bytes[HEADER_LEN..trailer_at], "{name}");
+    assert_eq!(pack.objects(), &bytes[header_len()..trailer_at], "{name}");
     assert_eq!(pack.bytes(), bytes, "{name}");
     assert_eq!(pack.format(), format, "{name}");
   }
@@ -100,9 +106,9 @@ fn a_pack_of_no_objects_is_the_one_git_writes() {
   let trailer = Writer::new(ObjectFormat::Sha1, 0, &mut out)
     .finish()
     .unwrap();
-  assert_eq!(&out[..HEADER_LEN], &EMPTY_PACK_HEADER[..]);
+  assert_eq!(&out[..header_len()], &EMPTY_PACK_HEADER[..]);
   assert_eq!(trailer.to_string(), EMPTY_PACK_TRAILER);
-  assert_eq!(&out[HEADER_LEN..], trailer.as_bytes());
+  assert_eq!(&out[header_len()..], trailer.as_bytes());
   let pack = check(&out, ObjectFormat::Sha1).unwrap();
   assert_eq!(pack.header().objects, 0);
   assert!(pack.objects().is_empty());
@@ -112,14 +118,14 @@ fn a_pack_of_no_objects_is_the_one_git_writes() {
 /// twelfth byte has arrived, then the header, whatever follows.
 #[test]
 fn a_header_is_read_as_its_bytes_arrive() {
-  for len in 0..HEADER_LEN {
+  for len in 0..header_len() {
     assert_eq!(read_header(&OFS_PACK[..len]), Ok(None), "{len} bytes");
   }
   let header = Header {
     version: Version::Two,
     objects: OBJECTS,
   };
-  assert_eq!(read_header(&OFS_PACK[..HEADER_LEN]), Ok(Some(header)));
+  assert_eq!(read_header(&OFS_PACK[..header_len()]), Ok(Some(header)));
   assert_eq!(read_header(OFS_PACK), Ok(Some(header)));
 }
 
@@ -128,7 +134,7 @@ fn a_header_is_read_as_its_bytes_arrive() {
 #[test]
 fn a_wrong_signature_byte_is_refused_as_soon_as_it_arrives() {
   for at in 0..SIGNATURE.len() {
-    let mut bytes = OFS_PACK[..HEADER_LEN].to_vec();
+    let mut bytes = OFS_PACK[..header_len()].to_vec();
     bytes[at] ^= 0x20;
     let refusal = Err(Refusal::Signature { at: at as u64 });
     assert_eq!(read_header(&bytes[..=at]), refusal, "byte {at}");
@@ -193,8 +199,8 @@ fn a_flipped_bit_anywhere_in_a_pack_is_refused_by_its_trailer() {
 #[test]
 fn a_pack_shorter_than_its_header_and_trailer_is_refused() {
   for (name, bytes, format) in PACKS {
-    let needed = (HEADER_LEN + format.raw_len()) as u64;
-    for len in 0..HEADER_LEN + format.raw_len() {
+    let needed = PACK_HEADER_LEN + format.raw_len() as u64;
+    for len in 0..header_len() + format.raw_len() {
       assert_eq!(
         check(&bytes[..len], format),
         Err(Refusal::Truncated {
diff --git a/crates/git/src/tests/pack_idx.rs b/crates/git/src/tests/pack_idx.rs
index a66e10f64..12f9c38b5 100644
--- a/crates/git/src/tests/pack_idx.rs
+++ b/crates/git/src/tests/pack_idx.rs
@@ -7,10 +7,11 @@ use std::collections::BTreeMap;
 use proptest::prelude::*;
 
 use super::{Entry, Idx, MAGIC, Mismatch, Refusal, WriteRefusal, write, write_above, written_len};
+use crate::entry::PACK_HEADER_LEN;
 use crate::object_name::{ObjectFormat, ObjectId};
 use crate::out::{Measure, Out};
 use crate::pack::tests::{OFS_PACK, REF_PACK, SHA256_PACK, checksum, resign};
-use crate::pack::{HEADER_LEN, Writer, check};
+use crate::pack::{Writer, check};
 use crate::{Admission, PastAdmission};
 
 /// `ofs.pack`'s index, as `git pack-objects` wrote it.
@@ -174,8 +175,8 @@ fn room(count: usize) -> u64 {
 }
 
 /// A-591: do decode each index git wrote and hold it to its pack. Expect git's twelve ids in order, each found at the
-/// offset git listed and spanning the bytes git listed (the 8-byte table read for `ofs-large.idx`), the pack's trailer
-/// copied, the pack held, and the exact room charged.
+/// offset git listed, by position and by id, and spanning the bytes git listed (the 8-byte table read for
+/// `ofs-large.idx`), the pack's trailer copied, the pack held, and the exact room charged.
 #[test]
 fn each_index_git_wrote_reads_as_git_listed_it() {
   for golden in GOLDEN {
@@ -204,6 +205,7 @@ fn each_index_git_wrote_reads_as_git_listed_it() {
         (object.id, object.offset),
         "{name}"
       );
+      assert_eq!(idx.offset_of(&object.id), Some(object.offset), "{name}");
       let next = offsets
         .iter()
         .copied()
@@ -622,7 +624,7 @@ fn shared_offsets_and_bytes_before_the_first_object_are_refused() {
   let pack = check(OFS_PACK, format).unwrap();
   let idx = Idx::decode(OFS_IDX, format).unwrap();
   let first = (0..OBJECTS)
-    .find(|position| idx.offset(*position) == Some(HEADER_LEN as u64))
+    .find(|position| idx.offset(*position) == Some(PACK_HEADER_LEN))
     .unwrap();
   let shared_offset = idx.offset(4).unwrap();
   let shared = changed(OFS_IDX, format, |bytes| {
@@ -776,11 +778,7 @@ type Generated = (Vec<u8>, u32, u64);
 fn model(format: ObjectFormat, objects: &[Generated]) -> BTreeMap<Vec<u8>, (u32, u64)> {
   let mut model = BTreeMap::new();
   for (index, (id, crc32, offset)) in objects.iter().enumerate() {
-    let offset = if index == 0 {
-      HEADER_LEN as u64
-    } else {
-      *offset
-    };
+    let offset = if index == 0 { PACK_HEADER_LEN } else { *offset };
     model
       .entry(id[..format.raw_len()].to_vec())
       .or_insert((*crc32, offset));
@@ -797,7 +795,7 @@ fn synthetic_pack(
   let mut pack = Vec::new();
   let mut writer = Writer::new(format, blobs.len() as u32, &mut pack);
   let mut entries = Vec::new();
-  let mut offset = HEADER_LEN as u64;
+  let mut offset = PACK_HEADER_LEN;
   for (id, blob) in blobs {
     writer.put(blob);
     entries.push(Entry {
@@ -815,13 +813,13 @@ proptest! {
   #![proptest_config(slates_test_seeds::unseeded(proptest::test_runner::Config::default()))]
 
   /// A-591, model: do write the index of up to 64 generated objects in either hash, handed over in any order, and
-  /// decode it. Expect the model's entries in id order, every id found at its place and an id the model lacks not
-  /// found, the same bytes whatever the order handed over, and the length `written_len` foretold.
+  /// decode it. Expect the model's entries in id order, every id found at its place and its offset, an id the model
+  /// lacks not found, the same bytes whatever the order handed over, and the length `written_len` foretold.
   #[test]
   fn any_entries_written_decode_to_the_model(
     sha256 in any::<bool>(),
     objects in proptest::collection::vec(
-      (proptest::collection::vec(any::<u8>(), 32), any::<u32>(), HEADER_LEN as u64..=u64::MAX),
+      (proptest::collection::vec(any::<u8>(), 32), any::<u32>(), PACK_HEADER_LEN..=u64::MAX),
       0..64,
     ),
     absent in proptest::collection::vec(any::<u8>(), 32),
@@ -851,10 +849,12 @@ proptest! {
     prop_assert_eq!(idx.pack_checksum(), pack_checksum);
     for (position, entry) in expected.iter().enumerate() {
       prop_assert_eq!(idx.find(&entry.id), Some(position));
+      prop_assert_eq!(idx.offset_of(&entry.id), Some(entry.offset));
     }
     let absent = ObjectId::from_bytes(format, &absent[..format.raw_len()]).unwrap();
     if !model.contains_key(absent.as_bytes()) {
       prop_assert_eq!(idx.find(&absent), None);
+      prop_assert_eq!(idx.offset_of(&absent), None);
     }
   }
 
diff --git a/docs/wip/GAPS.md b/docs/wip/GAPS.md
index daa99df26..2edcf45cd 100644
--- a/docs/wip/GAPS.md
+++ b/docs/wip/GAPS.md
@@ -76,9 +76,9 @@
 > --lib`: 54 of 54 on the Mac Studio, the 32 new failing first against stubs. This is A-582's owed "idx v2". Owed: the
 > pack reader (piece 3: entries, inflation, deltas, the streamed trailer, ids checked against objects); a sliced decode
 > and check for a large pack, with the import of a repository on disk (piece 15), which also remakes version 1 indexes;
-> a vector that reaches the checksums' collision refusal (no published collision begins with `PACK` or `\377tOc`);
-> `pack::HEADER_LEN` and g258's `entry::PACK_HEADER_LEN` made one (whichever lands second). Clippy, `xtask` and the
-> lint matrix over the change are main's (run requests).
+> a vector that reaches the checksums' collision refusal (no published collision begins with `PACK` or `\377tOc`).
+> A pack header's length is one number, A-582 part 3's `entry::PACK_HEADER_LEN`, which `pack` uses. Clippy, `xtask` and
+> the lint matrix over the change are main's (run requests).
 
 > **§4.17.3 the Mercurial server: designed (A-499); piece 1 built, its framings (A-514) and its part payloads (A-545);
 > piece 2 built, the protocol codecs (A-569) (2026-11-19; UI goal 91; owner g291).** Built on §4.17.1: a named branch's
diff --git a/docs/wip/SLATES_DESIGN.md b/docs/wip/SLATES_DESIGN.md
index e3d671f9f..a6bcd427f 100644
--- a/docs/wip/SLATES_DESIGN.md
+++ b/docs/wip/SLATES_DESIGN.md
@@ -39523,7 +39523,9 @@ Applied in the same change to:
   count has arrived. `check` holds a whole pack in memory: it must be at least a header and a trailer long, and its
   trailer must be the checksum of every byte before it, verified before any of them is read; then its header is read.
   `Writer` writes git's header at version 2 and the trailer computed as the objects pass, so a fetch's pack needs no
-  second pass (§4.17.2 "A fetch", item 3).
+  second pass (§4.17.2 "A fetch", item 3). The header's length is one number for the pack and its entries, A-582 part
+  3's `entry::PACK_HEADER_LEN`: part 3 landed first, so `pack` and `pack::idx` use its constant, as main asked
+  (2026-11-19).
 - **The index** [B: `gitformat-pack.adoc`, "Version 2 pack-*.idx files"; C: `pack-write.c`'s `write_idx_file` and
   `need_large_offset`, `packfile.c`'s `load_idx` and `nth_packed_object_offset`, `pack-check.c`'s `verify_packfile`].
   `Idx::decode` verifies the index's own checksum first, then the magic `\377tOc` and version 2, then the layout:
@@ -39533,7 +39535,8 @@ Applied in the same change to:
   - the ids strictly ascend, a duplicate refused apart from a step down;
   - each fanout count is the number of ids whose first byte is at most its byte;
   - every large offset names an entry of the table, and no offset lies inside a pack's header.
-  It then answers by position and by id in place, building nothing: the fanout narrows a lookup, then a binary search.
+  It then answers by position and by id in place, building nothing: the fanout narrows a lookup, then a binary search,
+  and `offset_of` gives the lookup from id to offset that the pack reader resolves a reference delta's base by (A-608).
   `Idx::check_pack` holds an index to its pack as `git verify-pack` does short of inflating. It checks one hash, one
   count and one trailer, in `open_packed_git_1`'s order. It orders the objects by offset in a table charged to the
   caller's admission first, and refuses an offset at or past the trailer, two objects at one offset, or bytes after the
@@ -39558,8 +39561,8 @@ Applied in the same change to:
 - **The CRC32 and its crate.** Each index entry keeps zlib's CRC32 (the IEEE polynomial, reflected; `index-pack.c`
   takes zlib's `crc32()` over each entry's bytes as they are used), and slates had
   only CRC32C (`slates_wire::crc32c`). Main approved `crc32fast` (2026-11-19) for one reason: it is the crate flate2
-  itself computes zlib's CRC32 with, and flate2 enters the workspace anyway (A-595's gzip bodies, piece 3's objects), so
-  it brings no code the tree would not hold. Its `std` feature picks ARMv8's CRC instructions or x86-64's carry-less
+  itself computes zlib's CRC32 with, and flate2 is the workspace's deflate API (A-607), so it brings no code the tree
+  would not hold once a crate takes flate2. Its `std` feature picks ARMv8's CRC instructions or x86-64's carry-less
   multiply at run time, with a table for the rest, and slates-git keeps no `unsafe` of its own.
 - **Golden vectors.** The packs, indexes and `git verify-pack -v` listings A-582's `make-fixtures.sh` captured
   (`ofs` and `ref`, SHA-1, with offset and with reference deltas), and `make-index-fixtures.sh`'s, run with git 2.50.1
@@ -39592,6 +39595,4 @@ Applied in the same change to:
 - **Owed.** The pack reader (piece 3): entries, inflation and deltas, the trailer checked as a stream arrives, and an
   index's ids checked against its pack's objects. `Idx::decode` and `Idx::check_pack` run whole, hashing and CRC-ing
   every byte in one call, so a caller on a shard gives them what its slice admits; a sliced form comes with the import
-  of a repository on disk (piece 15), which also remakes a version 1 index from its pack. `pack::HEADER_LEN` and g258's
-  draft `entry::PACK_HEADER_LEN` (A-582 part 3) are one number: whichever lands second points at the other's (main,
-  2026-11-19).
+  of a repository on disk (piece 15), which also remakes a version 1 index from its pack.
PATCHEND
git apply --whitespace=nowarn "$L/b.patch" || { echo "patch did not apply"; exit 3; }
echo "=== tree with change B applied:"; git status --short
export CARGO_TARGET_DIR="$T"
echo "=== MERGED + B: cargo test --locked -p slates-git --lib"
cargo test --locked -p slates-git --lib > "$L/merged.log" 2>&1; echo "merged exit $?"
command grep -E "^test " "$L/merged.log" | awk '{print $NF}' | sort | uniq -c
command grep -E "^test [a-z_]+::" "$L/merged.log" | awk '{split($2,a,"::"); print a[1]}' | sort | uniq -c
command grep -E "^test pack" "$L/merged.log" | command grep -v " ok$"
command grep -E "^test result|^error|FAILED|panicked|^warning: (unused|dead)" "$L/merged.log" | head -20
echo "=== fmt"; cargo fmt -p slates-git -- --check > "$L/fmt3.log" 2>&1; echo "fmt exit $?"; head -20 "$L/fmt3.log"
echo "=== tree after the run:"; git status --short
date -u '+end %Y-%m-%dT%H:%M:%SZ'; uptime
