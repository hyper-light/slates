//! The DNS servers a Windows host's network adapters list, read once at a fleet node's start in place of
//! `/etc/resolv.conf`, which Windows does not have (A-490; the bug record
//! `docs/bugs/2026-11-18-a-windows-fleet-node-looked-for-etc-resolv-conf.md`). One `GetAdaptersAddresses` call from IP Helper (the call Go's resolver makes there, `net/dnsconfig_windows.go`) and
//! the walk over the lists it writes, nothing more: which adapters and servers count is the cfg-free
//! `super::resolver_of_adapters`, tested on every host.
//!
//! The call writes a linked list of adapters, each with a linked list of DNS servers and each server's socket address,
//! all inside the caller's buffer. The walk reads a link or an address only once it is known to lie wholly inside the
//! buffer and aligned for its type, and walks each list at most as many steps as its entries fit in the buffer, so a
//! wrong link ends the walk rather than reading memory the call never wrote.

use core::net::Ipv4Addr;

use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, ERROR_NO_DATA, NO_ERROR};
use windows_sys::Win32::NetworkManagement::IpHelper::{
  GAA_FLAG_INCLUDE_GATEWAYS, GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_FRIENDLY_NAME,
  GAA_FLAG_SKIP_MULTICAST, GAA_FLAG_SKIP_UNICAST, GET_ADAPTERS_ADDRESSES_FLAGS,
  GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH, IP_ADAPTER_DNS_SERVER_ADDRESS_XP,
};
use windows_sys::Win32::NetworkManagement::Ndis::IfOperStatusUp;
use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_UNSPEC, SOCKADDR_IN, SOCKET_ADDRESS};

use super::Adapter;

/// Format: what the call is asked to write: every family's adapters (as Go asks), each with its gateways (whether it
/// has one decides whether its servers count) and its DNS servers; none of the unicast, anycast or multicast addresses
/// or the friendly names the resolver never reads, so the list is as small as the call makes it.
const FLAGS: GET_ADAPTERS_ADDRESSES_FLAGS = GAA_FLAG_INCLUDE_GATEWAYS
  | GAA_FLAG_SKIP_UNICAST
  | GAA_FLAG_SKIP_ANYCAST
  | GAA_FLAG_SKIP_MULTICAST
  | GAA_FLAG_SKIP_FRIENDLY_NAME;

/// This host's network adapters in the order Windows lists them, each with whether it is up, whether it has a gateway
/// and its IPv4 DNS servers; or why Windows would not list them. The call is asked twice: once for the size the list
/// takes, then into a buffer of that size. An adapter added between the two is a refusal naming it, not a retry: a
/// start that meets it fails by name, and the next start reads the new list.
pub(super) fn adapters() -> Result<Vec<Adapter>, String> {
  let (status, size) = list_into(&mut []);
  match status {
    ERROR_BUFFER_OVERFLOW => {}
    NO_ERROR | ERROR_NO_DATA => return Ok(Vec::new()),
    code => return Err(format!("sizing the list: error {code}")),
  }
  let bytes = usize::try_from(size).map_err(|_| format!("a list of {size} bytes"))?;
  let mut words = vec![0u64; bytes.div_ceil(size_of::<u64>())];
  match list_into(&mut words).0 {
    NO_ERROR => Ok(walk(&words)),
    ERROR_NO_DATA => Ok(Vec::new()),
    ERROR_BUFFER_OVERFLOW => Err(
      "the list grew between its sizing and its read (an adapter came up meanwhile); start again"
        .to_owned(),
    ),
    code => Err(format!("reading the list: error {code}")),
  }
}

/// One `GetAdaptersAddresses` call writing into `words` (empty: a call that only asks the size): its status, and the
/// size in bytes the list needs or took.
fn list_into(words: &mut [u64]) -> (u32, u32) {
  // The call is told no more than the buffer holds; a buffer past `u32::MAX` bytes is told `u32::MAX`.
  let mut size = u32::try_from(size_of_val(words)).unwrap_or(u32::MAX);
  let list = if words.is_empty() {
    core::ptr::null_mut()
  } else {
    words.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()
  };
  // SAFETY: `list` is null with `size` 0, so the call writes only the size it needs, or the start of `words`, which
  // holds `size` bytes or more, is aligned to 8 bytes as the list's entries need (their widest fields are 8 bytes),
  // and is borrowed mutably for this call alone; `size` is a live local the call reads and writes; the reserved
  // argument is null, as the call requires.
  let status = unsafe {
    GetAdaptersAddresses(
      u32::from(AF_UNSPEC),
      FLAGS,
      core::ptr::null(),
      list,
      &raw mut size,
    )
  };
  (status, size)
}

