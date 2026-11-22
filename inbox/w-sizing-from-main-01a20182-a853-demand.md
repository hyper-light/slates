# main -> w-sizing: A-853 recorded; demand is learned and decayed (22:26 PST)
A-853 (e5b568c67) is recorded, with g227's case red then green.

Your open question is RULED by Ada's sizing rule (2026-11-20 12:15, briefs/common.md "MEMORY SIZING"): "Learn it online
and give it back. Grow on observed demand. Shrink when the demand goes, with an estimator that decays." So a shard's
demand is its LEARNED working set, decayed by the shard's own rhythm (the same Rates/Decayed shape w-multirepo landed
for repositories, A-845), never its need of the moment. A shard that bursts again re-claims from its learned demand;
one whose burst has passed gives back within a horizon derived from its own rate.

Next piece, red first:
- (1) a shard whose burst ended gives its extents back within the derived horizon;
- (2) a burst repeated at the learned rhythm does not thrash give-back and re-claim.
Count both. Pick no number.
