//! DIAG (diag-g251-spin, never main): names for the task a shard polls, so a stuck verdict says which task it was in.
//! A site is a spawned future's type name or a spawn request's caller, interned to a number the pulse carries.

use std::cell::Cell;
use std::sync::{Mutex, OnceLock};

static SITES: OnceLock<Mutex<Vec<&'static str>>> = OnceLock::new();

thread_local! {
  static NEXT: Cell<u64> = const { Cell::new(0) };
}

/// The number of `site`, one-based.
pub fn intern(site: &str) -> u64 {
  let Ok(mut sites) = SITES.get_or_init(Default::default).lock() else {
    return 0;
  };
  if let Some(index) = sites.iter().position(|known| *known == site) {
    return u64::try_from(index).unwrap_or(0).saturating_add(1);
  }
  sites.push(Box::leak(site.to_owned().into_boxed_str()));
  u64::try_from(sites.len()).unwrap_or(0)
}

/// The number of a caller's location.
pub fn intern_location(location: &'static std::panic::Location<'static>) -> u64 {
  intern(&format!("{}:{}", location.file(), location.line()))
}

/// The site numbered `number`, or "unknown".
pub fn name(number: u64) -> &'static str {
  let Some(sites) = SITES.get() else {
    return "unknown";
  };
  let Ok(sites) = sites.lock() else {
    return "unknown";
  };
  usize::try_from(number)
    .ok()
    .and_then(|number| number.checked_sub(1))
    .and_then(|index| sites.get(index).copied())
    .unwrap_or("unknown")
}

/// Names the next task this thread admits.
pub fn set_next(number: u64) {
  NEXT.with(|next| next.set(number));
}

/// The name set for the task being admitted, cleared.
pub fn take_next() -> u64 {
  NEXT.with(|next| next.replace(0))
}
