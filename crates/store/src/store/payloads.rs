//! Payload places for A-767 C3. A read uses one point lookup. Each further physical copy gets one
//! suffix key, so forgetting the place used for reads needs at most one surviving copy. The regression
//! in `tests/store.rs` lands identical bytes in two repositories and forgets them across a restart.

use super::{
  BLOCK_DIGESTS, BlockFile, CHANGESET_ENTRY_BYTES, CHANGESETS, CONTENT_KEY_BYTES,
  CONTENT_VALUE_BYTES, CONTENTS, Changeset, Identity, LogError, Rows, SEAL_BYTES, SectionKind,
  ShardDb, StoreError, U64, block_key, content_key, decode_changeset, decode_content,
  delete_block_digests, field, index_verified_payload_from_log, put, sections_of,
  verified_identity_of_section,
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

/// Deletes any alternate `P` row for `primary` inside `[exclude_start, exclude_end)` and returns
/// the first surviving alternate offset outside that range (still present in `rows` until
/// published) along with the number of in-range alternates deleted.
fn find_surviving_alternate<F: BlockFile>(
  rows: &mut ShardDb<F>,
  primary: &[u8; CONTENT_KEY_BYTES],
  (exclude_start, exclude_end): (u64, u64),
) -> Result<(Option<u64>, u64), StoreError> {
  // No committed section can start at u64::MAX because a record trailer follows it.
  let mut start = place_key(primary, 0);
  let end = place_key(primary, u64::MAX);
  let mut places = Rows::new();
  let mut next = Vec::new();
  let mut deleted = 0_u64;
  loop {
    places.clear();
    let more = rows.scan(&start, Some(&end), 1, &mut places, &mut next)?;
    let Some((key, _)) = places.get(0) else {
      return Ok((None, deleted));
    };
    let alt_offset = field(key, CONTENT_KEY_BYTES)
      .map(u64::from_be_bytes)
      .ok_or(StoreError::BadRow)?;
    if !(exclude_start..exclude_end).contains(&alt_offset) {
      return Ok((Some(alt_offset), deleted));
    }
    rows.delete(key)?;
    deleted = deleted.saturating_add(1);
    if !more {
      return Ok((None, deleted));
    }
    start = place_key(primary, alt_offset.saturating_add(1));
  }
}

/// Promotes one remaining alternate place outside `exclude` to the primary `P` row (publishing the
/// promoted primary before retiring its suffix key), or deletes the primary row when no surviving
/// alternate remains outside `exclude`. Returns the number of `P` rows modified.
fn promote_or_delete_primary_excluding<F: BlockFile>(
  rows: &mut ShardDb<F>,
  primary: &[u8; CONTENT_KEY_BYTES],
  len: u64,
  exclude: (u64, u64),
) -> Result<u64, StoreError> {
  let (survivor, deleted) = find_surviving_alternate(rows, primary, exclude)?;
  if let Some(next) = survivor {
    publish(rows, primary, next, len)?;
    rows.delete(&place_key(primary, next))?;
    Ok(deleted.saturating_add(2))
  } else {
    rows.delete(primary)?;
    Ok(deleted.saturating_add(1))
  }
}

/// Removes this occurrence and its per-block MAC `B` rows (when `len > PIECE_BYTES`). If it
/// supplied the reads, publishes one remaining occurrence before retiring its suffix key. Retrying
/// either deletion after a partial error preserves the same result.
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
    if alternate {
      delete_block_digests(rows, offset, len)?;
    }
    return Ok(u64::from(alternate));
  }
  let changed = promote_or_delete_primary_excluding(rows, &primary, len, (0, 0))?;
  delete_block_digests(rows, offset, len)?;
  Ok(changed.saturating_add(u64::from(alternate)))
}

