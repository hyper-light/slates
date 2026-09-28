# A released test port was taken before the daemon bound it

Date: 2026-09-28. Design: §4.8 (fleet membership, boot step 6), §4.10a (the runtime's UDP socket), §4.14
(refusals are counted, never silent). Found by the fleet suite running 1,152 s instead of about 235 s: three
waits of 300 s each (the harness's frozen-daemon backstop), then the failure of
`a_volume_on_a_non_control_shard_replicates_its_content_and_places`.

## Evidence

- **Reproduction.** Six concurrent copies of that one test, each looping, sampled any run longer than 40 s.
  The first hunt hit a hang after about 553 runs, the second after about 1,040.
- **The stuck daemon never ran a period.** With `SLATES_FLEET_TRACE` on, its per-second line read
  `periods=0 refusals={"fleet.bind": 1}` on every one of 40 lines. Its peer's line read
  `refusals={"fleet.dial.fault": 11, "fleet.dial.redial": 1}`.
- **The fixture released its ports before the daemon bound them.** `four_free_ports` bound four sockets to
  `127.0.0.1:0`, read the ports, and dropped the sockets. Each daemon bound its port by number later, on its
  control shard. In between, anything on the machine could take the port: another test copy, another test's
  allocator, or an OS-assigned port for any other socket. The daemon's bind then failed. It counted
  `fleet.bind`, its membership loop ended, and its peer dialed nothing forever.

## Root cause

The allocation is a check-then-use race (TOCTOU): the port was free when checked and in use when used. A
restart has the same race. A daemon that stopped released its ports, and its restart bound them again by
number.

## Fix

The daemon never binds a port it was handed by number. Nothing ever gives a port up between learning it and
serving on it.

- **Adoption in the runtime.** `slates_rt::udp::UdpSocket::adopt` takes an already-bound OS datagram socket
  (the socket-activation pattern). It refuses a socket that is not a datagram socket, is not bound to an IPv4
  port, or is on the simulation driver. It is a paired `#[cfg]` seam: `netsys::adopt` takes an `OwnedFd` on
  Unix and an `OwnedSocket` on Windows.
- **The daemon takes bound sockets.** `FleetTransport` carries its serve point as a type parameter:
  - a deployment plan carries `ServeAddresses`, and planning binds nothing;
  - the daemon takes `ServeSockets`;
  - `FleetTransport::bind` is the one step between the two. The CLI takes it before the daemon starts.

  An address in use now refuses the start with `ServeBindError`, which names the plane and the address.
  Before, the daemon ran but could never be probed. `fleet.bind` still counts the refusals that are left to
  it: the shard would not keep the identity, a socket could not name its port, or a demultiplexer would not
  start.
- **The fixtures hold their ports.** `free_ports` returns a `PortLease`, and each allocated socket stays
  bound, in a process table, until the test ends. `held_serve_sockets` gives a daemon a duplicate of each
  held socket. The held original keeps the port through the daemon's life, its stop, and any restart. The
  silent peers of the bounded-budget test are held too, so no other socket can answer as one.

## Tests

- `an_adopted_socket_receives_and_its_port_is_never_released_in_between` (rt): the adopted duplicate
  receives through the driver. No other socket can bind the port while it is held, and that is still true
  after the adopted socket closes.
- `adopting_a_stream_socket_is_refused` (rt): the refusal is by kind.
- `a_serve_socket_that_cannot_be_bound_refuses_the_start_by_name` (fleet): this replaces the test that
  counted `fleet.bind` from a running daemon. That daemon no longer exists, because the bind refuses the
  start.

## Sibling found in the sweep, not fixed here

`crates/cli/tests/cli.rs` `free_port_block` has the same pattern across real processes. It searches a port
block in 20,000–50,000 and drops it, and then the child daemons bind it by number. That range overlaps
Linux's ephemeral range (32,768–60,999), and two concurrent CLI test processes can pick the same block. The
race-free fix for separate processes is to hand the child its bound sockets (inherited descriptors, the
systemd `LISTEN_FDS` protocol), which `UdpSocket::adopt` now makes possible. That is a deployment feature
(`slates daemon` taking inherited serve sockets), recorded in the gap ledger for Ada's call.
