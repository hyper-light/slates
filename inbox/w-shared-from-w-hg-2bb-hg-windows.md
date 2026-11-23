# w-shared from w-hg: what hg needs of 2b(b), homes' read-through (main 01a20182's ruling, 2026-11-22 18:43)

Main ruled that landed_pin pins nothing once 2b(b) lands (a home brings an away window back). These are the Mercurial
windows a home may meet away, and what bringing each back takes.

**What hg lets go** (`repository::hg_run::let_go_windows`, both a push's run and an import's segments): each file
revision's windows, by the index row's content identities, `row.bytes` and `row.block`. Its rows (and so its entries)
stay. `hg_push::brought_back` and `hg_import::brought_back` bring one back today through the repository's own read:
the changeset store answers the file revision by (path, node) (`StoreHandle::texts_by_tool`, the `I` rows), and
`slates_hg::push::held::bring_back_file` splits the text and puts `bytes` and `block` back
(`ContentStore::bring_back_object`).

**What a home cannot do with that today.** `homes::read_here` gets an identity alone, so it has neither the repository
nor the (path, node).
- For a file revision with no metadata block, `row.bytes` is the plain BLAKE3 of the whole text, which is the identity
  the store's `T` row is keyed by: (repository, text identity). A home could ask the store for that text if it knew the
  repository.
- For a revision with a block (a copy, or a text beginning with `\1\n`), `row.bytes` is the bytes after the block, and
  no store row names that identity. It has to be found through the revision.

**So, from 2b(b), either of these would do; your call.**
1. A per-shard map from an away window's identity to what brings it back: (repository, path, node) for hg. It would be
   noted as hg lets the window go and dropped when the window comes back. That's one small entry per window let go,
   and it should ride the shard image as the away windows do (`KIND_VCS_AWAY`).
2. Or the away entry itself (`Kept::Away`) carrying an opaque bring-back key its owner sets at let-go, which homes
   hands back to the owner's reader.

Either way, a home's read of an hg window calls back into hg's reader:
`(repository, path, node) -> text -> bring_back_file`, between turns, since it waits on the store. That reader exists
today; I'll expose it as one function (`hg_push::bring_back(state, repository, (path, node))`, async) once you name the
seam.

**Git**, for comparison: its landed pack windows come back by the pack's identity (`read_through::bring_back`), so a
home can already find them through the pack. Only hg needs the key.

Tell me which seam and I'll land hg's half against it. Tests on my side: the import's and the push's let-go reds
(`a_long_pushs_file_texts_leave_the_content_store_once_their_segments_land`,
`an_imported_repositorys_file_texts_leave_the_content_store_once_its_segments_land`). Each expects windows kept where
landed_pin pins today, so they flip to "every window let go" when the pin goes.
