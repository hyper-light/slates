//! Commits by use: headers, ids, identities, signatures, the payload a signature covers, a merged tag, encodings, extra
//! headers, and every finding of an identity line.

use super::super::commit::Commit;
use super::super::tag::Tag;
use super::super::{FsckId, Header, Ident};
use super::{FORMATS, ample, golden};
use crate::object_name::ObjectFormat;

/// The identity the fixture script gives every author.
const AUTHOR: Ident<'static> = Ident {
  name: b"A U Thor",
  email: b"author@example.com",
  seconds: 1_112_911_993,
  zone: b"+0000",
};

/// A-590 (commit.c's `parse_commit_buffer` reads `tree` then each `parent`; `fixtures/fsck` `ok-commit-merge`, written
/// by `git commit-tree`). Do: read the merge stock git wrote, in both formats, and write it back. Expect: its tree the
/// tree git wrote with every mode, its two parents in order (the root commit first), its author and committer read
/// into their parts, its message, and the same bytes back.
#[test]
fn a_commit_is_read_header_by_header_and_written_back_byte_for_byte() {
  for format in FORMATS {
    let golden = golden(format, "ok-commit-merge");
    let commit = Commit::parse(golden.bytes, &mut ample()).unwrap();
    merge_reads_header_by_header(format, &commit);
    merge_writes_back_byte_for_byte(format, &commit, golden.bytes);
  }
}

/// The merge's tree, its two parents in order, its author and committer in their parts, and its message.
fn merge_reads_header_by_header(format: ObjectFormat, commit: &Commit<'_>) {
  assert_eq!(
    commit.tree(format),
    Some(super::golden(format, "ok-tree").id)
  );
  let parents: Vec<_> = commit.parents(format).collect();
  assert_eq!(parents.len(), 2);
  assert_eq!(parents[0], super::golden(format, "ok-commit-root").id);
  assert_eq!(Ident::parse(commit.author().unwrap()), Ok(AUTHOR));
  let committer = Ident::parse(commit.committer().unwrap()).unwrap();
  assert_eq!(
    (committer.name, committer.email),
    (&b"C O Mitter"[..], &b"committer@example.com"[..])
  );
  assert_eq!(commit.message(), Some(&b"merge\n"[..]));
  assert_eq!(commit.encoding(), None);
  assert_eq!(commit.extra_headers().count(), 0);
}

/// The merge written back as `bytes`, and no tree read from it in the other format.
fn merge_writes_back_byte_for_byte(format: ObjectFormat, commit: &Commit<'_>, bytes: &[u8]) {
  let mut written = Vec::new();
  commit.write(&mut written);
  assert_eq!(written, bytes);
  let other = match format {
    ObjectFormat::Sha1 => ObjectFormat::Sha256,
    ObjectFormat::Sha256 => ObjectFormat::Sha1,
  };
  assert_eq!(
    commit.tree(other),
    None,
    "an id of the other format's length is no id"
  );
}

/// A-590 (gitformat-signature; commit.c's `parse_buffer_signed_by_header`; t1450 "commit with multiple signatures is
/// okay"). Do: read the commit with a `gpgsig` and a `gpgsig-sha256` header, each of continuation lines, in both
/// formats; unfold each format's signature; write the payload the signature covers. Expect: SHA-1's signature in
/// `gpgsig` and SHA-256's in `gpgsig-sha256`, each the armored lines with the continuation's marking space taken off;
/// the payload the unsigned commit the script began from, byte for byte; the commit written back whole.
#[test]
fn a_signature_unfolds_to_its_lines_and_covers_the_commit_without_it() {
  let armored = b"-----BEGIN PGP SIGNATURE-----\n VGhpcyBpcyBub3QgcmVhbGx5IGEgc2lnbmF0dXJlLg==\n -----END PGP SIGNATURE-----\n";
  for format in FORMATS {
    let golden = golden(format, "ok-commit-gpgsig");
    let commit = Commit::parse(golden.bytes, &mut ample()).unwrap();
    for (signature_format, name) in [
      (ObjectFormat::Sha1, "gpgsig"),
      (ObjectFormat::Sha256, "gpgsig-sha256"),
    ] {
      let signature = commit.signature(signature_format).unwrap();
      assert_eq!(signature.name, name.as_bytes());
      let mut unfolded = Vec::new();
      signature.unfold(&mut unfolded);
      assert_eq!(unfolded, armored);
    }
    let mut payload = Vec::new();
    commit.write_signed_payload(&mut payload);
    assert_eq!(payload, super::golden(format, "ok-commit-root").bytes);
    let mut written = Vec::new();
    commit.write(&mut written);
    assert_eq!(written, golden.bytes);
    assert_eq!(
      commit.extra_headers().count(),
      0,
      "both signature headers are git's own"
    );
  }
}

