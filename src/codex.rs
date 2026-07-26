use std::{
    collections::BTreeMap,
    env, fmt, io,
    path::Path,
    process::{Command, Stdio},
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{codex_runtime, tmux};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CodexSession {
    pub id: String,
    pub host: String,
    pub native_session_id: String,
    pub title: String,
    pub cwd: String,
    pub source: String,
    pub activity: CodexActivity,
    pub last_message: String,
    pub last_interaction_unix_seconds: u64,
    pub updated_unix_seconds: u64,
    pub runtime: CodexRuntime,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux: Option<TmuxBinding>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexRuntime {
    Resumable,
    SharedBackend,
    #[serde(alias = "loadedOutsideTmux")]
    ExternalFrontend,
    #[serde(alias = "loadedInTmux")]
    TmuxFrontend,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CodexActivity {
    Working,
    WaitingApproval,
    WaitingInput,
    Completed,
    Failed,
    Unknown,
}

impl CodexActivity {
    #[must_use]
    pub const fn can_auto_settle(self) -> bool {
        matches!(self, Self::Completed | Self::Unknown)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TmuxBinding {
    pub session: String,
    pub pane: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum LiveOwner {
    SharedBackend,
    ExternalFrontend,
    Tmux(TmuxBinding),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LiveObservation {
    pub activity: CodexActivity,
    pub owner: LiveOwner,
}

const LIVE_OBSERVATION_SCRIPT: &str = r#"
declare -A pane_session pane_id seen

latest_lifecycle() {
  local target="$1"
  if command -v tac >/dev/null 2>&1; then
    tac -- "$target" 2>/dev/null |
      grep -m1 -E '"type":"(task_started|task_complete|turn_aborted)"'
  elif tail -r /dev/null >/dev/null 2>&1; then
    tail -r "$target" 2>/dev/null |
      grep -m1 -E '"type":"(task_started|task_complete|turn_aborted)"'
  else
    tail -n 4096 "$target" 2>/dev/null |
      grep -E '"type":"(task_started|task_complete|turn_aborted)"' |
      tail -n 1
  fi
}

walk_tree() {
  local pid="$1" session="$2" pane="$3" children child
  pane_session["$pid"]="$session"
  pane_id["$pid"]="$pane"
  if [[ -r "/proc/$pid/task/$pid/children" ]]; then
    read -r children < "/proc/$pid/task/$pid/children" || true
    for child in $children; do
      walk_tree "$child" "$session" "$pane"
    done
  fi
}

if command -v tmux >/dev/null 2>&1; then
  pane_format=$'#{session_name}\t#{pane_id}\t#{pane_pid}'
  while IFS=$'\t' read -r session pane pid; do
    [[ -n "$pid" ]] && walk_tree "$pid" "$session" "$pane"
  done < <(tmux list-panes -a -F "$pane_format" 2>/dev/null || true)
fi

for proc in /proc/[0-9]*; do
  pid="${proc##*/}"
  [[ -r "$proc/comm" ]] || continue
  read -r comm < "$proc/comm" || continue
  [[ "$comm" == codex ]] || continue
  cmdline="$(tr '\0' ' ' < "$proc/cmdline" 2>/dev/null || true)"
  is_app_server=0
  is_rollcall_backend=0
  case " $cmdline " in
    *" app-server "*) is_app_server=1 ;;
  esac
  case " $cmdline " in
    *" app-server "*"--listen "*"unix://"*)
      case "$cmdline" in
        */rollcall/codex-*.sock*) is_rollcall_backend=1 ;;
      esac
    ;;
  esac
  session="${pane_session[$pid]-}"
  pane="${pane_id[$pid]-}"
  if [[ "$is_rollcall_backend" == 1 && "$session" == rc-codex-* ]]; then
    owner="shared"
    session=""
    pane=""
  elif [[ "$is_app_server" == 1 ]]; then
    owner="external"
    session=""
    pane=""
  elif [[ -n "$session" ]]; then
    owner="tmux"
  else
    owner="external"
  fi
  if [[ "$owner" == tmux &&
        "$cmdline" =~ (^|[[:space:]])resume[[:space:]]+([0-9a-fA-F-]{36})([[:space:]]|$) ]]; then
    native="${BASH_REMATCH[2]}"
    key="$native|$owner|$session|$pane"
    if [[ -z "${seen[$key]-}" ]]; then
      seen["$key"]=1
      printf '%s\t%s\t%s\t%s\t%s\n' \
        "$native" "$owner" "$session" "$pane" "unknown"
    fi
  fi
  for fd in "$proc"/fd/*; do
    target="$(readlink "$fd" 2>/dev/null || true)"
    case "$target" in
      */sessions/*/rollout-*.jsonl)
        base="${target##*/}"
        native="${base%.jsonl}"
        native="${native: -36}"
        key="$native|$owner|$session|$pane"
        [[ -n "${seen[$key]-}" ]] && continue
        seen["$key"]=1
        lifecycle="$(latest_lifecycle "$target" || true)"
        case "$lifecycle" in
          *'"type":"task_started"'*) activity="working" ;;
          *'"type":"task_complete"'*|*'"type":"turn_aborted"'*) activity="completed" ;;
          *) activity="unknown" ;;
        esac
        printf '%s\t%s\t%s\t%s\t%s\n' \
          "$native" "$owner" "$session" "$pane" "$activity"
        ;;
    esac
  done
