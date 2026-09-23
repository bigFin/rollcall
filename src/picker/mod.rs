use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fmt,
    io::{self, Write},
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use crate::{
    agents::{self, AgentError},
    domain::{LiveObservation, Session},
    hosts::HostDiscoveryError,
    reconnect::HostReconnectState,
    store::{SessionTransition, Store, StoreError},
};

mod dashboard;
mod input;
mod refresh;
mod view;

#[cfg(test)]
mod tests;

use refresh::{
    discovery_targets, normalize_host_filter, reconnect_jitter_seed, restrict_discovery_targets,
};

const ALL_HOSTS: &str = "all";
const LOCAL_ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const REMOTE_ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_HOST_REFRESHES: usize = 4;
const NOTICE_DURATION: Duration = Duration::from_secs(8);
const PREVIEW_CAPTURE_LINES: usize = 120;
const DAY_SECONDS: u64 = 24 * 60 * 60;
const WEEK_SECONDS: u64 = 7 * DAY_SECONDS;

#[derive(Debug)]
pub enum PickerError {
    Agent(AgentError),
    HostDiscovery(HostDiscoveryError),
    Io(io::Error),
    PopupFailed(String),
    Store(StoreError),
}

impl fmt::Display for PickerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Agent(error) => write!(formatter, "{error}"),
            Self::HostDiscovery(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "terminal picker failed: {error}"),
            Self::PopupFailed(message) => write!(formatter, "tmux popup failed: {message}"),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<AgentError> for PickerError {
    fn from(error: AgentError) -> Self {
        Self::Agent(error)
    }
}

impl From<HostDiscoveryError> for PickerError {
    fn from(error: HostDiscoveryError) -> Self {
        Self::HostDiscovery(error)
    }
}

impl From<io::Error> for PickerError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<StoreError> for PickerError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

pub fn run(initial_host: &str, limit: usize) -> Result<(), PickerError> {
    view::theme::init()?;
    let mut app = PickerApp::new(initial_host, limit)?;
    let selection = run_terminal(&mut app)?;
    if let Some(session_id) = selection {
        agents::attach(&session_id)?;
        app.acknowledge_session(&session_id, false)?;
    }
    Ok(())
}

pub fn watch(initial_host: &str, limit: usize, once: bool, json: bool) -> Result<(), PickerError> {
    let (transition_tx, transition_rx) = mpsc::channel();
    let mut app = PickerApp::new_watch(initial_host, limit, transition_tx)?;
    let mut stdout = io::stdout().lock();

    loop {
        app.drain_refresh_events()?;
        while let Ok(transition) = transition_rx.try_recv() {
            write_transition(&mut stdout, &transition, json)?;
        }
        app.maybe_start_host_refreshes();
        if once && app.initial_refresh_complete() {
            stdout.flush()?;
            return Ok(());
        }
        app.maybe_start_activity_poll();
        thread::sleep(Duration::from_millis(250));
    }
}

