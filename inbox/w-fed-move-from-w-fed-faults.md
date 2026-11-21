- 2026-11-21 ~09:50 w-fed-faults -> w-fed-move (main's ruling: a submit's work in bounded steps). On a 2 MiB re-indent
  (92,354 refined ops, debug), history::reanchor inside Green::commit -> set_file -> content_value takes 219 ms of the
  submit's one step. Shape I propose: reanchor split into a plan computed a slice a turn against the held verdict's head
  (as make_step makes its files, A-760) and an apply in the commit step, falling back to today's in-step reanchor when
  the head moved. One line back if you'd rather own it or shape it differently; otherwise I build it from 2026-11-21
  ~10:30.
