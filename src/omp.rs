use std::{
    collections::BTreeMap,
    env, fmt,
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

use serde_json::Value;

use crate::domain::{Activity, AgentKind, LiveObservation, RuntimeOwner, Session, TmuxBinding};
use crate::tmux;

const SESSION_SCRIPT: &str = r#"
root="${PI_CODING_AGENT_DIR:-$HOME/.omp/agent}/sessions"
[ -d "$root" ] || exit 0
if [ -n "${ROLLCALL_SESSION_LIMIT:-}" ]; then
  find "$root" -type f -name '*.jsonl' -printf '%T@\t%p\n' 2>/dev/null |
    sort -nr |
    sed -n "1,${ROLLCALL_SESSION_LIMIT}p" |
    cut -f2- |
    while IFS= read -r file; do
      mtime=$(stat -c %Y "$file" 2>/dev/null || stat -f %m "$file" 2>/dev/null || printf '0')
      printf 'FILE\t%s\t%s\n' "$mtime" "$file"
      sed -n '1,3p' "$file" 2>/dev/null
      printf 'END\n'
    done
else
  find "$root" -type f -name '*.jsonl' -print0 2>/dev/null |
  while IFS= read -r -d '' file; do
    mtime=$(stat -c %Y "$file" 2>/dev/null || stat -f %m "$file" 2>/dev/null || printf '0')
    printf 'FILE\t%s\t%s\n' "$mtime" "$file"
    sed -n '1,3p' "$file" 2>/dev/null
    printf 'END\n'
  done
fi
"#;

const LIVE_SCRIPT: &str = r#"
declare -A pane_session pane_id seen
walk_tree() {
  local pid="$1" session="$2" pane="$3" children child
  pane_session["$pid"]="$session"
  pane_id["$pid"]="$pane"
  if [[ -r "/proc/$pid/task/$pid/children" ]]; then
    read -r children < "/proc/$pid/task/$pid/children" || true
    for child in $children; do walk_tree "$child" "$session" "$pane"; done
  fi
}
if command -v tmux >/dev/null 2>&1; then
  while IFS=$'\t' read -r session pane pid; do
    [[ -n "$pid" ]] && walk_tree "$pid" "$session" "$pane"
  done < <(tmux list-panes -a -F $'#{session_name}\t#{pane_id}\t#{pane_pid}' 2>/dev/null || true)
fi
for proc in /proc/[0-9]*; do
  pid="${proc##*/}"
  [[ -r "$proc/cmdline" ]] || continue
  cmdline="$(tr '\0' ' ' < "$proc/cmdline" 2>/dev/null || true)"
  case " $cmdline " in
    *" omp "*|*"/omp "*|*" oh-my-pi "*|*"/coding-agent"*) ;;
    *) continue ;;
  esac
  session="${pane_session[$pid]-}"
  pane="${pane_id[$pid]-}"
  owner="external"
  [[ -n "$session" ]] && owner="tmux"
  for fd in "$proc"/fd/*; do
    target="$(readlink "$fd" 2>/dev/null || true)"
    case "$target" in
      */sessions/*/*.jsonl)
        base="${target##*/}"
        native="${base%.jsonl}"
        native="${native##*_}"
        key="$native|$owner|$session|$pane"
        [[ -n "${seen[$key]-}" ]] && continue
        seen["$key"]=1
        printf '%s\t%s\t%s\t%s\tworking\n' "$native" "$owner" "$session" "$pane"
        ;;
    esac
  done
done
"#;

#[derive(Debug)]
pub enum OmpError {
    ExternalFrontend(String),
    Start { program: &'static str, source: std::io::Error },
    CommandFailed(String),
    Json(serde_json::Error),
    Io(std::io::Error),
    Tmux(tmux::TmuxError),
}
impl fmt::Display for OmpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ExternalFrontend(session) => write!(f, "OMP session {session} is owned by an external frontend; use `rollcall resume` only when a second frontend is intentional"),
            Self::Start { program, source } => write!(f, "could not start {program}: {source}"),
            Self::CommandFailed(message) => write!(f, "OMP command failed: {message}"),
            Self::Json(error) => write!(f, "OMP session contained invalid JSON: {error}"),
            Self::Io(error) => write!(f, "OMP adapter I/O failed: {error}"),
            Self::Tmux(error) => write!(f, "{error}"),
        }
    }
}
impl From<serde_json::Error> for OmpError { fn from(error: serde_json::Error) -> Self { Self::Json(error) } }
impl From<std::io::Error> for OmpError { fn from(error: std::io::Error) -> Self { Self::Io(error) } }
impl From<tmux::TmuxError> for OmpError { fn from(error: tmux::TmuxError) -> Self { Self::Tmux(error) } }
pub fn discover(host: &str, limit: Option<usize>) -> Result<Vec<Session>, OmpError> {
    let script = limit
        .map(|limit| format!("ROLLCALL_SESSION_LIMIT={limit} {SESSION_SCRIPT}"))
        .unwrap_or_else(|| SESSION_SCRIPT.to_owned());
    let output = run(host, &script)?;
    let mut sessions = parse_sessions(&output)?;
    correlate_live(host, &mut sessions)?;
    sessions.sort_by_key(|session| std::cmp::Reverse((session.last_interaction_unix_seconds, session.updated_unix_seconds)));
    if let Some(limit) = limit { sessions.truncate(limit); }
    Ok(sessions)
}

