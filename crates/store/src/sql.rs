//! Statement-cached SQL execution.
//!
//! Compiling SQL dominates small writes: a session commit runs hundreds of
//! short statements. Static SQL goes through the connection's statement cache
//! so each statement is compiled once per connection.

use rusqlite::{Connection, Params, Result, Row};

/// Prepared statements kept per connection. Large enough to hold every static
/// statement of a session commit, so a commit never evicts its own statements.
pub(crate) const STATEMENT_CACHE_CAPACITY: usize = 512;

pub(crate) trait CachedSql {
    /// [`Connection::execute`] through the statement cache.
    fn execute_cached<P: Params>(&self, sql: &str, params: P) -> Result<usize>;

    /// [`Connection::query_row`] through the statement cache.
    fn query_row_cached<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T>;
}

impl CachedSql for Connection {
    fn execute_cached<P: Params>(&self, sql: &str, params: P) -> Result<usize> {
        self.prepare_cached(sql)?.execute(params)
    }

    fn query_row_cached<T, P, F>(&self, sql: &str, params: P, f: F) -> Result<T>
    where
        P: Params,
        F: FnOnce(&Row<'_>) -> Result<T>,
    {
        self.prepare_cached(sql)?.query_row(params, f)
    }
}
