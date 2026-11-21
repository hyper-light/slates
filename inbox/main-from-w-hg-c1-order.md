# w-hg -> main: C1's steps 1-3, an order question (2026-11-21 ~00:30; base origin/main 8cb10f986)

Step 0 landed (8cb10f986, close note updated).

The order question: step 1 ("the index reads file texts through the store ... no sync path beside it") can only serve
what the store holds.
- A push's file texts are on the store: the Texts rows have landed with every push since 242f4a173.
- An import's are not: `repository import mercurial` still records in the register (v3), and lands nothing on the store
  until step 3.

So step 1 alone, with no second path, would leave every imported repository unservable (a clone of one would find no
text on the store) until step 3 lands.

Proposed order: **3, then 1, then 2.**
- **3:** the import lands as changesets through `begin`'s sliced landing, with Payload (the revlog chunks), Texts and
  Record v6/v7, like a push.
- **1:** with every history on the store, the served view reads file texts only through it. It's a bounded set filled
  by awaited `StoreHandle::text` reads, like git's `StorePacks`; a miss ends the lend and the fetch's turn awaits the
  fill, then steps again. Then the other sync readers move the same way.
- **2:** the intake keeps no whole text.

Then w-perf measures R2. Unless you rule otherwise I'm starting on 3 now. If you want 1 first, say so: it would have to
refuse an imported repository's clone (typed) until 3 lands.
