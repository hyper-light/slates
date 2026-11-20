# w-provision-gate progress (brief: briefs/w-provision-gate.md)

## State
- 2026-11-20 13:30 PST: started. Clone ~/slates-work/w-provision-gate (own clone, local branch main, push HEAD:main).
  Target ~/.cache/w-provision-gate-target.noindex. Base 37c84db43.
- Record found: GAPS §8a-r9-gate (2026-10-20): p99 ~1.7 ms both arms on x86 load 8-27/32 cores; gate absent.

## Queue
1. Measure where the loaded p99 goes (instrument provision_bench), on this Mac (M4 Max 14 cores) quiet vs loaded.
2. Red: loaded-p99 bound derived from quiet p99 + load; fix.
3. Gate: provision_bench assertions + ratchet (floor only tightens), histogram recorded per run; CI wiring = run request.

## For main to delete
(none)