done
"#;

#[derive(Debug)]
pub enum CodexError {
    InvalidSessionId(String),
    MissingSession(String),
    ExternalFrontend(String),
    Protocol(String),
    Quote(shlex::QuoteError),
    Start {
        program: &'static str,
        source: io::Error,
    },
    Io(io::Error),
    Json(serde_json::Error),
    Runtime(codex_runtime::RuntimeError),
    Tmux(tmux::TmuxError),
}

impl fmt::Display for CodexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSessionId(session) => write!(
                formatter,
                "invalid Codex session {session:?}; expected HOST:codex:SESSION_ID"
            ),
            Self::MissingSession(session) => {
                write!(
                    formatter,
                    "Codex session {session} was not found on its host"
                )
            }
            Self::ExternalFrontend(session) => write!(
                formatter,
                "Codex session {session} is owned by an external frontend; close or release it before resuming through Rollcall"
            ),
            Self::Protocol(message) => {
                write!(formatter, "Codex app-server protocol error: {message}")
            }
            Self::Quote(error) => write!(formatter, "could not quote remote command: {error}"),
            Self::Start { program, source } => {
                write!(formatter, "could not start {program}: {source}")
            }
            Self::Io(error) => write!(formatter, "Codex adapter I/O failed: {error}"),
            Self::Json(error) => write!(formatter, "Codex adapter returned invalid JSON: {error}"),
            Self::Runtime(error) => write!(formatter, "{error}"),
            Self::Tmux(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<io::Error> for CodexError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<serde_json::Error> for CodexError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json(error)
    }
}

