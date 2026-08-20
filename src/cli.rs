use std::{
    fmt,
    io::IsTerminal,
    path::PathBuf,
    process::ExitCode,
    time::{SystemTime, UNIX_EPOCH},
};

use clap::{Parser, Subcommand};
use serde::Serialize;

use crate::{
    agents::{self, AgentError},
    domain::{Activity, RuntimeOwner},
    hosts::{self, HostDiscoveryError, SshHost},
    picker::{self, PickerError},
    shell::{self, ShellError},
    store::{DEFAULT_STALE_AFTER_SECONDS, HistoryEntry, Store, StoreError},
    tmux::{self, TmuxError},
};

const DEFAULT_SESSION_LIMIT: usize = 50;

#[derive(Debug, Parser)]
#[command(
    name = "rollcall",
    version,
    about = "Control plane for coding-agent sessions"
)]
pub struct Cli {
    #[arg(long, global = true, help = "Emit machine-readable JSON")]
    json: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List resumable coding-agent sessions
    List {
        /// Inspect the local machine or one configured SSH host
        #[arg(long, default_value = "local", value_name = "HOST")]
        host: String,

        /// Return at most this many sessions
        #[arg(long, value_name = "N", conflicts_with = "all")]
        limit: Option<usize>,

        /// Return the complete native session inventory
        #[arg(long)]
        all: bool,
    },

    /// Search all cached sessions by native chronology without contacting hosts
    History {
        /// Restrict history to one observed host
        #[arg(long, value_name = "HOST")]
        host: Option<String>,

        /// Match words across titles, messages, paths, hosts, states, and IDs
        #[arg(long, value_name = "QUERY")]
        search: Option<String>,

        /// Return at most this many matching sessions
        #[arg(long, value_name = "N")]
        limit: Option<usize>,
    },

    /// Open the interactive session picker
    Pick {
        /// Show all configured hosts or filter to one host
        #[arg(long, default_value = "all", value_name = "HOST")]
        host: String,

        /// Load at most this many sessions per host
        #[arg(long, default_value_t = DEFAULT_SESSION_LIMIT, value_name = "N")]
        limit: usize,
    },

    /// Open the session picker in a tmux popup
    Popup {
        /// Show all configured hosts or filter to one host
        #[arg(long, default_value = "all", value_name = "HOST")]
        host: String,

        /// Load at most this many sessions per host
        #[arg(long, default_value_t = DEFAULT_SESSION_LIMIT, value_name = "N")]
        limit: usize,
    },

    /// List literal aliases from the user SSH configuration
    Hosts {
        /// Read aliases from this SSH config instead of ~/.ssh/config
        #[arg(long, value_name = "PATH")]
        config: Option<PathBuf>,
    },

    /// Attach to or resume a discovered session
    Attach {
        /// Stable control-plane session identifier
        session: String,
    },
    /// Start a new managed frontend and resume a discovered session
    Resume {
        /// Stable control-plane session identifier
        session: String,
    },

    /// Print Bash/Zsh wrappers that transparently launch agents inside tmux
    ShellInit {
        /// Agent command names to wrap; defaults to codex, claude, pi, and omp
        #[arg(value_name = "AGENT")]
        agents: Vec<String>,
    },

    /// List raw tmux sessions for diagnostics
    Tmux {
        /// Inspect the local machine or one configured SSH host
        #[arg(long, default_value = "local", value_name = "HOST")]
        host: String,
    },

    /// Observe hosts and stream normalized lifecycle events
    Watch {
        /// Observe all configured hosts or restrict monitoring to one host
        #[arg(long, default_value = "all", value_name = "HOST")]
        host: String,

        /// Load at most this many sessions per host
        #[arg(long, default_value_t = DEFAULT_SESSION_LIMIT, value_name = "N")]
        limit: usize,

        /// Reconcile each selected host once, emit transitions, and exit
        #[arg(long)]
        once: bool,
    },
}

