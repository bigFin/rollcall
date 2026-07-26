use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    pub host: String,
    pub harness: String,
    pub native_session_id: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeState {
    Offline,
    Connecting,
    Idle,
    Working,
    Failed,
    Unknown,
}

impl RuntimeState {
    #[must_use]
    pub const fn should_surface(self) -> bool {
        matches!(self, Self::Connecting | Self::Working | Self::Failed)
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AttentionState {
    #[default]
    None,
    Completed,
    Approval,
    Input,
    PlanReady,
}

impl AttentionState {
    #[must_use]
    pub const fn requires_attention(self) -> bool {
        !matches!(self, Self::None)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub key: SessionKey,
    pub title: String,
    pub cwd: Option<String>,
    pub runtime: RuntimeState,
    pub attention: AttentionState,
    pub last_activity_unix_seconds: Option<u64>,
    pub pinned: bool,
    pub archived: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActiveViewPolicy {
    pub recent_window_seconds: u64,
}

impl Default for ActiveViewPolicy {
    fn default() -> Self {
        Self {
            recent_window_seconds: 7 * 24 * 60 * 60,
        }
    }
}

impl ActiveViewPolicy {
    #[must_use]
    pub fn includes(self, session: &SessionSummary, now_unix_seconds: u64) -> bool {
        if session.archived {
            return false;
        }

        if session.pinned
            || session.runtime.should_surface()
            || session.attention.requires_attention()
        {
            return true;
        }

        session
            .last_activity_unix_seconds
            .is_some_and(|last_activity| {
                now_unix_seconds.saturating_sub(last_activity) <= self.recent_window_seconds
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{ActiveViewPolicy, AttentionState, RuntimeState, SessionKey, SessionSummary};

    const NOW: u64 = 1_000_000;

    fn session() -> SessionSummary {
        SessionSummary {
            key: SessionKey {
                host: "topo".into(),
                harness: "codex".into(),
                native_session_id: "session-1".into(),
            },
            title: "Design session control plane".into(),
            cwd: Some("/fabric".into()),
            runtime: RuntimeState::Idle,
            attention: AttentionState::None,
            last_activity_unix_seconds: Some(NOW),
            pinned: false,
            archived: false,
        }
    }

    #[test]
    fn includes_working_sessions_regardless_of_age() {
        let mut candidate = session();
        candidate.runtime = RuntimeState::Working;
        candidate.last_activity_unix_seconds = Some(0);

        assert!(ActiveViewPolicy::default().includes(&candidate, NOW));
    }

    #[test]
    fn includes_sessions_that_need_attention() {
        let mut candidate = session();
        candidate.attention = AttentionState::Approval;
        candidate.last_activity_unix_seconds = Some(0);

        assert!(ActiveViewPolicy::default().includes(&candidate, NOW));
    }

    #[test]
    fn includes_recent_idle_sessions() {
        let policy = ActiveViewPolicy {
            recent_window_seconds: 60,
        };
        let mut candidate = session();
        candidate.last_activity_unix_seconds = Some(NOW - 60);

        assert!(policy.includes(&candidate, NOW));
    }

    #[test]
    fn excludes_stale_idle_sessions() {
        let policy = ActiveViewPolicy {
            recent_window_seconds: 60,
        };
        let mut candidate = session();
        candidate.last_activity_unix_seconds = Some(NOW - 61);

        assert!(!policy.includes(&candidate, NOW));
    }

    #[test]
    fn includes_pinned_sessions() {
        let mut candidate = session();
        candidate.pinned = true;
        candidate.last_activity_unix_seconds = None;

        assert!(ActiveViewPolicy::default().includes(&candidate, NOW));
    }

    #[test]
    fn explicit_archive_wins_over_runtime_and_attention() {
        let mut candidate = session();
        candidate.runtime = RuntimeState::Working;
        candidate.attention = AttentionState::Input;
        candidate.pinned = true;
        candidate.archived = true;

        assert!(!ActiveViewPolicy::default().includes(&candidate, NOW));
    }
}
