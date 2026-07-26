use std::{
    env, fmt, fs, io,
    os::unix::net::UnixStream,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tungstenite::{Message, WebSocket, client};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeStatus {
    pub socket_path: String,
    pub running: bool,
}

#[derive(Debug)]
pub enum RuntimeError {
    MissingHome,
    CommandFailed {
        context: &'static str,
        message: String,
    },
    Start {
        program: &'static str,
        source: io::Error,
    },
    Io(io::Error),
    Handshake(String),
    WebSocket(tungstenite::Error),
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => write!(
                formatter,
                "neither XDG_RUNTIME_DIR nor HOME is available for the Codex runtime socket"
            ),
            Self::CommandFailed { context, message } => {
                write!(formatter, "{context} failed: {message}")
            }
            Self::Start { program, source } => {
                write!(formatter, "could not start {program}: {source}")
            }
            Self::Io(error) => write!(formatter, "Codex shared-runtime I/O failed: {error}"),
            Self::Handshake(error) => {
                write!(
                    formatter,
                    "Codex shared-runtime WebSocket handshake failed: {error}"
                )
            }
            Self::WebSocket(error) => {
                write!(formatter, "Codex shared-runtime WebSocket failed: {error}")
            }
        }
    }
}

impl From<io::Error> for RuntimeError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<tungstenite::Error> for RuntimeError {
    fn from(error: tungstenite::Error) -> Self {
        Self::WebSocket(error)
    }
}

pub struct RuntimeConnection {
    websocket: WebSocket<UnixStream>,
    tunnel: Option<Child>,
    forwarded_socket: Option<PathBuf>,
}

impl RuntimeConnection {
    pub fn send_text(&mut self, text: &str) -> Result<(), RuntimeError> {
        self.websocket.send(Message::Text(text.to_owned().into()))?;
        Ok(())
    }

    pub fn read_text(&mut self) -> Result<Option<String>, RuntimeError> {
        loop {
            match self.websocket.read()? {
                Message::Text(text) => return Ok(Some(text.to_string())),
                Message::Close(_) => return Ok(None),
                Message::Ping(_) | Message::Pong(_) | Message::Binary(_) | Message::Frame(_) => {
                    self.websocket.flush()?;
                }
            }
        }
    }

    pub fn close(&mut self) {
        let _ = self.websocket.close(None);
    }
}

impl Drop for RuntimeConnection {
    fn drop(&mut self) {
        self.close();
        if let Some(tunnel) = self.tunnel.as_mut() {
            let _ = tunnel.kill();
            let _ = tunnel.wait();
        }
        if let Some(socket) = self.forwarded_socket.as_ref() {
            let _ = fs::remove_file(socket);
        }
    }
}

pub fn status(host: &str) -> Result<RuntimeStatus, RuntimeError> {
    let output = run_host_script(host, runtime_status_script())?;
    parse_status(&output)
}

pub fn ensure(host: &str) -> Result<String, RuntimeError> {
    let output = run_host_script(host, runtime_start_script())?;
    let status = parse_status(&output)?;
    if status.running {
        Ok(status.socket_path)
    } else {
        Err(RuntimeError::CommandFailed {
            context: "starting the Codex shared runtime",
            message: output.trim().to_owned(),
        })
    }
}

pub fn stop(host: &str) -> Result<RuntimeStatus, RuntimeError> {
    let output = run_host_script(host, runtime_stop_script())?;
    parse_status(&output)
}

pub fn connect_existing(host: &str) -> Result<Option<RuntimeConnection>, RuntimeError> {
    let status = status(host)?;
    if !status.running {
        return Ok(None);
    }
    connect(host, &status.socket_path).map(Some)
}

pub fn connect_or_start(host: &str) -> Result<RuntimeConnection, RuntimeError> {
    if let Some(connection) = connect_existing(host)? {
        return Ok(connection);
    }
    let socket_path = ensure(host)?;
    connect(host, &socket_path)
}