#[derive(Debug)]
enum CliError {
    Agent(AgentError),
    HostDiscovery(HostDiscoveryError),
    Picker(PickerError),
    Serialization(serde_json::Error),
    Shell(ShellError),
    Store(StoreError),
    Tmux(TmuxError),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::HostDiscovery(error) => write!(formatter, "{error}"),
            Self::Picker(error) => write!(formatter, "{error}"),
            Self::Serialization(error) => write!(formatter, "could not serialize output: {error}"),
            Self::Shell(error) => write!(formatter, "{error}"),
            Self::Store(error) => write!(formatter, "{error}"),
            Self::Tmux(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<AgentError> for CliError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(error: serde_json::Error) -> Self {
        Self::Serialization(error)
    }
}

impl From<HostDiscoveryError> for CliError {
    fn from(error: HostDiscoveryError) -> Self {
        Self::HostDiscovery(error)
    }
}

impl From<ShellError> for CliError {
    fn from(error: ShellError) -> Self {
        Self::Shell(error)
    }
}

impl From<PickerError> for CliError {
    fn from(error: PickerError) -> Self {
        Self::Picker(error)
    }
}

impl From<StoreError> for CliError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<TmuxError> for CliError {
    fn from(error: TmuxError) -> Self {
        Self::Tmux(error)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Inventory<'a, T> {
    kind: &'a str,
    items: T,
}

pub fn main_entry() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("rollcall: {error}");
            ExitCode::from(2)
        }
    }
}

fn run(cli: Cli) -> Result<(), CliError> {
    let command = match cli.command {
        Some(command) => command,
        None if cli.json || !std::io::stdout().is_terminal() => Command::List {
            host: "local".to_owned(),
            limit: None,
            all: false,
        },
        None => Command::Pick {
            host: "all".to_owned(),
            limit: DEFAULT_SESSION_LIMIT,
        },
    };

    match command {
        Command::List { host, limit, all } => {
            let limit = if all {
                None
            } else {
                Some(limit.unwrap_or(DEFAULT_SESSION_LIMIT))
            };
            print_sessions(&host, limit, cli.json)
        }
        Command::History {
            host,
            search,
            limit,
        } => print_history(host.as_deref(), search.as_deref(), limit, cli.json),
        Command::Pick { host, limit } => picker::run(&host, limit).map_err(Into::into),
        Command::Popup { host, limit } => picker::popup(&host, limit).map_err(Into::into),
        Command::Hosts { config } => print_hosts(config.as_deref(), cli.json),
        Command::Attach { session } => attach(&session),
        Command::Resume { session } => resume(&session),
        Command::ShellInit { agents } => {
            print!("{}", shell::render(&agents)?);
            Ok(())
        }
        Command::Tmux { host } => print_tmux_sessions(&host, cli.json),
        Command::Watch { host, limit, once } => {
            picker::watch(&host, limit, once, cli.json).map_err(Into::into)
        }
    }
}

fn print_history(
    host: Option<&str>,
    search: Option<&str>,
    limit: Option<usize>,
    json: bool,
) -> Result<(), CliError> {
    let store = Store::open()?;
    store.auto_settle_stale(DEFAULT_STALE_AFTER_SECONDS)?;
    let mut history = store
        .load_history()?
        .into_iter()
        .filter(|entry| history_matches(entry, host, search))
        .collect::<Vec<_>>();
    if let Some(limit) = limit {
        history.truncate(limit);
    }

    if json {
        return print_json_inventory("history", history);
    }
    if history.is_empty() {
        println!("No cached sessions matched the history query.");
        return Ok(());
    }

    let titles = history
        .iter()
        .map(|entry| truncate(&entry.session.title, 42))
        .collect::<Vec<_>>();
    let working_directories = history
        .iter()
        .map(|entry| truncate(&entry.session.cwd, 36))
        .collect::<Vec<_>>();
    let title_width = titles
        .iter()
        .map(String::len)
        .max()
        .unwrap_or_default()
        .max("TITLE".len());
    let host_width = history
        .iter()
        .map(|entry| entry.session.host.len())
        .max()
        .unwrap_or_default()
        .max("HOST".len());
    let cwd_width = working_directories
        .iter()
        .map(String::len)
        .max()
        .unwrap_or_default()
        .max("CWD".len());

    println!(
        "{:<title_width$}  {:<host_width$}  {:<7}  {:<9}  {:<9}  {:>16}  {:<cwd_width$}  ID",
        "TITLE", "HOST", "VIEW", "STATUS", "OWNER", "LAST INTERACTION", "CWD"
    );
    for ((entry, title), cwd) in history.into_iter().zip(titles).zip(working_directories) {
        println!(
            "{:<title_width$}  {:<host_width$}  {:<7}  {:<9}  {:<9}  {:>16}  {:<cwd_width$}  {}",
            title,
            entry.session.host,
            if entry.settled { "settled" } else { "active" },
            activity_label(entry.session.activity),
            runtime_label(entry.session.runtime),
            format_age(entry.session.last_interaction_unix_seconds),
            cwd,
            entry.session.id
        );
    }
    Ok(())
}