/// Removes one `CONTENTS` row whose offset falls in `[start_offset, end_offset)` (promoting a
/// surviving duplicate outside that range when `key` is primary, or deleting an alternate suffix
/// key) and returns the count of `P` rows modified.
fn forget_matching_content_row_in_range<F: BlockFile>(
  rows: &mut ShardDb<F>,
  key: &[u8],
  entry: &[u8],
  (start_offset, end_offset): (u64, u64),
) -> Result<u64, StoreError> {
  if key.len() == CONTENT_KEY_BYTES {
    let (current, current_len) = decode_content(entry).ok_or(StoreError::BadRow)?;
    if (start_offset..end_offset).contains(&current) {
      let primary: [u8; CONTENT_KEY_BYTES] = key.try_into().map_err(|_| StoreError::BadRow)?;
      return promote_or_delete_primary_excluding(
        rows,
        &primary,
        current_len,
        (start_offset, end_offset),
      );
    }
  } else if key.len() == PLACE_KEY_BYTES {
    let alt_offset = field(key, CONTENT_KEY_BYTES)
      .map(u64::from_be_bytes)
      .ok_or(StoreError::BadRow)?;
    if (start_offset..end_offset).contains(&alt_offset) {
      rows.delete(key)?;
      return Ok(1_u64);
    }
  }
  Ok(0_u64)
}

/// Deletes every per-block MAC `B` row whose section offset lies in `[start_offset, end_offset)`
/// in bounded engine pages without touching sibling sections outside that range.
fn delete_block_digests_in_range<F: BlockFile>(
  rows: &mut ShardDb<F>,
  (start_offset, end_offset): (u64, u64),
  per_page: usize,
) -> Result<(), StoreError> {
  if start_offset >= end_offset {
    return Ok(());
  }
  let mut start = block_key(start_offset, 0).to_vec();
  let end = block_key(end_offset, 0);
  let mut next = Vec::new();
  let mut page = Rows::new();
  loop {
    page.clear();
    let more = rows.scan(&start, Some(&end), per_page, &mut page, &mut next)?;
    for (key, _) in page.iter() {
      rows.delete(key)?;
    }
    if !more {
      return Ok(());
    }
    std::mem::swap(&mut start, &mut next);
  }
}

/// Fallback cleanup when a record or payload section on disk fails verification during
/// `Store::forget` or `put_changeset_counted`: scans `CONTENTS` in bounded pages to delete every
/// primary or alternate `P` row whose offset lies in `[start_offset, end_offset)` (promoting
/// surviving duplicates outside that range), and deletes every `B` row in `[start_offset,
/// end_offset)` so neither `P` nor `B` rows leak or poison healthy siblings.
pub(super) fn forget_by_offset_range<F: BlockFile>(
  rows: &mut ShardDb<F>,
  (start_offset, end_offset): (u64, u64),
  page_bytes: usize,
) -> Result<u64, StoreError> {
  if start_offset >= end_offset {
    return Ok(0);
  }
  let per_page = page_bytes
    .checked_div(CHANGESET_ENTRY_BYTES)
    .unwrap_or(1)
    .max(1);
  let mut changed = 0_u64;
  scan_table(rows, CONTENTS, per_page, |rows, key, entry| {
    let removed =
      forget_matching_content_row_in_range(rows, key, entry, (start_offset, end_offset))?;
    changed = changed.saturating_add(removed);
    Ok(())
  })?;
  delete_block_digests_in_range(rows, (start_offset, end_offset), per_page)?;
  Ok(changed)
}

/// Fallback cleanup for one damaged payload section at `offset`, delegating to
/// [`forget_by_offset_range`].
pub(super) fn forget_by_offset<F: BlockFile>(
  rows: &mut ShardDb<F>,
  offset: u64,
  page_bytes: usize,
) -> Result<u64, StoreError> {
  forget_by_offset_range(rows, (offset, offset.saturating_add(1)), page_bytes)
}

/// Visits only this record's payloads, verifying each section's CRC-32C alongside BLAKE3 through
/// the same bounded buffer as log recovery.
pub(super) fn visit<F: BlockFile>(
  file: &F,
  (end, seal): (u64, &[u8; SEAL_BYTES]),
  row: &Changeset,
  buffer: &mut Vec<u8>,
  mut each: impl FnMut(&Identity, u64, u64) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
  for section in sections_of(file, end, seal, row)? {
    if section.kind == SectionKind::Payload {
      let identity = verified_identity_of_section(file, &section, buffer)?;
      each(&identity, section.offset, section.len)?;
    }
  }
  Ok(())
}