pub fn observe_live(host: &str) -> Result<BTreeMap<String, LiveObservation>, OmpError> {
    let output = run(host, LIVE_SCRIPT)?;
    Ok(parse_live(&output))
}

pub fn attach(host: &str, native_id: &str) -> Result<(), OmpError> {
    let session = discover(host, None)?
        .into_iter()
        .find(|session| session.native_session_id == native_id)
        .ok_or_else(|| OmpError::CommandFailed(format!("OMP session {native_id} was not found")))?;
    match session.runtime {
        RuntimeOwner::TmuxFrontend => session
            .tmux
            .as_ref()
            .ok_or_else(|| OmpError::CommandFailed("OMP tmux frontend had no binding".to_owned()))
            .and_then(|binding| tmux::attach_pane(&session.host, &binding.session, &binding.pane).map_err(Into::into)),
        RuntimeOwner::ExternalFrontend => Err(OmpError::ExternalFrontend(session.id)),
        _ => resume_session(&session),
    }
}

pub fn resume(host: &str, native_id: &str) -> Result<(), OmpError> {
    let session = discover(host, None)?
        .into_iter()
        .find(|session| session.native_session_id == native_id)
        .ok_or_else(|| OmpError::CommandFailed(format!("OMP session {native_id} was not found")))?;
    resume_session(&session)
}

fn resume_session(session: &Session) -> Result<(), OmpError> {
    let command = shlex::try_join(["omp", "--resume", &session.native_session_id].iter().copied())
        .map_err(|error| OmpError::CommandFailed(error.to_string()))?;
    tmux::resume_command(
        &session.host,
        &deterministic_tmux_name(&session.cwd, &session.native_session_id),
        &session.cwd,
        &command,
    )?;
    Ok(())
}

fn deterministic_tmux_name(cwd: &str, native_id: &str) -> String {
    let project = cwd
        .rsplit('/')
        .find(|part| !part.is_empty())
        .unwrap_or("session");
    let suffix = native_id
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>();
    format!("rc-omp-{}-{}", project.replace(|c: char| !c.is_ascii_alphanumeric(), "-"), suffix)
}