/// A commit with every kind of header a signature parser meets: an extra header whose name begins `gpgsig`, the
/// SHA-1 signature header twice, the SHA-256 one, each with a continuation line, and a message.
const SIGNED_HEADERS: &[u8] = b"tree 9bfbe8e1e9b6f4a2b8c41d3a14f5d1c6c5e0b8e1\n\
author A U Thor <author@example.com> 1112911993 +0000\n\
committer C O Mitter <committer@example.com> 1112911993 +0000\n\
gpgsigx extra\n continued\n\
gpgsig one\n first\n\
gpgsig-sha256 other\n second\n\
gpgsig two\n third\n\
mergetag kept\n\
\n\
message\n";

/// A-644 (commit.c's `parse_buffer_signed_by_header`, v2.50.1: a line of the repository's own signature header and its
/// continuation lines are the signature, any other line beginning `gpgsig` is left out with its continuation lines,
/// and everything else is the payload). Do: read a commit holding an extra header named `gpgsigx`, two `gpgsig`
/// headers and a `gpgsig-sha256` one; write each format's signature and the payload. Expect: SHA-1's signature both
/// `gpgsig` headers unfolded in order, SHA-256's its one header; the payload, in either format, the commit without
/// any of the four headers (`gpgsigx` too, as git leaves it out); and a commit with no signature header writes none.
#[test]
fn a_signature_is_every_own_header_and_covers_no_header_named_gpgsig() {
  let commit = Commit::parse(SIGNED_HEADERS, &mut ample()).unwrap();
  let signature = |format| {
    let mut out = Vec::new();
    (commit.write_signature(format, &mut out), out)
  };
  assert_eq!(
    signature(ObjectFormat::Sha1),
    (true, b"one\nfirst\ntwo\nthird\n".to_vec())
  );
  assert_eq!(
    signature(ObjectFormat::Sha256),
    (true, b"other\nsecond\n".to_vec())
  );
  let mut payload = Vec::new();
  commit.write_signed_payload(&mut payload);
  let expected = b"tree 9bfbe8e1e9b6f4a2b8c41d3a14f5d1c6c5e0b8e1\n\
author A U Thor <author@example.com> 1112911993 +0000\n\
committer C O Mitter <committer@example.com> 1112911993 +0000\n\
mergetag kept\n\
\n\
message\n";
  assert_eq!(payload, expected);
  let unsigned = Commit::parse(
    golden(ObjectFormat::Sha1, "ok-commit-root").bytes,
    &mut ample(),
  );
  let mut none = Vec::new();
  assert!(
    !unsigned
      .unwrap()
      .write_signature(ObjectFormat::Sha1, &mut none)
  );
  assert!(none.is_empty());
}

/// A-590 (gitformat-signature, "mergetag"). Do: read the merge of a signed tag, in both formats, and unfold its
/// `mergetag` header. Expect: one, holding the signed tag stock git wrote, byte for byte, its empty lines included;
/// read as a tag, it names the root commit and splits at its signature into the unsigned tag and the block.
#[test]
fn a_mergetag_carries_the_whole_signed_tag() {
  for format in FORMATS {
    let golden = golden(format, "ok-commit-mergetag");
    let commit = Commit::parse(golden.bytes, &mut ample()).unwrap();
    let mergetags: Vec<&Header<'_>> = commit.mergetags().collect();
    assert_eq!(mergetags.len(), 1);
    let mut carried = Vec::new();
    mergetags[0].unfold(&mut carried);
    assert_eq!(carried, super::golden(format, "ok-tag-signed").bytes);
    let tag = Tag::parse(&carried, &mut ample()).unwrap();
    assert_eq!(
      tag.object(format),
      Some(super::golden(format, "ok-commit-root").id)
    );
    let (signed, block) = tag.signature().unwrap();
    assert_eq!(signed, super::golden(format, "ok-tag").bytes);
    assert!(block.starts_with(b"-----BEGIN PGP SIGNATURE-----\n"));
    assert_eq!(commit.message(), Some(&b"merge tag v1\n"[..]));
  }
}