fn history_matches(entry: &HistoryEntry, host: Option<&str>, search: Option<&str>) -> bool {
    if host.is_some_and(|host| entry.session.host != host) {
        return false;
    }
    let Some(search) = search.filter(|search| !search.trim().is_empty()) else {
        return true;
    };
    let corpus = format!(
        "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
        entry.session.title,
        entry.session.last_message,
        entry.session.cwd,
        entry.session.host,
        entry.session.id,
        entry.session.native_session_id,
        activity_label(entry.session.activity),
        runtime_label(entry.session.runtime),
        if entry.settled { "settled" } else { "active" },
        if entry.unread { "unread" } else { "read" },
        entry.settle_reason.as_deref().unwrap_or_default(),
    )
    .to_lowercase();
    search
        .split_whitespace()
        .all(|term| corpus.contains(&term.to_lowercase()))
}

fn resume(session: &str) -> Result<(), CliError> {
    agents::resume(session)?;
    Store::open()?.acknowledge(session)?;
    Ok(())
}

fn attach(session: &str) -> Result<(), CliError> {
    if is_tmux_target(session) {
        tmux::attach(session)?;
    } else {
        agents::attach(session)?;
    }
    Store::open()?.acknowledge(session)?;
    Ok(())
}

fn is_tmux_target(session: &str) -> bool {
    let fields = session.splitn(3, ':').collect::<Vec<_>>();
    matches!(fields.as_slice(), [name] if !name.is_empty())
        || matches!(
            fields.as_slice(),
            [host, "tmux", name] if !host.is_empty() && !name.is_empty()
        )
}

fn print_sessions(host: &str, limit: Option<usize>, json: bool) -> Result<(), CliError> {
    let sessions = agents::discover(host, limit)?;

    if json {
        return print_json_inventory("sessions", sessions);
    }

    if sessions.is_empty() {
        println!("No resumable coding-agent sessions were found on {host}.");
        return Ok(());
    }

    let titles = sessions
        .iter()
        .map(|session| truncate(&session.title, 42))
        .collect::<Vec<_>>();
    let working_directories = sessions
        .iter()
        .map(|session| truncate(&session.cwd, 36))
        .collect::<Vec<_>>();
    let title_width = titles
        .iter()
        .map(String::len)
        .max()
        .unwrap_or_default()
        .max("TITLE".len());
    let host_width = sessions
        .iter()
        .map(|session| session.host.len())
        .max()
        .unwrap_or_default()
        .max("HOST".len());
    let tmux_width = sessions
        .iter()
        .filter_map(|session| session.tmux.as_ref())
        .map(|binding| binding.session.len())
        .max()
        .unwrap_or_default()
        .max("TMUX".len());
    let cwd_width = working_directories
        .iter()
        .map(String::len)
        .max()
        .unwrap_or_default()
        .max("CWD".len());

    println!(
        "{:<title_width$}  {:<host_width$}  {:<9}  {:<tmux_width$}  {:>16}  {:<cwd_width$}  ID",
        "TITLE", "HOST", "STATE", "TMUX", "LAST INTERACTION", "CWD"
    );
    for ((session, title), cwd) in sessions.into_iter().zip(titles).zip(working_directories) {
        println!(
            "{:<title_width$}  {:<host_width$}  {:<9}  {:<tmux_width$}  {:>16}  {:<cwd_width$}  {}",
            title,
            session.host,
            runtime_label(session.runtime),
            session
                .tmux
                .as_ref()
                .map_or("-", |binding| binding.session.as_str()),
            format_age(session.last_interaction_unix_seconds),
            cwd,
            session.id
        );
    }

    Ok(())
}

