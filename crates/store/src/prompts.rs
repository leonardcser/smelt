use rusqlite::OptionalExtension;

use crate::error::Result;
use crate::Store;

impl Store {
    /// Every submitted prompt, oldest first.
    pub fn prompts(&self) -> Result<Vec<String>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare("SELECT text FROM prompts ORDER BY id")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Records a submitted prompt unless it repeats the latest one.
    pub fn push_prompt(&self, text: &str) -> Result<()> {
        let conn = self.connect_ledger()?;
        let latest: Option<String> = conn
            .query_row(
                "SELECT text FROM prompts ORDER BY id DESC LIMIT 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if latest.as_deref() != Some(text) {
            conn.execute("INSERT INTO prompts (text) VALUES (?1)", [text])?;
        }
        Ok(())
    }

    /// Appends prompts saved by smelt 0.7, in one transaction.
    // COMPAT(state-v0.7)
    pub fn import_prompts(&self, texts: &[String]) -> Result<()> {
        let mut conn = self.connect_ledger()?;
        let tx = conn.transaction()?;
        {
            let mut insert = tx.prepare("INSERT INTO prompts (text) VALUES (?1)")?;
            for text in texts {
                insert.execute([text])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::Store;

    #[test]
    fn prompts_keep_order_and_skip_repeats_of_the_latest() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path());
        for text in ["one", "two", "two", "one"] {
            store.push_prompt(text).unwrap();
        }
        assert_eq!(store.prompts().unwrap(), ["one", "two", "one"]);
    }
}
