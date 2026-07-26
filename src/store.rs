use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, params};

use crate::codex::CodexSession;

pub const DEFAULT_STALE_AFTER_SECONDS: u64 = 7 * 24 * 60 * 60;

#[derive(Debug)]
pub enum StoreError {
    Io(io::Error),
    Json(serde_json::Error),
    MissingStateDirectory,
    Sql(rusqlite::Error),
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "could not prepare Rollcall state: {error}"),
            Self::Json(error) => write!(formatter, "invalid Rollcall session snapshot: {error}"),
            Self::MissingStateDirectory => {
                write!(formatter, "could not determine a local state directory")
            }
            Self::Sql(error) => write!(formatter, "Rollcall state database failed: {error}"),
        }
    }
}

impl From<io::Error> for StoreError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

pub struct Store {
    connection: Connection,
}

impl Store {
    pub fn open() -> Result<Self, StoreError> {
        let path = state_path()?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Self::open_at(&path)
    }

    pub(crate) fn open_at(path: &Path) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    #[cfg(test)]
    pub(crate) fn open_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        connection.execute_batch(
            "
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS sessions (
                session_id TEXT PRIMARY KEY,
                snapshot TEXT NOT NULL,
                archived INTEGER NOT NULL DEFAULT 0,
                archived_at INTEGER,
                last_seen INTEGER NOT NULL,
                last_interaction INTEGER NOT NULL DEFAULT 0,
                archive_reason TEXT,
                archive_basis_interaction INTEGER,
                active_override_interaction INTEGER
            );

            CREATE INDEX IF NOT EXISTS sessions_archived_last_seen
                ON sessions (archived, last_seen DESC);
            ",
        )?;
        ensure_column(
            &connection,
            "last_interaction",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&connection, "archive_reason", "TEXT")?;
        ensure_column(&connection, "archive_basis_interaction", "INTEGER")?;
        ensure_column(&connection, "active_override_interaction", "INTEGER")?;
        backfill_last_interaction(&connection)?;
        Ok(Self { connection })
    }

    pub fn record(&mut self, sessions: &[CodexSession]) -> Result<(), StoreError> {
        let transaction = self.connection.transaction()?;
        let last_seen = now_unix_seconds();
        {
            let mut statement = transaction.prepare(
                "
                INSERT INTO sessions (
                    session_id,
                    snapshot,
                    archived,
                    last_seen,
                    last_interaction
                )
                VALUES (?1, ?2, 0, ?3, ?4)
                ON CONFLICT(session_id) DO UPDATE SET
                    snapshot = excluded.snapshot,
                    last_seen = excluded.last_seen,
                    last_interaction = excluded.last_interaction,
                    archived = CASE
                        WHEN sessions.archive_reason = 'stale'
                            AND excluded.last_interaction
                                > COALESCE(sessions.archive_basis_interaction, -1)
                        THEN 0
                        ELSE sessions.archived
                    END,
                    archived_at = CASE
                        WHEN sessions.archive_reason = 'stale'
                            AND excluded.last_interaction
                                > COALESCE(sessions.archive_basis_interaction, -1)
                        THEN NULL
                        ELSE sessions.archived_at
                    END,
                    archive_reason = CASE
                        WHEN sessions.archive_reason = 'stale'
                            AND excluded.last_interaction
                                > COALESCE(sessions.archive_basis_interaction, -1)
                        THEN NULL
                        ELSE sessions.archive_reason
                    END,
                    archive_basis_interaction = CASE
                        WHEN sessions.archive_reason = 'stale'
                            AND excluded.last_interaction
                                > COALESCE(sessions.archive_basis_interaction, -1)
                        THEN NULL
                        ELSE sessions.archive_basis_interaction
                    END,
                    active_override_interaction = CASE
                        WHEN excluded.last_interaction
                            > COALESCE(
                                sessions.active_override_interaction,
                                excluded.last_interaction
                            )
                        THEN NULL
                        ELSE sessions.active_override_interaction
                    END
                ",
            )?;
            for session in sessions {
                statement.execute(params![
                    session.id,
                    serde_json::to_string(session)?,
                    last_seen,
                    i64::try_from(session.last_interaction_unix_seconds).unwrap_or(i64::MAX)
                ])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn load(&self, archived: bool) -> Result<Vec<CodexSession>, StoreError> {
        let mut statement = self.connection.prepare(
            "
            SELECT snapshot
            FROM sessions
            WHERE archived = ?1
            ORDER BY last_seen DESC
            ",
        )?;
        let snapshots = statement
            .query_map([archived], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        snapshots
            .into_iter()
            .map(|snapshot| serde_json::from_str(&snapshot).map_err(Into::into))
            .collect()
    }

    pub fn set_archived(&self, session_id: &str, archived: bool) -> Result<(), StoreError> {
        let changed = self.connection.execute(
            "
            UPDATE sessions
            SET archived = ?2,
                archived_at = CASE WHEN ?2 THEN ?3 ELSE NULL END,
                archive_reason = CASE WHEN ?2 THEN 'manual' ELSE NULL END,
                archive_basis_interaction = NULL,
                active_override_interaction = CASE
                    WHEN ?2 THEN NULL
                    ELSE last_interaction
                END
            WHERE session_id = ?1
            ",
            params![session_id, archived, now_unix_seconds()],
        )?;
        if changed == 0 {
            return Err(StoreError::Sql(rusqlite::Error::QueryReturnedNoRows));
        }
        Ok(())
    }

    pub fn auto_settle_stale(&self, stale_after_seconds: u64) -> Result<usize, StoreError> {
        self.auto_settle_stale_at(now_unix_seconds(), stale_after_seconds)
    }

    fn auto_settle_stale_at(
        &self,
        now_unix_seconds: i64,
        stale_after_seconds: u64,
    ) -> Result<usize, StoreError> {
        let stale_after_seconds = i64::try_from(stale_after_seconds).unwrap_or(i64::MAX);
        let cutoff = now_unix_seconds.saturating_sub(stale_after_seconds);
        let mut statement = self.connection.prepare(
            "
            SELECT session_id, snapshot, last_interaction
            FROM sessions
            WHERE archived = 0
                AND last_interaction <= ?1
                AND (
                    active_override_interaction IS NULL
                    OR active_override_interaction != last_interaction
                )
            ",
        )?;
        let candidates = statement
            .query_map([cutoff], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut settled = 0;
        for (session_id, snapshot, last_interaction) in candidates {
            let session: CodexSession = serde_json::from_str(&snapshot)?;
            if !session.activity.can_auto_settle() {
                continue;
            }
            settled += self.connection.execute(
                "
                UPDATE sessions
                SET archived = 1,
                    archived_at = ?2,
                    archive_reason = 'stale',
                    archive_basis_interaction = last_interaction
                WHERE session_id = ?1
                    AND archived = 0
                    AND last_interaction = ?3
                    AND (
                        active_override_interaction IS NULL
                        OR active_override_interaction != last_interaction
                    )
                ",
                params![session_id, now_unix_seconds, last_interaction],
            )?;
        }
        Ok(settled)
    }
}

fn ensure_column(connection: &Connection, name: &str, definition: &str) -> Result<(), StoreError> {
    let mut statement = connection.prepare("PRAGMA table_info(sessions)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;
    if !columns.iter().any(|column| column == name) {
        connection.execute_batch(&format!(
            "ALTER TABLE sessions ADD COLUMN {name} {definition};"
        ))?;
    }
    Ok(())
}

fn backfill_last_interaction(connection: &Connection) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "
        SELECT session_id, snapshot
        FROM sessions
        WHERE last_interaction = 0
        ",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    for (session_id, snapshot) in rows {
        let session: CodexSession = serde_json::from_str(&snapshot)?;
        connection.execute(
            "
            UPDATE sessions
            SET last_interaction = ?2
            WHERE session_id = ?1
            ",
            params![
                session_id,
                i64::try_from(session.last_interaction_unix_seconds).unwrap_or(i64::MAX)
            ],
        )?;
    }
    Ok(())
}

fn state_path() -> Result<PathBuf, StoreError> {
    if let Some(path) = env::var_os("ROLLCALL_STATE_PATH") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(path).join("rollcall").join("rollcall.db"));
    }
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(".local/state/rollcall/rollcall.db"))
        .ok_or(StoreError::MissingStateDirectory)
}

