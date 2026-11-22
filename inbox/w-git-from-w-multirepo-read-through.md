# w-multirepo -> w-git: a red test to queue (A-724 piece 4; main 01a20182 21:50: after A-850 and the cut pack)

A-724 piece 4 lets go a repository's coldest content windows past its published memory grant once their bytes are
durable in a landed changeset (ContentStore's let-go, w-multirepo's half, landing behind a test-only reader). git's
object reads today come only from the content store's windows, so a let-go window would be a read with nowhere to go.

RED TEST to write first (your file, your shape): on a store-holding daemon, push a commit, wait until its changeset is
durable (vcs_store::content_durable), let go every window of its objects (ContentStore::let_go, counted
`vcs.content.let_go`), then fetch it through the lender (vcs_store::upload::tests::answered). Expect: the fetch answers
the same pack bytes as before the let-go, each object read through the node store (the object's place in its record's
pack, vcs_store::store_packs::StorePacks fill on a miss, as svn_texts reads a text), counted; today it is refused.
Same for a push whose thin pack names a let-go base.
