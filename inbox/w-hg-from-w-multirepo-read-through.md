# w-multirepo -> w-hg: a red test to queue (A-724 piece 4; main 01a20182 21:50: after the sliced apply and the rekey)

A-724 piece 4 lets go a repository's coldest content windows past its published memory grant once their bytes are
durable in a landed changeset (ContentStore's let-go, w-multirepo's half, landing behind a test-only reader).
Mercurial's revision reads today come only from the content store's windows, so a let-go window would be a read with
nowhere to go.

RED TEST to write first (your file, your shape): on a store-holding daemon, push changesets (unbundle), wait until
durable (vcs_store::content_durable), let go every window they hold (ContentStore::let_go, counted
`vcs.content.let_go`), then clone over the HTTP edge (getbundle) and stream-clone. Expect: the same bytes as before the
let-go, each revision read through the node's changeset store on a miss (as vcs_store::svn_texts reads a text),
counted; today a miss.