fn runtime_label(runtime: RuntimeOwner) -> &'static str {
    match runtime {
        RuntimeOwner::Resumable => "resumable",
        RuntimeOwner::SharedBackend => "backend",
        RuntimeOwner::ExternalFrontend => "external",
        RuntimeOwner::TmuxFrontend => "tmux",
    }
}

fn activity_label(activity: Activity) -> &'static str {
    match activity {
        Activity::Working => "working",
        Activity::WaitingApproval => "approval",
        Activity::WaitingInput => "input",
        Activity::Completed => "completed",
        Activity::Failed => "failed",
        Activity::Unknown => "unknown",
    }
}

fn truncate(value: &str, maximum_characters: usize) -> String {
    let mut characters = value.chars();
    let prefix = characters
        .by_ref()
        .take(maximum_characters)
        .collect::<String>();
    if characters.next().is_some() {
        format!("{prefix}…")
    } else {
        prefix
    }
}

fn print_tmux_sessions(host: &str, json: bool) -> Result<(), CliError> {
    let sessions = tmux::discover(host)?;

    if json {
        return print_json_inventory("tmuxSessions", sessions);
    }

    if sessions.is_empty() {
        println!("No matching tmux sessions were found on {host}.");
        return Ok(());
    }

    let name_width = sessions
        .iter()
        .map(|session| session.name.len())
        .max()
        .unwrap_or_default()
        .max("SESSION".len());
    let host_width = sessions
        .iter()
        .map(|session| session.host.len())
        .max()
        .unwrap_or_default()
        .max("HOST".len());

    println!(
        "{:<name_width$}  {:<host_width$}  {:<9}  {:>7}  {:>12}  ID",
        "SESSION", "HOST", "STATE", "WINDOWS", "LAST ACTIVE"
    );
    for session in sessions {
        println!(
            "{:<name_width$}  {:<host_width$}  {:<9}  {:>7}  {:>12}  {}",
            session.name,
            session.host,
            if session.attached_clients > 0 {
                "attached"
            } else {
                "detached"
            },
            session.windows,
            format_age(session.last_activity_unix_seconds),
            session.id
        );
    }

    Ok(())
}

fn format_age(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(timestamp, |duration| duration.as_secs());
    let seconds = now.saturating_sub(timestamp);

    match seconds {
        0..=59 => format!("{seconds}s ago"),
        60..=3_599 => format!("{}m ago", seconds / 60),
        3_600..=86_399 => format!("{}h ago", seconds / 3_600),
        _ => format!("{}d ago", seconds / 86_400),
    }
}

fn print_hosts(config: Option<&std::path::Path>, json: bool) -> Result<(), CliError> {
    let hosts = hosts::discover(config)?;

    if json {
        return print_json_inventory("hosts", hosts);
    }

    if hosts.is_empty() {
        println!("No literal SSH host aliases were found.");
        return Ok(());
    }

    let targets = hosts.iter().map(render_target).collect::<Vec<_>>();
    let alias_width = hosts
        .iter()
        .map(|host| host.alias.len())
        .max()
        .unwrap_or_default()
        .max("ALIAS".len());
    let target_width = targets
        .iter()
        .map(String::len)
        .max()
        .unwrap_or_default()
        .max("TARGET".len());

    println!(
        "{:<alias_width$}  {:<target_width$}  {:<12}  SOURCE",
        "ALIAS", "TARGET", "CONNECTIVITY"
    );
    for (host, target) in hosts.iter().zip(targets) {
        let connectivity = if host.resolution_error.is_some() {
            "config-error"
        } else {
            "unknown"
        };
        println!(
            "{:<alias_width$}  {:<target_width$}  {:<12}  {}",
            host.alias, target, connectivity, host.source
        );
        if let Some(error) = &host.resolution_error {
            eprintln!("rollcall: could not resolve {}: {error}", host.alias);
        }
    }

    Ok(())
}

fn render_target(host: &SshHost) -> String {
    let user = host
        .user
        .as_deref()
        .map(|user| format!("{user}@"))
        .unwrap_or_default();
    format!("{user}{}:{}", host.hostname, host.port)
}

