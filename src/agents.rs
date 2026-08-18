use std::{collections::BTreeMap, fmt};

use crate::{
    codex::{self, CodexError},
    domain::{AgentKind, LiveObservation, Session, SessionKey},
    omp::{self, OmpError},
};

#[derive(Debug)]
pub enum AgentError {
    InvalidSessionId(String),
    Codex(CodexError),
    Omp(OmpError),
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

pub fn discover(host: &str, limit: Option<usize>) -> Result<Vec<Session>, AgentError> {
    let codex_result = codex::discover(host, None);
    let omp_result = omp::discover(host, None);
    let mut sessions = match codex_result {
        Ok(sessions) => sessions,
        Err(_error) if omp_result.is_ok() => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    if let Ok(mut omp_sessions) = omp_result {
        sessions.append(&mut omp_sessions);
    }
    sessions.sort_by_key(|session| {
        std::cmp::Reverse((session.last_interaction_unix_seconds, session.updated_unix_seconds))
    });
    if let Some(limit) = limit {
        sessions.truncate(limit);
    }
    Ok(sessions)
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
        Ok(observations) => qualify_live_observations(&observed_host, AgentKind::Codex, observations),
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
    }
}

#[must_use]
pub fn observed_host(host: &str) -> String {
    codex::observed_host(host)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{AgentError, attach, qualify_live_observations};
    use crate::domain::{Activity, AgentKind, LiveObservation, RuntimeOwner};

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
