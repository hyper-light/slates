# Enrollment (§4.13 × §4.10a) — how a node gets its fleet keys and identity

> **Status: draft to ratify (2026-09-08).** This connects the *already-designed* pieces — §4.13's
> "a trusted enrollment establishes a scoped identity, never an ambient admin channel" and §4.8's
> "the regional configuration group decides membership" — to the *already-built* fleet-transport
> seams (`schedule.rs`, `accept.rs`, `handshake.rs`). It invents no new trust root; it says where the
> transport's keys come from. Ada ratifies the model before the membership-distribution half is coded.
> The control-plane half — turning an admitted-membership record into this node's `Sealer` and a
> `Keyring` over the enrolled peers — is built (`crates/transport/src/enrollment.rs`) because it needs
> only local key derivation and is testable end to end against the control plane with no network.

## 1. What enrollment must produce

A node cannot speak on either fleet plane until it holds:

1. **A control-plane key context** — the shared **control secret** from which `schedule.rs`
   (HKDF-Expand-Label) derives this node's per-channel `Sealer` and an `Opener` for every peer whose
   sealed control datagrams it must accept. The `Keyring` `accept.rs` consults is exactly this set of
   openers; its population "is owed with enrollment" (the transport's own note). This is that.
2. **A session-plane identity** — the enrolled certificate `handshake.rs` pins (`Identity::from_der`
   is the seam), plus the peer certificates to trust. slates has **no CA PKI** (D-15 direction, RFC
   7250 raw/self-signed keys); trust is by pinning the identities the fleet has admitted.
3. **Its place in membership** — its `HostId`/sender id, its neighbourhood, and the current host
   epoch, so it seals under the right identity and refuses a peer on a stale epoch (D-16 fencing).

## 2. The model (grounded, not invented)

Enrollment **is admission to the §4.8 configuration group's membership**, authorized by a human or a
trusted harness per §4.13 (never inferred from a uid, a command name, or possession of an id). The
configuration group (the hecate Raft dialect, one per region, a root group across regions) already
"decides membership, neighbourhoods, host epochs" — so it is the natural, already-designed authority
that vouches for a node. Admission does three things:

- **Records the joining node's session identity** (its certificate) in the configuration, so every
  member can pin it. Removal/rotation is a configuration change like any membership change (§4.8),
  which is where revocation (`ConsumerRevoked`, a stale epoch) lives.
- **Delivers the control secret** to the admitted node over a channel established by the trusted
  enrollment (§4.13: "a capability delivered and retained outside other agents' reach"), never over
  the ring, MCP, or SDK. The control secret is **fleet-shared** (or per-region): `schedule.rs` already
  derives *distinct* per-sender, per-epoch, per-direction keys from one secret via HKDF, so one shared
  secret yields a full mesh of pairwise-distinct channel keys without distributing a key per pair. A
  compromise scope and a rotation cadence for this secret are **open question (a)** below.
- **Publishes the membership list** (sender ids + certificates + host epochs), from which each node
  builds its `Keyring` (an `Opener` per member) and its set of pinned session certs. A node not in the
  list is an `UnknownSender` the control plane drops before crypto (`accept.rs`), and an un-pinned cert
  fails the session handshake — so **only enrolled peers are reachable on either plane**.

Key rotation and host-epoch advance ride the same path: the configuration bumps the epoch (§4.8
takeover), members re-derive keys at the new `key_epoch` (`schedule.rs` already keys by epoch), and a
peer presenting a lower epoch is refused (D-16). No key is ever a bearer token on a data path (§4.13,
hecate "an id is never a bearer capability" [C]).

## 3. What is built now, and what this slice deliberately does not do

**Built (`enrollment.rs`):** the pure transformation *admitted-membership record → this node's control
keys*. Given the control secret, this node's id, the key epoch, the channel, and the member ids, it
produces this node's `Sealer` and an `EnrolledKeyring` (an `Opener` per member) that `accept.rs`
consults. Proven end to end against the built control plane (seal on one enrolled node, `accept` on
another), with the non-vacuity that an **un-enrolled** sender is refused `UnknownSender`.

**Owed (needs ratification / a real fleet):** the distribution itself — the configuration-group
admission verb, the human-authorized enrollment channel that carries the control secret, the
membership-list publication, and the session-identity pinning wired through it. These are §4.8/§4.13
membership work, not local key math, and they wait on this draft's ratification and on the
configuration group's own protocol (currently simulated, GAPS §8h).

## 4. Open questions for ratification

- **(a) Control-secret scope and rotation.** Fleet-shared, per-region, or per-neighbourhood? A shared
  secret means any admitted node can derive any pair's key (it *is* trusted fleet membership), which
  matches "a daemon serves enrolled consumers under one authority" (§4.13) but widens a compromise to
  the whole secret's scope until the next rotation. Per-region narrows it. Rotation cadence and the
  overlap window (accept the old epoch for how long?) are unset. **Recommendation:** per-region shared
  secret, epoch-rotated on membership change, with a one-epoch overlap — smallest mechanism that
  matches the existing per-region configuration group and `schedule.rs`'s epoch keying.
- **(b) Session identity: self-signed pinned, or group-signed?** Pinning each admitted self-signed
  cert (built today) is simplest and needs no PKI. A group-signed cert (the configuration group's root
  as a tiny in-fleet CA) would let a node verify a peer without the full membership list. **Recommendation:**
  start pinned (no PKI, matches D-15/RFC 7250); revisit if the membership list proves too large to
  distribute whole.
- **(c) Bootstrap.** How does the very first node / the configuration group's root secret come to be?
  (A human mints it; the CLI is the surface, like grants.) Needs the CLI enrollment verb designed.