fn now_unix_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};
    use tempfile::tempdir;

    use super::Store;
    use crate::codex::{CodexActivity, CodexRuntime, CodexSession};

    fn session(title: &str) -> CodexSession {
        CodexSession {
            id: "topo:codex:019f".to_owned(),
            host: "topo".to_owned(),
            native_session_id: "019f".to_owned(),
            title: title.to_owned(),
            cwd: "/fabric".to_owned(),
            source: "cli".to_owned(),
            activity: CodexActivity::Completed,
            last_message: "done".to_owned(),
            last_interaction_unix_seconds: 100,
            updated_unix_seconds: 100,
            runtime: CodexRuntime::Resumable,
            tmux: None,
        }
    }

    #[test]
    fn snapshots_remain_archived_when_native_metadata_refreshes() {
        let directory = tempdir().expect("temporary directory should exist");
        let mut store =
            Store::open_at(&directory.path().join("state.db")).expect("store should open");

        store
            .record(&[session("first")])
            .expect("snapshot should save");
        store
            .set_archived("topo:codex:019f", true)
            .expect("session should archive");
        store
            .record(&[session("updated")])
            .expect("snapshot should update");

        assert!(
            store
                .load(false)
                .expect("active view should load")
                .is_empty()
        );
        assert_eq!(
            store.load(true).expect("archive should load")[0].title,
            "updated"
        );
    }

    #[test]
    fn archived_sessions_can_be_restored() {
        let directory = tempdir().expect("temporary directory should exist");
        let mut store =
            Store::open_at(&directory.path().join("state.db")).expect("store should open");

        store
            .record(&[session("saved")])
            .expect("snapshot should save");
        store
            .set_archived("topo:codex:019f", true)
            .expect("session should archive");
        store
            .set_archived("topo:codex:019f", false)
            .expect("session should restore");

        assert_eq!(store.load(false).expect("active view should load").len(), 1);
        assert!(store.load(true).expect("archive should load").is_empty());
    }

    #[test]
    fn completed_sessions_auto_settle_after_one_week() {
        let mut store = Store::open_memory().expect("store should open");
        let mut stale = session("stale");
        stale.last_interaction_unix_seconds = 100;
        store.record(&[stale]).expect("snapshot should save");

        assert_eq!(
            store
                .auto_settle_stale_at(
                    100 + super::DEFAULT_STALE_AFTER_SECONDS as i64,
                    super::DEFAULT_STALE_AFTER_SECONDS
                )
                .expect("stale policy should run"),
            1
        );
        assert!(
            store
                .load(false)
                .expect("active view should load")
                .is_empty()
        );
        assert_eq!(store.load(true).expect("archive should load").len(), 1);
    }

    #[test]
    fn attention_states_never_auto_settle() {
        for activity in [
            CodexActivity::Working,
            CodexActivity::WaitingApproval,
            CodexActivity::WaitingInput,
            CodexActivity::Failed,
        ] {
            let mut store = Store::open_memory().expect("store should open");
            let mut candidate = session("attention");
            candidate.activity = activity;
            candidate.last_interaction_unix_seconds = 100;
            store.record(&[candidate]).expect("snapshot should save");

            assert_eq!(
                store
                    .auto_settle_stale_at(1_000_000, 60)
                    .expect("stale policy should run"),
                0
            );
            assert_eq!(store.load(false).expect("active view should load").len(), 1);
        }
    }

    #[test]
    fn manually_restored_stale_sessions_stay_active_until_new_activity() {
        let mut store = Store::open_memory().expect("store should open");
        let mut stale = session("stale");
        stale.last_interaction_unix_seconds = 100;
        store
            .record(&[stale.clone()])
            .expect("snapshot should save");
        store
            .auto_settle_stale_at(1_000, 60)
            .expect("stale policy should run");
        store
            .set_archived(&stale.id, false)
            .expect("session should restore");

        assert_eq!(
            store
                .auto_settle_stale_at(1_000, 60)
                .expect("stale policy should rerun"),
            0
        );
        store
            .record(&[stale.clone()])
            .expect("unchanged refresh should save");
        assert_eq!(
            store
                .auto_settle_stale_at(1_000, 60)
                .expect("stale policy should still preserve restore"),
            0
        );

        stale.last_interaction_unix_seconds = 200;
        store.record(&[stale]).expect("new activity should save");
        assert_eq!(
            store
                .auto_settle_stale_at(1_000, 60)
                .expect("new stale period should settle"),
            1
        );
    }

    #[test]
    fn new_activity_wakes_an_auto_settled_session_but_not_a_manual_archive() {
        let mut auto_store = Store::open_memory().expect("store should open");
        let mut auto = session("auto");
        auto.last_interaction_unix_seconds = 100;
        auto_store
            .record(&[auto.clone()])
            .expect("snapshot should save");
        auto_store
            .auto_settle_stale_at(1_000, 60)
            .expect("stale policy should run");
        auto.last_interaction_unix_seconds = 950;
        auto_store
            .record(&[auto])
            .expect("new activity should save");
        assert_eq!(
            auto_store
                .load(false)
                .expect("active view should load")
                .len(),
            1
        );

        let mut manual_store = Store::open_memory().expect("store should open");
        let mut manual = session("manual");
        manual.last_interaction_unix_seconds = 100;
        manual_store
            .record(&[manual.clone()])
            .expect("snapshot should save");
        manual_store
            .set_archived(&manual.id, true)
            .expect("session should archive");
        manual.last_interaction_unix_seconds = 950;
        manual_store
            .record(&[manual])
            .expect("new activity should save");
        assert!(
            manual_store
                .load(false)
                .expect("active view should load")
                .is_empty()
        );
    }

    #[test]
    fn existing_databases_backfill_native_interaction_time() {
        let directory = tempdir().expect("temporary directory should exist");
        let path = directory.path().join("state.db");
        let connection = Connection::open(&path).expect("legacy database should open");
        connection
            .execute_batch(
                "
                CREATE TABLE sessions (
                    session_id TEXT PRIMARY KEY,
                    snapshot TEXT NOT NULL,
                    archived INTEGER NOT NULL DEFAULT 0,
                    archived_at INTEGER,
                    last_seen INTEGER NOT NULL
                );
                ",
            )
            .expect("legacy schema should be created");
        let candidate = session("legacy");
        connection
            .execute(
                "
                INSERT INTO sessions (session_id, snapshot, archived, last_seen)
                VALUES (?1, ?2, 0, 100)
                ",
                params![
                    candidate.id,
                    serde_json::to_string(&candidate).expect("snapshot should serialize")
                ],
            )
            .expect("legacy snapshot should save");
        drop(connection);

        let store = Store::open_at(&path).expect("legacy database should migrate");
        let last_interaction = store
            .connection
            .query_row(
                "SELECT last_interaction FROM sessions WHERE session_id = ?1",
                [&candidate.id],
                |row| row.get::<_, i64>(0),
            )
            .expect("backfilled interaction should load");

        assert_eq!(last_interaction, 100);
    }
}