impl From<codex_runtime::RuntimeError> for CodexError {
    fn from(error: codex_runtime::RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl From<shlex::QuoteError> for CodexError {
    fn from(error: shlex::QuoteError) -> Self {
        Self::Quote(error)
    }
}

impl From<tmux::TmuxError> for CodexError {
    fn from(error: tmux::TmuxError) -> Self {
        Self::Tmux(error)
    }
}

pub fn discover(host: &str, limit: Option<usize>) -> Result<Vec<CodexSession>, CodexError> {
    let observed_host = observed_host(host);
    let mut client = AppServerClient::start(host)?;
    let mut threads = client.list_threads(limit)?;
    client.stop();

    correlate_live_sessions(host, &observed_host, &mut threads)?;
    Ok(threads)
}

pub fn discover_detailed(
    host: &str,
    limit: Option<usize>,
) -> Result<Vec<CodexSession>, CodexError> {
    let mut sessions = discover(host, limit)?;
    enrich_details(host, &mut sessions)?;
    Ok(sessions)
}

pub fn enrich_details(host: &str, sessions: &mut [CodexSession]) -> Result<(), CodexError> {
    let mut client = AppServerClient::start(host)?;
    client.enrich_threads(sessions);
    client.stop();
    Ok(())
}

fn correlate_live_sessions(
    host: &str,
    observed_host: &str,
    threads: &mut [CodexSession],
) -> Result<(), CodexError> {
    let live = observe_live(host)?;
    for thread in threads.iter_mut() {
        thread.host = observed_host.to_owned();
        thread.id = format!("{observed_host}:codex:{}", thread.native_session_id);
        if let Some(observation) = live.get(&thread.native_session_id) {
            thread.runtime = match &observation.owner {
                LiveOwner::Tmux(binding) => {
                    let mut binding = binding.clone();
                    if let Some(desired) = normalized_managed_tmux_name(
                        &binding.session,
                        &thread.cwd,
                        &thread.native_session_id,
                    ) && tmux::rename_session(host, &binding.session, &desired).is_ok()
                    {
                        binding.session = desired;
                    }
                    thread.tmux = Some(binding);
                    CodexRuntime::TmuxFrontend
                }
                LiveOwner::SharedBackend => {
                    thread.tmux = None;
                    CodexRuntime::SharedBackend
                }
                LiveOwner::ExternalFrontend => {
                    thread.tmux = None;
                    CodexRuntime::ExternalFrontend
                }
            };
            thread.activity = merge_observed_activity(thread.activity, observation.activity);
        }
    }

    threads.sort_by_key(|thread| {
        std::cmp::Reverse((
            thread.last_interaction_unix_seconds,
            thread.updated_unix_seconds,
        ))
    });
    Ok(())
}

pub fn attach(session_id: &str) -> Result<(), CodexError> {
    let (host, native_id) = parse_session_id(session_id)?;
    let session = discover(host, None)?
        .into_iter()
        .find(|session| session.native_session_id == native_id)
        .ok_or_else(|| CodexError::MissingSession(session_id.to_owned()))?;

    match (&session.runtime, &session.tmux) {
        (CodexRuntime::TmuxFrontend, Some(binding)) => {
            tmux::attach_pane(&session.host, &binding.session, &binding.pane)?;
            Ok(())
        }
        (CodexRuntime::ExternalFrontend, _) => Err(CodexError::ExternalFrontend(session.id)),
        _ => {
            let app_server_socket = codex_runtime::ensure(host)?;
            tmux::resume_codex(
                &session.host,
                &deterministic_tmux_name(&session.cwd, &session.native_session_id),
                &session.cwd,
                &session.native_session_id,
                &app_server_socket,
            )
            .map_err(Into::into)
        }
    }
}

fn parse_session_id(session: &str) -> Result<(&str, &str), CodexError> {
    let fields = session.splitn(3, ':').collect::<Vec<_>>();
    match fields.as_slice() {
        [host, "codex", native_id] if !host.is_empty() && !native_id.is_empty() => {
            Ok((host, native_id))
        }
        _ => Err(CodexError::InvalidSessionId(session.to_owned())),
    }
}

fn deterministic_tmux_name(cwd: &str, native_id: &str) -> String {
    let compact = native_id.replace('-', "");
    let suffix = compact
        .get(compact.len().saturating_sub(8)..)
        .unwrap_or(&compact);
    let project = Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .map(slugify_tmux_component)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "session".to_owned());
    format!("rc-{project}-{suffix}")
}

fn normalized_managed_tmux_name(current: &str, cwd: &str, native_id: &str) -> Option<String> {
    current
        .starts_with("rc-pending-")
        .then(|| deterministic_tmux_name(cwd, native_id))
        .filter(|desired| desired != current)
}

fn slugify_tmux_component(value: &str) -> String {
    let mut slug = String::new();
    let mut separator_pending = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            if separator_pending && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character.to_ascii_lowercase());
            separator_pending = false;
        } else {
            separator_pending = true;
        }
        if slug.len() >= 24 {
            break;
        }
    }
    slug.trim_end_matches('-').to_owned()
}

