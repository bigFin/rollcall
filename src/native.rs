//! Read-only file/SQLite adapters, shared over local/SSH transport.
use std::{
    collections::BTreeMap,
    fmt,
    process::{Command, Stdio},
};

use serde::Deserialize;

use crate::{
    codex,
    domain::{AgentKind, LiveObservation, RuntimeOwner, Session},
    tmux,
};

const PROBE: &str = include_str!("probes/native.py");

#[derive(Debug)]
pub enum NativeError {
    Io(std::io::Error),
    Json(serde_json::Error),
    Command(String),
    Tmux(tmux::TmuxError),
}

impl fmt::Display for NativeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "native session probe could not start: {error}"),
            Self::Json(error) => write!(f, "invalid native session inventory: {error}"),
            Self::Command(message) => f.write_str(message),
            Self::Tmux(error) => write!(f, "{error}"),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeSession {
    session: Session,
    locator: String,
    raw_id: String,
    profile: Option<String>,
    ownership_uncertain: bool,
}

fn probe(
    host: &str,
    agent: AgentKind,
    limit: Option<usize>,
    live_only: bool,
) -> Result<Vec<NativeSession>, NativeError> {
    let limit = limit.map_or_else(|| "all".to_owned(), |n| n.to_string());
    let mode = if live_only { "live" } else { "inventory" };
    let script = shlex::try_join(["python3", "-c", PROBE, agent.as_str(), &limit, mode])
        .map_err(|error| NativeError::Command(error.to_string()))?;
    let local = host == "local" || host == codex::observed_host("local");
    let mut command = if local {
        let mut command = Command::new("bash");
        command.args(["-lc", &script]);
        command
    } else {
        let remote = shlex::try_join(["bash", "-lc", &script])
            .map_err(|error| NativeError::Command(error.to_string()))?;
        let mut command = Command::new("ssh");
        command.args([
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=5",
            "-o",
            "ConnectionAttempts=1",
            "--",
            host,
            &remote,
        ]);
        command
    };
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(NativeError::Io)?;
    if !output.status.success() {
        return Err(NativeError::Command(format!(
            "{} probe on {host}: {}",
            agent.as_str(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    let mut records: Vec<NativeSession> =
        serde_json::from_slice(&output.stdout).map_err(NativeError::Json)?;
    let observed_host = codex::observed_host(host);
    for record in &mut records {
        record.session.host.clone_from(&observed_host);
        record.session.agent = agent;
        record.session.id = record.session.key().stable_id();
    }
    Ok(records)
}

pub fn discover(
    host: &str,
    agent: AgentKind,
    limit: Option<usize>,
) -> Result<Vec<Session>, NativeError> {
    Ok(probe(host, agent, limit, false)?
        .into_iter()
        .map(|row| row.session)
        .collect())
}

pub fn observe_live(
    host: &str,
    agent: AgentKind,
) -> Result<BTreeMap<String, LiveObservation>, NativeError> {
    Ok(probe(host, agent, None, true)?
        .into_iter()
        .filter(|row| row.session.runtime != RuntimeOwner::Resumable)
        .map(|row| {
            (
                row.session.native_session_id,
                LiveObservation {
                    activity: row.session.activity,
                    runtime: row.session.runtime,
                    tmux: row.session.tmux,
                },
            )
        })
        .collect())
}

pub fn attach(
    host: &str,
    agent: AgentKind,
    native_id: &str,
    force_resume: bool,
) -> Result<(), NativeError> {
    let row = probe(host, agent, None, false)?
        .into_iter()
        .find(|row| row.session.native_session_id == native_id)
        .ok_or_else(|| {
            NativeError::Command(format!(
                "{} session {native_id} was not found on {host}",
                agent.as_str()
            ))
        })?;
    if !force_resume {
        if row.ownership_uncertain {
            return Err(NativeError::Command(format!(
                "{} may already be open in another frontend; refusing an implicit second writer. Use its existing terminal, or `rollcall resume` deliberately. See docs/native-adapters.md for live ownership requirements.",
                row.session.id
            )));
        }
        if let Some(binding) = &row.session.tmux {
            return tmux::attach_pane(host, &binding.session, &binding.pane)
                .map_err(NativeError::Tmux);
        }
        if row.session.runtime == RuntimeOwner::ExternalFrontend {
            return Err(NativeError::Command(format!(
                "{} is open outside a verified terminal; use its existing frontend or `rollcall resume` deliberately.",
                row.session.id
            )));
        }
    }
    if row.session.cwd.is_empty() {
        return Err(NativeError::Command(format!(
            "{} has no known workspace path; reopen it in its native harness",
            row.session.id
        )));
    }
    let command = resume_command(&row)?;
    // Hex encoding preserves the full identity, including profile separators and punctuation.
    let identity = row
        .session
        .native_session_id
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let name = format!("rc-{}-{identity}", agent.as_str());
    tmux::resume_command(host, &name, &row.session.cwd, &command).map_err(NativeError::Tmux)
}

fn resume_command(row: &NativeSession) -> Result<String, NativeError> {
    let args = match row.session.agent {
        AgentKind::Pi => vec!["pi", "--session", &row.locator],
        AgentKind::Claude => {
            return shlex::try_join([
                "env",
                &format!("CLAUDE_CONFIG_DIR={}", row.locator),
                "claude",
                "--resume",
                &row.raw_id,
            ])
            .map_err(|error| NativeError::Command(error.to_string()));
        }
        AgentKind::Agy => {
            let mut args = vec!["agy", "--conversation", &row.raw_id];
            if let Some(project) = row.profile.as_deref() {
                args.extend(["--project", project]);
            }
            args
        }
        AgentKind::Hermes => {
            let Some(profile) = row.profile.as_deref() else {
                return shlex::try_join([
                    "env",
                    &format!("HERMES_HOME={}", row.locator),
                    "hermes",
                    "--profile",
                    "default",
                    "--resume",
                    &row.raw_id,
                ])
                .map_err(|error| NativeError::Command(error.to_string()));
            };
            vec!["hermes", "--profile", profile, "--resume", &row.raw_id]
        }
        _ => {
            return Err(NativeError::Command(
                "unsupported native adapter".to_owned(),
            ));
        }
    };
    shlex::try_join(args).map_err(|error| NativeError::Command(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(agent: &str) -> NativeSession {
        serde_json::from_value(json!({
            "session": {"id":"", "host":"local", "agent":agent, "nativeSessionId":"main/raw-id", "title":"Test", "cwd":"/work", "source":agent, "activity":"completed", "lastMessage":"", "lastInteractionUnixSeconds":1, "updatedUnixSeconds":1, "runtime":"resumable"},
            "locator":"/tmp/a directory/session 'quoted'.jsonl", "rawId":"raw-id", "profile":"main", "ownershipUncertain":false
        })).unwrap()
    }

    #[test]
    fn native_resume_uses_exact_file_or_profile_and_id() {
        let pi = row("pi");
        assert_eq!(
            shlex::split(&resume_command(&pi).unwrap()).unwrap(),
            ["pi", "--session", pi.locator.as_str()]
        );
        assert_eq!(
            shlex::split(&resume_command(&row("hermes")).unwrap()).unwrap(),
            ["hermes", "--profile", "main", "--resume", "raw-id"]
        );
    }

    #[test]
    fn claude_and_agy_resume_preserve_exact_identity_and_configuration() {
        let claude = row("claude");
        assert_eq!(
            shlex::split(&resume_command(&claude).unwrap()).unwrap(),
            [
                "env",
                &format!("CLAUDE_CONFIG_DIR={}", claude.locator),
                "claude",
                "--resume",
                "raw-id"
            ]
        );
        let mut agy = row("agy");
        agy.profile = Some("a project 'quoted'".to_owned());
        assert_eq!(
            shlex::split(&resume_command(&agy).unwrap()).unwrap(),
            [
                "agy",
                "--conversation",
                "raw-id",
                "--project",
                "a project 'quoted'"
            ]
        );
        agy.profile = None;
        assert_eq!(
            shlex::split(&resume_command(&agy).unwrap()).unwrap(),
            ["agy", "--conversation", "raw-id"]
        );
    }

    #[test]
    fn pi_extension_regressions() {
        let status = Command::new("node")
            .args(["--test", "tests/pi-extension.test.mjs"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("Node.js 24+ should start (use nix develop)");
        assert!(status.success());
    }

    #[test]
    fn python_probe_regressions() {
        let status = Command::new("python3")
            .args([
                "-B",
                "-m",
                "unittest",
                "discover",
                "-s",
                "tests",
                "-p",
                "test_native.py",
            ])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .status()
            .expect("Python 3 should start (use nix develop)");
        assert!(status.success());
    }
}
