use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior};

use crate::error::{Result, StoreError};

pub(crate) const WRITE_DEADLINE: Duration = Duration::from_secs(5);
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Only acquisition is retried. Once acquired, a transaction runs exactly once.
/// Callers performing interactive work must use a worker, not the UI thread.
pub(crate) fn begin_write<'a>(
    conn: &'a mut Connection,
    operation: &'static str,
) -> Result<Transaction<'a>> {
    begin_write_until(conn, operation, Instant::now() + WRITE_DEADLINE, &|| false)
}

pub(crate) fn begin_write_until<'a>(
    conn: &'a mut Connection,
    operation: &'static str,
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<Transaction<'a>> {
    let started = Instant::now();
    let mut attempts = 0;
    loop {
        if cancelled() {
            return Err(StoreError::Cancelled);
        }
        attempts += 1;
        match Transaction::new_unchecked(conn, TransactionBehavior::Immediate) {
            Ok(transaction) => return Ok(transaction),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == ErrorCode::DatabaseBusy => {}
            Err(error) => return Err(error.into()),
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(StoreError::Busy {
                operation,
                attempts,
                waited_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
            });
        }
        std::thread::sleep(RETRY_INTERVAL.min(deadline - now));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contention_has_a_deadline_and_never_runs_a_partial_transaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.db");
        let mut owner = Connection::open(&path).unwrap();
        owner
            .execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE items(value INTEGER)")
            .unwrap();
        let _transaction = owner
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        let mut contender = Connection::open(&path).unwrap();
        contender.busy_timeout(Duration::ZERO).unwrap();
        let result = begin_write_until(
            &mut contender,
            "test write",
            Instant::now() + Duration::from_millis(30),
            &|| false,
        );
        assert!(matches!(
            result,
            Err(StoreError::Busy {
                operation: "test write",
                ..
            })
        ));
        drop(result);
        assert!(contender.is_autocommit());
    }

    #[test]
    fn cancelled_acquisition_does_not_take_the_write_lock() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            begin_write_until(
                &mut conn,
                "cancelled write",
                Instant::now() + WRITE_DEADLINE,
                &|| true
            ),
            Err(StoreError::Cancelled)
        ));
        assert!(conn.is_autocommit());
    }
}
