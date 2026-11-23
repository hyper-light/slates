//! Payload places for A-767 C3. A read uses one point lookup. Each further physical copy gets one
//! suffix key, so forgetting the place used for reads needs at most one surviving copy. The regression
//! in `tests/store.rs` lands identical bytes in two repositories and forgets them across a restart.

use super::{
  BlockFile, CHANGESET_ENTRY_BYTES, CHANGESETS, CONTENT_KEY_BYTES, CONTENT_VALUE_BYTES, CONTENTS,
  Changeset, Identity, Rows, SectionKind, ShardDb, StoreError, U64, content_key, decode_changeset,
  decode_content, field, identity_of_range, put, sections_of,
};

/// Width of an alternate place's key. Its final field is the section's unique offset in the log.
const PLACE_KEY_BYTES: usize = CONTENT_KEY_BYTES + U64;

/// The section's row within the ordered range for one payload.
fn place_key(primary: &[u8; CONTENT_KEY_BYTES], offset: u64) -> [u8; PLACE_KEY_BYTES] {
  let mut key = [0; PLACE_KEY_BYTES];
  put(&mut key, 0, primary);
  put(&mut key, CONTENT_KEY_BYTES, &offset.to_be_bytes());
  key
}

/// Publishes the place read by identity.
fn publish<F: BlockFile>(
  rows: &mut ShardDb<F>,
  key: &[u8],
  offset: u64,
  len: u64,
) -> Result<(), StoreError> {
  let mut value = [0; CONTENT_VALUE_BYTES];
  put(&mut value, 0, &offset.to_le_bytes());
  put(&mut value, U64, &len.to_le_bytes());
  Ok(rows.put(key, &value)?)
}

/// Records this section once, including when replay reapplies it.
pub(super) fn keep<F: BlockFile>(
  rows: &mut ShardDb<F>,
  identity: &Identity,
  offset: u64,
  len: u64,
  value: &mut Vec<u8>,
) -> Result<(), StoreError> {
  let key = content_key(identity);
  if !rows.get(&key, value)? {
    return publish(rows, &key, offset, len);
  }
  let (current, current_len) = decode_content(value).ok_or(StoreError::BadRow)?;
  if current_len != len {
    return Err(StoreError::BadRow);
  }
  if current != offset {
    rows.put(&place_key(&key, offset), &[])?;
  }
  Ok(())
}

/// Removes this occurrence. If it supplied the reads, publishes one remaining occurrence before
/// retiring its suffix key. Retrying either deletion after a partial error preserves the same result.
pub(super) fn forget<F: BlockFile>(
  rows: &mut ShardDb<F>,
  identity: &Identity,
  offset: u64,
  value: &mut Vec<u8>,
) -> Result<u64, StoreError> {
  let primary = content_key(identity);
  if !rows.get(&primary, value)? {
    return Ok(0);
  }
  let (current, len) = decode_content(value).ok_or(StoreError::BadRow)?;
  let own = place_key(&primary, offset);
  let alternate = rows.get(&own, value)?;
  if alternate {
    rows.delete(&own)?;
  }
  if current != offset {
    return Ok(u64::from(alternate));
  }
  // No committed section can start at u64::MAX because a record trailer follows it.
  let mut places = Rows::new();
  rows.scan(
    &place_key(&primary, 0),
    Some(&place_key(&primary, u64::MAX)),
    1,
    &mut places,
    &mut Vec::new(),
  )?;
  let changed = if let Some((key, _)) = places.get(0) {
    let next = field(key, CONTENT_KEY_BYTES)
      .map(u64::from_be_bytes)
      .ok_or(StoreError::BadRow)?;
    publish(rows, &primary, next, len)?;
    rows.delete(key)?;
    2_u64
  } else {
    rows.delete(&primary)?;
    1_u64
  };
  Ok(changed.saturating_add(u64::from(alternate)))
}

/// Visits only this record's payloads, hashing through the same bounded buffer as log recovery.
pub(super) fn visit<F: BlockFile>(
  file: &F,
  end: u64,
  row: &Changeset,
  buffer: &mut Vec<u8>,
  mut each: impl FnMut(&Identity, u64, u64) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
  for section in sections_of(file, end, row)? {
    if section.kind == SectionKind::Payload {
      let identity = identity_of_range(file, section.offset, section.len, buffer)?;
      each(&identity, section.offset, section.len)?;
    }
  }
  Ok(())
}

/// The bare table tag marks a complete checkpoint of payload ownership. An older checkpoint knows
/// only one place per identity and can already lack a live payload's lookup. Rebuild once from its
/// surviving changesets. The marker commits with all the places before replay resumes at `applied`.
pub(super) fn index_checkpoint<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  applied: u64,
  page_bytes: usize,
) -> Result<(), StoreError> {
  let mut value = Vec::new();
  if rows.get(&[CONTENTS], &mut value)? {
    return Ok(());
  }
  if applied > 0 {
    // One engine page of the larger changeset rows bounds both scans' retained keys and values.
    let per_page = page_bytes
      .checked_div(CHANGESET_ENTRY_BYTES)
      .unwrap_or(1)
      .max(1);
    scan_table(rows, CONTENTS, per_page, |rows, key, _| {
      Ok(rows.delete(key)?)
    })?;
    let mut buffer = Vec::new();
    scan_table(rows, CHANGESETS, per_page, |rows, key, entry| {
      let row = decode_changeset(key, entry)?;
      visit(file, applied, &row, &mut buffer, |identity, offset, len| {
        keep(rows, identity, offset, len, &mut value)
      })
    })?;
  }
  rows.put(&[CONTENTS], &[])?;
  if applied > 0 {
    rows.checkpoint(applied)?;
  }
  Ok(())
}

/// Streams one table through a bounded engine page. The continuation survives writes by the visitor.
fn scan_table<F: BlockFile>(
  rows: &mut ShardDb<F>,
  table: u8,
  per_page: usize,
  mut each: impl FnMut(&mut ShardDb<F>, &[u8], &[u8]) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
  let mut start = vec![table];
  let mut next = Vec::new();
  let mut page = Rows::new();
  let end = [table.saturating_add(1)];
  loop {
    page.clear();
    let more = rows.scan(&start, Some(&end), per_page, &mut page, &mut next)?;
    for (key, entry) in page.iter() {
      each(rows, key, entry)?;
    }
    if !more {
      return Ok(());
    }
    std::mem::swap(&mut start, &mut next);
  }
}

#[cfg(test)]
mod tests;
