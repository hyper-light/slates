# w-svn -> main, 2026-11-21 ~02:55 PST: C2 stage B part 2 (b), what it is in the code, and the order I am taking

Where Subversion history sits in RAM today (read in the code at d31502d48):
1. the records' kept texts (`migration::Repository::texts`, `KeptText{base, delta, windows}`): every text an import or a
   contents-carrying record brought, as a skip-delta svndiff, bytes in RAM (git-2000: 34.8 MB);
2. the content store's copy of each stored text a commit brought, until the deltify job turns it into (1);
3. at recovery, `fetch_stored_texts` reads every stored text the records name back into the content store;
4. the green's held head texts (merge engine; not mine, named in GAPS before).

Plan, smallest first, each its own landed piece with a store test:
- (b.1) the content store lets go a commit's stored texts once their changeset has landed (the store holds them, and
  every reader now waits for a piece through it, (a)); recovery stops reading them back (3). The deltify job then reads
  its target through the store (built in d31502d48).
- (b.2) the records' kept texts leave RAM: the store already has a slot for a delta (`texts::TextEntry.base`), so a
  record's texts land as Payload deltas with Texts rows (base = the skip-delta predecessor), the records keep only each
  text's window index (target start, length, encoding offset: 3 words per 100 KiB window), and a kept text's window is
  read through the store with the same wait (`Wanted{text, offset}` naming the delta's bytes). The deltify job's entry
  lands its delta the same way instead of inline in a Record. Disk stays delta-sized (git-2000: ~35 MB, against 437 MiB
  were texts stored whole).
- Rejected on the numbers above: storing every version whole and dropping the deltas (12x the store's disk).
Starting (b.1) now; (b.2) after it unless you say otherwise.
