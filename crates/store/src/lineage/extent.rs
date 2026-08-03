use super::*;

use crate::history::{
    transcript_extent_profile, TranscriptExtentChunk, TranscriptExtentProfile,
    TranscriptRecordOffset, TranscriptRecordRange, TRANSCRIPT_EXTENT_CHUNK_RECORDS,
    TRANSCRIPT_EXTENT_PROFILE_WIDTHS,
};

pub(crate) fn install_transcript_extent_chunks(
    conn: &Connection,
    lineage: &LineageId,
    prior_root: &SequenceRoot,
    new_root: &SequenceRoot,
    start: u64,
    items: &[Vec<u8>],
) -> Result<()> {
    if prior_root.kind != SequenceKind::Transcript || new_root.kind != SequenceKind::Transcript {
        return Err(StoreError::Integrity(
            "transcript extent profiles require transcript roots".into(),
        ));
    }
    if prior_root.id == new_root.id {
        return Ok(());
    }

    let start = usize::try_from(start).map_err(|_| {
        StoreError::Integrity("transcript extent start exceeds platform limits".into())
    })?;
    let total = usize::try_from(new_root.item_count).map_err(|_| {
        StoreError::Integrity("transcript extent length exceeds platform limits".into())
    })?;
    let first_chunk = start / TRANSCRIPT_EXTENT_CHUNK_RECORDS;
    let chunk_start = first_chunk.saturating_mul(TRANSCRIPT_EXTENT_CHUNK_RECORDS);
    let source_chunks: i64 = conn.query_row(
        "SELECT COUNT(*) FROM lineage_transcript_extent_chunks
         WHERE lineage_id = ?1 AND transcript_root_id = ?2 AND chunk_index < ?3",
        (
            lineage.as_str(),
            prior_root.id.as_str(),
            checked_i64(first_chunk as u64, "transcript extent source chunk")?,
        ),
        |row| row.get(0),
    )?;
    if nonnegative_usize(source_chunks, "transcript extent source chunk count")? != first_chunk {
        return Err(StoreError::Integrity(format!(
            "transcript root {} has {source_chunks} extent chunks before {first_chunk}",
            prior_root.id.as_str()
        )));
    }
    let first_chunk_i64 = checked_i64(first_chunk as u64, "transcript extent copied chunk")?;
    conn.execute(
        "INSERT INTO lineage_transcript_extent_chunks (
             lineage_id, transcript_root_id, chunk_index, record_count,
             rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
         )
         SELECT lineage_id, ?3, chunk_index, record_count,
                rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
         FROM lineage_transcript_extent_chunks
         WHERE lineage_id = ?1 AND transcript_root_id = ?2 AND chunk_index < ?4
         ON CONFLICT (lineage_id, transcript_root_id, chunk_index) DO NOTHING",
        (
            lineage.as_str(),
            prior_root.id.as_str(),
            new_root.id.as_str(),
            first_chunk_i64,
        ),
    )?;
    let conflicting_chunks: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1
             FROM lineage_transcript_extent_chunks source
             JOIN lineage_transcript_extent_chunks target
               ON target.lineage_id = source.lineage_id
              AND target.chunk_index = source.chunk_index
             WHERE source.lineage_id = ?1
               AND source.transcript_root_id = ?2
               AND target.transcript_root_id = ?3
               AND source.chunk_index < ?4
               AND (target.record_count != source.record_count
                    OR target.rows_20 != source.rows_20
                    OR target.rows_40 != source.rows_40
                    OR target.rows_80 != source.rows_80
                    OR target.rows_120 != source.rows_120
                    OR target.rows_160 != source.rows_160
                    OR target.rows_240 != source.rows_240)
         )",
        (
            lineage.as_str(),
            prior_root.id.as_str(),
            new_root.id.as_str(),
            first_chunk_i64,
        ),
        |row| row.get(0),
    )?;
    if conflicting_chunks {
        return Err(StoreError::Integrity(
            "transcript extent chunks conflict with immutable root".into(),
        ));
    }

    let mut records = if chunk_start < start {
        deserialize_sequence_range(conn, lineage, prior_root, chunk_start as u64, start as u64)?
    } else {
        Vec::new()
    };
    records.extend(
        items
            .iter()
            .map(|item| serde_json::from_slice::<StoredTranscriptBlock>(item))
            .collect::<std::result::Result<Vec<_>, _>>()?,
    );
    let expected_records = total.checked_sub(chunk_start).ok_or_else(|| {
        StoreError::Integrity("transcript extent chunk starts after the new root".into())
    })?;
    if records.len() != expected_records {
        return Err(StoreError::Integrity(format!(
            "transcript extent suffix has {} records, expected {expected_records}",
            records.len()
        )));
    }

    for (offset, records) in records.chunks(TRANSCRIPT_EXTENT_CHUNK_RECORDS).enumerate() {
        let chunk_index = first_chunk.saturating_add(offset);
        insert_extent_chunk(conn, lineage, &new_root.id, chunk_index, records)?;
    }
    let expected_chunks = total.div_ceil(TRANSCRIPT_EXTENT_CHUNK_RECORDS);
    let stored_chunks: i64 = conn.query_row(
        "SELECT COUNT(*) FROM lineage_transcript_extent_chunks
         WHERE lineage_id = ?1 AND transcript_root_id = ?2",
        (lineage.as_str(), new_root.id.as_str()),
        |row| row.get(0),
    )?;
    let stored_chunks = nonnegative_usize(stored_chunks, "transcript extent chunk count")?;
    if stored_chunks != expected_chunks {
        return Err(StoreError::Integrity(format!(
            "transcript root {} has {stored_chunks} extent chunks, expected {expected_chunks}",
            new_root.id.as_str()
        )));
    }
    Ok(())
}

