# Copa froze a window slow start had overshot

Date: 2026-09-28. Contracts: Copa (Arun & Balakrishnan, NSDI 2018) §2.1; RFC 9002 §7.8 (an unused window
must not grow). Found by the congestion grid on `2e60c2f`: at 100 Mbit/s and 20 ms with no loss, Copa's
steady ping p99 was 103 ms, against 33–40 ms for every other controller, and Copa overflowed the one-BDP
bottleneck queue about 92,500 times per run while the others dropped about 100.

## Evidence

A temporary trace in Copa's mode test (every mode change and every 100 virtual ms, run once, removed):

- **The window was frozen at 1.5 MB** (six BDPs at 250 kB) for the whole run.
- **The queue stayed full**: smoothed RTT about 40 ms, the path's 20 ms plus a full 20 ms queue.
- **The velocity never left 1.**
- **The competitive mode was not the cause** (a first hypothesis, refuted): Copa was competitive in 2.7 %
  of samples, and `1/δ` stayed at its default of 2.

## Root cause

Copa's `on_ack` returned early whenever the sender was not window-limited, applying RFC 9002 §7.8's growth
guard to **every** window update, decreases included. Slow start overshot to 1.5 MB. The bottleneck then
dropped packets, and losses kept the flight below that window, so the sender never looked window-limited
and Copa never applied the decrease its delay signal called for. Pacing at `2·cwnd/RTTstanding` from a
window six BDPs wide kept the queue overflowing.

## Fix

The guard now blocks only increases: an unused window still does not grow, but a standing queue shrinks it.
`a_window_the_sender_is_not_filling_still_shrinks_but_never_grows` fails with the old guard and passes with
the fix.

## Result

At 100 Mbit/s, 20 ms, no loss, Copa's steady p99 fell from 103 ms to 20.5 ms (the RTT), queue drops per run
from 92,640 to 9,676 (now only the one-time slow-start overshoot), and goodput rose from 82 % to 88 % of
capacity.

## Siblings

NewReno and CUBIC only grow in `on_ack` (they decrease on loss events), so a whole-update guard there is
correct. BBR follows its specification's `is_cwnd_limited`.
