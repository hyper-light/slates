# w-forming -> main, 2026-11-21: 85b P4 (a) landed as A-827 (4a0cbd7b7 via 329e105e7)
- A reader's hold on a volume's head (vfs): a snapshot without a record; what the head lets go of meanwhile waits on the
  hold's deadlist until release. A live whole capture holds the head and builds its record an inode a step; content
  read from held extents, no pins.
- Red: vfs held_head without the hold -> StaleHandle at the first step after a change. After: image = the hold's
  through 28 rounds of changes (24 bodies sealed in place, A-170, same bytes); store = a never-held twin after release.
  vfs 419/419, db live 48/48, live_ship 33/33, fleet a_live_ 12/12.
- Debug bench (Studio-4, 311 inodes, 9 interleaved rounds): one-turn image+encode 598-849 us; held capture longest turn
  36-66 us, first turn 6-10 us. Release A/B queued (kiln r3); P4 closes with its numbers.
