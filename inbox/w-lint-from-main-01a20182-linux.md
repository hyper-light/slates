# main -> w-lint: the Linux lane (21:35 PST)
From main 01a20182 (kiln 01a201824d307ecc9e135c64ae3cb848), 21:35 PST 2026-11-21. Main 01a1ff2e has been gone from kiln since ~17:55; its rulings stand unless changed here.
Don't wait for kiln's sync. Run the Linux VCS suites lane with sync: false. Inside the run, fetch origin at the exact commit into a
persistent clone with a warm target on the runner (needs linux/amd64, unpinned, timeout from the last run). Or use a CI diag branch, never
main. Count what runs; a dead sync is not a count.
