use std::{
    env, fmt, io,
    process::{Command, Stdio},
};

use serde::Serialize;

const SESSION_FORMAT: &str = "#{session_name}\t#{session_created}\t#{session_activity}\t#{session_attached}\t#{session_windows}";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxSession {
    pub id: String,
    pub host: String,
    pub name: String,
    pub created_unix_seconds: u64,
    pub last_activity_unix_seconds: u64,
    pub attached_clients: u32,
    pub windows: u32,
}

#[derive(Debug)]
pub enum TmuxError {
    InvalidSessionId(String),
    Quote(shlex::QuoteError),
    Start {
        program: &'static str,
        source: io::Error,
    },
    CommandFailed {
        context: String,
        message: String,
    },
    MalformedOutput(String),
}

impl fmt::Display for TmuxError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId(session) => write!(
                formatter,
                "invalid tmux session {session:?}; expected a name or HOST:tmux:NAME"
            ),
            Self::Quote(error) => write!(formatter, "could not quote remote tmux command: {error}"),
            Self::Start { program, source } => {
                write!(formatter, "could not start {program}: {source}")
            }
            Self::CommandFailed { context, message } => {
                write!(formatter, "{context} failed: {message}")
            }
            Self::MalformedOutput(line) => {
                write!(
                    formatter,
                    "tmux returned an unrecognized session row: {line:?}"
                )
            }
        }
    }
}

impl From<shlex::QuoteError> for TmuxError {
    fn from(error: shlex::QuoteError) -> Self {
        Self::Quote(error)
    }
}

pub fn discover(host: &str) -> Result<Vec<TmuxSession>, TmuxError> {
    let local_hostname = local_hostname();
    let is_local = host == "local" || host == local_hostname;
    let observed_host = if is_local { &local_hostname } else { host };
    let output = if is_local {
        Command::new("tmux")
            .args(["list-sessions", "-F", SESSION_FORMAT])
            .stdin(Stdio::null())
            .output()
            .map_err(|source| TmuxError::Start {
                program: "tmux",
                source,
            })?
    } else {
        let remote_command = shlex::try_join(
            ["tmux", "list-sessions", "-F", SESSION_FORMAT]
                .iter()
                .copied(),
        )?;
        Command::new("ssh")
            .args([
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ConnectionAttempts=1",
                "--",
                host,
                &remote_command,
            ])
            .stdin(Stdio::null())
            .output()
            .map_err(|source| TmuxError::Start {
                program: "ssh",
                source,
            })?
    };

    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if is_missing_tmux_server(&message) {
            return Ok(Vec::new());
        }

        return Err(TmuxError::CommandFailed {
            context: if is_local {
                "local tmux inventory".to_owned()
            } else {
                format!("tmux inventory on {host}")
            },
            message: if message.is_empty() {
                output.status.to_string()
            } else {
                message
            },
        });
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut sessions = stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| parse_session(observed_host, line))
        .collect::<Result<Vec<_>, _>>()?;
    sessions.sort_by_key(|session| std::cmp::Reverse(session.last_activity_unix_seconds));
    Ok(sessions)
}

pub fn attach(session: &str) -> Result<(), TmuxError> {
    let target = parse_target(session)?;
    let (program, arguments) =
        attach_command(&target, env::var_os("TMUX").is_some(), &local_hostname())?;
    let status = Command::new(program)
        .args(&arguments)
        .status()
        .map_err(|source| TmuxError::Start { program, source })?;

    if status.success() {
        Ok(())
    } else {
        Err(TmuxError::CommandFailed {
            context: format!("attachment to {}", target.id()),
            message: status.to_string(),
        })
    }
}

pub fn attach_pane(host: &str, session_name: &str, pane: &str) -> Result<(), TmuxError> {
    let target = TmuxTarget {
        host: host.to_owned(),
        name: session_name.to_owned(),
    };
    let (program, arguments) = attach_pane_command(
        &target,
        pane,
        env::var_os("TMUX").is_some(),
        &local_hostname(),
    )?;
    let status = Command::new(program)
        .args(&arguments)
        .status()
        .map_err(|source| TmuxError::Start { program, source })?;

    if status.success() {
        Ok(())
    } else {
        Err(TmuxError::CommandFailed {
            context: format!("attachment to {} pane {pane}", target.id()),
            message: status.to_string(),
        })
    }
}