/// Rebuilds the `P` and multi-piece `B` rows for one changeset row within `applied`, verifying
/// every payload section's CRC-32C before writing any `P` or `B` row.
fn rebuild_changeset_payloads<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  (applied, seal): (u64, &[u8; SEAL_BYTES]),
  row: &Changeset,
  buffers: (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(), StoreError> {
  let mut verified = Vec::new();
  for section in sections_of(file, applied, seal, row)? {
    if section.kind == SectionKind::Payload {
      let identity = verified_identity_of_section(file, &section, buffers.1)?;
      verified.push((section, identity));
    }
  }
  for (section, identity) in verified {
    index_verified_payload_from_log(
      rows,
      file,
      seal,
      &section,
      &identity,
      (buffers.0, buffers.1),
    )?;
  }
  Ok(())
}

/// Rebuilds `CONTENTS` (`P`) and multi-piece `BLOCK_DIGESTS` (`B`) rows from all surviving
/// `CHANGESETS` rows in the checkpointed prefix up to `applied`, logging and counting any row
/// whose log header or section heads no longer verify.
fn rebuild_payload_places<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  (applied, seal): (u64, &[u8; SEAL_BYTES]),
  per_page: usize,
  value: &mut Vec<u8>,
) -> Result<u64, StoreError> {
  scan_table(rows, CONTENTS, per_page, |rows, key, _| {
    Ok(rows.delete(key)?)
  })?;
  scan_table(rows, BLOCK_DIGESTS, per_page, |rows, key, _| {
    Ok(rows.delete(key)?)
  })?;
  let mut buffer = Vec::new();
  let mut corrupt_rows_skipped = 0u64;
  scan_table(rows, CHANGESETS, per_page, |rows, key, entry| {
    let row = decode_changeset(key, entry)?;
    match rebuild_changeset_payloads(rows, file, (applied, seal), &row, (value, &mut buffer)) {
      Ok(()) => Ok(()),
      Err(StoreError::Log(LogError::Corrupt(what))) => {
        corrupt_rows_skipped = corrupt_rows_skipped.saturating_add(1);
        slates_machine::log_line!(
          "slates-store: index_checkpoint skipped corrupt changeset row repo={} seq={} offset={} ({what})",
          row.repository,
          row.sequence,
          row.offset
        );
        Ok(())
      }
      Err(error) => Err(error),
    }
  })?;
  Ok(corrupt_rows_skipped)
}

/// The bare table tags `[CONTENTS]` and `[BLOCK_DIGESTS]` mark a complete checkpoint of payload
/// ownership and per-block MACs. An older checkpoint lacking either marker is rebuilt once from
/// its surviving changesets, and the markers commit with all rebuilt rows before replay resumes at
/// `applied`. Returns the number of corrupt changeset rows skipped during the rebuild.
pub(super) fn index_checkpoint<F: BlockFile>(
  rows: &mut ShardDb<F>,
  file: &F,
  (applied, seal): (u64, &[u8; SEAL_BYTES]),
  page_bytes: usize,
) -> Result<u64, StoreError> {
  let mut value = Vec::new();
  if rows.get(&[CONTENTS], &mut value)? && rows.get(&[BLOCK_DIGESTS], &mut value)? {
    return Ok(0);
  }
  let corrupt_rows_skipped = if applied > 0 {
    // One engine page of the larger changeset rows bounds both scans' retained keys and values.
    let per_page = page_bytes
      .checked_div(CHANGESET_ENTRY_BYTES)
      .unwrap_or(1)
      .max(1);
    rebuild_payload_places(rows, file, (applied, seal), per_page, &mut value)?
  } else {
    0
  };
  rows.put(&[CONTENTS], &[])?;
  rows.put(&[BLOCK_DIGESTS], &[])?;
  if applied > 0 {
    rows.checkpoint(applied)?;
  }
  Ok(corrupt_rows_skipped)
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
