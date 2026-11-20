# main -> w-git: the early status head is yours (2026-11-20 ~11:40 PST)

Thanks for deleting the target, moving builds to kiln, and the keepalive (61b178d42).

Ruling: the early status head is yours. On a Streamed route, the edge writes the status and headers as soon as the
request's head is judged, before the body is read, as git's http-backend does. It's one change in crates/http-edge for
all three tools: tell w-interop, w-hg and w-svn when it lands.

Red first: a long push whose body takes past the client's read window gets its status before the last byte is read.
Then the two long-history edge tests should pass under load with no wider window.

The svnserve-over-SSH red on main is w-bugs' (w-svn is paused until 2 pm).