fn insert_extent_chunk(
    conn: &Connection,
    lineage: &LineageId,
    root: &RootId,
    chunk_index: usize,
    records: &[StoredTranscriptBlock],
) -> Result<()> {
    if records.is_empty() || records.len() > TRANSCRIPT_EXTENT_CHUNK_RECORDS {
        return Err(StoreError::Integrity(
            "transcript extent chunk has an invalid record count".into(),
        ));
    }
    let rows = transcript_extent_profile(records).rows();
    let chunk_index = checked_i64(chunk_index as u64, "transcript extent chunk index")?;
    let inserted = conn.execute(
        "INSERT INTO lineage_transcript_extent_chunks (
             lineage_id, transcript_root_id, chunk_index, record_count,
             rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT (lineage_id, transcript_root_id, chunk_index) DO NOTHING",
        rusqlite::params![
            lineage.as_str(),
            root.as_str(),
            chunk_index,
            checked_i64(records.len() as u64, "transcript extent record count")?,
            checked_i64(rows[0], "transcript extent rows 20")?,
            checked_i64(rows[1], "transcript extent rows 40")?,
            checked_i64(rows[2], "transcript extent rows 80")?,
            checked_i64(rows[3], "transcript extent rows 120")?,
            checked_i64(rows[4], "transcript extent rows 160")?,
            checked_i64(rows[5], "transcript extent rows 240")?,
        ],
    )?;
    if inserted == 0 {
        let (record_count, stored_rows) = conn.query_row(
            "SELECT record_count, rows_20, rows_40, rows_80,
                    rows_120, rows_160, rows_240
             FROM lineage_transcript_extent_chunks
             WHERE lineage_id = ?1 AND transcript_root_id = ?2 AND chunk_index = ?3",
            (lineage.as_str(), root.as_str(), chunk_index),
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    [
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, i64>(5)?,
                        row.get::<_, i64>(6)?,
                    ],
                ))
            },
        )?;
        let record_count = nonnegative_usize(record_count, "transcript extent record count")?;
        let stored_rows = validated_profile_rows(stored_rows, record_count)?;
        if record_count != records.len() || stored_rows != rows {
            return Err(StoreError::Integrity(
                "transcript extent chunk conflicts with immutable root".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn lineage_transcript_extent_chunks(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
) -> Result<Vec<TranscriptExtentChunk>> {
    if root.kind != SequenceKind::Transcript {
        return Err(StoreError::Integrity(
            "transcript extent query requires a transcript root".into(),
        ));
    }
    let mut statement = conn.prepare(
        "SELECT chunk_index, record_count,
                rows_20, rows_40, rows_80, rows_120, rows_160, rows_240
         FROM lineage_transcript_extent_chunks
         WHERE lineage_id = ?1 AND transcript_root_id = ?2
         ORDER BY chunk_index",
    )?;
    let rows = statement.query_map((lineage.as_str(), root.id.as_str()), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            [
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ],
        ))
    })?;
    let total = usize::try_from(root.item_count).map_err(|_| {
        StoreError::Integrity("transcript extent length exceeds platform limits".into())
    })?;
    let mut chunks = Vec::with_capacity(total.div_ceil(TRANSCRIPT_EXTENT_CHUNK_RECORDS));
    let mut expected_start = 0_usize;
    for row in rows {
        let (chunk_index, record_count, rows) = row?;
        let chunk_index = nonnegative_usize(chunk_index, "transcript extent chunk index")?;
        let record_count = nonnegative_usize(record_count, "transcript extent record count")?;
        if chunk_index.saturating_mul(TRANSCRIPT_EXTENT_CHUNK_RECORDS) != expected_start
            || record_count == 0
            || record_count > TRANSCRIPT_EXTENT_CHUNK_RECORDS
        {
            return Err(StoreError::Integrity(format!(
                "transcript root {} has invalid extent chunk {chunk_index}",
                root.id.as_str()
            )));
        }
        let rows = validated_profile_rows(rows, record_count)?;
        chunks.push(TranscriptExtentChunk {
            start: TranscriptRecordOffset::new(expected_start),
            record_count,
            profile: TranscriptExtentProfile::new(rows),
        });
        expected_start = expected_start.saturating_add(record_count);
    }
    if expected_start != total {
        return Err(StoreError::Integrity(format!(
            "transcript root {} extent profiles cover {expected_start} of {total} records",
            root.id.as_str()
        )));
    }
    Ok(chunks)
}