pub fn rename_session(host: &str, current: &str, desired: &str) -> Result<(), TmuxError> {
    if current == desired {
        return Ok(());
    }
    let local_hostname = local_hostname();
    let target = TmuxTarget {
        host: host.to_owned(),
        name: current.to_owned(),
    };
    let (program, arguments) = rename_session_command(&target, desired, &local_hostname)?;
    let output = Command::new(program)
        .args(&arguments)
        .stdin(Stdio::null())
        .output()
        .map_err(|source| TmuxError::Start { program, source })?;

    if output.status.success() {
        Ok(())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(TmuxError::CommandFailed {
            context: format!("renaming {} to {desired}", target.id()),
            message: if message.is_empty() {
                output.status.to_string()
            } else {
                message
            },
        })
    }
}

pub fn resume_codex(
    host: &str,
    session_name: &str,
    cwd: &str,
    native_session_id: &str,
    app_server_socket: &str,
) -> Result<(), TmuxError> {
    let local_hostname = local_hostname();
    let target = TmuxTarget {
        host: host.to_owned(),
        name: session_name.to_owned(),
    };
    let inside_tmux = env::var_os("TMUX").is_some();
    let (program, arguments) = resume_codex_command(
        &target,
        cwd,
        native_session_id,
        app_server_socket,
        inside_tmux,
        &local_hostname,
    )?;
    let status = Command::new(program)
        .args(&arguments)
        .status()
        .map_err(|source| TmuxError::Start { program, source })?;

    if status.success() {
        Ok(())
    } else {
        Err(TmuxError::CommandFailed {
            context: format!(
                "resuming Codex session {native_session_id} in {}",
                target.id()
            ),
            message: status.to_string(),
        })
    }
}

fn is_missing_tmux_server(message: &str) -> bool {
    message.contains("no server running")
        || message.contains("failed to connect to server")
        || message.contains("error connecting to")
        || message.contains("no sessions")
}

fn parse_session(host: &str, line: &str) -> Result<TmuxSession, TmuxError> {
    let fields = line.split('\t').collect::<Vec<_>>();
    let [name, created, activity, attached, windows] = fields.as_slice() else {
        return Err(TmuxError::MalformedOutput(line.to_owned()));
    };

    Ok(TmuxSession {
        id: format!("{host}:tmux:{name}"),
        host: host.to_owned(),
        name: (*name).to_owned(),
        created_unix_seconds: parse_number(created, line)?,
        last_activity_unix_seconds: parse_number(activity, line)?,
        attached_clients: parse_number(attached, line)?,
        windows: parse_number(windows, line)?,
    })
}

fn parse_number<T>(value: &str, line: &str) -> Result<T, TmuxError>
where
    T: std::str::FromStr,
{
    value
        .parse()
        .map_err(|_| TmuxError::MalformedOutput(line.to_owned()))
}