fn print_json_inventory<T>(kind: &str, items: T) -> Result<(), CliError>
where
    T: Serialize,
{
    println!(
        "{}",
        serde_json::to_string_pretty(&Inventory { kind, items })?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{SystemTime, UNIX_EPOCH};

    use clap::Parser;

    use super::{
        Cli, Command, activity_label, format_age, history_matches, is_tmux_target, runtime_label,
        truncate,
    };
    use crate::{
        domain::{Activity, AgentKind, RuntimeOwner, Session},
        store::HistoryEntry,
    };

    #[test]
    fn no_subcommand_uses_the_default_active_view() {
        let cli = Cli::try_parse_from(["rollcall"]).expect("default command should parse");

        assert!(cli.command.is_none());
    }

    #[test]
    fn attach_preserves_the_native_session_identifier() {
        let cli = Cli::try_parse_from(["rollcall", "attach", "topo:codex:019f"])
            .expect("attach should parse");

        match cli.command {
            Some(Command::Attach { session }) => assert_eq!(session, "topo:codex:019f"),
            _ => panic!("expected attach command"),
        }
    }

    #[test]
    fn resume_is_an_explicit_new_frontend_action() {
        let cli = Cli::try_parse_from(["rollcall", "resume", "topo:omp:019f"])
            .expect("resume should parse");
        assert!(matches!(
            cli.command,
            Some(Command::Resume { session }) if session == "topo:omp:019f"
        ));
    }

    #[test]
    fn attach_dispatch_distinguishes_raw_tmux_and_agent_ids() {
        assert!(is_tmux_target("agents"));
        assert!(is_tmux_target("topo:tmux:agents"));
        assert!(!is_tmux_target("topo:codex:019f"));
        assert!(!is_tmux_target("topo:unknown:019f"));
    }

    #[test]
    fn shell_init_accepts_optional_agent_commands() {
        let defaults =
            Cli::try_parse_from(["rollcall", "shell-init"]).expect("shell-init should parse");
        assert!(matches!(
            defaults.command,
            Some(Command::ShellInit { agents }) if agents.is_empty()
        ));

        let custom = Cli::try_parse_from(["rollcall", "shell-init", "codex", "aider"])
            .expect("custom shell-init should parse");
        assert!(matches!(
            custom.command,
            Some(Command::ShellInit { agents }) if agents == ["codex", "aider"]
        ));
    }

    #[test]
    fn hosts_accepts_an_explicit_ssh_config() {
        let cli =
            Cli::try_parse_from(["rollcall", "hosts", "--config", "/tmp/rollcall-ssh-config"])
                .expect("hosts config should parse");

        match cli.command {
            Some(Command::Hosts { config }) => {
                assert_eq!(
                    config.as_deref(),
                    Some(std::path::Path::new("/tmp/rollcall-ssh-config"))
                );
            }
            _ => panic!("expected hosts command"),
        }
    }

    #[test]
    fn list_accepts_a_remote_host() {
        let cli = Cli::try_parse_from(["rollcall", "list", "--host", "topo"])
            .expect("remote list should parse");

        match cli.command {
            Some(Command::List {
                host, limit, all, ..
            }) => {
                assert_eq!(host, "topo");
                assert_eq!(limit, None);
                assert!(!all);
            }
            _ => panic!("expected list command"),
        }
    }

    #[test]
    fn list_can_request_the_complete_inventory() {
        let cli = Cli::try_parse_from(["rollcall", "list", "--all"]).expect("--all should parse");

        match cli.command {
            Some(Command::List { all, .. }) => assert!(all),
            _ => panic!("expected list command"),
        }
    }

    #[test]
    fn history_accepts_host_search_and_limit_filters() {
        let cli = Cli::try_parse_from([
            "rollcall",
            "history",
            "--host",
            "coda",
            "--search",
            "rollcall picker",
            "--limit",
            "12",
        ])
        .expect("history filters should parse");

        assert!(matches!(
            cli.command,
            Some(Command::History {
                host: Some(host),
                search: Some(search),
                limit: Some(12),
            }) if host == "coda" && search == "rollcall picker"
        ));
    }

    #[test]
    fn raw_tmux_inventory_is_explicit() {
        let cli = Cli::try_parse_from(["rollcall", "tmux", "--host", "coda"])
            .expect("tmux command should parse");

        match cli.command {
            Some(Command::Tmux { host }) => assert_eq!(host, "coda"),
            _ => panic!("expected tmux command"),
        }
    }

    #[test]
    fn picker_accepts_an_initial_host_and_limit() {
        let cli = Cli::try_parse_from(["rollcall", "pick", "--host", "coda", "--limit", "12"])
            .expect("picker options should parse");

        match cli.command {
            Some(Command::Pick { host, limit }) => {
                assert_eq!(host, "coda");
                assert_eq!(limit, 12);
            }
            _ => panic!("expected pick command"),
        }
    }

    #[test]
    fn popup_uses_picker_defaults() {
        let cli = Cli::try_parse_from(["rollcall", "popup"]).expect("popup should parse");

        match cli.command {
            Some(Command::Popup { host, limit }) => {
                assert_eq!(host, "all");
                assert_eq!(limit, super::DEFAULT_SESSION_LIMIT);
            }
            _ => panic!("expected popup command"),
        }
    }

    #[test]
    fn watch_supports_host_limit_and_one_shot_reconciliation() {
        let cli = Cli::try_parse_from([
            "rollcall", "watch", "--host", "coda", "--limit", "12", "--once",
        ])
        .expect("watch options should parse");

        assert!(matches!(
            cli.command,
            Some(Command::Watch {
                host,
                limit: 12,
                once: true,
            }) if host == "coda"
        ));
    }

    #[test]
    fn codex_runtime_labels_distinguish_native_loading_state() {
        assert_eq!(runtime_label(RuntimeOwner::Resumable), "resumable");
        assert_eq!(runtime_label(RuntimeOwner::SharedBackend), "backend");
        assert_eq!(runtime_label(RuntimeOwner::ExternalFrontend), "external");
        assert_eq!(runtime_label(RuntimeOwner::TmuxFrontend), "tmux");
    }

    #[test]
    fn codex_activity_labels_are_compact() {
        assert_eq!(activity_label(Activity::Working), "working");
        assert_eq!(activity_label(Activity::WaitingApproval), "approval");
        assert_eq!(activity_label(Activity::WaitingInput), "input");
        assert_eq!(activity_label(Activity::Completed), "completed");
    }

    #[test]
    fn table_values_are_truncated_by_characters() {
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hellos", 5), "hello…");
        assert_eq!(truncate("你好世界", 3), "你好世…");
    }

    #[test]
    fn age_format_uses_compact_units() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow the Unix epoch")
            .as_secs();

        assert_eq!(format_age(now.saturating_sub(5)), "5s ago");
        assert_eq!(format_age(now.saturating_sub(120)), "2m ago");
        assert_eq!(format_age(now.saturating_sub(7_200)), "2h ago");
        assert_eq!(format_age(now.saturating_sub(172_800)), "2d ago");
    }

    #[test]
    fn history_search_matches_all_terms_across_cached_metadata() {
        let entry = HistoryEntry {
            session: Session {
                id: "topo:codex:019f".to_owned(),
                host: "topo".to_owned(),
                agent: AgentKind::Codex,
                native_session_id: "019f".to_owned(),
                title: "Rollcall picker".to_owned(),
                cwd: "/home/fin/projects/rollcall".to_owned(),
                source: "cli".to_owned(),
                activity: Activity::Completed,
                last_message: "Ownership boundary is ready.".to_owned(),
                last_interaction_unix_seconds: 100,
                updated_unix_seconds: 100,
                runtime: RuntimeOwner::Resumable,
                tmux: None,
            },
            settled: true,
            unread: false,
            notification_kind: None,
            notification_at_unix_seconds: None,
            settled_at_unix_seconds: Some(200),
            settle_reason: Some("manual".to_owned()),
            last_seen_unix_seconds: 200,
        };

        assert!(history_matches(
            &entry,
            Some("topo"),
            Some("rollcall ownership settled")
        ));
        assert!(!history_matches(&entry, Some("coda"), None));
        assert!(!history_matches(&entry, None, Some("rollcall missing")));
    }
}