pub(crate) fn lineage_transcript_estimated_rows(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    range: TranscriptRecordRange,
    width: u16,
) -> Result<u64> {
    if root.kind != SequenceKind::Transcript {
        return Err(StoreError::Integrity(
            "transcript extent query requires a transcript root".into(),
        ));
    }
    let total = usize::try_from(root.item_count).map_err(|_| {
        StoreError::Integrity("transcript extent length exceeds platform limits".into())
    })?;
    let start = range.start().get().min(total);
    let end = range.end().get().min(total);
    if start >= end {
        return Ok(0);
    }

    let first_full_chunk = start.div_ceil(TRANSCRIPT_EXTENT_CHUNK_RECORDS);
    let full_chunk_end = end / TRANSCRIPT_EXTENT_CHUNK_RECORDS;
    let leading_end = end.min(first_full_chunk.saturating_mul(TRANSCRIPT_EXTENT_CHUNK_RECORDS));
    let mut profile_rows = [0_u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()];
    add_record_range_profile(conn, lineage, root, start..leading_end, &mut profile_rows)?;

    if first_full_chunk < full_chunk_end {
        let (chunk_count, record_count, rows): (i64, i64, [i64; 6]) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(record_count), 0),
                    COALESCE(SUM(rows_20), 0), COALESCE(SUM(rows_40), 0),
                    COALESCE(SUM(rows_80), 0), COALESCE(SUM(rows_120), 0),
                    COALESCE(SUM(rows_160), 0), COALESCE(SUM(rows_240), 0)
             FROM lineage_transcript_extent_chunks
             WHERE lineage_id = ?1 AND transcript_root_id = ?2
               AND chunk_index >= ?3 AND chunk_index < ?4",
            (
                lineage.as_str(),
                root.id.as_str(),
                checked_i64(first_full_chunk as u64, "transcript extent range start")?,
                checked_i64(full_chunk_end as u64, "transcript extent range end")?,
            ),
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    [
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ],
                ))
            },
        )?;
        let expected_chunks = full_chunk_end - first_full_chunk;
        let expected_records = expected_chunks.saturating_mul(TRANSCRIPT_EXTENT_CHUNK_RECORDS);
        if nonnegative_usize(chunk_count, "transcript extent range chunk count")? != expected_chunks
            || nonnegative_usize(record_count, "transcript extent range record count")?
                != expected_records
        {
            return Err(StoreError::Integrity(format!(
                "transcript root {} has incomplete extent range",
                root.id.as_str()
            )));
        }
        add_profile_rows(
            &mut profile_rows,
            validated_profile_rows(rows, expected_records)?,
        );
    }

    let trailing_start = leading_end.max(
        full_chunk_end
            .saturating_mul(TRANSCRIPT_EXTENT_CHUNK_RECORDS)
            .min(end),
    );
    add_record_range_profile(conn, lineage, root, trailing_start..end, &mut profile_rows)?;
    Ok(TranscriptExtentProfile::new(profile_rows).estimated_rows(width))
}

fn add_record_range_profile(
    conn: &Connection,
    lineage: &LineageId,
    root: &SequenceRoot,
    range: std::ops::Range<usize>,
    profile_rows: &mut [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
) -> Result<()> {
    if range.start >= range.end {
        return Ok(());
    }
    let records =
        deserialize_sequence_range(conn, lineage, root, range.start as u64, range.end as u64)?;
    if records.len() != range.end - range.start {
        return Err(StoreError::Integrity(
            "transcript extent boundary returned the wrong record count".into(),
        ));
    }
    add_profile_rows(profile_rows, transcript_extent_profile(&records).rows());
    Ok(())
}

fn add_profile_rows(
    target: &mut [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
    source: [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
) {
    for (target, source) in target.iter_mut().zip(source) {
        *target = target.saturating_add(source);
    }
}

fn validated_profile_rows(
    rows: [i64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()],
    record_count: usize,
) -> Result<[u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()]> {
    let rows = rows.map(|value| {
        u64::try_from(value)
            .map_err(|_| StoreError::Integrity("transcript extent rows are negative".into()))
    });
    let rows = rows.into_iter().collect::<Result<Vec<_>>>()?;
    let rows: [u64; TRANSCRIPT_EXTENT_PROFILE_WIDTHS.len()] = rows.try_into().map_err(|_| {
        StoreError::Integrity("transcript extent profile has the wrong width".into())
    })?;
    if rows.iter().any(|rows| *rows < record_count as u64)
        || !rows.windows(2).all(|pair| pair[0] >= pair[1])
    {
        return Err(StoreError::Integrity(
            "transcript extent profile is invalid".into(),
        ));
    }
    Ok(rows)
}

fn nonnegative_usize(value: i64, field: &str) -> Result<usize> {
    usize::try_from(value).map_err(|_| StoreError::Integrity(format!("{field} is negative")))
}