pub(crate) fn observed_host(host: &str) -> String {
    if host == "local" || host == local_hostname() {
        local_hostname()
    } else {
        host.to_owned()
    }
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

struct AppServerClient {
    connection: codex_runtime::RuntimeConnection,
    next_id: u64,
}

impl AppServerClient {
    fn start(host: &str) -> Result<Self, CodexError> {
        let mut client = Self {
            connection: codex_runtime::connect_or_start(host)?,
            next_id: 1,
        };

        let initialize_id = client.request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "rollcall",
                    "version": env!("CARGO_PKG_VERSION")
                },
                "capabilities": {
                    "experimentalApi": true
                }
            }),
        )?;
        client.response(initialize_id)?;
        client.notify("initialized", json!({}))?;
        Ok(client)
    }

    fn list_threads(&mut self, limit: Option<usize>) -> Result<Vec<CodexSession>, CodexError> {
        let mut sessions = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let remaining = limit.map(|limit| limit.saturating_sub(sessions.len()));
            if remaining == Some(0) {
                break;
            }
            let page_size = remaining.unwrap_or(100).min(100);
            let request_id = self.request(
                "thread/list",
                json!({
                    "archived": false,
                    "cursor": cursor,
                    "limit": page_size,
                    "sortDirection": "desc",
                    "sortKey": "recency_at",
                    "sourceKinds": ["cli", "vscode", "exec", "appServer", "unknown"],
                    "useStateDbOnly": true
                }),
            )?;
            let response = self.response(request_id)?;
            let result: ThreadListResult = serde_json::from_value(response)?;
            sessions.extend(result.data.into_iter().map(CodexSession::from));
            cursor = result.next_cursor;

            if cursor.is_none() {
                break;
            }
        }

        Ok(sessions)
    }

    fn enrich_threads(&mut self, sessions: &mut [CodexSession]) {
        for session in sessions {
            let Ok(request_id) = self.request(
                "thread/read",
                json!({
                    "threadId": session.native_session_id,
                    "includeTurns": true
                }),
            ) else {
                break;
            };
            let Ok(response) = self.response(request_id) else {
                continue;
            };
            let Ok(result) = serde_json::from_value::<ThreadReadResult>(response) else {
                continue;
            };
            if let Some(turn) = result.thread.turns.last()
                && session.activity != CodexActivity::Working
                && session.activity != CodexActivity::WaitingApproval
                && session.activity != CodexActivity::WaitingInput
            {
                session.activity = match turn.status.as_str() {
                    "inProgress" => CodexActivity::Working,
                    "failed" => CodexActivity::Failed,
                    "completed" | "interrupted" => CodexActivity::Completed,
                    _ => session.activity,
                };
            }
            if let Some(message) = last_agent_message(&result.thread.turns) {
                session.last_message = compact_message(message);
            }
        }
    }

    fn request(&mut self, method: &str, params: Value) -> Result<u64, CodexError> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        Ok(id)
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), CodexError> {
        self.write(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        }))
    }

    fn write(&mut self, message: &Value) -> Result<(), CodexError> {
        self.connection
            .send_text(&serde_json::to_string(message)?)?;
        Ok(())
    }

    fn response(&mut self, expected_id: u64) -> Result<Value, CodexError> {
        loop {
            let Some(message) = self.connection.read_text()? else {
                return Err(CodexError::Protocol(
                    "shared app-server connection closed before replying".to_owned(),
                ));
            };
            let Ok(message) = serde_json::from_str::<RpcMessage>(&message) else {
                continue;
            };
            if message.id == Some(expected_id) {
                if let Some(error) = message.error {
                    return Err(CodexError::Protocol(error.to_string()));
                }
                return message.result.ok_or_else(|| {
                    CodexError::Protocol("response did not contain a result".to_owned())
                });
            }
        }
    }

    fn stop(&mut self) {
        self.connection.close();
    }
}

impl Drop for AppServerClient {
    fn drop(&mut self) {
        self.stop();
    }
}

