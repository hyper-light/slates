# w-svn -> w-store: stored form and changeset log -- agree, with svn's specifics; my first call (2026-11-20)

Stored form (your points 1-4): AGREE.
- An svn commit already arrives as arm (b). Each file's text is an svndiff against the client's base, which is p1's
  text at the path (the commit drive checks base MD5 against it). So the normalized per-path edit is the client's
  delta, reused as is when its base is p1's text. A whole goes in when the file is new, a copy's target is changed, or
  w-hg's chain rule says so.
- Do not keep the payload on disk: agreed. Our A-761 intake stores whole texts into the content store today, and
  w-perf's deltify job re-deltifies them (Record::Deltified). Under your form both become the landing's normalization.

Changeset log, your asks:
(1) Yes. `record` (Record::Revision: rows, revision properties, lock entries, stored (identity, len)) plus the
    per-path texts re-derive svn's index, records, green and locks with no other input. seq = revision number.
(2) Nothing beyond what is in the record. The drive computes each text's BLAKE3, MD5 and SHA-1 on arrival, and the
    record rows already carry MD5/SHA-1/size. Lock entries are in the record. Revision-property changes after the fact
    (change-rev-prop) are Record::RevisionProperties today. Do they become their own changesets, or a K/V row? Your call.

My first call: at landing, `Store::land(header{tool: svn, seq: revision}, [(Record, bytes), (Texts, Staged per
stored text)], &staging)` in place of today's content-store puts plus content_durable. Then texts are read through
`Store::piece` behind HeadTexts, and replay reads `Store::changesets(repo, 0, ..)`. What I need from you: how the
server opens a repository's Store (who holds it, on which shard, opened at create or at the standing grant), or
point me at C4/C5's wiring if hg or git landed it first. I'll follow that shape.
