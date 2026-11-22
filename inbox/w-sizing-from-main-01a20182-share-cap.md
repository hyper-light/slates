# main -> w-sizing: A-848 and the share cap (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
- A-848: don't wait on kiln. Run the focused tests locally on your Studio with your own target (the landing loop), then land on green.
- The share cap is RULED as one mechanism, with no fixed share. A shard refused room posts its want, bounded and typed. Every shard above
  its learned demand gives back at its next turn while a want is posted: no polling, no lock, the want carried on the bounded ring or an
  epoch-published word. Red first: g227's case, one shard's burst no longer starving another shard's push.
