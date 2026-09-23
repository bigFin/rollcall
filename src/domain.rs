use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Codex,
    Omp,
    Pi,
    Hermes,
}

impl AgentKind {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Omp => "omp",
            Self::Pi => "pi",
            Self::Hermes => "hermes",
        }
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "codex" => Some(Self::Codex),
            "omp" => Some(Self::Omp),
            "pi" => Some(Self::Pi),
            "hermes" => Some(Self::Hermes),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionKey {
    pub host: String,
    pub agent: AgentKind,
    pub native_session_id: String,
}

impl SessionKey {
    #[must_use]
    pub fn stable_id(&self) -> String {
        format!(
            "{}:{}:{}",
            self.host,
            self.agent.as_str(),
            self.native_session_id
        )
    }

    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        let fields = value.splitn(3, ':').collect::<Vec<_>>();
        let [host, agent, native_session_id] = fields.as_slice() else {
            return None;
        };
        if host.is_empty() || native_session_id.is_empty() {
            return None;
        }
        Some(Self {
            host: (*host).to_owned(),
            agent: AgentKind::parse(agent)?,
            native_session_id: (*native_session_id).to_owned(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub host: String,
    #[serde(default)]
    pub agent: AgentKind,
    pub native_session_id: String,
    pub title: String,
    pub cwd: String,
    pub source: String,
    pub activity: Activity,
    pub last_message: String,
    pub last_interaction_unix_seconds: u64,
    pub updated_unix_seconds: u64,
    pub runtime: RuntimeOwner,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux: Option<TmuxBinding>,
}

impl Session {
    #[must_use]
    pub fn key(&self) -> SessionKey {
        SessionKey {
            host: self.host.clone(),
            agent: self.agent,
            native_session_id: self.native_session_id.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RuntimeOwner {
    Resumable,
    SharedBackend,
    #[serde(alias = "loadedOutsideTmux")]
    ExternalFrontend,
    #[serde(alias = "loadedInTmux")]
    TmuxFrontend,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Activity {
    Working,
    WaitingApproval,
    WaitingInput,
    Completed,
    Failed,
    Unknown,
}

impl Activity {
    #[must_use]
    pub const fn can_auto_settle(self) -> bool {
        matches!(self, Self::Completed | Self::Unknown)
    }
}

#[must_use]
pub const fn merge_observed_activity(current: Activity, observed: Activity) -> Activity {
    match observed {
        Activity::Completed => Activity::Completed,
        Activity::Working
            if !matches!(current, Activity::WaitingApproval | Activity::WaitingInput) =>
        {
            Activity::Working
        }
        _ => current,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxBinding {
    pub session: String,
    pub pane: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LiveObservation {
    pub activity: Activity,
    pub runtime: RuntimeOwner,
    pub tmux: Option<TmuxBinding>,
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{Activity, AgentKind, RuntimeOwner, Session, SessionKey};

    #[test]
    fn stable_session_ids_include_the_agent_boundary() {
        let key = SessionKey {
            host: "topo".to_owned(),
            agent: AgentKind::Codex,
            native_session_id: "019f".to_owned(),
        };

        assert_eq!(key.stable_id(), "topo:codex:019f");
        assert_eq!(
            SessionKey::parse("topo:codex:019f"),
            Some(key),
            "stable IDs should round-trip"
        );
        assert!(SessionKey::parse("topo:unknown:019f").is_none());
    }

    #[test]
    fn pi_omp_and_hermes_have_distinct_round_tripping_keys() {
        for agent in [AgentKind::Pi, AgentKind::Omp, AgentKind::Hermes] {
            let key = SessionKey {
                host: "topo".to_owned(),
                agent,
                native_session_id: "main/session-1".to_owned(),
            };
            assert_eq!(SessionKey::parse(&key.stable_id()), Some(key));
        }
        assert_ne!(AgentKind::parse("pi"), AgentKind::parse("omp"));
    }

    #[test]
    fn old_codex_snapshots_default_to_the_codex_agent() {
        let session = serde_json::from_value::<Session>(json!({
            "id": "topo:codex:019f",
            "host": "topo",
            "nativeSessionId": "019f",
            "title": "Legacy snapshot",
            "cwd": "/fabric",
            "source": "cli",
            "activity": "completed",
            "lastMessage": "",
            "lastInteractionUnixSeconds": 100,
            "updatedUnixSeconds": 100,
            "runtime": "resumable"
        }))
        .expect("legacy snapshot should deserialize");

        assert_eq!(session.agent, AgentKind::Codex);
        assert_eq!(session.activity, Activity::Completed);
    }

    #[test]
    fn new_snapshots_serialize_the_agent_discriminator() {
        let session = Session {
            id: "topo:codex:019f".to_owned(),
            host: "topo".to_owned(),
            agent: AgentKind::Codex,
            native_session_id: "019f".to_owned(),
            title: "Current snapshot".to_owned(),
            cwd: "/fabric".to_owned(),
            source: "cli".to_owned(),
            activity: Activity::Completed,
            last_message: String::new(),
            last_interaction_unix_seconds: 100,
            updated_unix_seconds: 100,
            runtime: RuntimeOwner::Resumable,
            tmux: None,
        };

        let value = serde_json::to_value(session).expect("snapshot should serialize");

        assert_eq!(value["agent"], json!("codex"));
    }

    #[test]
    fn legacy_runtime_names_remain_compatible() {
        assert_eq!(
            serde_json::from_value::<RuntimeOwner>(json!("loadedOutsideTmux"))
                .expect("legacy external runtime should deserialize"),
            RuntimeOwner::ExternalFrontend
        );
        assert_eq!(
            serde_json::from_value::<RuntimeOwner>(json!("loadedInTmux"))
                .expect("legacy tmux runtime should deserialize"),
            RuntimeOwner::TmuxFrontend
        );
        assert_eq!(
            serde_json::to_value(RuntimeOwner::ExternalFrontend)
                .expect("new runtime should serialize"),
            json!("externalFrontend")
        );
    }
}