/// A-590. Do: read a commit with an `encoding` header and a message in that encoding, one with an extra header, and
/// one with no message. Expect: the encoding named and the message's bytes as written; the extra header kept by name
/// and value; no message, and no empty line written after the headers; each written back byte for byte.
#[test]
fn an_encoding_an_extra_header_and_no_message_are_kept_as_written() {
  for format in FORMATS {
    let read = |name: &str| {
      let golden = golden(format, name);
      let commit = Commit::parse(golden.bytes, &mut ample()).unwrap();
      let mut written = Vec::new();
      commit.write(&mut written);
      assert_eq!(written, golden.bytes, "{name}");
      commit
    };
    let encoded = read("ok-commit-encoding");
    assert_eq!(encoded.encoding(), Some(&b"ISO-8859-1"[..]));
    assert_eq!(encoded.message(), Some(&b"caf\xe9\n"[..]));
    let extra = read("ok-commit-extra");
    let extras: Vec<&Header<'_>> = extra.extra_headers().collect();
    assert_eq!(
      extras,
      [&Header {
        name: b"x-extra",
        value: Some(b"one value")
      }]
    );
    let bare = read("ok-commit-nomessage");
    assert_eq!(bare.message(), None);
    assert_eq!(bare.headers().len(), 3);
  }
}

/// A-590 (fsck.c's `fsck_ident`, its checks in its order; date.c's `date_overflows`). Do: read identity lines, each
/// well formed or broken the way one check catches. Expect: the well-formed read into their parts (an empty name,
/// blanks before the date, a date of 0 and of `i64::MAX`); each broken one refused with the first finding git reports
/// of it.
#[test]
fn an_identity_is_read_into_its_parts_or_refused_with_gits_first_finding() {
  let read = |line: &'static str| Ident::parse(line.as_bytes());
  assert_eq!(
    read("A U Thor <author@example.com> 1112911993 +0000"),
    Ok(AUTHOR)
  );
  let parts = |line| read(line).map(|ident| (ident.name, ident.email, ident.seconds, ident.zone));
  assert_eq!(
    parts(" <a@b> 1 -0130"),
    Ok((&b""[..], &b"a@b"[..], 1, &b"-0130"[..]))
  );
  assert_eq!(
    parts("A <a@b> \t 0 +0000"),
    Ok((&b"A"[..], &b"a@b"[..], 0, &b"+0000"[..]))
  );
  let most = u64::try_from(i64::MAX).unwrap();
  assert_eq!(
    read("A <a@b> 9223372036854775807 +0000").map(|ident| ident.seconds),
    Ok(most)
  );
  let refused = [
    ("<a@b> 1 +0000", FsckId::MissingNameBeforeEmail),
    ("A a@b> 1 +0000", FsckId::BadName),
    ("A U Thor", FsckId::MissingEmail),
    ("A<a@b> 1 +0000", FsckId::MissingSpaceBeforeEmail),
    ("A <a@b 1 +0000", FsckId::BadEmail),
    ("A <a<b> 1 +0000", FsckId::BadEmail),
    ("A <a@b>1 +0000", FsckId::MissingSpaceBeforeDate),
    ("A <a@b> x +0000", FsckId::BadDate),
    ("A <a@b> ", FsckId::BadDate),
    ("A <a@b> 1x +0000", FsckId::BadDate),
    ("A <a@b> 01 +0000", FsckId::ZeroPaddedDate),
    ("A <a@b> 9223372036854775808 +0000", FsckId::BadDateOverflow),
    (
      "A <a@b> 18446744073709551617 +0000",
      FsckId::BadDateOverflow,
    ),
    ("A <a@b> 1 0000", FsckId::BadTimezone),
    ("A <a@b> 1 +000", FsckId::BadTimezone),
    ("A <a@b> 1 +00000", FsckId::BadTimezone),
    ("A <a@b> 1 +0a00", FsckId::BadTimezone),
  ];
  for (line, finding) in refused {
    assert_eq!(read(line), Err(finding), "{line:?}");
  }
  let mut digits = "A <a@b> ".to_owned();
  digits.push_str(&"9".repeat(100_000));
  digits.push_str(" +0000");
  assert_eq!(
    Ident::parse(digits.as_bytes()),
    Err(FsckId::BadDateOverflow),
    "a date of 100000 digits"
  );
}
