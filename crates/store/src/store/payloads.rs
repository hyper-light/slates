//! Payload places for A-767 C3. A read uses one point lookup. Each further physical copy gets one
//! suffix key, so forgetting the place used for reads needs at most one surviving copy. The regression
//! in `tests/store.rs` lands identical bytes in two repositories and forgets them across a restart.

use crate::log::{SECTION_HEAD_BYTES, Section, decode_section_head};

use super::{
  AFTER_TAG, BLOCK_DIGESTS, BlockFile, CHANGESET_ENTRY_BYTES, CHANGESETS, CONTENT_KEY_BYTES,
  CONTENT_VALUE_BYTES, CONTENTS, Changeset, IDENTITY, Identity, LogError, PIECE_BYTES, Rows,
  SEAL_BYTES, SectionKind, ShardDb, StoreError, U64, block_key, content_key, decode_changeset,
  decode_content, delete_block_digests, field, index_verified_payload_from_log, keyed_block_tag,
  put, sections_of, verified_identity_of_section,
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

/// Reads and checks the closed `Payload` section head immediately preceding `alt_offset` within
/// the committed prefix `[0, end)`.
fn read_alternate_head<F: BlockFile>(
  file: &F,
  end: u64,
  (alt_offset, len): (u64, u64),
) -> Result<Section, StoreError> {
  let head_width = u64::try_from(SECTION_HEAD_BYTES).map_err(|_| StoreError::BadRow)?;
  let Some(head_offset) = alt_offset.checked_sub(head_width) else {
    return Err(StoreError::Log(LogError::Corrupt(
      "a payload's section head",
    )));
  };
  let Some(payload_end) = alt_offset.checked_add(len) else {
    return Err(StoreError::Log(LogError::Corrupt("a payload's length")));
  };
  if payload_end > end {
    return Err(StoreError::Log(LogError::Corrupt(
      "a payload past the committed records",
    )));
  }
  let mut head = [0u8; SECTION_HEAD_BYTES];
  file
    .read_exact_at(&mut head, head_offset)
    .map_err(LogError::from)?;
  let Some((kind, sec_len, crc)) = decode_section_head(&head) else {
    return Err(StoreError::Log(LogError::Corrupt("a section's head")));
  };
  if kind != SectionKind::Payload || sec_len != len {
    return Err(StoreError::Log(LogError::Corrupt("a section's head")));
  }
  Ok(Section {
    kind,
    offset: alt_offset,
    len,
    crc,
  })
}

/// Checks one [`PIECE_BYTES`] block of a multi-piece candidate alternate against its durable `B`
/// row in `rows`.
fn verify_alternate_block<F: BlockFile>(
  rows: &mut ShardDb<F>,
  seal: &[u8; SEAL_BYTES],
  (alt_offset, len): (u64, u64),
  (block_idx, slice): (u64, &[u8]),
  expected_tag: &mut Vec<u8>,
) -> Result<(), StoreError> {
  expected_tag.clear();
  if !rows.get(&block_key(alt_offset, block_idx), expected_tag)? || expected_tag.len() != IDENTITY {
    return Err(StoreError::Log(LogError::Corrupt(
      "a payload's block digest",
    )));
  }
  let tag = keyed_block_tag(seal, alt_offset, len, block_idx, slice);
  if tag.as_slice() != expected_tag.as_slice() {
    return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
  }
  Ok(())
}

/// Streams a candidate alternate payload section in [`PIECE_BYTES`] chunks (O(1) RAM) to verify
/// its header-bound CRC-32C, its whole-payload BLAKE3 `identity`, and (when `len > PIECE_BYTES`)
/// every durable per-block MAC `B` row.
fn verify_alternate_payload<F: BlockFile>(
  rows: &mut ShardDb<F>,
  (file, end, seal): (&F, u64, &[u8; SEAL_BYTES]),
  identity: &Identity,
  (alt_offset, len): (u64, u64),
  (buffer, expected_tag): (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<(), StoreError> {
  let section = read_alternate_head(file, end, (alt_offset, len))?;
  let piece_cap = u64::try_from(PIECE_BYTES).map_err(|_| StoreError::BadRow)?;
  let check_blocks = section.len > piece_cap;
  buffer.resize(PIECE_BYTES, 0);
  let mut crc = section.crc_verifier();
  let mut hasher = blake3::Hasher::new();
  let mut done = 0u64;
  let mut block_idx = 0u64;
  while done < section.len {
    let left = section.len.checked_sub(done).ok_or(StoreError::BadRow)?;
    let step = usize::try_from(left.min(piece_cap)).map_err(|_| StoreError::BadRow)?;
    let slice = buffer.get_mut(..step).ok_or(StoreError::BadRow)?;
    let at = section.offset.checked_add(done).ok_or(StoreError::BadRow)?;
    file.read_exact_at(slice, at).map_err(LogError::from)?;
    crc.update(slice);
    hasher.update(slice);
    if check_blocks {
      verify_alternate_block(
        rows,
        seal,
        (section.offset, section.len),
        (block_idx, slice),
        expected_tag,
      )?;
    }
    done = done
      .checked_add(u64::try_from(step).map_err(|_| StoreError::BadRow)?)
      .ok_or(StoreError::BadRow)?;
    block_idx = block_idx.checked_add(1).ok_or(StoreError::BadRow)?;
  }
  crc.finish()?;
  if *hasher.finalize().as_bytes() != *identity {
    return Err(StoreError::Log(LogError::Corrupt("a payload's bytes")));
  }
  Ok(())
}

/// Decision for one candidate alternate `P` row during primary promotion.
enum AlternateOutcome {
  /// Verified healthy outside `exclude`: promote to primary `P`.
  Promote,
  /// Excluded or corrupt: delete this alternate `P` row and keep scanning.
  Skip,
}

/// Evaluates one candidate alternate `P` row at `alt_offset`: skips excluded offsets and logs,
/// cleans `B` rows of, and skips any corrupt alternate rather than promoting it to primary `P`.
fn check_alternate_candidate<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
  identity: &Identity,
  (alt_offset, len): (u64, u64),
  (exclude_start, exclude_end): (u64, u64),
  buffers: (&mut Vec<u8>, &mut Vec<u8>),
) -> Result<AlternateOutcome, StoreError> {
  if (exclude_start..exclude_end).contains(&alt_offset) {
    return Ok(AlternateOutcome::Skip);
  }
  match verify_alternate_payload(rows, log_ctx, identity, (alt_offset, len), buffers) {
    Ok(()) => Ok(AlternateOutcome::Promote),
    Err(StoreError::Log(LogError::Corrupt(what))) => {
      slates_machine::log_line!(
        "slates-store: forget skipped corrupt alternate payload offset={alt_offset} len={len} ({what})"
      );
      delete_block_digests(rows, alt_offset, len)?;
      Ok(AlternateOutcome::Skip)
    }
    Err(error) => Err(error),
  }
}

/// Deletes any alternate `P` row for `primary` inside `exclude` or failing log verification, and
/// returns the first verified healthy alternate offset outside `exclude` (still present in `rows`
/// until published) along with the number of skipped alternates deleted.
fn find_surviving_alternate<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
  primary: &[u8; CONTENT_KEY_BYTES],
  len: u64,
  exclude: (u64, u64),
) -> Result<(Option<u64>, u64), StoreError> {
  let identity: Identity = field(primary, AFTER_TAG).ok_or(StoreError::BadRow)?;
  // No committed section can start at u64::MAX because a record trailer follows it.
  let mut start = place_key(primary, 0);
  let end = place_key(primary, u64::MAX);
  let mut places = Rows::new();
  let mut next = Vec::new();
  let mut buffer = Vec::new();
  let mut expected_tag = Vec::with_capacity(IDENTITY);
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
    let decision = check_alternate_candidate(
      rows,
      log_ctx,
      &identity,
      (alt_offset, len),
      exclude,
      (&mut buffer, &mut expected_tag),
    )?;
    if matches!(decision, AlternateOutcome::Promote) {
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

/// Promotes one verified healthy alternate place outside `exclude` to the primary `P` row
/// (publishing the promoted primary before retiring its suffix key), or deletes the primary row
/// when no healthy alternate survives outside `exclude`. Returns the number of `P` rows modified.
fn promote_or_delete_primary_excluding<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
  primary: &[u8; CONTENT_KEY_BYTES],
  len: u64,
  exclude: (u64, u64),
) -> Result<u64, StoreError> {
  let (survivor, deleted) = find_surviving_alternate(rows, log_ctx, primary, len, exclude)?;
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
/// supplied the reads, validates and publishes the first healthy remaining occurrence from `log`
/// before retiring its suffix key (deleting any corrupt alternates and their `B` rows along the
/// way). Retrying either deletion after a partial error preserves the same result.
pub(super) fn forget_in_log<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
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
  let changed = promote_or_delete_primary_excluding(rows, log_ctx, &primary, len, (0, 0))?;
  delete_block_digests(rows, offset, len)?;
  Ok(changed.saturating_add(u64::from(alternate)))
}

/// Test-only adapter preserving `payloads/tests.rs`'s log-less call signature; production code
/// exclusively calls [`forget_in_log`].
#[cfg(test)]
fn forget<F: BlockFile>(
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
  let start = place_key(&primary, 0);
  let end = place_key(&primary, u64::MAX);
  let mut places = Rows::new();
  rows.scan(&start, Some(&end), 1, &mut places, &mut Vec::new())?;
  let changed = if let Some((key, _)) = places.get(0) {
    let alt_offset = field(key, CONTENT_KEY_BYTES)
      .map(u64::from_be_bytes)
      .ok_or(StoreError::BadRow)?;
    publish(rows, &primary, alt_offset, len)?;
    rows.delete(key)?;
    2_u64
  } else {
    rows.delete(&primary)?;
    1_u64
  };
  delete_block_digests(rows, offset, len)?;
  Ok(changed.saturating_add(u64::from(alternate)))
}

/// Removes one `CONTENTS` row whose offset falls in `[start_offset, end_offset)` (promoting a
/// verified healthy duplicate outside that range when `key` is primary, or deleting a still-live
/// alternate suffix key without double-counting rows already retired during promotion) and returns
/// the count of `P` rows modified.
fn forget_matching_content_row_in_range<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
  (key, entry): (&[u8], &[u8]),
  (start_offset, end_offset): (u64, u64),
  scratch: &mut Vec<u8>,
) -> Result<u64, StoreError> {
  if key.len() == CONTENT_KEY_BYTES {
    let (current, current_len) = decode_content(entry).ok_or(StoreError::BadRow)?;
    if (start_offset..end_offset).contains(&current) {
      let primary: [u8; CONTENT_KEY_BYTES] = key.try_into().map_err(|_| StoreError::BadRow)?;
      return promote_or_delete_primary_excluding(
        rows,
        log_ctx,
        &primary,
        current_len,
        (start_offset, end_offset),
      );
    }
  } else if key.len() == PLACE_KEY_BYTES {
    let alt_offset = field(key, CONTENT_KEY_BYTES)
      .map(u64::from_be_bytes)
      .ok_or(StoreError::BadRow)?;
    if (start_offset..end_offset).contains(&alt_offset) && rows.get(key, scratch)? {
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
/// verified healthy duplicates outside that range), and deletes every `B` row in `[start_offset,
/// end_offset)` so neither `P` nor `B` rows leak or poison healthy siblings.
pub(super) fn forget_by_offset_range<F: BlockFile>(
  rows: &mut ShardDb<F>,
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
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
  let mut scratch = Vec::new();
  scan_table(rows, CONTENTS, per_page, |rows, key, entry| {
    let removed = forget_matching_content_row_in_range(
      rows,
      log_ctx,
      (key, entry),
      (start_offset, end_offset),
      &mut scratch,
    )?;
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
  log_ctx: (&F, u64, &[u8; SEAL_BYTES]),
  offset: u64,
  page_bytes: usize,
) -> Result<u64, StoreError> {
  forget_by_offset_range(
    rows,
    log_ctx,
    (offset, offset.saturating_add(1)),
    page_bytes,
  )
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
