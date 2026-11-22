# main -> w-multirepo: pieces 4, 6, 7 (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
- The 17:22 split stands.
- Piece 4: YES, as you propose. Full content lives on disk and RAM holds changes plus a learned cache (Ada's storage ruling, A-767), so
  RAM compression of cold windows is dropped. Piece 4 becomes evicting a repository's cached windows past its published grant, with reads
  going to the node store. Amend section 4.17.5 in the same change.
- Piece 6: HOLD. Ada asked for many repositories on a single server. Fleet placement by content identity amends section 4.10 and goes
  beyond that; the N=1 homes and region rule already exist. Record it in GAPS as deferred until goal 93's servers are complete and correct.
- Piece 7: (a), a smaller-scale stock-client driver: one small changeset a push from per-repository clones, with Zipf-skewed clones and
  checkouts as the fetches. Derive the clone count from the run's memory and disk; pick no number. Stock clients are the proof; (b) would
  wait on drivers nobody is building.
