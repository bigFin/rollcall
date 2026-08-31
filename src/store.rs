use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::domain::{Activity, AgentKind, Session};

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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SessionTransitionKind {
    Completed,
    Approval,
    Input,
    Failed,
}

impl SessionTransitionKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Approval => "approval",
            Self::Input => "input",
            Self::Failed => "failed",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTransition {
    pub session_id: String,
    pub title: String,
    pub host: String,
    pub kind: SessionTransitionKind,
    pub observed_at_unix_seconds: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryEntry {
    pub session: Session,
    pub settled: bool,
    pub unread: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notification_at_unix_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settled_at_unix_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub settle_reason: Option<String>,
    pub last_seen_unix_seconds: u64,
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
                active_override_interaction INTEGER,
                notification_unread INTEGER NOT NULL DEFAULT 0,
                notification_kind TEXT,
                notification_at INTEGER
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
        ensure_column(
            &connection,
            "notification_unread",
            "INTEGER NOT NULL DEFAULT 0",
        )?;
        ensure_column(&connection, "notification_kind", "TEXT")?;
        ensure_column(&connection, "notification_at", "INTEGER")?;
        backfill_last_interaction(&connection)?;
        Ok(Self { connection })
    }

    pub fn record(&mut self, sessions: &[Session]) -> Result<Vec<SessionTransition>, StoreError> {
        let transaction = self.connection.transaction()?;
        let last_seen = now_unix_seconds();
        let mut transitions = Vec::new();
        {
            let mut previous_statement = transaction.prepare(
                "
                SELECT snapshot, archived, archive_reason
                FROM sessions
                WHERE session_id = ?1
                ",
            )?;
            let mut statement = transaction.prepare(
                "
                INSERT INTO sessions (
                    session_id,
                    snapshot,
                    archived,
                    last_seen,
                    last_interaction,
                    notification_unread,
                    notification_kind,
                    notification_at
                )
                VALUES (?1, ?2, 0, ?3, ?4, 0, NULL, NULL)
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
                    END,
                    notification_unread = CASE
                        WHEN ?5 IS NOT NULL THEN 1
                        ELSE sessions.notification_unread
                    END,
                    notification_kind = CASE
                        WHEN ?5 IS NOT NULL THEN ?5
                        ELSE sessions.notification_kind
                    END,
                    notification_at = CASE
                        WHEN ?5 IS NOT NULL THEN ?3
                        ELSE sessions.notification_at
                    END
                ",
            )?;
            for session in sessions {
                if is_legacy_omp_subagent(session) {
                    continue;
                }
                let previous = previous_statement
                    .query_row([&session.id], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, bool>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    })
                    .optional()?
                    .map(|(snapshot, archived, archive_reason)| {
                        serde_json::from_str::<Session>(&snapshot)
                            .map(|session| (session, archived, archive_reason))
                    })
                    .transpose()?;
                let transition = previous
                    .as_ref()
                    .filter(|(_, archived, archive_reason)| {
                        !archived || archive_reason.as_deref() == Some("stale")
                    })
                    .and_then(|(previous, _, _)| attention_transition(previous, session));
                statement.execute(params![
                    session.id,
                    serde_json::to_string(session)?,
                    last_seen,
                    i64::try_from(session.last_interaction_unix_seconds).unwrap_or(i64::MAX),
                    transition.map(SessionTransitionKind::as_str),
                ])?;
                if let Some(kind) = transition {
                    transitions.push(SessionTransition {
                        session_id: session.id.clone(),
                        title: session.title.clone(),
                        host: session.host.clone(),
                        kind,
                        observed_at_unix_seconds: nonnegative_u64(last_seen).unwrap_or_default(),
                    });
                }
            }
        }
        transaction.commit()?;
        Ok(transitions)
    }

    pub fn load(&self, archived: bool) -> Result<Vec<Session>, StoreError> {
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
            .filter(|snapshot| !matches!(snapshot, Ok(session) if is_legacy_omp_subagent(session)))
            .collect()
    }

    pub fn load_history(&self) -> Result<Vec<HistoryEntry>, StoreError> {
        let mut statement = self.connection.prepare(
            "
            SELECT
                snapshot,
                archived,
                notification_unread,
                notification_kind,
                notification_at,
                archived_at,
                archive_reason,
                last_seen
            FROM sessions
            ORDER BY last_interaction DESC, last_seen DESC
            ",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, bool>(1)?,
                    row.get::<_, bool>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.into_iter()
            .map(
                |(
                    snapshot,
                    settled,
                    unread,
                    notification_kind,
                    notification_at,
                    settled_at,
                    settle_reason,
                    last_seen,
                )| {
                    Ok(HistoryEntry {
                        session: serde_json::from_str(&snapshot)?,
                        settled,
                        unread,
                        notification_kind,
                        notification_at_unix_seconds: notification_at.and_then(nonnegative_u64),
                        settled_at_unix_seconds: settled_at.and_then(nonnegative_u64),
                        settle_reason,
                        last_seen_unix_seconds: nonnegative_u64(last_seen).unwrap_or_default(),
                    })
                },
            )
            .filter(|entry| !matches!(entry, Ok(entry) if is_legacy_omp_subagent(&entry.session)))
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
                END,
                notification_unread = CASE
                    WHEN ?2 THEN 0
                    ELSE notification_unread
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

    pub fn unread_session_ids(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare(
            "
            SELECT session_id
            FROM sessions
            WHERE notification_unread = 1
            ",
        )?;
        statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(Into::into)
    }

    pub fn acknowledge(&self, session_id: &str) -> Result<bool, StoreError> {
        Ok(self.connection.execute(
            "
            UPDATE sessions
            SET notification_unread = 0
            WHERE session_id = ?1
                AND notification_unread = 1
            ",
            [session_id],
        )? > 0)
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
                AND notification_unread = 0
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
            let session: Session = serde_json::from_str(&snapshot)?;
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
                    AND notification_unread = 0
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

fn attention_transition(previous: &Session, current: &Session) -> Option<SessionTransitionKind> {
    if previous.activity == current.activity {
        return None;
    }
    match current.activity {
        Activity::Completed
            if matches!(
                previous.activity,
                Activity::Working | Activity::WaitingApproval | Activity::WaitingInput
            ) =>
        {
            Some(SessionTransitionKind::Completed)
        }
        Activity::WaitingApproval => Some(SessionTransitionKind::Approval),
        Activity::WaitingInput => Some(SessionTransitionKind::Input),
        Activity::Failed => Some(SessionTransitionKind::Failed),
        Activity::Working | Activity::Completed | Activity::Unknown => None,
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
        let session: Session = serde_json::from_str(&snapshot)?;
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

fn nonnegative_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn is_legacy_omp_subagent(session: &Session) -> bool {
    (session.agent == AgentKind::Omp || session.source == "omp") && session.title.trim().is_empty()
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};
    use tempfile::tempdir;

    use super::{SessionTransitionKind, Store, attention_transition};
    use crate::domain::{Activity, AgentKind, RuntimeOwner, Session};

    fn session(title: &str) -> Session {
        Session {
            id: "topo:codex:019f".to_owned(),
            host: "topo".to_owned(),
            agent: AgentKind::Codex,
            native_session_id: "019f".to_owned(),
            title: title.to_owned(),
            cwd: "/fabric".to_owned(),
            source: "cli".to_owned(),
            activity: Activity::Completed,
            last_message: "done".to_owned(),
            last_interaction_unix_seconds: 100,
            updated_unix_seconds: 100,
            runtime: RuntimeOwner::Resumable,
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
    fn legacy_unnamed_omp_subagents_are_hidden_from_inventory_views() {
        let mut store = Store::open_memory().expect("memory store should open");
        let mut legacy = session("");
        legacy.id = "topo:omp:legacy".to_owned();
        legacy.agent = AgentKind::Omp;
        legacy.native_session_id = "legacy".to_owned();
        legacy.source = "omp".to_owned();

        store
            .record(&[session("visible"), legacy])
            .expect("snapshots should save");

        assert_eq!(store.load(false).expect("active view should load").len(), 1);
        assert_eq!(
            store.load_history().expect("history should load")[0]
                .session
                .title,
            "visible"
        );
    }

    #[test]
    fn working_to_completed_creates_one_persisted_unread_transition() {
        let mut store = Store::open_memory().expect("store should open");
        let mut candidate = session("turn");
        candidate.activity = Activity::Working;
        assert!(
            store
                .record(&[candidate.clone()])
                .expect("initial snapshot should save")
                .is_empty()
        );

        candidate.activity = Activity::Completed;
        let transitions = store
            .record(&[candidate.clone()])
            .expect("completion should save");

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].kind, SessionTransitionKind::Completed);
        assert_eq!(
            store
                .unread_session_ids()
                .expect("unread state should load"),
            [candidate.id.clone()]
        );
        assert!(
            store
                .record(&[candidate.clone()])
                .expect("unchanged completion should save")
                .is_empty()
        );
        assert!(
            store
                .acknowledge(&candidate.id)
                .expect("attention should acknowledge")
        );
        assert!(
            store
                .unread_session_ids()
                .expect("acknowledged state should load")
                .is_empty()
        );
        assert!(
            !store
                .acknowledge(&candidate.id)
                .expect("repeat acknowledgement should be harmless")
        );
    }

    #[test]
    fn attention_transitions_cover_approval_input_failure_and_completion() {
        let mut previous = session("previous");
        previous.activity = Activity::Working;
        let mut current = previous.clone();

        current.activity = Activity::WaitingApproval;
        assert_eq!(
            attention_transition(&previous, &current),
            Some(SessionTransitionKind::Approval)
        );
        previous.activity = Activity::WaitingApproval;
        current.activity = Activity::WaitingInput;
        assert_eq!(
            attention_transition(&previous, &current),
            Some(SessionTransitionKind::Input)
        );
        previous.activity = Activity::WaitingInput;
        current.activity = Activity::Failed;
        assert_eq!(
            attention_transition(&previous, &current),
            Some(SessionTransitionKind::Failed)
        );
        previous.activity = Activity::WaitingApproval;
        current.activity = Activity::Completed;
        assert_eq!(
            attention_transition(&previous, &current),
            Some(SessionTransitionKind::Completed)
        );
    }

    #[test]
    fn unread_completed_sessions_do_not_auto_settle_until_acknowledged() {
        let mut store = Store::open_memory().expect("store should open");
        let mut candidate = session("attention");
        candidate.activity = Activity::Working;
        candidate.last_interaction_unix_seconds = 100;
        store
            .record(&[candidate.clone()])
            .expect("working snapshot should save");
        candidate.activity = Activity::Completed;
        store
            .record(&[candidate.clone()])
            .expect("completion should save");

        assert_eq!(
            store
                .auto_settle_stale_at(1_000_000, 60)
                .expect("unread attention should remain active"),
            0
        );
        store
            .acknowledge(&candidate.id)
            .expect("completion should acknowledge");
        assert_eq!(
            store
                .auto_settle_stale_at(1_000_000, 60)
                .expect("read stale session should settle"),
            1
        );
    }

    #[test]
    fn manually_archived_sessions_do_not_emit_new_attention() {
        let mut store = Store::open_memory().expect("store should open");
        let mut candidate = session("archived");
        candidate.activity = Activity::Working;
        store
            .record(&[candidate.clone()])
            .expect("working snapshot should save");
        store
            .set_archived(&candidate.id, true)
            .expect("session should archive");
        candidate.activity = Activity::Completed;

        assert!(
            store
                .record(&[candidate])
                .expect("archived completion should save")
                .is_empty()
        );
        assert!(
            store
                .unread_session_ids()
                .expect("unread state should load")
                .is_empty()
        );
    }

    #[test]
    fn auto_settled_sessions_wake_and_emit_new_attention() {
        let mut store = Store::open_memory().expect("store should open");
        let mut candidate = session("stale");
        candidate.activity = Activity::Completed;
        candidate.last_interaction_unix_seconds = 100;
        store
            .record(&[candidate.clone()])
            .expect("completed snapshot should save");
        assert_eq!(
            store
                .auto_settle_stale_at(1_000_000, 60)
                .expect("stale session should settle"),
            1
        );

        candidate.activity = Activity::WaitingInput;
        candidate.last_interaction_unix_seconds = 101;
        let transitions = store
            .record(&[candidate.clone()])
            .expect("new attention should save");

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].kind, SessionTransitionKind::Input);
        assert_eq!(
            store
                .load(false)
                .expect("new activity should wake the session"),
            [candidate.clone()]
        );
        assert_eq!(
            store
                .unread_session_ids()
                .expect("new attention should be unread"),
            [candidate.id]
        );
    }

    #[test]
    fn history_combines_active_and_settled_sessions_by_native_recency() {
        let mut store = Store::open_memory().expect("store should open");
        let mut older = session("older active");
        older.id = "topo:codex:older".to_owned();
        older.native_session_id = "older".to_owned();
        older.last_interaction_unix_seconds = 100;
        let mut newer = session("newer settled");
        newer.id = "topo:codex:newer".to_owned();
        newer.native_session_id = "newer".to_owned();
        newer.last_interaction_unix_seconds = 200;
        store
            .record(&[older, newer.clone()])
            .expect("snapshots should save");
        store
            .set_archived(&newer.id, true)
            .expect("newer session should settle");

        let history = store.load_history().expect("history should load");

        assert_eq!(
            history
                .iter()
                .map(|entry| entry.session.title.as_str())
                .collect::<Vec<_>>(),
            ["newer settled", "older active"]
        );
        assert!(history[0].settled);
        assert_eq!(history[0].settle_reason.as_deref(), Some("manual"));
        assert!(!history[1].settled);
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
            Activity::Working,
            Activity::WaitingApproval,
            Activity::WaitingInput,
            Activity::Failed,
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

    #[test]
    fn existing_snapshots_without_an_agent_discriminator_still_load() {
        let store = Store::open_memory().expect("store should open");
        let candidate = session("legacy adapter snapshot");
        let mut snapshot =
            serde_json::to_value(&candidate).expect("snapshot should serialize to JSON");
        snapshot
            .as_object_mut()
            .expect("session snapshot should be an object")
            .remove("agent");
        store
            .connection
            .execute(
                "
                INSERT INTO sessions (
                    session_id,
                    snapshot,
                    archived,
                    last_seen,
                    last_interaction
                )
                VALUES (?1, ?2, 0, 100, 100)
                ",
                params![
                    candidate.id,
                    serde_json::to_string(&snapshot).expect("legacy snapshot should serialize")
                ],
            )
            .expect("legacy snapshot should save");

        let loaded = store.load(false).expect("legacy snapshot should load");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].agent, AgentKind::Codex);
    }
}