fn run(host: &str, script: &str) -> Result<String, OmpError> {
    let output = if is_local(host) {
        Command::new("bash").args(["-lc", script]).stdin(Stdio::null()).output().map_err(|source| OmpError::Start { program: "bash", source })?
    } else {
        let remote = shlex::try_join(["bash", "-lc", script].iter().copied()).map_err(|error| OmpError::CommandFailed(error.to_string()))?;
        Command::new("ssh").args(["-o", "BatchMode=yes", "-o", "ConnectTimeout=5", "-o", "ConnectionAttempts=1", "--", host, &remote]).stdin(Stdio::null()).output().map_err(|source| OmpError::Start { program: "ssh", source })?
    };
    if !output.status.success() {
        return Err(OmpError::CommandFailed(String::from_utf8_lossy(&output.stderr).trim().to_owned()));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn is_local(host: &str) -> bool { host == "local" || host == observed_host() }

fn observed_host() -> String {
    env::var("HOSTNAME").ok().filter(|value| !value.is_empty()).or_else(|| {
        Command::new("hostname").output().ok().filter(|output| output.status.success()).map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
    }).filter(|value| !value.is_empty()).unwrap_or_else(|| "local".to_owned())
}

fn parse_sessions(output: &str) -> Result<Vec<Session>, OmpError> {
    let mut sessions = Vec::new();
    let mut mtime = 0;
    let mut lines = Vec::new();
    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("FILE\t") {
            let mut fields = rest.splitn(2, '\t');
            mtime = fields.next().and_then(|value| value.parse().ok()).unwrap_or_default();
            lines.clear();
        } else if line == "END" {
            if let Some(session) = parse_file(&lines, mtime)? { sessions.push(session); }
            lines.clear();
        } else if !line.is_empty() { lines.push(line); }
    }
    Ok(sessions)
}

fn parse_file(lines: &[&str], mtime: u64) -> Result<Option<Session>, OmpError> {
    let mut header = None;
    let mut title = None;
    let mut last_message = None;
    let mut completed = false;
    for line in lines {
        let value: Value = match serde_json::from_str(line) { Ok(value) => value, Err(_) => continue };
        match value.get("type").and_then(Value::as_str) {
            Some("session") => header = Some(value),
            Some("title") | Some("title_change") => title = value.get("title").and_then(Value::as_str).map(str::to_owned),
            Some("message") => {
                let message = value.get("message");
                if message.and_then(|item| item.get("role")).and_then(Value::as_str) == Some("assistant") {
                    last_message = message.and_then(|item| item.get("content")).and_then(Value::as_array).and_then(|items| items.iter().rev().find_map(|item| item.get("text").and_then(Value::as_str))).map(str::to_owned);
                }
            }
            Some("custom") if value.get("customType").and_then(Value::as_str) == Some("session_exit") => completed = true,
            _ => {}
        }
    }
    let Some(header) = header else { return Ok(None) };
    let native_id = header.get("id").and_then(Value::as_str).unwrap_or_default();
    let cwd = header.get("cwd").and_then(Value::as_str).unwrap_or(".");
    if native_id.is_empty() { return Ok(None) }
    let title = title.or_else(|| header.get("title").and_then(Value::as_str).map(str::to_owned)).unwrap_or_else(|| cwd.to_owned());
    let now = if mtime == 0 { unix_now() } else { mtime };
    Ok(Some(Session {
        id: String::new(), host: String::new(), agent: AgentKind::Omp, native_session_id: native_id.to_owned(), title,
        cwd: cwd.to_owned(), source: "omp".to_owned(), activity: if completed { Activity::Completed } else { Activity::Unknown },
        last_message: last_message.unwrap_or_default(), last_interaction_unix_seconds: now, updated_unix_seconds: now,
        runtime: RuntimeOwner::Resumable, tmux: None,
    }))
}

fn correlate_live(host: &str, sessions: &mut [Session]) -> Result<(), OmpError> {
    let observed_host = if is_local(host) { observed_host() } else { host.to_owned() };
    let live = observe_live(host)?;
    for session in sessions {
        session.host = observed_host.clone();
        session.agent = AgentKind::Omp;
        session.id = session.key().stable_id();
        if let Some(observation) = live.get(&session.native_session_id) {
            session.runtime = observation.runtime;
            session.tmux = observation.tmux.clone();
            session.activity = observation.activity;
        }
    }
    Ok(())
}

fn parse_live(output: &str) -> BTreeMap<String, LiveObservation> {
    output.lines().filter_map(|line| {
        let fields = line.split('\t').collect::<Vec<_>>();
        let [native_id, owner, session, pane, activity] = fields.as_slice() else { return None };
        let (runtime, tmux) = match *owner {
            "tmux" => (RuntimeOwner::TmuxFrontend, Some(TmuxBinding { session: (*session).to_owned(), pane: (*pane).to_owned() })),
            "external" => (RuntimeOwner::ExternalFrontend, None),
            _ => return None,
        };
        Some(((*native_id).to_owned(), LiveObservation { activity: if *activity == "working" { Activity::Working } else { Activity::Unknown }, runtime, tmux }))
    }).collect()
}

fn unix_now() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_secs()).unwrap_or_default() }

#[cfg(test)]
mod tests {
    use super::{parse_file, parse_live};
    use crate::domain::{Activity, RuntimeOwner};

    #[test]
    fn parses_omp_session_header_and_latest_assistant_message() {
        let lines = [
            r#"{"type":"session","version":3,"id":"019abc","cwd":"/work/rollcall","title":"Old"}"#,
            r#"{"type":"title","title":"New title"}"#,
            r#"{"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#,
        ];
        let session = parse_file(&lines, 100).expect("valid session").expect("header");
        assert_eq!(session.native_session_id, "019abc");
        assert_eq!(session.title, "New title");
        assert_eq!(session.last_message, "done");
        assert_eq!(session.activity, Activity::Unknown);
    }

    #[test]
    fn parses_live_omp_frontend() {
        let live = parse_live("019abc\ttmux\twork\t%4\tworking\n");
        let observation = live.get("019abc").expect("observation");
        assert_eq!(observation.runtime, RuntimeOwner::TmuxFrontend);
        assert_eq!(observation.activity, Activity::Working);
    }
}