pub fn popup(initial_host: &str, limit: usize) -> Result<(), PickerError> {
    let theme = view::theme::init()?;
    if env::var_os("TMUX").is_none() {
        return run(initial_host, limit);
    }

    let executable = env::current_exe()?;
    let executable = executable.to_string_lossy().into_owned();
    let limit = limit.to_string();
    let picker_command = shlex::try_join([
        executable.as_str(),
        "pick",
        "--host",
        initial_host,
        "--limit",
        limit.as_str(),
    ])
    .map_err(|error| PickerError::PopupFailed(error.to_string()))?;
    let status = Command::new("tmux")
        .args([
            "display-popup",
            "-E",
            "-e",
            &format!("ROLLCALL_THEME={theme}"),
            "-w",
            "90%",
            "-h",
            "80%",
            &picker_command,
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()?;

    if status.success() {
        Ok(())
    } else {
        Err(PickerError::PopupFailed(status.to_string()))
    }
}

fn run_terminal(app: &mut PickerApp) -> Result<Option<String>, PickerError> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    if let Err(error) = execute!(stdout, EnterAlternateScreen) {
        let _ = disable_raw_mode();
        return Err(error.into());
    }
    let backend = CrosstermBackend::new(stdout);
    let result = match Terminal::new(backend) {
        Ok(mut terminal) => {
            let result = event_loop(&mut terminal, app);
            let _ = terminal.show_cursor();
            result
        }
        Err(error) => Err(PickerError::Io(error)),
    };

    let raw_result = disable_raw_mode();
    let screen_result = execute!(io::stdout(), LeaveAlternateScreen);
    let selection = result?;
    raw_result?;
    screen_result?;
    Ok(selection)
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut PickerApp,
) -> Result<Option<String>, PickerError> {
    loop {
        app.drain_refresh_events()?;
        app.expire_notice();
        app.maybe_start_host_refreshes();
        app.maybe_start_activity_poll();
        terminal.draw(|frame| view::draw(frame, app))?;
        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match app.handle_key(key) {
            Action::None => {}
            Action::Quit => return Ok(None),
            Action::Attach(session_id) => return Ok(Some(session_id)),
            Action::Refresh => {
                app.start_refresh();
            }
            Action::Acknowledge(session_id) => {
                app.acknowledge_session(&session_id, true)?;
            }
            Action::SetArchived {
                session_id,
                archived,
                attach,
            } => {
                app.set_archived(&session_id, archived)?;
                if attach {
                    return Ok(Some(session_id));
                }
            }
            Action::SetHostFilter(host) => app.set_host_filter(host),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum DashboardSection {
    Current,
    LastDay,
    LastWeek,
    Archive,
}

impl DashboardSection {
    const ALL: [Self; 4] = [Self::Current, Self::LastDay, Self::LastWeek, Self::Archive];

    const fn label(self) -> &'static str {
        match self {
            Self::Current => "Currently active",
            Self::LastDay => "Last day",
            Self::LastWeek => "Last week",
            Self::Archive => "Archive",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRef {
    Active(usize),
    Settled(usize),
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
enum DashboardGroupKey {
    Host {
        section: DashboardSection,
        host: String,
    },
    Project {
        section: DashboardSection,
        host: String,
        cwd: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DashboardSelection {
    Section(DashboardSection),
    Group(DashboardGroupKey),
    Session(String),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputMode {
    Browse,
    Search,
    Hosts,
    Help,
    Preview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GroupConnectivity {
    Online,
    Checking,
    Offline(Option<u64>),
    Blocked,
    Cached,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum DashboardRow {
    Section {
        section: DashboardSection,
        count: usize,
        expanded: bool,
    },
    Host {
        section: DashboardSection,
        host: String,
        count: usize,
        fresh: bool,
        expanded: bool,
    },
    Group {
        section: DashboardSection,
        cwd: String,
        host: String,
        count: usize,
        fresh: bool,
        expanded: bool,
    },
    Session(SessionRef),
}

#[derive(Debug, Eq, PartialEq)]
enum Action {
    None,
    Quit,
    Attach(String),
    Acknowledge(String),
    Refresh,
    SetArchived {
        session_id: String,
        archived: bool,
        attach: bool,
    },
    SetHostFilter(String),
}

#[derive(Debug)]
enum RefreshEvent {
    Basic {
        generation: u64,
        target: String,
        sessions: Vec<Session>,
    },
    Detailed {
        generation: u64,
        target: String,
        sessions: Vec<Session>,
    },
    Failed {
        generation: u64,
        target: String,
        error: String,
    },
    DetailFailed {
        generation: u64,
        target: String,
    },
    Activity {
        generation: u64,
        target: String,
        observations: Result<BTreeMap<String, LiveObservation>, String>,
    },
    Preview {
        request_id: u64,
        session_id: String,
        result: Result<String, String>,
    },
}

#[derive(Debug)]
enum PreviewContent {
    Loading,
    Ready { source: &'static str, body: String },
    Failed { error: String, fallback: String },
}

#[derive(Debug)]
struct SessionPreview {
    request_id: u64,
    session_id: String,
    title: String,
    host: String,
    scroll_from_bottom: usize,
    notice: Option<String>,
    content: PreviewContent,
}

struct PickerApp {
    store: Store,
    active: Vec<Session>,
    settled: Vec<Session>,
    rows: Vec<DashboardRow>,
    query: String,
    selected_row: usize,
    input_mode: InputMode,
    expanded_sections: BTreeSet<DashboardSection>,
    collapsed_groups: HashSet<DashboardGroupKey>,
    show_details: bool,
    host_filter: String,
    host_choices: Vec<String>,
    host_selected: usize,
    discovery_targets: Vec<String>,
    pending_hosts: BTreeSet<String>,
    reachable_hosts: BTreeSet<String>,
    enriching_hosts: BTreeSet<String>,
    host_errors: BTreeMap<String, String>,
    fresh_ids: HashSet<String>,
    unread_ids: HashSet<String>,
    transition_tx: Option<Sender<SessionTransition>>,
    initial_targets_remaining: BTreeSet<String>,
    refresh_generation: u64,
    refresh_tx: Sender<RefreshEvent>,
    refresh_rx: Receiver<RefreshEvent>,
    reconnect: BTreeMap<String, HostReconnectState>,
    reconnect_jitter_seed: u64,
    activity_pending: BTreeSet<String>,
    last_activity_poll: BTreeMap<String, Instant>,
    limit: usize,
    status: Option<String>,
    notice: Option<(String, Instant)>,
    preview_request_id: u64,
    preview: Option<SessionPreview>,
}

impl PickerApp {
    fn new(initial_host: &str, limit: usize) -> Result<Self, PickerError> {
        Self::new_inner(initial_host, limit, None, false)
    }

    fn new_watch(
        initial_host: &str,
        limit: usize,
        transition_tx: Sender<SessionTransition>,
    ) -> Result<Self, PickerError> {
        Self::new_inner(initial_host, limit, Some(transition_tx), true)
    }

    fn new_inner(
        initial_host: &str,
        limit: usize,
        transition_tx: Option<Sender<SessionTransition>>,
        restrict_targets: bool,
    ) -> Result<Self, PickerError> {
        let mut discovery_targets = discovery_targets()?;
        if restrict_targets {
            discovery_targets = restrict_discovery_targets(discovery_targets, initial_host);
        }
        let now = Instant::now();
        let reconnect = discovery_targets
            .iter()
            .cloned()
            .map(|target| (target, HostReconnectState::new(now)))
            .collect();
        let initial_targets_remaining = discovery_targets.iter().cloned().collect();
        let store = Store::open()?;
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let host_filter = normalize_host_filter(initial_host);
        let mut app = Self {
            store,
            active: Vec::new(),
            rows: Vec::new(),
            query: String::new(),
            settled: Vec::new(),
            selected_row: 0,
            input_mode: InputMode::Browse,
            expanded_sections: BTreeSet::from([
                DashboardSection::Current,
                DashboardSection::LastDay,
                DashboardSection::LastWeek,
            ]),
            collapsed_groups: HashSet::new(),
            show_details: false,
            host_filter,
            host_choices: vec![ALL_HOSTS.to_owned()],
            host_selected: 0,
            discovery_targets,
            pending_hosts: BTreeSet::new(),
            reachable_hosts: BTreeSet::new(),
            enriching_hosts: BTreeSet::new(),
            host_errors: BTreeMap::new(),
            fresh_ids: HashSet::new(),
            unread_ids: HashSet::new(),
            transition_tx,
            initial_targets_remaining,
            refresh_generation: 0,
            refresh_tx,
            refresh_rx,
            reconnect,
            reconnect_jitter_seed: reconnect_jitter_seed(),
            activity_pending: BTreeSet::new(),
            last_activity_poll: BTreeMap::new(),
            limit,
            status: None,
            notice: None,
            preview_request_id: 0,
            preview: None,
        };
        app.reload_snapshots()?;
        app.rebuild_host_choices();
        app.rebuild_rows();
        app.start_refresh();
        Ok(app)
    }
}

fn write_transition(
    writer: &mut impl Write,
    transition: &SessionTransition,
    json: bool,
) -> io::Result<()> {
    if json {
        let line = serde_json::to_string(transition).map_err(io::Error::other)?;
        writeln!(writer, "{line}")
    } else {
        writeln!(
            writer,
            "{}\t{}\t{}\t{}\t{}",
            transition.observed_at_unix_seconds,
            transition.kind.as_str(),
            transition.host,
            transition.title.replace(['\t', '\n', '\r'], " "),
            transition.session_id
        )
    }
}