fn connect(host: &str, remote_socket: &str) -> Result<RuntimeConnection, RuntimeError> {
    let (socket_path, tunnel, forwarded_socket) = if is_local(host) {
        (PathBuf::from(remote_socket), None, None)
    } else {
        let local_socket = unique_forward_socket();
        if local_socket.exists() {
            fs::remove_file(&local_socket)?;
        }
        let forward = format!("{}:{remote_socket}", local_socket.display());
        let mut tunnel = Command::new("ssh")
            .args([
                "-N",
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=5",
                "-o",
                "ConnectionAttempts=1",
                "-o",
                "ExitOnForwardFailure=yes",
                "-o",
                "ServerAliveInterval=15",
                "-o",
                "ServerAliveCountMax=3",
                "-o",
                "StreamLocalBindUnlink=yes",
                "-L",
                &forward,
                "--",
                host,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| RuntimeError::Start {
                program: "ssh",
                source,
            })?;
        wait_for_socket(&local_socket, Some(&mut tunnel))?;
        (local_socket.clone(), Some(tunnel), Some(local_socket))
    };

    let stream = UnixStream::connect(&socket_path)?;
    let (websocket, _) = client("ws://localhost/rpc", stream)
        .map_err(|error| RuntimeError::Handshake(error.to_string()))?;
    Ok(RuntimeConnection {
        websocket,
        tunnel,
        forwarded_socket,
    })
}

fn wait_for_socket(path: &Path, mut child: Option<&mut Child>) -> Result<(), RuntimeError> {
    let deadline = Instant::now() + CONNECT_TIMEOUT;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        if let Some(process) = child.as_mut()
            && let Some(status) = process.try_wait()?
        {
            return Err(RuntimeError::CommandFailed {
                context: "opening the SSH Unix-socket forward",
                message: status.to_string(),
            });
        }
        thread::sleep(Duration::from_millis(50));
    }
    Err(RuntimeError::CommandFailed {
        context: "opening the SSH Unix-socket forward",
        message: format!("timed out waiting for {}", path.display()),
    })
}

fn run_host_script(host: &str, script: &str) -> Result<String, RuntimeError> {
    let output = if is_local(host) {
        Command::new("bash")
            .args(["-c", script])
            .stdin(Stdio::null())
            .output()
            .map_err(|source| RuntimeError::Start {
                program: "bash",
                source,
            })?
    } else {
        let remote_command = shlex::try_join(["bash", "-c", script]).map_err(|error| {
            RuntimeError::CommandFailed {
                context: "quoting the remote Codex runtime command",
                message: error.to_string(),
            }
        })?;
        Command::new("ssh")
            .args([
                "-T",
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
            .map_err(|source| RuntimeError::Start {
                program: "ssh",
                source,
            })?
    };

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Err(RuntimeError::CommandFailed {
            context: "probing the Codex shared runtime",
            message: if message.is_empty() {
                output.status.to_string()
            } else {
                message
            },
        })
    }
}

fn parse_status(output: &str) -> Result<RuntimeStatus, RuntimeError> {
    let line = output
        .lines()
        .rev()
        .find(|line| line.starts_with("running\t") || line.starts_with("stopped\t"))
        .ok_or_else(|| RuntimeError::CommandFailed {
            context: "reading Codex shared-runtime status",
            message: output.trim().to_owned(),
        })?;
    let (state, socket_path) =
        line.split_once('\t')
            .ok_or_else(|| RuntimeError::CommandFailed {
                context: "reading Codex shared-runtime status",
                message: line.to_owned(),
            })?;
    Ok(RuntimeStatus {
        socket_path: socket_path.to_owned(),
        running: state == "running",
    })
}

fn runtime_status_script() -> &'static str {
    r#"
set -eu
runtime_root="${XDG_RUNTIME_DIR:-${HOME:?HOME is required}/.cache/rollcall/runtime}"
codex_home="${CODEX_HOME:-${HOME:?HOME is required}/.codex}"
profile="$(printf '%s' "$codex_home" | cksum)"
profile="${profile%% *}"
socket="$runtime_root/rollcall/codex-$profile.sock"
session="rc-codex-$profile"
if command -v tmux >/dev/null 2>&1 &&
   tmux has-session -t "$session" 2>/dev/null &&
   [ -S "$socket" ]; then
  printf 'running\t%s\n' "$socket"
else
  printf 'stopped\t%s\n' "$socket"
fi
"#
}

fn runtime_start_script() -> &'static str {
    r#"
set -eu
runtime_root="${XDG_RUNTIME_DIR:-${HOME:?HOME is required}/.cache/rollcall/runtime}"
runtime_dir="$runtime_root/rollcall"
codex_home="${CODEX_HOME:-${HOME:?HOME is required}/.codex}"
profile="$(printf '%s' "$codex_home" | cksum)"
profile="${profile%% *}"
socket="$runtime_dir/codex-$profile.sock"
session="rc-codex-$profile"
mkdir -p "$runtime_dir"
chmod 700 "$runtime_dir" 2>/dev/null || true

if tmux has-session -t "$session" 2>/dev/null && [ -S "$socket" ]; then
  printf 'running\t%s\n' "$socket"
  exit 0
fi

