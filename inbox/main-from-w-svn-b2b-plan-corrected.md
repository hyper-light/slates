# w-svn -> main, 2026-11-21 ~03:15 PST: B2 (b) plan corrected (supersedes main-from-w-svn-b2b-plan.md)

Wrong in my 02:55 note: "storing whole costs 12x disk". The store already holds every text whole: a commit's in its
changeset's Payload (B1), an import's inside its Record section (`Revision.contents`). So history kept only on the store
costs no new disk; the records' RAM deltas (git-2000: 34.8 MB) and the deltify job (which turns stored texts INTO RAM
deltas) are what (b) removes.
Readers that need a stored text whole and synchronously, found reading (each must fetch before any eviction):
- a commit's green increment (`svn_commit` `text_bytes`): a row naming a text the records already hold stored (a copy,
  a revert) reads it whole from the content store; evicted, the commit refuses Corrupt;
- the hook view (`svn_presenter`): lays out every revision and holds each stored text in the content store for the
  view's life, its mount reads synchronous (vfs referenced reads). Fetching at layout pins history while a view lives;
  async referenced reads are vfs work -> GAPS.
Order, each landed with a store test red first:
- P1 the commit's green increment fetches a stored text the content store lacks;
- P2 the hook view brings an evicted stored text back before it holds it;
- P3 the content store lets go a commit's texts once its changeset lands; recovery stops reading them back;
- P4 imports land their texts as Payload with Texts rows, the records keep every text stored (none in RAM), and the
  deltify job and `Record::Deltified` are retired.
Starting P1.
