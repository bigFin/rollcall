use std::{collections::BTreeMap, fmt};

use crate::{
    codex::{self, CodexError},
    domain::{AgentKind, LiveObservation, Session, SessionKey},
    native::{self, NativeError},
    omp::{self, OmpError},
};

#[derive(Debug)]
pub enum AgentError {
    InvalidSessionId(String),
    Codex(CodexError),
    Omp(OmpError),
    Native(NativeError),
}

impl fmt::Display for AgentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId(session) => write!(
                formatter,
                "invalid coding-agent session {session:?}; expected HOST:AGENT:SESSION_ID"
            ),
            Self::Codex(error) => write!(formatter, "{error}"),
            Self::Omp(error) => write!(formatter, "{error}"),
            Self::Native(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<CodexError> for AgentError {
    fn from(error: CodexError) -> Self {
        Self::Codex(error)
    }
}
impl From<OmpError> for AgentError {
    fn from(error: OmpError) -> Self {
        Self::Omp(error)
    }
}

impl From<NativeError> for AgentError {
    fn from(error: NativeError) -> Self {
        Self::Native(error)
    }
}

pub fn discover(host: &str, limit: Option<usize>) -> Result<Vec<Session>, AgentError> {
    let codex_result = match codex::available(host) {
        Ok(true) => codex::discover(host, limit),
        Ok(false) => Ok(Vec::new()),
        Err(error) => Err(error),
    };
    let results = [
        codex_result.map_err(AgentError::from),
        omp::discover(host, limit).map_err(AgentError::from),
        native::discover(host, AgentKind::Pi, limit).map_err(AgentError::from),
        native::discover(host, AgentKind::Hermes, limit).map_err(AgentError::from),
        native::discover(host, AgentKind::Claude, limit).map_err(AgentError::from),
        native::discover(host, AgentKind::Agy, limit).map_err(AgentError::from),
    ];
    let mut sessions = Vec::new();
    let mut first_error = None;
    for result in results {
        match result {
            Ok(mut found) => sessions.append(&mut found),
            Err(error) => {
                first_error.get_or_insert(error);
            }
        }
    }
    // An absent adapter is an empty success, not proof that a failed host is healthy.
    if sessions.is_empty()
        && let Some(error) = first_error
    {
        return Err(error);
    }
    sort_and_limit_sessions(&mut sessions, limit);
    Ok(sessions)
}

fn sort_and_limit_sessions(sessions: &mut Vec<Session>, limit: Option<usize>) {
    // Apply the host-wide limit only after merging the adapters' newest-first pages.
    sessions.sort_by(|left, right| {
        right
            .last_interaction_unix_seconds
            .cmp(&left.last_interaction_unix_seconds)
            .then_with(|| right.updated_unix_seconds.cmp(&left.updated_unix_seconds))
            .then_with(|| left.id.cmp(&right.id))
    });
    if let Some(limit) = limit {
        sessions.truncate(limit);
    }
}

pub fn discover_detailed(host: &str, limit: Option<usize>) -> Result<Vec<Session>, AgentError> {
    let mut sessions = discover(host, limit)?;
    enrich_details(host, &mut sessions)?;
    Ok(sessions)
}

pub fn enrich_details(host: &str, sessions: &mut [Session]) -> Result<(), AgentError> {
    let mut codex_sessions = sessions
        .iter_mut()
        .filter(|session| session.agent == AgentKind::Codex)
        .collect::<Vec<_>>();
    if !codex_sessions.is_empty() {
        let mut owned = codex_sessions
            .iter()
            .map(|session| (**session).clone())
            .collect::<Vec<_>>();
        codex::enrich_details(host, &mut owned)?;
        for (target, enriched) in codex_sessions.iter_mut().zip(owned) {
            **target = enriched;
        }
    }
    Ok(())
}

pub fn observe_live(host: &str) -> Result<BTreeMap<String, LiveObservation>, AgentError> {
    let observed_host = observed_host(host);
    let codex_result = codex::observe_live(host);
    let omp_result = omp::observe_live(host);
    let mut observations = match codex_result {
        Ok(observations) => {
            qualify_live_observations(&observed_host, AgentKind::Codex, observations)
        }
        Err(_error) if omp_result.is_ok() => BTreeMap::new(),
        Err(error) => return Err(error.into()),
    };
    if let Ok(omp_observations) = omp_result {
        observations.extend(qualify_live_observations(
            &observed_host,
            AgentKind::Omp,
            omp_observations,
        ));
    }
    for agent in [
        AgentKind::Pi,
        AgentKind::Hermes,
        AgentKind::Claude,
        AgentKind::Agy,
    ] {
        observations.extend(qualify_live_observations(
            &observed_host,
            agent,
            native::observe_live(host, agent)?,
        ));
    }
    Ok(observations)
}

fn qualify_live_observations(
    observed_host: &str,
    agent: AgentKind,
    observations: BTreeMap<String, LiveObservation>,
) -> BTreeMap<String, LiveObservation> {
    observations
        .into_iter()
        .map(|(native_session_id, observation)| {
            let id = SessionKey {
                host: observed_host.to_owned(),
                agent,
                native_session_id,
            }
            .stable_id();
            (id, observation)
        })
        .collect()
}

pub fn attach(session_id: &str) -> Result<(), AgentError> {
    let key = SessionKey::parse(session_id)
        .ok_or_else(|| AgentError::InvalidSessionId(session_id.to_owned()))?;
    match key.agent {
        AgentKind::Codex => codex::attach(&key.host, &key.native_session_id).map_err(Into::into),
        AgentKind::Omp => omp::attach(&key.host, &key.native_session_id).map_err(Into::into),
        AgentKind::Pi | AgentKind::Hermes | AgentKind::Claude | AgentKind::Agy => {
            native::attach(&key.host, key.agent, &key.native_session_id, false).map_err(Into::into)
        }
    }
}

pub fn resume(session_id: &str) -> Result<(), AgentError> {
    let key = SessionKey::parse(session_id)
        .ok_or_else(|| AgentError::InvalidSessionId(session_id.to_owned()))?;
    match key.agent {
        AgentKind::Codex => codex::resume(&key.host, &key.native_session_id).map_err(Into::into),
        AgentKind::Omp => omp::resume(&key.host, &key.native_session_id).map_err(Into::into),
        AgentKind::Pi | AgentKind::Hermes | AgentKind::Claude | AgentKind::Agy => {
            native::attach(&key.host, key.agent, &key.native_session_id, true).map_err(Into::into)
        }
    }
}

#[must_use]
pub fn observed_host(host: &str) -> String {
    codex::observed_host(host)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{AgentError, attach, qualify_live_observations, sort_and_limit_sessions};
    use crate::domain::{Activity, AgentKind, LiveObservation, RuntimeOwner, Session};

    fn session(agent: AgentKind, native_id: &str, recency: u64, updated: u64) -> Session {
        Session {
            id: format!("local:{}:{native_id}", agent.as_str()),
            host: "local".to_owned(),
            agent,
            native_session_id: native_id.to_owned(),
            title: native_id.to_owned(),
            cwd: "/work".to_owned(),
            source: agent.as_str().to_owned(),
            activity: Activity::Completed,
            last_message: String::new(),
            last_interaction_unix_seconds: recency,
            updated_unix_seconds: updated,
            runtime: RuntimeOwner::Resumable,
            tmux: None,
        }
    }

    #[test]
    fn newest_sessions_survive_the_limit_regardless_of_agent() {
        // Each adapter supplies its own newest-first page; Codex is appended first.
        let mut sessions = vec![
            session(AgentKind::Codex, "codex-new", 200, 200),
            session(AgentKind::Codex, "codex-old", 100, 100),
            session(AgentKind::Omp, "omp-new", 300, 300),
            session(AgentKind::Omp, "omp-old", 50, 50),
            session(AgentKind::Pi, "pi-new", 500, 500),
            session(AgentKind::Hermes, "main/hermes-new", 400, 400),
        ];
        sort_and_limit_sessions(&mut sessions, Some(4));
        let ids = sessions
            .iter()
            .map(|s| s.native_session_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["pi-new", "main/hermes-new", "omp-new", "codex-new"]);
    }

    #[test]
    fn unlimited_inventory_uses_native_recency_before_metadata_updates() {
        let mut sessions = vec![
            session(AgentKind::Codex, "metadata-only", 100, 900),
            session(AgentKind::Omp, "newest", 300, 300),
            session(AgentKind::Codex, "same-recency", 300, 400),
        ];
        sort_and_limit_sessions(&mut sessions, None);
        let ids = sessions
            .iter()
            .map(|s| s.native_session_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, ["same-recency", "newest", "metadata-only"]);
        sort_and_limit_sessions(&mut sessions, Some(0));
        assert!(sessions.is_empty());
    }

    #[test]
    fn invalid_or_unknown_agent_ids_fail_at_the_dispatch_boundary() {
        assert!(matches!(
            attach("topo:unknown:019f"),
            Err(AgentError::InvalidSessionId(_))
        ));
        assert!(matches!(
            attach("not-a-session-id"),
            Err(AgentError::InvalidSessionId(_))
        ));
    }

    #[test]
    fn live_observations_are_keyed_by_the_stable_control_plane_id() {
        let observations = qualify_live_observations(
            "topo",
            AgentKind::Codex,
            BTreeMap::from([(
                "019f".to_owned(),
                LiveObservation {
                    activity: Activity::Working,
                    runtime: RuntimeOwner::ExternalFrontend,
                    tmux: None,
                },
            )]),
        );

        assert!(observations.contains_key("topo:codex:019f"));
        assert!(!observations.contains_key("019f"));
    }
}
