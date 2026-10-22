use super::{ChangeKind, Repository, RepositoryChange, checked_revision, commit_change, schema};
use crate::domain::lists::{DislikeAction, DislikeRules, SEARCH_HISTORY_LIMIT, trim_dislike};
use anyhow::{Result, ensure};
use rusqlite::{Connection, TransactionBehavior, params};

pub const MAX_DISLIKE_BYTES: usize = 2 * 1024 * 1024;
impl Repository {
    pub fn dislike_rules(&self) -> Result<DislikeRules> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        Ok(DislikeRules::parse(&raw_dislike(&connection)?))
    }
    pub fn apply_dislike_action(
        &self,
        action: &DislikeAction,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        self.apply_dislike_action_with_origin(action, expected_revision, "local")
    }
    pub fn apply_dislike_action_with_origin(
        &self,
        action: &DislikeAction,
        expected_revision: Option<u64>,
        origin: &str,
    ) -> Result<RepositoryChange> {
        ensure!(
            !origin.is_empty() && origin.len() <= 256,
            "Dislike origin is invalid"
        );
        let(_,change)=self.write(expected_revision,|tx|{
            let raw=match action{DislikeAction::Clear=>String::new(),DislikeAction::Overwrite{rules}=>rules.split('\n').filter(|line|!trim_dislike(line).is_empty()).collect::<Vec<_>>().join("\n"),DislikeAction::Add{infos}=>{
                let mut raw=raw_dislike(tx)?;for info in infos{if !raw.is_empty(){raw.push('\n');}raw.push_str(&info.name);raw.push('@');raw.push_str(&info.singer);}raw
            }};
            ensure!(raw.len()<=MAX_DISLIKE_BYTES&&!raw.contains('\0'),"Dislike rules are oversized or contain NUL");
            tx.execute("UPDATE dislike_state SET rules=?1 WHERE singleton=1",[&raw])?;
            Ok(((),ChangeKind::DislikeChanged,None,serde_json::json!({"action":action,"rules":DislikeRules::parse(&raw).rules,"origin":origin})))
        })?;
        Ok(change)
    }
    pub fn search_history(&self) -> Result<Vec<String>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        history(&connection)
    }
    /// The current first word is a true no-op, including revision and outbox.
    pub fn record_search(
        &self,
        keyword: &str,
        expected_revision: Option<u64>,
    ) -> Result<Option<RepositoryChange>> {
        ensure!(
            !keyword.is_empty() && keyword.len() <= 8192 && !keyword.contains('\0'),
            "Search word is empty or too long"
        );
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository lock was poisoned"))?;
        schema::validate_current(&connection)?;
        let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: i64 = tx.query_row(
            "SELECT revision FROM repository_state WHERE singleton=1",
            [],
            |row| row.get(0),
        )?;
        if let Some(expected) = expected_revision {
            ensure!(
                checked_revision(expected)? == current,
                "Repository revision conflict"
            );
        }
        let mut words = history(&tx)?;
        if words.first().is_some_and(|word| word == keyword) {
            tx.commit()?;
            return Ok(None);
        }
        words.retain(|word| word != keyword);
        words.truncate(SEARCH_HISTORY_LIMIT - 1);
        words.insert(0, keyword.into());
        write_history(&tx, &words)?;
        let change = commit_change(
            &tx,
            current,
            ChangeKind::SearchHistoryChanged,
            None,
            serde_json::json!({"operation":"add","word":keyword,"history":words}),
        )?;
        tx.commit()?;
        Ok(Some(change))
    }
    pub fn clear_search_history(&self, expected_revision: Option<u64>) -> Result<RepositoryChange> {
        let (_, change) = self.write(expected_revision, |tx| {
            tx.execute("DELETE FROM search_history", [])?;
            Ok((
                (),
                ChangeKind::SearchHistoryChanged,
                None,
                serde_json::json!({"operation":"clear","history":[]}),
            ))
        })?;
        Ok(change)
    }
    pub fn remove_search_history(
        &self,
        keyword: &str,
        expected_revision: Option<u64>,
    ) -> Result<RepositoryChange> {
        let (_, change) = self.write(expected_revision, |tx| {
            let mut words = history(tx)?;
            words.retain(|word| word != keyword);
            write_history(tx, &words)?;
            Ok((
                (),
                ChangeKind::SearchHistoryChanged,
                None,
                serde_json::json!({"operation":"remove","word":keyword,"history":words}),
            ))
        })?;
        Ok(change)
    }
}
fn raw_dislike(connection: &Connection) -> Result<String> {
    let rules: String = connection.query_row(
        "SELECT rules FROM dislike_state WHERE singleton=1",
        [],
        |row| row.get(0),
    )?;
    ensure!(
        rules.len() <= MAX_DISLIKE_BYTES,
        "Dislike rules exceed the limit"
    );
    Ok(rules)
}
fn history(connection: &Connection) -> Result<Vec<String>> {
    let mut statement =
        connection.prepare("SELECT word FROM search_history ORDER BY position LIMIT 15")?;
    statement
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()
        .map_err(Into::into)
}
fn write_history(connection: &Connection, words: &[String]) -> Result<()> {
    connection.execute("DELETE FROM search_history", [])?;
    for (position, word) in words.iter().take(SEARCH_HISTORY_LIMIT).enumerate() {
        connection.execute(
            "INSERT INTO search_history(word,position) VALUES(?1,?2)",
            params![word, position as i64],
        )?;
    }
    Ok(())
}
