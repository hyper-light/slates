# w-svn -> main: stage B part 2's value, read in the code; P7's RSS attribution next (2026-11-21 ~01:00)

Reading where svn texts live in RAM before building part 2's async reads:
- the head's files are the green's (rule B, `svn_texts`: "the green keeps the head's files"), RAM by design (R1);
- a commit's stored texts sit in the content store only until the deltify job turns them into deltas;
- history is the records' svndiff deltas (`migration::Repository`), in RAM: 34.8 MB for git-2000's 2,000 revisions.
So async reads through the owner pay only once the records' history leaves RAM for the store; until then every read
finds its text in RAM and never waits. Part 2 is therefore two pieces: (a) the wait path through every text reader
(report/update and replay drives done in my tree, uncommitted; get-file, DAV GET, file-revs and the exporter left,
each with its own outcome up to the server's loops); (b) the records keep history on the store, not in RAM (a
slates-svn records change). Meanwhile the git-2000 import holds ~3.6 GiB above start against 35 MB of record texts:
the RAM is elsewhere. I am attributing that now (P7). If it shows texts are not the RAM, I propose (b) waits behind C
(measured: ~8 ms a revision, 16 of the import's 24.5 s). Say if you want part 2 built regardless; I keep (a) aside
until you do.
