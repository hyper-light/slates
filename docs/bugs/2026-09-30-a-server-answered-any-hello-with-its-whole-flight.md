# A server answered any hello with its whole flight (AUD-29-49)

## Description

- **An amplifier.** Any source that sent a ClientHello got the server's entire fragmented flight back,
  including the certificate chain. A spoofed source could turn a server into an amplifier. With a wide
  certificate, one 1,200-byte hello drew 120,351 bytes (measured with the limit disabled).
- **Reflection on established sessions.** A raw handshake datagram on an established session drew a
  final-flight resend or a confirmation every time.

## Root cause

- **No account and no validation.** Nothing counted the bytes sent to an unvalidated address, and nothing
  validated the address. RFC 9000 §8.1 bounds a server at three times what it received until the
  address is validated.

## Fix

- **The account.** Server endpoints start with an `Amplification` account. It counts bytes received from
  the peer during the handshake, and it lets a handshake datagram go only while the total sent stays
  within three times that. It ends when a sealed fragment from the peer opens, which validates the peer
  (only a peer that processed this end's flight holds the Handshake keys).
- **Padding.** The plain fragment gains a payload-length field, so padding after the payload is never
  flight bytes. A client pads its Initial-level datagrams to `MIN_DATAGRAM_BYTES`.
- **Resuming.** A flight the allowance stops resumes from `flight_cursor` at the next resend (drawn by the
  peer's next datagram), so later fragments are reached rather than the first ones repeatedly.
- **Established sessions.** They answer raw handshake datagrams at most once per probe timeout.
- **Existing bounds.** Work before authentication is bounded, as before, by the demultiplexer's
  pending-handshake reservation, which cannot consume the authenticated reservation.

## Evidence

- **`crates/transport/tests/amplification.rs`** (a recording relay; the server has a wide certificate).
  - `a_silent_source_draws_at_most_three_times_what_it_sent`: 2,499 bytes for 1,200, with 33 holds.
    It is red with the account disabled: 120,351 bytes and no holds.
  - `a_talking_client_completes_a_flight_larger_than_its_allowance`: it completes after two holds.
- **`endpoint::tests::raw_datagrams_draw_one_answer_per_probe_timeout`.** Fifty raw datagrams draw one
  answer, and a later one a second. It is red without the limit: 50.
- **Suites.** Transport and cluster pass, the fleet suite passes 59/59, and the three-daemon CLI flow
  passes.
