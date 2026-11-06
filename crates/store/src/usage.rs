use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::Store;

const DAY_MS: u64 = 86_400_000;
/// Calls before yesterday that trigger folding them into daily rows.
const FOLD_ROWS: i64 = 1_000;

/// Tokens and cost of one model call, or of a day of calls to one model.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UsageEntry {
    pub timestamp_ms: u64,
    pub calls: u64,
    pub model: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub cost_usd: Option<f64>,
    pub cache_read_tokens: Option<u64>,
    pub cache_write_tokens: Option<u64>,
    pub reasoning_tokens: Option<u64>,
}

impl Store {
    /// Records one entry. Once enough calls predate yesterday, folds them into
    /// one row per UTC day and model, so totals, calls, daily activity and the
    /// last day stay exact.
    pub fn record_usage(&self, entry: &UsageEntry) -> Result<()> {
        self.insert_usage(std::slice::from_ref(entry))
    }

    /// Appends entries saved by smelt 0.7, in one transaction.
    // COMPAT(state-v0.7)
    pub fn import_usage(&self, entries: &[UsageEntry]) -> Result<()> {
        self.insert_usage(entries)
    }

    fn insert_usage(&self, entries: &[UsageEntry]) -> Result<()> {
        let Some(latest) = entries.iter().map(|entry| entry.timestamp_ms).max() else {
            return Ok(());
        };
        let mut conn = self.connect_ledger()?;
        let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        {
            let mut insert = tx.prepare(
                "INSERT INTO usage (timestamp_ms, calls, model, prompt_tokens, completion_tokens,
                     cost_usd, cache_read_tokens, cache_write_tokens, reasoning_tokens)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for entry in entries {
                insert.execute(rusqlite::params![
                    signed(entry.timestamp_ms),
                    signed(entry.calls),
                    entry.model,
                    signed(entry.prompt_tokens),
                    signed(entry.completion_tokens),
                    entry.cost_usd,
                    entry.cache_read_tokens.map(signed),
                    entry.cache_write_tokens.map(signed),
                    entry.reasoning_tokens.map(signed),
                ])?;
            }
        }
        let cutoff = signed((latest / DAY_MS).saturating_sub(1) * DAY_MS);
        let old: i64 = tx.query_row(
            "SELECT COUNT(*) FROM usage WHERE timestamp_ms < ?1",
            [cutoff],
            |row| row.get(0),
        )?;
        if old > FOLD_ROWS {
            fold_before(&tx, cutoff)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Every entry, oldest first.
    pub fn usage(&self) -> Result<Vec<UsageEntry>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT timestamp_ms, calls, model, prompt_tokens, completion_tokens, cost_usd,
                 cache_read_tokens, cache_write_tokens, reasoning_tokens
             FROM usage ORDER BY timestamp_ms, id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(UsageEntry {
                timestamp_ms: unsigned(row.get(0)?),
                calls: unsigned(row.get(1)?),
                model: row.get(2)?,
                prompt_tokens: unsigned(row.get(3)?),
                completion_tokens: unsigned(row.get(4)?),
                cost_usd: row.get(5)?,
                cache_read_tokens: row.get::<_, Option<i64>>(6)?.map(unsigned),
                cache_write_tokens: row.get::<_, Option<i64>>(7)?.map(unsigned),
                reasoning_tokens: row.get::<_, Option<i64>>(8)?.map(unsigned),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }
}

fn fold_before(conn: &rusqlite::Connection, cutoff: i64) -> Result<()> {
    let last: i64 = conn.query_row("SELECT COALESCE(MAX(id), 0) FROM usage", [], |row| {
        row.get(0)
    })?;
    conn.execute(
        "INSERT INTO usage (timestamp_ms, calls, model, prompt_tokens, completion_tokens,
             cost_usd, cache_read_tokens, cache_write_tokens, reasoning_tokens)
         SELECT timestamp_ms / ?2 * ?2, SUM(calls), model, SUM(prompt_tokens),
             SUM(completion_tokens), SUM(cost_usd), SUM(cache_read_tokens),
             SUM(cache_write_tokens), SUM(reasoning_tokens)
         FROM usage WHERE timestamp_ms < ?1
         GROUP BY timestamp_ms / ?2, model",
        rusqlite::params![cutoff, signed(DAY_MS)],
    )?;
    conn.execute(
        "DELETE FROM usage WHERE timestamp_ms < ?1 AND id <= ?2",
        rusqlite::params![cutoff, last],
    )?;
    Ok(())
}

fn signed(value: u64) -> i64 {
    value.min(i64::MAX as u64) as i64
}

fn unsigned(value: i64) -> u64 {
    value.max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(timestamp_ms: u64, model: &str, prompt: u64, cost: Option<f64>) -> UsageEntry {
        UsageEntry {
            timestamp_ms,
            calls: 1,
            model: model.into(),
            prompt_tokens: prompt,
            completion_tokens: 1,
            cost_usd: cost,
            cache_write_tokens: Some(2),
            ..UsageEntry::default()
        }
    }

    fn totals(entries: &[UsageEntry]) -> (u64, u64, u64, f64) {
        entries.iter().fold((0, 0, 0, 0.0), |acc, e| {
            (
                acc.0 + e.calls,
                acc.1 + e.prompt_tokens,
                acc.2 + e.cache_write_tokens.unwrap_or(0),
                acc.3 + e.cost_usd.unwrap_or(0.0),
            )
        })
    }

    #[test]
    fn folding_keeps_totals_calls_days_and_the_last_day_exact() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        let now = 100 * DAY_MS + 5;
        let mut all = Vec::new();
        for index in 0..=FOLD_ROWS as u64 {
            all.push(entry(90 * DAY_MS + index, "a", 10, Some(0.5)));
        }
        all.push(entry(91 * DAY_MS, "b", 40, None));
        all.push(entry(99 * DAY_MS + 7, "a", 50, None));
        all.push(entry(now, "a", 60, None));
        for e in &all {
            store.record_usage(e).unwrap();
        }

        let stored = store.usage().unwrap();
        assert_eq!(totals(&stored), totals(&all));
        let days: Vec<_> = stored
            .iter()
            .map(|e| (e.timestamp_ms, e.model.as_str(), e.calls))
            .collect();
        assert_eq!(
            days,
            [
                (90 * DAY_MS, "a", FOLD_ROWS as u64 + 1),
                (91 * DAY_MS, "b", 1),
                (99 * DAY_MS + 7, "a", 1),
                (now, "a", 1),
            ]
        );
    }
}