fn local_hostname() -> String {
    env::var("HOSTNAME")
        .ok()
        .filter(|hostname| !hostname.is_empty())
        .or_else(|| {
            Command::new("hostname")
                .stdin(Stdio::null())
                .output()
                .ok()
                .filter(|output| output.status.success())
                .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
                .filter(|hostname| !hostname.is_empty())
        })
        .unwrap_or_else(|| "local".to_owned())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct TmuxTarget {
    host: String,
    name: String,
}

impl TmuxTarget {
    fn id(&self) -> String {
        format!("{}:tmux:{}", self.host, self.name)
    }
}

fn parse_target(session: &str) -> Result<TmuxTarget, TmuxError> {
    let fields = session.splitn(3, ':').collect::<Vec<_>>();
    match fields.as_slice() {
        [name] if !name.is_empty() => Ok(TmuxTarget {
            host: "local".to_owned(),
            name: (*name).to_owned(),
        }),
        [host, "tmux", name] if !host.is_empty() && !name.is_empty() => Ok(TmuxTarget {
            host: (*host).to_owned(),
            name: (*name).to_owned(),
        }),
        _ => Err(TmuxError::InvalidSessionId(session.to_owned())),
    }
}

fn attach_command(
    target: &TmuxTarget,
    inside_tmux: bool,
    local_hostname: &str,
) -> Result<(&'static str, Vec<String>), TmuxError> {
    if target.host == "local" || target.host == local_hostname {
        let action = if inside_tmux {
            "switch-client"
        } else {
            "attach-session"
        };
        return Ok((
            "tmux",
            vec![action.to_owned(), "-t".to_owned(), target.name.to_owned()],
        ));
    }

    let remote_command = shlex::try_join(
        ["tmux", "attach-session", "-t", &target.name]
            .iter()
            .copied(),
    )?;
    Ok((
        "ssh",
        vec![
            "-t".to_owned(),
            "--".to_owned(),
            target.host.to_owned(),
            remote_command,
        ],
    ))
}

fn attach_pane_command(
    target: &TmuxTarget,
    pane: &str,
    inside_tmux: bool,
    local_hostname: &str,
) -> Result<(&'static str, Vec<String>), TmuxError> {
    let action = if inside_tmux {
        "switch-client"
    } else {
        "attach-session"
    };
    let tmux_arguments = [
        "tmux",
        "select-window",
        "-t",
        pane,
        ";",
        "select-pane",
        "-t",
        pane,
        ";",
        action,
        "-t",
        &target.name,
    ];

    if target.host == "local" || target.host == local_hostname {
        return Ok((
            "tmux",
            tmux_arguments[1..]
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
        ));
    }

    let remote_command = shlex::try_join(tmux_arguments)?;
    Ok((
        "ssh",
        vec![
            "-t".to_owned(),
            "--".to_owned(),
            target.host.to_owned(),
            remote_command,
        ],
    ))
}

fn rename_session_command(
    target: &TmuxTarget,
    desired: &str,
    local_hostname: &str,
) -> Result<(&'static str, Vec<String>), TmuxError> {
    let tmux_arguments = ["tmux", "rename-session", "-t", &target.name, desired];
    if target.host == "local" || target.host == local_hostname {
        return Ok((
            "tmux",
            tmux_arguments[1..]
                .iter()
                .map(|argument| (*argument).to_owned())
                .collect(),
        ));
    }

    let remote_command = shlex::try_join(tmux_arguments)?;
    Ok((
        "ssh",
        vec![
            "-T".to_owned(),
            "--".to_owned(),
            target.host.to_owned(),
            remote_command,
        ],
    ))
}

fn resume_codex_command(
    target: &TmuxTarget,
    cwd: &str,
    native_session_id: &str,
    app_server_socket: &str,
    inside_tmux: bool,
    local_hostname: &str,
) -> Result<(&'static str, Vec<String>), TmuxError> {
    let remote_endpoint = format!("unix://{app_server_socket}");
    let resume_command = shlex::try_join([
        "exec",
        "codex",
        "--remote",
        &remote_endpoint,
        "resume",
        native_session_id,
    ])?;
    let login_command = shlex::try_join(["exec", "bash", "-lc", &resume_command])?;

    if target.host == "local" || target.host == local_hostname {
        let detached = if inside_tmux { "-Ad" } else { "-A" };
        let mut arguments = vec![
            "new-session".to_owned(),
            detached.to_owned(),
            "-s".to_owned(),
            target.name.to_owned(),
            "-c".to_owned(),
            cwd.to_owned(),
            login_command,
        ];
        if inside_tmux {
            arguments.extend([
                ";".to_owned(),
                "switch-client".to_owned(),
                "-t".to_owned(),
                target.name.to_owned(),
            ]);
        }
        return Ok(("tmux", arguments));
    }

    let remote_command = shlex::try_join([
        "tmux",
        "new-session",
        "-A",
        "-s",
        &target.name,
        "-c",
        cwd,
        &login_command,
    ])?;
    Ok((
        "ssh",
        vec![
            "-t".to_owned(),
            "--".to_owned(),
            target.host.to_owned(),
            remote_command,
        ],
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        TmuxTarget, attach_command, attach_pane_command, is_missing_tmux_server, parse_session,
        parse_target, rename_session_command, resume_codex_command,
    };

    #[test]
    fn parses_tmux_inventory_rows() {
        let session =
            parse_session("local", "fabric\t100\t200\t1\t3").expect("inventory row should parse");

        assert_eq!(session.id, "local:tmux:fabric");
        assert_eq!(session.name, "fabric");
        assert_eq!(session.created_unix_seconds, 100);
        assert_eq!(session.last_activity_unix_seconds, 200);
        assert_eq!(session.attached_clients, 1);
        assert_eq!(session.windows, 3);
    }

    #[test]
    fn bare_names_refer_to_local_sessions() {
        assert_eq!(
            parse_target("fabric").expect("target should parse"),
            TmuxTarget {
                host: "local".to_owned(),
                name: "fabric".to_owned(),
            }
        );
    }

    #[test]
    fn stable_ids_preserve_colons_in_session_names() {
        assert_eq!(
            parse_target("topo:tmux:agent:review").expect("target should parse"),
            TmuxTarget {
                host: "topo".to_owned(),
                name: "agent:review".to_owned(),
            }
        );
    }

    #[test]
    fn local_attachment_switches_clients_when_already_inside_tmux() {
        let target = TmuxTarget {
            host: "local".to_owned(),
            name: "fabric".to_owned(),
        };

        assert_eq!(
            attach_command(&target, true, "topo").expect("command should be built"),
            (
                "tmux",
                vec![
                    "switch-client".to_owned(),
                    "-t".to_owned(),
                    "fabric".to_owned()
                ]
            )
        );
    }

    #[test]
    fn remote_attachment_shell_quotes_the_session_name() {
        let target = TmuxTarget {
            host: "topo".to_owned(),
            name: "agent review; false".to_owned(),
        };
        let (program, arguments) =
            attach_command(&target, false, "laptop").expect("command should be built");

        assert_eq!(program, "ssh");
        assert_eq!(&arguments[..3], ["-t", "--", "topo"]);
        assert_eq!(
            shlex::split(&arguments[3]).expect("remote command should parse"),
            ["tmux", "attach-session", "-t", "agent review; false"]
        );
    }

    #[test]
    fn stable_id_for_the_current_host_attaches_locally() {
        let target = TmuxTarget {
            host: "topo".to_owned(),
            name: "fabric".to_owned(),
        };

        let (program, arguments) =
            attach_command(&target, false, "topo").expect("command should be built");

        assert_eq!(program, "tmux");
        assert_eq!(arguments, ["attach-session", "-t", "fabric"]);
    }

    #[test]
    fn missing_socket_is_treated_as_an_empty_inventory() {
        assert!(is_missing_tmux_server(
            "error connecting to /tmp/tmux-1000/default (No such file or directory)"
        ));
    }

    #[test]
    fn pane_attachment_selects_the_exact_local_codex_pane() {
        let target = TmuxTarget {
            host: "topo".to_owned(),
            name: "fabric".to_owned(),
        };
        let (program, arguments) =
            attach_pane_command(&target, "%19", true, "topo").expect("command should be built");

        assert_eq!(program, "tmux");
        assert_eq!(
            arguments,
            [
                "select-window",
                "-t",
                "%19",
                ";",
                "select-pane",
                "-t",
                "%19",
                ";",
                "switch-client",
                "-t",
                "fabric"
            ]
        );
    }

    #[test]
    fn remote_pane_attachment_preserves_tmux_command_separators() {
        let target = TmuxTarget {
            host: "coda".to_owned(),
            name: "fabric".to_owned(),
        };
        let (program, arguments) =
            attach_pane_command(&target, "%19", false, "topo").expect("command should be built");

        assert_eq!(program, "ssh");
        assert_eq!(&arguments[..3], ["-t", "--", "coda"]);
        assert_eq!(
            shlex::split(&arguments[3]).expect("remote tmux command should parse"),
            [
                "tmux",
                "select-window",
                "-t",
                "%19",
                ";",
                "select-pane",
                "-t",
                "%19",
                ";",
                "attach-session",
                "-t",
                "fabric"
            ]
        );
    }

    #[test]
    fn managed_session_renames_are_local_when_the_host_matches() {
        let target = TmuxTarget {
            host: "topo".to_owned(),
            name: "rc-pending-fabric-123".to_owned(),
        };
        let (program, arguments) = rename_session_command(&target, "rc-fabric-deadbeef", "topo")
            .expect("rename command should be built");

        assert_eq!(program, "tmux");
        assert_eq!(
            arguments,
            [
                "rename-session",
                "-t",
                "rc-pending-fabric-123",
                "rc-fabric-deadbeef"
            ]
        );
    }

    #[test]
    fn remote_managed_session_renames_are_shell_quoted() {
        let target = TmuxTarget {
            host: "coda".to_owned(),
            name: "rc-pending-fabric-123".to_owned(),
        };
        let (program, arguments) = rename_session_command(&target, "rc-fabric-deadbeef", "topo")
            .expect("rename command should be built");

        assert_eq!(program, "ssh");
        assert_eq!(&arguments[..3], ["-T", "--", "coda"]);
        assert_eq!(
            shlex::split(&arguments[3]).expect("remote rename should parse"),
            [
                "tmux",
                "rename-session",
                "-t",
                "rc-pending-fabric-123",
                "rc-fabric-deadbeef"
            ]
        );
    }

    #[test]
    fn local_codex_resume_creates_or_attaches_atomically() {
        let target = TmuxTarget {
            host: "local".to_owned(),
            name: "codex-deadbeef".to_owned(),
        };
        let (program, arguments) = resume_codex_command(
            &target,
            "/tmp/project with spaces",
            "019f-dead-beef",
            "/run/user/1000/rollcall/codex-123.sock",
            false,
            "topo",
        )
        .expect("command should be built");

        assert_eq!(program, "tmux");
        assert_eq!(
            &arguments[..6],
            [
                "new-session",
                "-A",
                "-s",
                "codex-deadbeef",
                "-c",
                "/tmp/project with spaces"
            ]
        );
        assert_eq!(
            shlex::split(&arguments[6]).expect("login command should parse"),
            [
                "exec",
                "bash",
                "-lc",
                "exec codex --remote unix:///run/user/1000/rollcall/codex-123.sock resume 019f-dead-beef"
            ]
        );
    }

    #[test]
    fn codex_resume_switches_the_current_tmux_client() {
        let target = TmuxTarget {
            host: "topo".to_owned(),
            name: "codex-deadbeef".to_owned(),
        };
        let (program, arguments) = resume_codex_command(
            &target,
            "/fabric",
            "019f",
            "/run/user/1000/rollcall/codex.sock",
            true,
            "topo",
        )
        .expect("command should be built");

        assert_eq!(program, "tmux");
        assert_eq!(&arguments[..2], ["new-session", "-Ad"]);
        assert_eq!(
            &arguments[7..],
            [";", "switch-client", "-t", "codex-deadbeef"]
        );
    }

    #[test]
    fn remote_codex_resume_quotes_all_native_values() {
        let target = TmuxTarget {
            host: "coda".to_owned(),
            name: "codex weird; false".to_owned(),
        };
        let (program, arguments) = resume_codex_command(
            &target,
            "/tmp/project with spaces",
            "019f; false",
            "/tmp/runtime with spaces/codex.sock",
            false,
            "topo",
        )
        .expect("command should be built");

        assert_eq!(program, "ssh");
        assert_eq!(&arguments[..3], ["-t", "--", "coda"]);
        let remote =
            shlex::split(&arguments[3]).expect("remote command should remain one safe command");
        assert_eq!(
            &remote[..7],
            [
                "tmux",
                "new-session",
                "-A",
                "-s",
                "codex weird; false",
                "-c",
                "/tmp/project with spaces"
            ]
        );
        assert_eq!(
            shlex::split(&remote[7]).expect("nested login command should parse"),
            [
                "exec",
                "bash",
                "-lc",
                "exec codex --remote 'unix:///tmp/runtime with spaces/codex.sock' resume '019f; false'"
            ]
        );
    }
}