#[derive(Deserialize)]
struct RpcMessage {
    id: Option<u64>,
    result: Option<Value>,
    error: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadListResult {
    data: Vec<RawThread>,
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    id: String,
    cwd: String,
    name: Option<String>,
    preview: String,
    recency_at: Option<i64>,
    updated_at: i64,
    source: Value,
    status: RawThreadStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", tag = "type")]
enum RawThreadStatus {
    NotLoaded,
    Idle,
    SystemError,
    Active {
        #[serde(default)]
        active_flags: Vec<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ThreadReadResult {
    thread: ReadThread,
}

#[derive(Deserialize)]
struct ReadThread {
    turns: Vec<ReadTurn>,
}

#[derive(Deserialize)]
struct ReadTurn {
    status: String,
    items: Vec<Value>,
}

impl From<RawThread> for CodexSession {
    fn from(thread: RawThread) -> Self {
        let title = thread
            .name
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| compact_title(&thread.preview, &thread.cwd));

        Self {
            id: String::new(),
            host: String::new(),
            native_session_id: thread.id,
            title,
            cwd: thread.cwd,
            source: source_name(&thread.source),
            activity: activity_from_status(&thread.status),
            last_message: String::new(),
            last_interaction_unix_seconds: positive_seconds(
                thread.recency_at.unwrap_or(thread.updated_at),
            ),
            updated_unix_seconds: positive_seconds(thread.updated_at),
            runtime: CodexRuntime::Resumable,
            tmux: None,
        }
    }
}

fn activity_from_status(status: &RawThreadStatus) -> CodexActivity {
    match status {
        RawThreadStatus::Active { active_flags }
            if active_flags.iter().any(|flag| flag == "waitingOnApproval") =>
        {
            CodexActivity::WaitingApproval
        }
        RawThreadStatus::Active { active_flags }
            if active_flags.iter().any(|flag| flag == "waitingOnUserInput") =>
        {
            CodexActivity::WaitingInput
        }
        RawThreadStatus::Active { .. } => CodexActivity::Working,
        RawThreadStatus::SystemError => CodexActivity::Failed,
        RawThreadStatus::Idle | RawThreadStatus::NotLoaded => CodexActivity::Completed,
    }
}

fn last_agent_message(turns: &[ReadTurn]) -> Option<&str> {
    turns.iter().rev().find_map(|turn| {
        turn.items.iter().rev().find_map(|item| {
            (item.get("type").and_then(Value::as_str) == Some("agentMessage"))
                .then(|| item.get("text").and_then(Value::as_str))
                .flatten()
                .filter(|text| !text.trim().is_empty())
        })
    })
}

fn compact_message(message: &str) -> String {
    let compact = message.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut characters = compact.chars();
    let prefix = characters.by_ref().take(120).collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn positive_seconds(value: i64) -> u64 {
    u64::try_from(value).unwrap_or_default()
}

fn compact_title(preview: &str, cwd: &str) -> String {
    let compact = preview.split_whitespace().collect::<Vec<_>>().join(" ");
    let candidate = if compact.is_empty() { cwd } else { &compact };
    let mut characters = candidate.chars();
    let title = characters.by_ref().take(72).collect::<String>();
    if characters.next().is_some() {
        format!("{title}…")
    } else {
        title
    }
}

fn source_name(source: &Value) -> String {
    source
        .as_str()
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| "unknown".to_owned())
}

pub(crate) fn observe_live(host: &str) -> Result<BTreeMap<String, LiveObservation>, CodexError> {
    let output = if host == "local" || host == local_hostname() {
        Command::new("bash")
            .args(["-c", LIVE_OBSERVATION_SCRIPT])
            .stdin(Stdio::null())
            .output()
            .map_err(|source| CodexError::Start {
                program: "bash",
                source,
            })?
    } else {
        let remote_command = shlex::try_join(["bash", "-c", LIVE_OBSERVATION_SCRIPT])?;
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
            .map_err(|source| CodexError::Start {
                program: "ssh",
                source,
            })?
    };

    if !output.status.success() {
        return Err(CodexError::Protocol(
            String::from_utf8_lossy(&output.stderr).trim().to_owned(),
        ));
    }

    Ok(parse_live_observations(&String::from_utf8_lossy(
        &output.stdout,
    )))
}

fn parse_live_observations(output: &str) -> BTreeMap<String, LiveObservation> {
    let mut observations: BTreeMap<String, LiveObservation> = BTreeMap::new();
    for line in output.lines() {
        let fields = line.split('\t').collect::<Vec<_>>();
        let [native_id, owner, session, pane, activity] = fields.as_slice() else {
            continue;
        };
        let owner = match *owner {
            "shared" => LiveOwner::SharedBackend,
            "external" => LiveOwner::ExternalFrontend,
            "tmux" if !session.is_empty() => LiveOwner::Tmux(TmuxBinding {
                session: (*session).to_owned(),
                pane: (*pane).to_owned(),
            }),
            _ => continue,
        };
        let activity = match *activity {
            "working" => CodexActivity::Working,
            "completed" => CodexActivity::Completed,
            _ => CodexActivity::Unknown,
        };
        observations
            .entry((*native_id).to_owned())
            .and_modify(|existing| {
                if owner_priority(&owner) > owner_priority(&existing.owner) {
                    existing.owner.clone_from(&owner);
                }
                existing.activity = merge_live_activity(existing.activity, activity);
            })
            .or_insert(LiveObservation { activity, owner });
    }
    observations
}

const fn merge_live_activity(current: CodexActivity, observed: CodexActivity) -> CodexActivity {
    if matches!(current, CodexActivity::Working) || matches!(observed, CodexActivity::Working) {
        CodexActivity::Working
    } else if matches!(current, CodexActivity::Completed)
        || matches!(observed, CodexActivity::Completed)
    {
        CodexActivity::Completed
    } else {
        CodexActivity::Unknown
    }
}

const fn owner_priority(owner: &LiveOwner) -> u8 {
    match owner {
        LiveOwner::SharedBackend => 0,
        LiveOwner::ExternalFrontend => 1,
        LiveOwner::Tmux(_) => 2,
    }
}

pub(crate) const fn merge_observed_activity(
    current: CodexActivity,
    observed: CodexActivity,
) -> CodexActivity {
    match observed {
        CodexActivity::Completed => CodexActivity::Completed,
        CodexActivity::Working
            if !matches!(
                current,
                CodexActivity::WaitingApproval | CodexActivity::WaitingInput
            ) =>
        {
            CodexActivity::Working
        }
        _ => current,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CodexActivity, CodexRuntime, CodexSession, LIVE_OBSERVATION_SCRIPT, LiveOwner, RawThread,
        RawThreadStatus, compact_title, deterministic_tmux_name, merge_live_activity,
        merge_observed_activity, normalized_managed_tmux_name, parse_live_observations,
        parse_session_id,
    };

    #[test]
    fn old_runtime_names_deserialize_into_the_new_ownership_model() {
        assert_eq!(
            serde_json::from_value::<CodexRuntime>(json!("loadedOutsideTmux"))
                .expect("legacy external runtime should deserialize"),
            CodexRuntime::ExternalFrontend
        );
        assert_eq!(
            serde_json::from_value::<CodexRuntime>(json!("loadedInTmux"))
                .expect("legacy tmux runtime should deserialize"),
            CodexRuntime::TmuxFrontend
        );
        assert_eq!(
            serde_json::to_value(CodexRuntime::ExternalFrontend)
                .expect("new runtime should serialize"),
            json!("externalFrontend")
        );
    }

    #[test]
    fn converts_native_threads_to_resumable_sessions() {
        let session = CodexSession::from(RawThread {
            id: "019f0000-0000-7000-8000-000000000001".to_owned(),
            cwd: "/fabric".to_owned(),
            name: Some("Rollcall".to_owned()),
            preview: "ignored".to_owned(),
            recency_at: Some(200),
            updated_at: 250,
            source: json!("cli"),
            status: RawThreadStatus::NotLoaded,
        });

        assert_eq!(session.title, "Rollcall");
        assert_eq!(session.last_interaction_unix_seconds, 200);
        assert_eq!(session.updated_unix_seconds, 250);
        assert_eq!(session.runtime, CodexRuntime::Resumable);
    }

    #[test]
    fn compact_titles_collapse_whitespace_and_truncate() {
        assert_eq!(compact_title("one\n two   three", "/tmp"), "one two three");
        assert!(compact_title(&"x".repeat(100), "/tmp").ends_with('…'));
    }

    #[test]
    fn parses_stable_codex_ids() {
        assert_eq!(
            parse_session_id("coda:codex:019f").expect("id should parse"),
            ("coda", "019f")
        );
    }

    #[test]
    fn tmux_names_are_deterministic_and_compact() {
        assert_eq!(
            deterministic_tmux_name(
                "/home/fin/projects/Roll Call!",
                "019f9631-f91c-7f20-95f3-019a0ba62241"
            ),
            "rc-roll-call-0ba62241"
        );
        assert_eq!(deterministic_tmux_name("/", "019f"), "rc-session-019f");
    }

    #[test]
    fn pending_shim_sessions_receive_the_deterministic_native_name() {
        assert_eq!(
            normalized_managed_tmux_name(
                "rc-pending-rollcall-123-456",
                "/home/fin/projects/rollcall",
                "019f9631-f91c-7f20-95f3-019a0ba62241"
            ),
            Some("rc-rollcall-0ba62241".to_owned())
        );
        assert_eq!(
            normalized_managed_tmux_name(
                "rc-rollcall-0ba62241",
                "/home/fin/projects/rollcall",
                "019f9631-f91c-7f20-95f3-019a0ba62241"
            ),
            None
        );
    }

    #[test]
    fn live_observations_prefer_an_attachable_tmux_frontend() {
        let observations = parse_live_observations(
            "\
019f\tshared\t\t\tcompleted
019f\texternal\t\t\tworking
019f\ttmux\tagents\t%3\tworking
019e\tshared\t\t\tcompleted
",
        );

        let working = &observations["019f"];
        assert_eq!(working.activity, CodexActivity::Working);
        assert!(matches!(
            &working.owner,
            LiveOwner::Tmux(binding) if binding.session == "agents"
        ));
        assert_eq!(observations["019e"].activity, CodexActivity::Completed);
        assert_eq!(observations["019e"].owner, LiveOwner::SharedBackend);
    }

    #[test]
    fn live_observations_distinguish_shared_and_external_app_servers() {
        let observations = parse_live_observations(
            "\
019f\tshared\trc-codex-123\t%1\tcompleted
019e\texternal\tt3-code\t%2\tworking
",
        );

        assert_eq!(observations["019f"].owner, LiveOwner::SharedBackend);
        assert_eq!(observations["019e"].owner, LiveOwner::ExternalFrontend);
        assert!(!matches!(observations["019f"].owner, LiveOwner::Tmux(_)));
    }

    #[test]
    fn remote_tui_observation_wins_regardless_of_process_iteration_order() {
        for output in [
            "\
019f\ttmux\trc-project-deadbeef\t%3\tunknown
019f\tshared\t\t\tcompleted
",
            "\
019f\tshared\t\t\tcompleted
019f\ttmux\trc-project-deadbeef\t%3\tunknown
",
        ] {
            let observation = &parse_live_observations(output)["019f"];
            assert_eq!(observation.activity, CodexActivity::Completed);
            assert!(matches!(
                &observation.owner,
                LiveOwner::Tmux(binding)
                    if binding.session == "rc-project-deadbeef" && binding.pane == "%3"
            ));
        }
    }

    #[test]
    fn live_activity_merge_is_order_independent() {
        assert_eq!(
            merge_live_activity(CodexActivity::Unknown, CodexActivity::Completed),
            CodexActivity::Completed
        );
        assert_eq!(
            merge_live_activity(CodexActivity::Completed, CodexActivity::Working),
            CodexActivity::Working
        );
        assert_eq!(
            merge_live_activity(CodexActivity::Working, CodexActivity::Completed),
            CodexActivity::Working
        );
    }

    #[test]
    fn only_the_reserved_socket_and_tmux_namespace_identify_the_shared_backend() {
        assert!(LIVE_OBSERVATION_SCRIPT.contains("--listen "));
        assert!(LIVE_OBSERVATION_SCRIPT.contains("unix://"));
        assert!(LIVE_OBSERVATION_SCRIPT.contains("/rollcall/codex-*.sock"));
        assert!(LIVE_OBSERVATION_SCRIPT.contains("\"$session\" == rc-codex-*"));
        assert!(LIVE_OBSERVATION_SCRIPT.contains("elif [[ \"$is_app_server\" == 1 ]]"));
        assert!(LIVE_OBSERVATION_SCRIPT.contains("BASH_REMATCH[2]"));
    }

    #[test]
    fn live_activity_preserves_attention_until_the_turn_finishes() {
        assert_eq!(
            merge_observed_activity(CodexActivity::WaitingApproval, CodexActivity::Working),
            CodexActivity::WaitingApproval
        );
        assert_eq!(
            merge_observed_activity(CodexActivity::WaitingInput, CodexActivity::Completed),
            CodexActivity::Completed
        );
        assert_eq!(
            merge_observed_activity(CodexActivity::Completed, CodexActivity::Unknown),
            CodexActivity::Completed
        );
    }
}