/// The adapters the call wrote into `words`, in the order its list links them.
fn walk(words: &[u64]) -> Vec<Adapter> {
  let mut adapters = Vec::new();
  let mut next = words.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>();
  for _ in 0..entries::<IP_ADAPTER_ADDRESSES_LH>(words) {
    if next.is_null() || !inside(words, next) {
      break;
    }
    // SAFETY: `next` is the list's head, which the call wrote at the start of `words`, or a link the call wrote; it
    // lies wholly inside `words` and is aligned for an adapter (checked above); `words` was zeroed before the call, so
    // every byte is initialized, and every bit pattern is a valid adapter (integers, raw pointers and arrays of them);
    // `words` is borrowed unchanged for the whole walk.
    let adapter = unsafe { &*next };
    adapters.push(Adapter {
      up: adapter.OperStatus == IfOperStatusUp,
      has_gateway: !adapter.FirstGatewayAddress.is_null(),
      dns_servers: dns_servers(words, adapter.FirstDnsServerAddress.cast_const()),
    });
    next = adapter.Next.cast_const();
  }
  adapters
}

/// One adapter's IPv4 DNS servers, in its list's order, from the list's head `first`.
fn dns_servers(words: &[u64], first: *const IP_ADAPTER_DNS_SERVER_ADDRESS_XP) -> Vec<Ipv4Addr> {
  let mut servers = Vec::new();
  let mut next = first;
  for _ in 0..entries::<IP_ADAPTER_DNS_SERVER_ADDRESS_XP>(words) {
    if next.is_null() || !inside(words, next) {
      break;
    }
    // SAFETY: `next` is a link the call wrote; it lies wholly inside `words` and is aligned for a server entry
    // (checked above); every byte of `words` is initialized and every bit pattern is a valid entry; `words` is
    // borrowed unchanged for the whole walk.
    let entry = unsafe { &*next };
    if let Some(server) = ipv4(words, entry.Address) {
      servers.push(server);
    }
    next = entry.Next.cast_const();
  }
  servers
}

/// The IPv4 address a server entry holds, or `None` for any other family (an IPv6 server: the fleet dials IPv4) or an
/// address that does not lie inside `words`.
fn ipv4(words: &[u64], address: SOCKET_ADDRESS) -> Option<Ipv4Addr> {
  let length = usize::try_from(address.iSockaddrLength).ok()?;
  let socket = address.lpSockaddr.cast_const().cast::<SOCKADDR_IN>();
  if length < size_of::<SOCKADDR_IN>() || socket.is_null() || !inside(words, socket) {
    return None;
  }
  // SAFETY: a whole `SOCKADDR_IN` lies inside `words` at `socket`, aligned for one (checked above), where the call
  // wrote the server's address; its address word is four initialized bytes whatever the family, so reading it through
  // the union is sound, and the family decides whether they are an IPv4 address.
  let (family, octets) = unsafe {
    let socket = &*socket;
    (socket.sin_family, socket.sin_addr.S_un.S_addr.to_ne_bytes())
  };
  (family == AF_INET).then_some(Ipv4Addr::from(octets))
}

/// How many `T`s fit in `words`: the most entries of one kind the call can have written there, so the most steps a
/// walk over a list of them takes.
fn entries<T>(words: &[u64]) -> usize {
  size_of_val(words).checked_div(size_of::<T>()).unwrap_or(0)
}

/// Whether a whole `T` at `at` lies inside `words` and is aligned for one: only then is it read.
fn inside<T>(words: &[u64], at: *const T) -> bool {
  let start = words.as_ptr().addr();
  let end = start.saturating_add(size_of_val(words));
  let first = at.addr();
  first >= start && first.saturating_add(size_of::<T>()) <= end && at.is_aligned()
}