if tmux has-session -t "$session" 2>/dev/null; then
  stale_id="$(tmux display-message -p -t "$session" '#{session_id}' 2>/dev/null || true)"
  attempt=0
  while [ "$attempt" -lt 50 ]; do
    if [ -S "$socket" ]; then
      printf 'running\t%s\n' "$socket"
      exit 0
    fi
    if ! tmux has-session -t "$session" 2>/dev/null; then
      break
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done

  current_id="$(tmux display-message -p -t "$session" '#{session_id}' 2>/dev/null || true)"
  if [ -n "$stale_id" ] && [ "$current_id" = "$stale_id" ]; then
    tmux kill-session -t "$session" 2>/dev/null || true
  fi
fi

if tmux has-session -t "$session" 2>/dev/null; then
  attempt=0
  while [ "$attempt" -lt 50 ]; do
    if [ -S "$socket" ]; then
      printf 'running\t%s\n' "$socket"
      exit 0
    fi
    attempt=$((attempt + 1))
    sleep 0.1
  done
  printf 'stopped\t%s\n' "$socket"
  exit 1
fi
if [ -e "$socket" ]; then
  unlink "$socket"
fi

command -v codex >/dev/null 2>&1
command -v tmux >/dev/null 2>&1
printf -v listen_url 'unix://%s' "$socket"
printf -v quoted_url '%q' "$listen_url"
printf -v quoted_home '%q' "$codex_home"
tmux new-session -d -s "$session" \
  "export CODEX_HOME=$quoted_home; exec codex app-server --listen $quoted_url" \
  2>/dev/null || true

attempt=0
while [ "$attempt" -lt 50 ]; do
  if [ -S "$socket" ]; then
    printf 'running\t%s\n' "$socket"
    exit 0
  fi
  if ! tmux has-session -t "$session" 2>/dev/null; then
    printf 'stopped\t%s\n' "$socket"
    exit 1
  fi
  attempt=$((attempt + 1))
  sleep 0.1
done

printf 'stopped\t%s\n' "$socket"
exit 1
"#
}

fn runtime_stop_script() -> &'static str {
    r#"
set -eu
runtime_root="${XDG_RUNTIME_DIR:-${HOME:?HOME is required}/.cache/rollcall/runtime}"
codex_home="${CODEX_HOME:-${HOME:?HOME is required}/.codex}"
profile="$(printf '%s' "$codex_home" | cksum)"
profile="${profile%% *}"
socket="$runtime_root/rollcall/codex-$profile.sock"
legacy_signature_file="$runtime_root/rollcall/codex-$profile.args"
session="rc-codex-$profile"
if command -v tmux >/dev/null 2>&1 &&
   tmux has-session -t "$session" 2>/dev/null; then
  tmux kill-session -t "$session"
fi
attempt=0
while [ -S "$socket" ] && [ "$attempt" -lt 20 ]; do
  attempt=$((attempt + 1))
  sleep 0.05
done
if [ -e "$socket" ]; then
  unlink "$socket"
fi
if [ -e "$legacy_signature_file" ]; then
  unlink "$legacy_signature_file"
fi
printf 'stopped\t%s\n' "$socket"
"#
}

fn is_local(host: &str) -> bool {
    host == "local" || host == local_hostname()
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

fn unique_forward_socket() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    env::temp_dir().join(format!(
        "rollcall-app-server-forward-{}-{nonce}.sock",
        std::process::id()
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        is_local, local_hostname, parse_status, runtime_start_script, runtime_status_script,
    };

    #[test]
    fn observed_local_hostname_uses_the_local_runtime_path() {
        assert!(is_local("local"));
        assert!(is_local(&local_hostname()));
    }

    #[test]
    fn parses_running_and_stopped_status() {
        let running =
            parse_status("noise\nrunning\t/tmp/codex.sock\n").expect("running status should parse");
        assert!(running.running);
        assert_eq!(running.socket_path, "/tmp/codex.sock");

        let stopped =
            parse_status("stopped\t/home/fin/.cache/codex.sock\n").expect("status should parse");
        assert!(!stopped.running);
    }

    #[test]
    fn runtime_scripts_namespace_the_stable_tmux_session_and_socket_by_codex_home() {
        assert!(runtime_status_script().contains("CODEX_HOME"));
        assert!(runtime_status_script().contains("rc-codex-$profile"));
        let script = runtime_start_script();
        assert!(script.contains("codex-$profile.sock"));
        assert!(script.contains("codex app-server --listen"));
    }

    #[test]
    fn runtime_start_waits_for_an_existing_session_before_replacing_it() {
        let script = runtime_start_script();
        let first_wait = script
            .find("while [ \"$attempt\" -lt 50 ]")
            .expect("existing runtime wait");
        let stale_kill = script
            .find("tmux kill-session -t \"$session\"")
            .expect("stale runtime cleanup");
        assert!(first_wait < stale_kill);
        assert!(script.contains("current_id"));
        assert!(script.contains("stale_id"));
    }
}
