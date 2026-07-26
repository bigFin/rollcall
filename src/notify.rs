use std::{
    env,
    process::{Command, Stdio},
};

#[cfg(not(test))]
use std::io::{self, Write};

use crate::store::{SessionTransition, SessionTransitionKind};

const NOTIFY_COMMAND_ENV: &str = "ROLLCALL_NOTIFY_COMMAND";

#[derive(Clone, Debug, Eq, PartialEq)]
struct Notification {
    kind: &'static str,
    title: String,
    body: String,
    host: String,
    session_id: Option<String>,
}

pub fn session_transition(transition: &SessionTransition) {
    dispatch(notification_for_transition(transition));
}

pub fn host_recovered(host: &str, session_count: usize) {
    dispatch(Notification {
        kind: "hostRecovered",
        title: format!("Rollcall: {host} is back online"),
        body: format!("Reconciled {session_count} sessions."),
        host: host.to_owned(),
        session_id: None,
    });
}

fn notification_for_transition(transition: &SessionTransition) -> Notification {
    let (kind, title, action) = match transition.kind {
        SessionTransitionKind::Completed => ("completed", "Turn complete", "is ready"),
        SessionTransitionKind::Approval => ("approval", "Approval requested", "needs approval"),
        SessionTransitionKind::Input => ("input", "Input requested", "needs input"),
        SessionTransitionKind::Failed => ("failed", "Session failed", "needs attention"),
    };
    Notification {
        kind,
        title: format!("Rollcall: {title}"),
        body: format!("{} on {} {action}.", transition.title, transition.host),
        host: transition.host.clone(),
        session_id: Some(transition.session_id.clone()),
    }
}

fn dispatch(notification: Notification) {
    match env::var_os(NOTIFY_COMMAND_ENV) {
        Some(command) if command.is_empty() => {}
        Some(command) => {
            std::thread::spawn(move || {
                let _ = Command::new("sh")
                    .args(["-lc", &command.to_string_lossy()])
                    .env("ROLLCALL_NOTIFICATION_KIND", notification.kind)
                    .env("ROLLCALL_NOTIFICATION_TITLE", notification.title)
                    .env("ROLLCALL_NOTIFICATION_BODY", notification.body)
                    .env("ROLLCALL_NOTIFICATION_HOST", notification.host)
                    .env(
                        "ROLLCALL_NOTIFICATION_SESSION_ID",
                        notification.session_id.unwrap_or_default(),
                    )
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            });
        }
        None => {
            #[cfg(not(test))]
            {
                let mut stderr = io::stderr().lock();
                let _ = stderr.write_all(b"\x07");
                let _ = stderr.flush();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Notification, notification_for_transition};
    use crate::store::{SessionTransition, SessionTransitionKind};

    fn transition(kind: SessionTransitionKind) -> SessionTransition {
        SessionTransition {
            session_id: "topo:codex:019f".to_owned(),
            title: "Rollcall notifications".to_owned(),
            host: "topo".to_owned(),
            kind,
            observed_at_unix_seconds: 100,
        }
    }

    #[test]
    fn completion_notifications_include_stable_hook_metadata() {
        assert_eq!(
            notification_for_transition(&transition(SessionTransitionKind::Completed)),
            Notification {
                kind: "completed",
                title: "Rollcall: Turn complete".to_owned(),
                body: "Rollcall notifications on topo is ready.".to_owned(),
                host: "topo".to_owned(),
                session_id: Some("topo:codex:019f".to_owned()),
            }
        );
    }

    #[test]
    fn attention_notifications_have_distinct_kinds() {
        assert_eq!(
            notification_for_transition(&transition(SessionTransitionKind::Approval)).kind,
            "approval"
        );
        assert_eq!(
            notification_for_transition(&transition(SessionTransitionKind::Input)).kind,
            "input"
        );
        assert_eq!(
            notification_for_transition(&transition(SessionTransitionKind::Failed)).kind,
            "failed"
        );
    }
}
