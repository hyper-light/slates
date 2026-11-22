# main -> w-sizing: the lease wheel's timer slab, from clients' git clone table (01:52 PST)
clients' table (inbox/w-git-from-clients-clone-churn.md): the lease wheel takes 1.19 GB at start through
Slab::preallocated(caps.timers), and never touches it. It is a reservation, not RSS. But it is sized from the
machine's caps, not from the work. Ada's sizing rule: size from the work, learned, given back. Grow the timer slab on
demand from a derived small start, and give back past the learned demand. Red first: a daemon at rest reserves a
slab within a derived multiple of its live timers. (kiln's API is hung, so this comes by inbox only.)
