use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    env, fmt,
    io::{self, Write},
    path::Path,
    process::{Command, Stdio},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    codex::{self, CodexActivity, CodexError, CodexRuntime, CodexSession},
    hosts::{self, HostDiscoveryError, SshHost},
    notify,
    reconnect::{HostPhase, HostReconnectState, classify_failure},
    store::{DEFAULT_STALE_AFTER_SECONDS, SessionTransition, Store, StoreError},
    tmux,
};

const ALL_HOSTS: &str = "all";
const LOCAL_ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(2);
const REMOTE_ACTIVITY_POLL_INTERVAL: Duration = Duration::from_secs(5);
const MAX_CONCURRENT_HOST_REFRESHES: usize = 4;
const NOTICE_DURATION: Duration = Duration::from_secs(8);
const PREVIEW_CAPTURE_LINES: usize = 120;

#[derive(Debug)]
pub enum PickerError {
    Codex(CodexError),
    HostDiscovery(HostDiscoveryError),
    Io(io::Error),
    PopupFailed(String),
    Store(StoreError),
}

impl fmt::Display for PickerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Codex(error) => write!(formatter, "{error}"),
            Self::HostDiscovery(error) => write!(formatter, "{error}"),
            Self::Io(error) => write!(formatter, "terminal picker failed: {error}"),
            Self::PopupFailed(message) => write!(formatter, "tmux popup failed: {message}"),
            Self::Store(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<CodexError> for PickerError {
    fn from(error: CodexError) -> Self {
        Self::Codex(error)
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
    let mut app = PickerApp::new(initial_host, limit)?;
    let selection = run_terminal(&mut app)?;
    if let Some(session_id) = selection {
        codex::attach(&session_id)?;
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
        terminal.draw(|frame| draw(frame, app))?;
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputMode {
    Browse,
    Search,
    Hosts,
    Help,
    Preview,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DashboardView {
    Active,
    Settled,
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
    Group {
        cwd: String,
        host: String,
        count: usize,
        fresh: bool,
    },
    Session(usize),
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
        sessions: Vec<CodexSession>,
    },
    Detailed {
        generation: u64,
        target: String,
        sessions: Vec<CodexSession>,
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
        observations: Result<BTreeMap<String, codex::LiveObservation>, String>,
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
    active: Vec<CodexSession>,
    settled: Vec<CodexSession>,
    rows: Vec<DashboardRow>,
    query: String,
    selected_row: usize,
    input_mode: InputMode,
    view: DashboardView,
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
            settled: Vec::new(),
            rows: Vec::new(),
            query: String::new(),
            selected_row: 0,
            input_mode: InputMode::Browse,
            view: DashboardView::Active,
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

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        match self.input_mode {
            InputMode::Browse => self.handle_browse_key(key),
            InputMode::Search => self.handle_search_key(key),
            InputMode::Hosts => self.handle_host_key(key),
            InputMode::Help => self.handle_help_key(key),
            InputMode::Preview => self.handle_preview_key(key),
        }
    }

    fn handle_browse_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Quit,
            KeyCode::Char('/') => {
                self.input_mode = InputMode::Search;
                self.status = None;
                Action::None
            }
            KeyCode::Char('h') => {
                self.input_mode = InputMode::Hosts;
                self.status = None;
                Action::None
            }
            KeyCode::Char('?') => {
                self.input_mode = InputMode::Help;
                self.status = None;
                Action::None
            }
            KeyCode::Char('i') => {
                self.show_details = !self.show_details;
                self.status = None;
                Action::None
            }
            KeyCode::Char('p') => {
                self.start_preview();
                Action::None
            }
            KeyCode::Tab => {
                self.toggle_view();
                Action::None
            }
            KeyCode::Char('r') => Action::Refresh,
            KeyCode::Char('a') => self.archive_action(false),
            KeyCode::Char('x') => self.acknowledge_action(),
            KeyCode::Char('j') | KeyCode::Down => {
                self.status = None;
                self.select_next();
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.status = None;
                self.select_previous();
                Action::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                self.status = None;
                self.select_first();
                Action::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                self.status = None;
                self.select_last();
                Action::None
            }
            KeyCode::Enter => self.selected_action(),
            _ => Action::None,
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.input_mode = InputMode::Browse;
                Action::None
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.rebuild_rows();
                Action::None
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.query.clear();
                self.rebuild_rows();
                Action::None
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(character);
                self.rebuild_rows();
                Action::None
            }
            _ => Action::None,
        }
    }

    fn handle_host_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.input_mode = InputMode::Browse;
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.host_choices.is_empty() {
                    self.host_selected = (self.host_selected + 1).min(self.host_choices.len() - 1);
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.host_selected = self.host_selected.saturating_sub(1);
                Action::None
            }
            KeyCode::Enter => self
                .host_choices
                .get(self.host_selected)
                .cloned()
                .map_or(Action::None, Action::SetHostFilter),
            _ => Action::None,
        }
    }

    fn handle_help_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q' | '?') => {
                self.input_mode = InputMode::Browse;
            }
            _ => {}
        }
        Action::None
    }

    fn handle_preview_key(&mut self, key: KeyEvent) -> Action {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'p') => {
                self.close_preview();
                Action::None
            }
            KeyCode::Char('j') | KeyCode::Down | KeyCode::PageDown => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = preview
                        .scroll_from_bottom
                        .saturating_sub(if key.code == KeyCode::PageDown { 10 } else { 1 });
                }
                Action::None
            }
            KeyCode::Char('k') | KeyCode::Up | KeyCode::PageUp => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = preview
                        .scroll_from_bottom
                        .saturating_add(if key.code == KeyCode::PageUp { 10 } else { 1 });
                }
                Action::None
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = usize::MAX;
                }
                Action::None
            }
            KeyCode::Char('G') | KeyCode::End => {
                if let Some(preview) = self.preview.as_mut() {
                    preview.scroll_from_bottom = 0;
                }
                Action::None
            }
            KeyCode::Char('a') => {
                let Some(session_id) = self
                    .preview
                    .as_ref()
                    .map(|preview| preview.session_id.clone())
                else {
                    return Action::None;
                };
                let Some(archived) = self.archived_state_for_session(&session_id) else {
                    self.close_preview();
                    self.status =
                        Some("That session is no longer in the cached inventory.".to_owned());
                    return Action::None;
                };
                self.close_preview();
                Action::SetArchived {
                    session_id,
                    archived: !archived,
                    attach: false,
                }
            }
            KeyCode::Char('x') => self
                .preview
                .as_ref()
                .map(|preview| Action::Acknowledge(preview.session_id.clone()))
                .unwrap_or(Action::None),
            KeyCode::Enter => {
                let Some(session_id) = self
                    .preview
                    .as_ref()
                    .map(|preview| preview.session_id.clone())
                else {
                    return Action::None;
                };
                self.action_for_session(&session_id)
            }
            _ => Action::None,
        }
    }

    fn selected_action(&mut self) -> Action {
        let Some(session_id) = self.selected_session().map(|session| session.id.clone()) else {
            return Action::None;
        };
        self.action_for_session(&session_id)
    }

    fn action_for_session(&mut self, session_id: &str) -> Action {
        let Some((session, settled)) = self
            .active
            .iter()
            .map(|session| (session, false))
            .chain(self.settled.iter().map(|session| (session, true)))
            .find(|(session, _)| session.id == session_id)
        else {
            self.close_preview();
            self.status = Some("That session is no longer in the cached inventory.".to_owned());
            return Action::None;
        };
        if session.runtime == CodexRuntime::ExternalFrontend && self.fresh_ids.contains(&session.id)
        {
            let message =
                "Owned by an external frontend; close or release it before resuming here."
                    .to_owned();
            if let Some(preview) = self.preview.as_mut() {
                preview.notice = Some(message);
            } else {
                self.status = Some(message);
            }
            return Action::None;
        }

        if settled {
            Action::SetArchived {
                session_id: session.id.clone(),
                archived: false,
                attach: true,
            }
        } else {
            Action::Attach(session.id.clone())
        }
    }

    fn archived_state_for_session(&self, session_id: &str) -> Option<bool> {
        self.active
            .iter()
            .any(|session| session.id == session_id)
            .then_some(false)
            .or_else(|| {
                self.settled
                    .iter()
                    .any(|session| session.id == session_id)
                    .then_some(true)
            })
    }

    fn start_preview(&mut self) {
        let Some(session) = self.selected_session().cloned() else {
            return;
        };
        self.preview_request_id = self.preview_request_id.wrapping_add(1);
        let request_id = self.preview_request_id;
        let fallback = cached_preview(&session);
        self.status = None;
        self.input_mode = InputMode::Preview;

        let can_capture = self.fresh_ids.contains(&session.id)
            && session.runtime == CodexRuntime::TmuxFrontend
            && session.tmux.is_some();
        let content = if can_capture {
            PreviewContent::Loading
        } else {
            PreviewContent::Ready {
                source: cached_preview_source(&session),
                body: fallback.clone(),
            }
        };
        self.preview = Some(SessionPreview {
            request_id,
            session_id: session.id.clone(),
            title: session.title.clone(),
            host: session.host.clone(),
            scroll_from_bottom: 0,
            notice: None,
            content,
        });

        if let Some(binding) = session.tmux {
            if !can_capture {
                return;
            }
            let sender = self.refresh_tx.clone();
            let host = session.host;
            let session_id = session.id;
            thread::spawn(move || {
                let result = tmux::capture_pane(
                    &host,
                    &binding.session,
                    &binding.pane,
                    PREVIEW_CAPTURE_LINES,
                )
                .map_err(|error| error.to_string());
                let _ = sender.send(RefreshEvent::Preview {
                    request_id,
                    session_id,
                    result,
                });
            });
        }
    }

    fn close_preview(&mut self) {
        self.preview = None;
        self.input_mode = InputMode::Browse;
    }

    fn archive_action(&self, attach: bool) -> Action {
        self.selected_session()
            .map_or(Action::None, |session| Action::SetArchived {
                session_id: session.id.clone(),
                archived: self.view == DashboardView::Active,
                attach,
            })
    }

    fn acknowledge_action(&self) -> Action {
        self.selected_session()
            .map(|session| Action::Acknowledge(session.id.clone()))
            .unwrap_or(Action::None)
    }

    fn toggle_view(&mut self) {
        self.view = match self.view {
            DashboardView::Active => DashboardView::Settled,
            DashboardView::Settled => DashboardView::Active,
        };
        self.status = None;
        self.rebuild_rows();
    }

    fn set_host_filter(&mut self, host: String) {
        self.host_filter = host;
        self.host_selected = self
            .host_choices
            .iter()
            .position(|candidate| candidate == &self.host_filter)
            .unwrap_or_default();
        self.input_mode = InputMode::Browse;
        self.status = None;
        if self.host_filter != ALL_HOSTS
            && let Some(target) = self.target_for_observed_host(&self.host_filter).cloned()
        {
            self.request_host_now(&target);
        }
        self.rebuild_rows();
    }

    fn start_refresh(&mut self) {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        self.enriching_hosts.clear();
        self.activity_pending.clear();
        self.pending_hosts.clear();
        self.last_activity_poll.clear();
        let now = Instant::now();
        for state in self.reconnect.values_mut() {
            state.request_now(now);
        }
        self.status = Some("Refreshing configured hosts…".to_owned());
        self.maybe_start_host_refreshes();
    }

    fn request_host_now(&mut self, target: &str) {
        if self.pending_hosts.contains(target) || self.enriching_hosts.contains(target) {
            return;
        }
        if let Some(state) = self.reconnect.get_mut(target) {
            state.request_now(Instant::now());
        }
    }

    fn maybe_start_host_refreshes(&mut self) {
        let capacity = MAX_CONCURRENT_HOST_REFRESHES.saturating_sub(self.pending_hosts.len());
        if capacity == 0 {
            return;
        }

        let now = Instant::now();
        let mut due = self
            .discovery_targets
            .iter()
            .filter(|target| {
                !self.pending_hosts.contains(*target)
                    && !self.enriching_hosts.contains(*target)
                    && self
                        .reconnect
                        .get(*target)
                        .is_some_and(|state| state.is_due(now))
            })
            .cloned()
            .collect::<Vec<_>>();
        due.sort_by_key(|target| (!self.is_important_target(target), target.clone()));

        for target in due.into_iter().take(capacity) {
            self.start_host_refresh(target);
        }
        self.update_refresh_status();
    }

    fn start_host_refresh(&mut self, target: String) {
        let Some(state) = self.reconnect.get_mut(&target) else {
            return;
        };
        state.begin_attempt();
        self.pending_hosts.insert(target.clone());
        let generation = self.refresh_generation;
        let sender = self.refresh_tx.clone();
        let limit = self.limit;
        let observed_host = codex::observed_host(&target);
        let cached = self
            .active
            .iter()
            .chain(&self.settled)
            .filter(|session| session.host == observed_host)
            .map(|session| {
                (
                    session.native_session_id.clone(),
                    (
                        session.last_interaction_unix_seconds,
                        session.updated_unix_seconds,
                        !session.last_message.is_empty(),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();
        thread::spawn(move || match codex::discover(&target, Some(limit)) {
            Ok(sessions) => {
                if sender
                    .send(RefreshEvent::Basic {
                        generation,
                        target: target.clone(),
                        sessions: sessions.clone(),
                    })
                    .is_err()
                {
                    return;
                }

                let mut detailed = sessions
                    .into_iter()
                    .filter(|session| session_needs_detail(session, &cached))
                    .collect::<Vec<_>>();
                if detailed.is_empty() || codex::enrich_details(&target, &mut detailed).is_ok() {
                    let _ = sender.send(RefreshEvent::Detailed {
                        generation,
                        target,
                        sessions: detailed,
                    });
                } else {
                    let _ = sender.send(RefreshEvent::DetailFailed { generation, target });
                }
            }
            Err(error) => {
                let _ = sender.send(RefreshEvent::Failed {
                    generation,
                    target,
                    error: error.to_string(),
                });
            }
        });
    }

    fn maybe_start_activity_poll(&mut self) {
        let now = Instant::now();
        let targets = self
            .discovery_targets
            .iter()
            .filter(|target| {
                if self.activity_pending.contains(*target)
                    || self.pending_hosts.contains(*target)
                    || self.host_errors.contains_key(*target)
                {
                    return false;
                }
                let interval = if target.as_str() == "local" {
                    LOCAL_ACTIVITY_POLL_INTERVAL
                } else {
                    REMOTE_ACTIVITY_POLL_INTERVAL
                };
                if self
                    .last_activity_poll
                    .get(*target)
                    .is_some_and(|started| started.elapsed() < interval)
                {
                    return false;
                }
                let observed_host = codex::observed_host(target);
                self.active.iter().chain(&self.settled).any(|session| {
                    session.host == observed_host
                        && self.fresh_ids.contains(&session.id)
                        && session.runtime != CodexRuntime::Resumable
                })
            })
            .cloned()
            .collect::<Vec<_>>();

        for target in targets {
            self.last_activity_poll.insert(target.clone(), now);
            self.activity_pending.insert(target.clone());
            let sender = self.refresh_tx.clone();
            let generation = self.refresh_generation;
            thread::spawn(move || {
                let observations = codex::observe_live(&target).map_err(|error| error.to_string());
                let _ = sender.send(RefreshEvent::Activity {
                    generation,
                    target,
                    observations,
                });
            });
        }
    }

    fn drain_refresh_events(&mut self) -> Result<(), PickerError> {
        while let Ok(event) = self.refresh_rx.try_recv() {
            self.apply_refresh_event(event)?;
        }
        Ok(())
    }

    fn apply_refresh_event(&mut self, event: RefreshEvent) -> Result<(), PickerError> {
        let selected_session_id = self.selected_session().map(|session| session.id.clone());
        match event {
            RefreshEvent::Basic {
                generation,
                target,
                mut sessions,
            } if generation == self.refresh_generation => {
                self.pending_hosts.remove(&target);
                self.initial_targets_remaining.remove(&target);
                self.host_errors.remove(&target);
                self.reachable_hosts.insert(target.clone());
                self.enriching_hosts.insert(target.clone());
                self.last_activity_poll.remove(&target);
                let important = self.is_important_target(&target);
                let recovered = self
                    .reconnect
                    .get_mut(&target)
                    .is_some_and(|state| state.succeeded(Instant::now(), important));
                self.mark_host_cached(&target);
                self.preserve_cached_messages(&mut sessions);
                self.record_fresh_sessions(&sessions)?;
                if recovered {
                    let observed_host = codex::observed_host(&target);
                    self.notice = Some((
                        format!(
                            "{} is back online · restored {} sessions",
                            observed_host,
                            sessions.len()
                        ),
                        Instant::now(),
                    ));
                    notify::host_recovered(&observed_host, sessions.len());
                }
            }
            RefreshEvent::Detailed {
                generation,
                target,
                mut sessions,
            } if generation == self.refresh_generation => {
                self.enriching_hosts.remove(&target);
                self.preserve_live_state(&mut sessions);
                self.record_fresh_sessions(&sessions)?;
            }
            RefreshEvent::Failed {
                generation,
                target,
                error,
            } if generation == self.refresh_generation => {
                self.pending_hosts.remove(&target);
                self.initial_targets_remaining.remove(&target);
                self.enriching_hosts.remove(&target);
                self.reachable_hosts.remove(&target);
                self.activity_pending.remove(&target);
                let important = self.is_important_target(&target);
                let class = classify_failure(&error);
                if let Some(state) = self.reconnect.get_mut(&target) {
                    state.failed(
                        Instant::now(),
                        &target,
                        important,
                        self.reconnect_jitter_seed,
                        class,
                    );
                }
                self.mark_host_cached(&target);
                self.host_errors.insert(target, error);
            }
            RefreshEvent::DetailFailed { generation, target }
                if generation == self.refresh_generation =>
            {
                self.enriching_hosts.remove(&target);
            }
            RefreshEvent::Activity {
                generation,
                target,
                observations,
            } if generation == self.refresh_generation => {
                self.activity_pending.remove(&target);
                if let Ok(observations) = observations {
                    self.apply_live_observations(&target, &observations)?;
                }
                return Ok(());
            }
            RefreshEvent::Preview {
                request_id,
                session_id,
                result,
            } => {
                if let Some(preview) = self.preview.as_mut()
                    && preview.request_id == request_id
                    && preview.session_id == session_id
                {
                    let fallback = self
                        .active
                        .iter()
                        .chain(&self.settled)
                        .find(|session| session.id == session_id)
                        .map(cached_preview)
                        .unwrap_or_else(|| "No cached response is available.".to_owned());
                    preview.content = match result {
                        Ok(body) if !body.trim().is_empty() => PreviewContent::Ready {
                            source: "live tmux pane",
                            body,
                        },
                        Ok(_) => PreviewContent::Ready {
                            source: "cached response",
                            body: fallback,
                        },
                        Err(error) => PreviewContent::Failed { error, fallback },
                    };
                    preview.scroll_from_bottom = 0;
                }
                return Ok(());
            }
            _ => return Ok(()),
        }

        self.reload_snapshots()?;
        self.rebuild_host_choices();
        self.rebuild_rows_selecting(selected_session_id.as_deref());
        self.update_refresh_status();
        Ok(())
    }

    fn apply_live_observations(
        &mut self,
        target: &str,
        observations: &BTreeMap<String, codex::LiveObservation>,
    ) -> Result<(), PickerError> {
        let selected_session_id = self.selected_session().map(|session| session.id.clone());
        let observed_host = codex::observed_host(target);
        let mut updates = Vec::new();

        for session in self.active.iter_mut().chain(self.settled.iter_mut()) {
            if session.host != observed_host || !self.fresh_ids.contains(&session.id) {
                continue;
            }
            let before = (session.activity, session.runtime, session.tmux.clone());
            if let Some(observation) = observations.get(&session.native_session_id) {
                session.runtime = match &observation.owner {
                    codex::LiveOwner::Tmux(binding) => {
                        session.tmux = Some(binding.clone());
                        CodexRuntime::TmuxFrontend
                    }
                    codex::LiveOwner::SharedBackend => {
                        session.tmux = None;
                        CodexRuntime::SharedBackend
                    }
                    codex::LiveOwner::ExternalFrontend => {
                        session.tmux = None;
                        CodexRuntime::ExternalFrontend
                    }
                };
                session.activity =
                    codex::merge_observed_activity(session.activity, observation.activity);
            } else if session.runtime != CodexRuntime::Resumable {
                session.runtime = CodexRuntime::Resumable;
                session.tmux = None;
                if session.activity == CodexActivity::Working {
                    session.activity = CodexActivity::Completed;
                }
            }
            if before != (session.activity, session.runtime, session.tmux.clone()) {
                updates.push(session.clone());
            }
        }

        if !updates.is_empty() {
            self.record_sessions(&updates)?;
            self.rebuild_rows_selecting(selected_session_id.as_deref());
        }
        Ok(())
    }

    fn record_fresh_sessions(&mut self, sessions: &[CodexSession]) -> Result<(), PickerError> {
        self.fresh_ids
            .extend(sessions.iter().map(|session| session.id.clone()));
        self.record_sessions(sessions)?;
        Ok(())
    }

    fn record_sessions(&mut self, sessions: &[CodexSession]) -> Result<(), PickerError> {
        let transitions = self.store.record(sessions)?;
        self.handle_transitions(&transitions);
        Ok(())
    }

    fn handle_transitions(&mut self, transitions: &[SessionTransition]) {
        if transitions.is_empty() {
            return;
        }
        for transition in transitions {
            self.unread_ids.insert(transition.session_id.clone());
            notify::session_transition(transition);
            if let Some(sender) = &self.transition_tx {
                let _ = sender.send(transition.clone());
            }
        }
        self.notice = if let [transition] = transitions {
            Some((
                format!(
                    "{} · {} on {}",
                    transition.kind.as_str(),
                    transition.title,
                    transition.host
                ),
                Instant::now(),
            ))
        } else {
            Some((
                format!("{} sessions need attention", transitions.len()),
                Instant::now(),
            ))
        };
    }

    fn preserve_cached_messages(&self, sessions: &mut [CodexSession]) {
        for session in sessions {
            if !session.last_message.is_empty() {
                continue;
            }
            if let Some(cached) = self
                .active
                .iter()
                .chain(&self.settled)
                .find(|cached| cached.id == session.id)
            {
                session.last_message.clone_from(&cached.last_message);
            }
        }
    }

    fn preserve_live_state(&self, sessions: &mut [CodexSession]) {
        for session in sessions {
            let Some(current) = self
                .active
                .iter()
                .chain(&self.settled)
                .find(|current| current.id == session.id)
            else {
                continue;
            };
            if current.runtime != CodexRuntime::Resumable {
                session.runtime = current.runtime;
                session.tmux.clone_from(&current.tmux);
                session.activity = current.activity;
            }
        }
    }

    fn mark_host_cached(&mut self, target: &str) {
        let observed_host = codex::observed_host(target);
        for session in self.active.iter_mut().chain(self.settled.iter_mut()) {
            if session.host == observed_host {
                self.fresh_ids.remove(&session.id);
                session.runtime = CodexRuntime::Resumable;
                session.tmux = None;
            }
        }
    }

    fn update_refresh_status(&mut self) {
        self.status = Some(if !self.pending_hosts.is_empty() {
            format!(
                "Checking {} hosts · cached sessions are ready",
                self.pending_hosts.len()
            )
        } else if !self.enriching_hosts.is_empty() {
            format!(
                "{} hosts reachable · loading message details from {}",
                self.reachable_hosts.len(),
                self.enriching_hosts.len()
            )
        } else if self.host_errors.is_empty() {
            format!(
                "Loaded {} active and {} settled sessions from {} hosts.",
                self.active.len(),
                self.settled.len(),
                self.reachable_hosts.len()
            )
        } else {
            format!(
                "Loaded {} active · {} reachable · {} unavailable · retries are automatic",
                self.active.len(),
                self.reachable_hosts.len(),
                self.host_errors.len()
            )
        });
    }

    fn expire_notice(&mut self) {
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, shown_at)| shown_at.elapsed() >= NOTICE_DURATION)
        {
            self.notice = None;
        }
    }

    fn initial_refresh_complete(&self) -> bool {
        self.initial_targets_remaining.is_empty()
            && self.pending_hosts.is_empty()
            && self.enriching_hosts.is_empty()
            && self.activity_pending.is_empty()
    }

    fn target_for_observed_host(&self, host: &str) -> Option<&String> {
        self.discovery_targets
            .iter()
            .find(|target| codex::observed_host(target) == host)
    }

    fn is_important_target(&self, target: &str) -> bool {
        if target == "local" {
            return true;
        }
        let observed_host = codex::observed_host(target);
        if self.host_filter == observed_host
            || self
                .selected_session()
                .is_some_and(|session| session.host == observed_host)
        {
            return true;
        }
        self.active.iter().any(|session| {
            session.host == observed_host
                && matches!(
                    session.activity,
                    CodexActivity::Working
                        | CodexActivity::WaitingApproval
                        | CodexActivity::WaitingInput
                        | CodexActivity::Failed
                )
        })
    }

    fn reload_snapshots(&mut self) -> Result<(), PickerError> {
        self.store.auto_settle_stale(DEFAULT_STALE_AFTER_SECONDS)?;
        self.active = self.store.load(false)?;
        self.settled = self.store.load(true)?;
        self.unread_ids = self.store.unread_session_ids()?.into_iter().collect();
        for session in self.active.iter_mut().chain(self.settled.iter_mut()) {
            if !self.fresh_ids.contains(&session.id) {
                session.runtime = CodexRuntime::Resumable;
                session.tmux = None;
            }
        }
        Ok(())
    }

    fn acknowledge_session(
        &mut self,
        session_id: &str,
        show_status: bool,
    ) -> Result<(), PickerError> {
        let title = self
            .active
            .iter()
            .chain(&self.settled)
            .find(|session| session.id == session_id)
            .map_or_else(|| session_id.to_owned(), |session| session.title.clone());
        let changed = self.store.acknowledge(session_id)?;
        self.unread_ids.remove(session_id);
        self.rebuild_rows_selecting(Some(session_id));
        if show_status {
            let message = if changed {
                format!("Marked {title} read.")
            } else {
                format!("{title} was already read.")
            };
            if let Some(preview) = self.preview.as_mut() {
                preview.notice = Some(message);
            } else {
                self.status = Some(message);
            }
        }
        Ok(())
    }

    fn set_archived(&mut self, session_id: &str, archived: bool) -> Result<(), PickerError> {
        let title = self
            .active
            .iter()
            .chain(&self.settled)
            .find(|session| session.id == session_id)
            .map_or_else(|| session_id.to_owned(), |session| session.title.clone());
        self.store.set_archived(session_id, archived)?;
        self.reload_snapshots()?;
        self.rebuild_rows();
        self.status = Some(if archived {
            format!("Settled {title}.")
        } else {
            format!("Restored {title}.")
        });
        Ok(())
    }

    fn rebuild_host_choices(&mut self) {
        let mut hosts = BTreeSet::new();
        hosts.insert(ALL_HOSTS.to_owned());
        hosts.extend(
            self.discovery_targets
                .iter()
                .map(|target| codex::observed_host(target)),
        );
        hosts.extend(
            self.active
                .iter()
                .chain(&self.settled)
                .map(|session| session.host.clone()),
        );
        self.host_choices = hosts.into_iter().collect();
        self.host_choices
            .sort_by_key(|host| if host == ALL_HOSTS { 0 } else { 1 });
        if !self
            .host_choices
            .iter()
            .any(|host| host == &self.host_filter)
        {
            self.host_filter = ALL_HOSTS.to_owned();
        }
        self.host_selected = self
            .host_choices
            .iter()
            .position(|host| host == &self.host_filter)
            .unwrap_or_default();
    }

    fn rebuild_rows(&mut self) {
        let selected_session_id = self.selected_session().map(|session| session.id.clone());
        self.rebuild_rows_selecting(selected_session_id.as_deref());
    }

    fn rebuild_rows_selecting(&mut self, selected_session_id: Option<&str>) {
        let query = self.query.to_lowercase();
        let sessions = self.current_sessions();
        let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (index, session) in sessions.iter().enumerate() {
            if (self.host_filter == ALL_HOSTS || session.host == self.host_filter)
                && session_matches(session, &query)
            {
                groups
                    .entry((session.cwd.clone(), session.host.clone()))
                    .or_default()
                    .push(index);
            }
        }

        let mut groups = groups.into_iter().collect::<Vec<_>>();
        groups.sort_by(|left, right| {
            let left_unread = left
                .1
                .iter()
                .any(|index| self.unread_ids.contains(&sessions[*index].id));
            let right_unread = right
                .1
                .iter()
                .any(|index| self.unread_ids.contains(&sessions[*index].id));
            let left_priority = left
                .1
                .iter()
                .map(|index| activity_priority(sessions[*index].activity))
                .min()
                .unwrap_or(u8::MAX);
            let right_priority = right
                .1
                .iter()
                .map(|index| activity_priority(sessions[*index].activity))
                .min()
                .unwrap_or(u8::MAX);
            let left_recency = left
                .1
                .iter()
                .map(|index| sessions[*index].last_interaction_unix_seconds)
                .max()
                .unwrap_or_default();
            let right_recency = right
                .1
                .iter()
                .map(|index| sessions[*index].last_interaction_unix_seconds)
                .max()
                .unwrap_or_default();
            right_unread
                .cmp(&left_unread)
                .then_with(|| left_priority.cmp(&right_priority))
                .then_with(|| right_recency.cmp(&left_recency))
                .then(left.0.cmp(&right.0))
        });

        let mut rows = Vec::new();
        for ((cwd, host), mut indices) in groups {
            indices.sort_by_key(|index| {
                (
                    !self.unread_ids.contains(&sessions[*index].id),
                    activity_priority(sessions[*index].activity),
                    std::cmp::Reverse(sessions[*index].last_interaction_unix_seconds),
                )
            });
            let fresh = indices
                .iter()
                .any(|index| self.fresh_ids.contains(&sessions[*index].id));
            rows.push(DashboardRow::Group {
                cwd,
                host,
                count: indices.len(),
                fresh,
            });
            rows.extend(indices.into_iter().map(DashboardRow::Session));
        }
        self.rows = rows;
        self.selected_row = selected_session_id
            .and_then(|selected_id| {
                self.rows.iter().position(|row| {
                    let DashboardRow::Session(index) = row else {
                        return false;
                    };
                    self.current_sessions()[*index].id == selected_id
                })
            })
            .unwrap_or_else(|| {
                self.rows
                    .iter()
                    .position(|row| matches!(row, DashboardRow::Session(_)))
                    .unwrap_or_default()
            });
    }

    fn current_sessions(&self) -> &[CodexSession] {
        match self.view {
            DashboardView::Active => &self.active,
            DashboardView::Settled => &self.settled,
        }
    }

    fn selected_session(&self) -> Option<&CodexSession> {
        let DashboardRow::Session(index) = self.rows.get(self.selected_row)? else {
            return None;
        };
        self.current_sessions().get(*index)
    }

    fn select_first(&mut self) {
        self.selected_row = self
            .rows
            .iter()
            .position(|row| matches!(row, DashboardRow::Session(_)))
            .unwrap_or_default();
    }

    fn select_last(&mut self) {
        self.selected_row = self
            .rows
            .iter()
            .rposition(|row| matches!(row, DashboardRow::Session(_)))
            .unwrap_or_default();
    }

    fn select_next(&mut self) {
        if let Some(index) = self
            .rows
            .iter()
            .enumerate()
            .skip(self.selected_row.saturating_add(1))
            .find_map(|(index, row)| matches!(row, DashboardRow::Session(_)).then_some(index))
        {
            self.selected_row = index;
        }
    }

    fn select_previous(&mut self) {
        if let Some(index) = self.rows[..self.selected_row.min(self.rows.len())]
            .iter()
            .rposition(|row| matches!(row, DashboardRow::Session(_)))
        {
            self.selected_row = index;
        }
    }

    fn visible_session_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| matches!(row, DashboardRow::Session(_)))
            .count()
    }

    fn group_connectivity(&self, host: &str, fresh: bool) -> GroupConnectivity {
        if fresh {
            return GroupConnectivity::Online;
        }
        let Some(target) = self.target_for_observed_host(host) else {
            return GroupConnectivity::Cached;
        };
        match self.reconnect.get(target).map(HostReconnectState::phase) {
            Some(HostPhase::Connecting) => GroupConnectivity::Checking,
            Some(HostPhase::Backoff) => GroupConnectivity::Offline(
                self.reconnect
                    .get(target)
                    .and_then(|state| state.retry_in(Instant::now()))
                    .map(|duration| duration.as_secs().max(1)),
            ),
            Some(HostPhase::Blocked) => GroupConnectivity::Blocked,
            Some(HostPhase::Online | HostPhase::Cached) | None => GroupConnectivity::Cached,
        }
    }
}

fn normalize_host_filter(host: &str) -> String {
    if host == ALL_HOSTS {
        ALL_HOSTS.to_owned()
    } else {
        codex::observed_host(host)
    }
}

fn reconnect_jitter_seed() -> u64 {
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let folded_time = u64::try_from(time ^ (time >> 64)).unwrap_or_default();
    folded_time ^ u64::from(std::process::id())
}

fn discovery_targets() -> Result<Vec<String>, HostDiscoveryError> {
    Ok(deduplicate_discovery_targets(hosts::discover(None)?))
}

fn restrict_discovery_targets(targets: Vec<String>, host: &str) -> Vec<String> {
    if host == ALL_HOSTS {
        return targets;
    }
    if host == "local" {
        return vec!["local".to_owned()];
    }
    if targets.iter().any(|target| target == host) {
        return vec![host.to_owned()];
    }

    let observed_host = normalize_host_filter(host);
    let matching = targets
        .into_iter()
        .filter(|target| target == host || codex::observed_host(target) == observed_host)
        .collect::<Vec<_>>();
    if matching.is_empty() {
        vec![host.to_owned()]
    } else {
        matching
    }
}

fn deduplicate_discovery_targets(hosts: Vec<SshHost>) -> Vec<String> {
    let mut endpoints: BTreeMap<(String, Option<String>, u16), String> = BTreeMap::new();
    for host in hosts
        .into_iter()
        .filter(|host| host.resolution_error.is_none())
    {
        let key = (host.hostname.to_lowercase(), host.user.clone(), host.port);
        endpoints
            .entry(key)
            .and_modify(|current| {
                if (host.alias.len(), &host.alias) < (current.len(), current) {
                    current.clone_from(&host.alias);
                }
            })
            .or_insert(host.alias);
    }
    let mut targets = endpoints.into_values().collect::<Vec<_>>();
    targets.sort();
    targets.insert(0, "local".to_owned());
    targets
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

fn session_needs_detail(
    session: &CodexSession,
    cached: &BTreeMap<String, (u64, u64, bool)>,
) -> bool {
    cached
        .get(&session.native_session_id)
        .is_none_or(|(last_interaction, updated, has_message)| {
            !has_message
                || *last_interaction != session.last_interaction_unix_seconds
                || *updated != session.updated_unix_seconds
        })
}

fn session_matches(session: &CodexSession, query: &str) -> bool {
    query.is_empty()
        || [
            session.title.as_str(),
            session.last_message.as_str(),
            session.cwd.as_str(),
            session.host.as_str(),
            session.source.as_str(),
            activity_label(session.activity),
            session.native_session_id.as_str(),
            session
                .tmux
                .as_ref()
                .map_or("", |binding| binding.session.as_str()),
        ]
        .iter()
        .any(|value| value.to_lowercase().contains(query))
}

const fn activity_priority(activity: CodexActivity) -> u8 {
    match activity {
        CodexActivity::Working => 0,
        CodexActivity::WaitingApproval | CodexActivity::WaitingInput => 1,
        CodexActivity::Failed => 2,
        CodexActivity::Completed => 3,
        CodexActivity::Unknown => 4,
    }
}

fn draw(frame: &mut Frame<'_>, app: &PickerApp) {
    if app.show_details && frame.area().height >= 20 {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(4),
                Constraint::Length(2),
            ])
            .split(frame.area());
        draw_header(frame, app, areas[0]);
        draw_sessions(frame, app, areas[1]);
        draw_selected_detail(frame, app, areas[2]);
        draw_footer(frame, app, areas[3]);
    } else {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(2),
            ])
            .split(frame.area());
        draw_header(frame, app, areas[0]);
        draw_sessions(frame, app, areas[1]);
        draw_footer(frame, app, areas[2]);
    }

    match app.input_mode {
        InputMode::Hosts => draw_hosts(frame, app),
        InputMode::Help => draw_help(frame),
        InputMode::Preview => draw_preview(frame, app),
        InputMode::Browse | InputMode::Search => {}
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    let active_style = if app.view == DashboardView::Active {
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD)
    } else {
        muted_style()
    };
    let settled_style = if app.view == DashboardView::Settled {
        Style::default()
            .add_modifier(Modifier::REVERSED)
            .add_modifier(Modifier::BOLD)
    } else {
        muted_style()
    };
    let title = Line::from(vec![
        Span::styled(" Rollcall ", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(format!(" Active {} ", app.active.len()), active_style),
        Span::raw(" "),
        Span::styled(format!(" Settled {} ", app.settled.len()), settled_style),
        Span::styled(
            format!(
                "  {}",
                if app.host_filter == ALL_HOSTS {
                    "all hosts"
                } else {
                    &app.host_filter
                }
            ),
            muted_style(),
        ),
    ]);
    let mut subtitle = vec![Span::styled(
        format!("{} up", app.reachable_hosts.len()),
        Style::default().fg(Color::Green),
    )];
    if !app.pending_hosts.is_empty() {
        subtitle.extend([
            Span::raw(" · "),
            Span::styled(
                format!("{} checking", app.pending_hosts.len()),
                Style::default().fg(Color::Yellow),
            ),
        ]);
    }
    subtitle.extend([
        Span::raw(" · "),
        Span::styled(
            format!("{} down", app.host_errors.len()),
            if app.host_errors.is_empty() {
                muted_style()
            } else {
                Style::default().fg(Color::Red)
            },
        ),
    ]);
    if !app.unread_ids.is_empty() {
        subtitle.extend([
            Span::raw(" · "),
            Span::styled(
                format!("{} unread", app.unread_ids.len()),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    if app.input_mode == InputMode::Search || !app.query.is_empty() {
        subtitle.extend([
            Span::raw("    "),
            Span::styled(
                format!("/{}", app.query),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {} shown", app.visible_session_count()),
                muted_style(),
            ),
        ]);
    }
    frame.render_widget(
        Paragraph::new(vec![title, Line::from(subtitle)])
            .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn draw_sessions(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    if app.rows.is_empty() {
        let noun = if app.view == DashboardView::Active {
            "active"
        } else {
            "settled"
        };
        let message = if app.query.is_empty() {
            format!("No {noun} sessions in this view.")
        } else {
            format!("No {noun} sessions match /{}.", app.query)
        };
        frame.render_widget(Paragraph::new(message).style(muted_style()), area);
        return;
    }

    let sessions = app.current_sessions();
    let items = app
        .rows
        .iter()
        .map(|row| match row {
            DashboardRow::Group {
                cwd,
                host,
                count,
                fresh,
            } => group_item(
                cwd,
                host,
                *count,
                app.group_connectivity(host, *fresh),
                area.width,
            ),
            DashboardRow::Session(index) => session_item(
                &sessions[*index],
                app.fresh_ids.contains(&sessions[*index].id),
                app.unread_ids.contains(&sessions[*index].id),
                area.width,
            ),
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("› ");
    let mut state = ListState::default();
    if app.selected_session().is_some() {
        state.select(Some(app.selected_row));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn group_item(
    cwd: &str,
    host: &str,
    count: usize,
    connectivity: GroupConnectivity,
    width: u16,
) -> ListItem<'static> {
    let (connectivity_label, connectivity_style) = match connectivity {
        GroupConnectivity::Online => (String::new(), Style::default().fg(Color::Green)),
        GroupConnectivity::Checking => {
            (" · checking".to_owned(), Style::default().fg(Color::Yellow))
        }
        GroupConnectivity::Offline(retry_in) => (
            retry_in.map_or_else(
                || " · offline".to_owned(),
                |seconds| format!(" · offline · retry {seconds}s"),
            ),
            Style::default().fg(Color::Red),
        ),
        GroupConnectivity::Blocked => (
            " · blocked".to_owned(),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ),
        GroupConnectivity::Cached => (" · cached".to_owned(), muted_style()),
    };
    let suffix = format!(" / {host} · {count}{connectivity_label}");
    let directory = display_directory(cwd);
    let cwd_width = usize::from(width)
        .saturating_sub(2 + suffix.chars().count() + 2)
        .max(8);
    ListItem::new(Line::from(vec![
        Span::styled("▾ ", connectivity_style.add_modifier(Modifier::BOLD)),
        Span::styled(
            truncate_with_ellipsis(&directory, cwd_width),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(suffix, connectivity_style),
    ]))
}

fn display_directory(cwd: &str) -> String {
    let compact = compact_home(cwd);
    let path = Path::new(&compact);
    path.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(&compact)
        .to_owned()
}

fn compact_home(path: &str) -> String {
    let Some(home) = env::var_os("HOME") else {
        return path.to_owned();
    };
    let home = home.to_string_lossy();
    if path == home {
        "~".to_owned()
    } else if let Some(relative) = path.strip_prefix(home.as_ref()) {
        if relative.starts_with('/') {
            format!("~{relative}")
        } else {
            path.to_owned()
        }
    } else {
        path.to_owned()
    }
}

fn session_item(
    session: &CodexSession,
    fresh: bool,
    unread: bool,
    width: u16,
) -> ListItem<'static> {
    let (marker, marker_style, _) = if fresh {
        activity_style(session.activity)
    } else {
        ("◌", muted_style(), "cached")
    };
    let status_label = match (fresh, session.activity) {
        (true, CodexActivity::WaitingApproval) => "approval ",
        (true, CodexActivity::WaitingInput) => "input ",
        (true, CodexActivity::Failed) => "failed ",
        _ => "",
    };
    let suffix = format!("  {}", format_age(session.last_interaction_unix_seconds));
    let fixed_width = 2 + 2 + status_label.chars().count() + suffix.chars().count() + 2;
    let text_width = usize::from(width).saturating_sub(fixed_width).max(8);
    let message_is_distinct =
        !session.last_message.is_empty() && session.last_message != session.title;
    let title_width = if message_is_distinct {
        (text_width / 3).clamp(12, 32).min(text_width)
    } else {
        text_width
    };
    let mut spans = vec![
        Span::styled(
            if unread { "• " } else { "  " },
            if unread {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            },
        ),
        Span::styled(format!("{marker} "), marker_style),
    ];
    if !status_label.is_empty() {
        spans.push(Span::styled(
            status_label,
            marker_style.add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        truncate_with_ellipsis(&session.title, title_width),
        if session.activity == CodexActivity::Working && fresh {
            Style::default().add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        },
    ));
    if message_is_distinct {
        let message_width = text_width.saturating_sub(title_width + 3);
        spans.push(Span::styled(
            format!(
                " — {}",
                truncate_with_ellipsis(&session.last_message, message_width)
            ),
            muted_style(),
        ));
    }
    spans.push(Span::styled(suffix, muted_style()));
    ListItem::new(Line::from(spans))
}

fn activity_style(activity: CodexActivity) -> (&'static str, Style, &'static str) {
    match activity {
        CodexActivity::Working => {
            let (marker, color) = working_pulse();
            (marker, Style::default().fg(color), "working")
        }
        CodexActivity::WaitingApproval => ("◆", Style::default().fg(Color::Yellow), "approval"),
        CodexActivity::WaitingInput => ("◆", Style::default().fg(Color::Yellow), "input"),
        CodexActivity::Completed => ("✓", muted_style(), "completed"),
        CodexActivity::Failed => ("!", Style::default().fg(Color::Red), "failed"),
        CodexActivity::Unknown => ("○", muted_style(), "unknown"),
    }
}

fn working_pulse() -> (&'static str, Color) {
    let frame = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() / 250 % 4);
    let frame = usize::try_from(frame).unwrap_or_default();
    (
        ["◐", "◓", "◑", "◒"][frame],
        [Color::Green, Color::LightGreen, Color::Green, Color::Cyan][frame],
    )
}

fn activity_label(activity: CodexActivity) -> &'static str {
    activity_style(activity).2
}

fn muted_style() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

fn draw_selected_detail(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    let Some(session) = app.selected_session() else {
        frame.render_widget(
            Paragraph::new("").block(Block::default().title(" Selected ").borders(Borders::TOP)),
            area,
        );
        return;
    };

    let (_, status_style, status) = if app.fresh_ids.contains(&session.id) {
        activity_style(session.activity)
    } else {
        ("◌", muted_style(), "cached")
    };
    let runtime = match session.runtime {
        CodexRuntime::TmuxFrontend => session.tmux.as_ref().map_or_else(
            || "tmux".to_owned(),
            |binding| format!("tmux {}", binding.session),
        ),
        CodexRuntime::SharedBackend => "Rollcall backend".to_owned(),
        CodexRuntime::ExternalFrontend => "external frontend".to_owned(),
        CodexRuntime::Resumable => "resumable".to_owned(),
    };
    let width = usize::from(area.width).saturating_sub(2);
    let title_width = width.saturating_sub(status.len() + 3);
    let message = if session.last_message.is_empty() {
        "No cached agent response yet."
    } else {
        &session.last_message
    };
    let metadata = format!(
        "{} / {} · {} · {} · {}{}",
        compact_home(&session.cwd),
        session.host,
        format_age(session.last_interaction_unix_seconds),
        runtime,
        session.native_session_id,
        if app.unread_ids.contains(&session.id) {
            " · unread"
        } else {
            ""
        }
    );
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{status:<9}"),
                status_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_with_ellipsis(&session.title, title_width),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            truncate_with_ellipsis(message, width),
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            truncate_with_ellipsis(&metadata, width),
            muted_style(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Selected ")
                .borders(Borders::TOP)
                .border_style(muted_style()),
        ),
        area,
    );
}

fn truncate_with_ellipsis(value: &str, maximum_characters: usize) -> String {
    if maximum_characters == 0 {
        return String::new();
    }
    let mut characters = value.chars();
    let prefix = characters
        .by_ref()
        .take(maximum_characters)
        .collect::<String>();
    if characters.next().is_some() {
        if maximum_characters == 1 {
            "…".to_owned()
        } else {
            format!(
                "{}…",
                prefix
                    .chars()
                    .take(maximum_characters.saturating_sub(1))
                    .collect::<String>()
            )
        }
    } else {
        prefix
    }
}

fn draw_footer(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    let content = match app.input_mode {
        InputMode::Browse => {
            if let Some((notice, _)) = &app.notice {
                Line::from(Span::styled(
                    notice.clone(),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if let Some(status) = &app.status {
                Line::from(Span::styled(
                    status.clone(),
                    Style::default().fg(Color::Yellow),
                ))
            } else {
                Line::from(vec![
                    Span::styled("↵", Style::default().fg(Color::Cyan)),
                    Span::raw(if app.view == DashboardView::Settled {
                        " restore+open  "
                    } else {
                        " open  "
                    }),
                    Span::styled("a", Style::default().fg(Color::Cyan)),
                    Span::raw(if app.view == DashboardView::Settled {
                        " restore  "
                    } else {
                        " settle  "
                    }),
                    Span::styled("x", Style::default().fg(Color::Cyan)),
                    Span::raw(" read  "),
                    Span::styled("tab", Style::default().fg(Color::Cyan)),
                    Span::raw(" view  "),
                    Span::styled("/", Style::default().fg(Color::Cyan)),
                    Span::raw(" search  "),
                    Span::styled("?", Style::default().fg(Color::Cyan)),
                    Span::raw(" keys"),
                ])
            }
        }
        InputMode::Search => Line::from(vec![
            Span::raw("type to filter  "),
            Span::styled("ctrl-u", Style::default().fg(Color::Cyan)),
            Span::raw(" clear  "),
            Span::styled("enter/esc", Style::default().fg(Color::Cyan)),
            Span::raw(" done"),
        ]),
        InputMode::Hosts => Line::from(vec![
            Span::styled("j/k", Style::default().fg(Color::Cyan)),
            Span::raw(" move  "),
            Span::styled("enter", Style::default().fg(Color::Cyan)),
            Span::raw(" filter  "),
            Span::styled("esc", Style::default().fg(Color::Cyan)),
            Span::raw(" close"),
        ]),
        InputMode::Help => Line::from(vec![
            Span::styled("?/enter/esc", Style::default().fg(Color::Cyan)),
            Span::raw(" close help"),
        ]),
        InputMode::Preview => Line::from(vec![
            Span::styled("enter", Style::default().fg(Color::Cyan)),
            Span::raw(" open  "),
            Span::styled("a", Style::default().fg(Color::Cyan)),
            Span::raw(
                app.preview
                    .as_ref()
                    .and_then(|preview| app.archived_state_for_session(&preview.session_id))
                    .map_or(" settle/restore  ", |archived| {
                        if archived { " restore  " } else { " settle  " }
                    }),
            ),
            Span::styled("x", Style::default().fg(Color::Cyan)),
            Span::raw(" read  "),
            Span::styled("j/k", Style::default().fg(Color::Cyan)),
            Span::raw(" scroll  "),
            Span::styled("esc", Style::default().fg(Color::Cyan)),
            Span::raw(" close"),
        ]),
    };
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().borders(Borders::TOP))
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn draw_help(frame: &mut Frame<'_>) {
    let area = centered_rect(72, 84, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from("  j/k or arrows   move"),
        Line::from("  g/G             first / last"),
        Line::from("  enter           attach / resume"),
        Line::from("  p               preview pane / response"),
        Line::from("  tab             Active / Settled"),
        Line::from("  a               settle / restore"),
        Line::from("  x               mark selected session read"),
        Line::from("  /               search"),
        Line::from("  h               host filter"),
        Line::from("  i               session details"),
        Line::from("  r               refresh now"),
        Line::from("  ?/q/esc         close help"),
        Line::from(""),
        Line::from(Span::styled(
            "  ◐ work   ◆ attention   ✓ done   ! fail",
            Style::default()
                .fg(Color::Gray)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  ◌ cached / checking / offline"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .title(" Rollcall help ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_preview(frame: &mut Frame<'_>, app: &PickerApp) {
    let Some(preview) = app.preview.as_ref() else {
        return;
    };
    let area = centered_rect(90, 88, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .title(" Session preview ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (source, body, style) = match &preview.content {
        PreviewContent::Loading => (
            "capturing live tmux pane",
            format!("{} Loading preview…", working_pulse().0),
            Style::default().fg(Color::Cyan),
        ),
        PreviewContent::Ready { source, body } => (*source, body.clone(), Style::default()),
        PreviewContent::Failed { error, fallback } => (
            "cached response · live capture failed",
            format!("{fallback}\n\nCapture error: {error}"),
            Style::default(),
        ),
    };
    let notice_height = u16::from(preview.notice.is_some()).saturating_mul(2);
    let sections = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(notice_height),
        ])
        .split(inner);
    let title = truncate_with_ellipsis(&preview.title, usize::from(inner.width).saturating_sub(1));
    frame.render_widget(
        Paragraph::new(vec![
            Line::from(Span::styled(
                title,
                Style::default().add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                format!("{} · {source}", preview.host),
                muted_style(),
            )),
        ]),
        sections[0],
    );

    let wrapped = wrap_preview_body(&body, usize::from(sections[1].width).max(1));
    let visible = preview_window(
        &wrapped,
        usize::from(sections[1].height).max(1),
        preview.scroll_from_bottom,
    );
    frame.render_widget(
        Paragraph::new(visible)
            .style(style)
            .wrap(Wrap { trim: false }),
        sections[1],
    );
    if let Some(notice) = &preview.notice {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                notice.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )))
            .block(Block::default().borders(Borders::TOP)),
            sections[2],
        );
    }
}

fn cached_preview(session: &CodexSession) -> String {
    if session.last_message.trim().is_empty() {
        format!(
            "{}\n\nNo cached agent response is available.\n\n{} / {}",
            session.title,
            compact_home(&session.cwd),
            session.host
        )
    } else {
        format!(
            "{}\n\n{}\n\n{} / {}",
            session.title,
            session.last_message,
            compact_home(&session.cwd),
            session.host
        )
    }
}

const fn cached_preview_source(session: &CodexSession) -> &'static str {
    match session.runtime {
        CodexRuntime::ExternalFrontend => "external frontend · cached response",
        CodexRuntime::SharedBackend => "Rollcall backend · cached response",
        CodexRuntime::TmuxFrontend | CodexRuntime::Resumable => "cached response",
    }
}

fn wrap_preview_body(body: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut wrapped = Vec::new();
    for line in body.lines() {
        if line.is_empty() {
            wrapped.push(String::new());
            continue;
        }
        let characters = line.chars().collect::<Vec<_>>();
        wrapped.extend(
            characters
                .chunks(width)
                .map(|chunk| chunk.iter().collect::<String>()),
        );
    }
    if wrapped.is_empty() {
        wrapped.push(String::new());
    }
    wrapped
}

fn preview_window(lines: &[String], height: usize, scroll_from_bottom: usize) -> String {
    if lines.is_empty() {
        return String::new();
    }
    let maximum_scroll = lines.len().saturating_sub(height);
    let scroll = scroll_from_bottom.min(maximum_scroll);
    let end = lines.len().saturating_sub(scroll);
    let start = end.saturating_sub(height);
    lines[start..end].join("\n")
}

fn draw_hosts(frame: &mut Frame<'_>, app: &PickerApp) {
    let area = centered_rect(54, 70, frame.area());
    frame.render_widget(Clear, area);
    let items = app
        .host_choices
        .iter()
        .map(|host| {
            let current = if host == &app.host_filter {
                "  current"
            } else {
                ""
            };
            let state = app
                .target_for_observed_host(host)
                .and_then(|target| app.reconnect.get(target))
                .map(HostReconnectState::phase);
            let connectivity = match state {
                Some(HostPhase::Connecting) => "  checking",
                Some(HostPhase::Backoff) => "  offline",
                Some(HostPhase::Blocked) => "  blocked",
                Some(HostPhase::Cached | HostPhase::Online) | None => "",
            };
            ListItem::new(format!("{host}{current}{connectivity}"))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .title(" Host filter ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_symbol("› ")
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::REVERSED)
                .add_modifier(Modifier::BOLD),
        );
    let mut state = ListState::default();
    if !app.host_choices.is_empty() {
        state.select(Some(app.host_selected));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

fn format_age(timestamp: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(timestamp, |duration| duration.as_secs());
    let seconds = now.saturating_sub(timestamp);

    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m", seconds / 60),
        3_600..=86_399 => format!("{}h", seconds / 3_600),
        _ => format!("{}d", seconds / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::{
        Action, DashboardRow, DashboardView, InputMode, PickerApp, PreviewContent, RefreshEvent,
        deduplicate_discovery_targets, format_age, normalize_host_filter, preview_window,
        restrict_discovery_targets, session_matches, session_needs_detail, truncate_with_ellipsis,
        wrap_preview_body, write_transition,
    };
    use crate::{
        codex::{
            self, CodexActivity, CodexRuntime, CodexSession, LiveObservation, LiveOwner,
            TmuxBinding,
        },
        hosts::{ConnectivityState, SshHost},
        store::{SessionTransition, SessionTransitionKind, Store},
    };

    fn session(id: &str, cwd: &str, title: &str) -> CodexSession {
        CodexSession {
            id: format!("topo:codex:{id}"),
            host: "topo".to_owned(),
            native_session_id: id.to_owned(),
            title: title.to_owned(),
            cwd: cwd.to_owned(),
            source: "cli".to_owned(),
            activity: CodexActivity::Completed,
            last_message: "The implementation is ready.".to_owned(),
            last_interaction_unix_seconds: 100,
            updated_unix_seconds: 100,
            runtime: CodexRuntime::TmuxFrontend,
            tmux: Some(TmuxBinding {
                session: "agents".to_owned(),
                pane: "%3".to_owned(),
            }),
        }
    }

    #[test]
    fn search_matches_session_metadata_status_and_last_message() {
        let session = session("019f", "/home/fin/projects/rollcall", "Rollcall picker");
        assert!(session_matches(&session, "picker"));
        assert!(session_matches(&session, "implementation is ready"));
        assert!(session_matches(&session, "completed"));
        assert!(session_matches(&session, "projects/rollcall"));
        assert!(session_matches(&session, "agents"));
        assert!(session_matches(&session, "019f"));
        assert!(!session_matches(&session, "coda"));
    }

    #[test]
    fn picker_ages_are_compact() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should follow Unix time")
            .as_secs();
        assert_eq!(format_age(now.saturating_sub(5)), "5s");
        assert_eq!(format_age(now.saturating_sub(120)), "2m");
        assert_eq!(format_age(now.saturating_sub(7_200)), "2h");
        assert_eq!(format_age(now.saturating_sub(172_800)), "2d");
    }

    #[test]
    fn local_host_filter_uses_the_observed_hostname() {
        assert_eq!(
            normalize_host_filter("local"),
            codex::observed_host("local")
        );
        assert_eq!(normalize_host_filter("all"), "all");
        assert_eq!(normalize_host_filter("coda"), "coda");
    }

    #[test]
    fn foreign_frontend_cannot_be_attached_without_managed_tmux() {
        let mut loaded = session("019f", "/fabric", "Loaded thread");
        loaded.runtime = CodexRuntime::ExternalFrontend;
        loaded.tmux = None;
        loaded.activity = CodexActivity::Working;
        let mut app = app_with_sessions(vec![loaded], Vec::new());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert!(
            app.status
                .as_deref()
                .is_some_and(|status| status.contains("close or release"))
        );
    }

    #[test]
    fn completed_foreign_frontend_is_also_left_alone() {
        let mut loaded = session("019f", "/fabric", "Loaded thread");
        loaded.runtime = CodexRuntime::ExternalFrontend;
        loaded.tmux = None;
        let mut app = app_with_sessions(vec![loaded], Vec::new());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
    }

    #[test]
    fn shared_backend_threads_can_open_a_terminal_frontend() {
        let mut loaded = session("019f", "/fabric", "Backend-only thread");
        loaded.runtime = CodexRuntime::SharedBackend;
        loaded.tmux = None;
        let mut app = app_with_sessions(vec![loaded], Vec::new());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Attach("topo:codex:019f".to_owned())
        );
    }

    #[test]
    fn sessions_are_nested_beneath_directory_and_host_groups() {
        let app = app_with_sessions(
            vec![
                session("019f", "/fabric", "One"),
                session("019e", "/fabric", "Two"),
                session("019d", "/tmp/project", "Three"),
            ],
            Vec::new(),
        );

        assert_eq!(
            app.rows
                .iter()
                .filter(|row| matches!(row, DashboardRow::Group { .. }))
                .count(),
            2
        );
        assert_eq!(app.visible_session_count(), 3);
    }

    #[test]
    fn archive_key_moves_active_session_to_settled_projection() {
        let mut app = app_with_sessions(vec![session("019f", "/fabric", "Archive me")], Vec::new());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Action::SetArchived {
                session_id: "topo:codex:019f".to_owned(),
                archived: true,
                attach: false,
            }
        );
    }

    #[test]
    fn acknowledge_key_targets_the_selected_session() {
        let mut app = app_with_sessions(vec![session("019f", "/fabric", "Read me")], Vec::new());
        app.unread_ids.insert("topo:codex:019f".to_owned());

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Action::Acknowledge("topo:codex:019f".to_owned())
        );
    }

    #[test]
    fn resumable_preview_uses_cached_response_without_starting_a_capture() {
        let mut candidate = session("019f", "/fabric", "Cached thread");
        candidate.runtime = CodexRuntime::Resumable;
        candidate.tmux = None;
        let mut app = app_with_sessions(vec![candidate], Vec::new());

        app.start_preview();

        assert_eq!(app.input_mode, InputMode::Preview);
        let preview = app.preview.as_ref().expect("preview should open");
        match &preview.content {
            PreviewContent::Ready { source, body } => {
                assert_eq!(*source, "cached response");
                assert!(body.contains("The implementation is ready."));
            }
            content => panic!("expected cached preview, got {content:?}"),
        }
        assert!(matches!(
            app.refresh_rx.try_recv(),
            Err(TryRecvError::Empty)
        ));
    }

    #[test]
    fn preview_archive_action_uses_the_current_projection() {
        let mut candidate = session("019f", "/fabric", "Moved thread");
        candidate.runtime = CodexRuntime::Resumable;
        candidate.tmux = None;
        let mut app = app_with_sessions(vec![candidate], Vec::new());
        app.start_preview();

        let moved = app.active.remove(0);
        app.settled.push(moved);

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            Action::SetArchived {
                session_id: "topo:codex:019f".to_owned(),
                archived: false,
                attach: false,
            }
        );
        assert_eq!(app.input_mode, InputMode::Browse);
        assert!(app.preview.is_none());
    }

    #[test]
    fn preview_enter_keeps_an_external_frontend_open_with_a_notice() {
        let mut candidate = session("019f", "/fabric", "External thread");
        candidate.runtime = CodexRuntime::ExternalFrontend;
        candidate.tmux = None;
        let mut app = app_with_sessions(vec![candidate], Vec::new());
        app.start_preview();

        assert_eq!(
            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Action::None
        );
        assert_eq!(app.input_mode, InputMode::Preview);
        assert!(
            app.preview
                .as_ref()
                .and_then(|preview| preview.notice.as_deref())
                .is_some_and(|notice| notice.contains("close or release"))
        );
    }

    #[test]
    fn preview_refresh_ignores_stale_results_and_accepts_the_matching_capture() {
        let mut candidate = session("019f", "/fabric", "Preview refresh");
        candidate.runtime = CodexRuntime::Resumable;
        candidate.tmux = None;
        let mut app = app_with_sessions(vec![candidate], Vec::new());
        app.start_preview();
        let request_id = app
            .preview
            .as_ref()
            .expect("preview should open")
            .request_id;

        app.apply_refresh_event(RefreshEvent::Preview {
            request_id: request_id.wrapping_add(1),
            session_id: "topo:codex:019f".to_owned(),
            result: Ok("stale capture".to_owned()),
        })
        .expect("stale result should be harmless");
        assert!(matches!(
            app.preview.as_ref().map(|preview| &preview.content),
            Some(PreviewContent::Ready {
                source: "cached response",
                ..
            })
        ));

        app.apply_refresh_event(RefreshEvent::Preview {
            request_id,
            session_id: "topo:codex:019f".to_owned(),
            result: Ok("live pane output".to_owned()),
        })
        .expect("matching result should apply");
        assert!(matches!(
            app.preview.as_ref().map(|preview| &preview.content),
            Some(PreviewContent::Ready {
                source: "live tmux pane",
                body,
            }) if body == "live pane output"
        ));
    }

    #[test]
    fn preview_window_starts_at_the_newest_lines_and_scrolls_toward_history() {
        let lines = wrap_preview_body("one\ntwo\nthree\nfour\nfive", 20);

        assert_eq!(preview_window(&lines, 3, 0), "three\nfour\nfive");
        assert_eq!(preview_window(&lines, 3, 1), "two\nthree\nfour");
        assert_eq!(preview_window(&lines, 3, usize::MAX), "one\ntwo\nthree");
        assert_eq!(wrap_preview_body("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn tab_switches_to_settled_view() {
        let mut app = app_with_sessions(
            vec![session("019f", "/fabric", "Active")],
            vec![session("019e", "/fabric", "Settled")],
        );

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

        assert_eq!(app.view, DashboardView::Settled);
        assert_eq!(app.visible_session_count(), 1);
        assert_eq!(
            app.selected_session().map(|session| session.title.as_str()),
            Some("Settled")
        );
    }

    #[test]
    fn search_mode_filters_last_messages() {
        let mut app = app_with_sessions(
            vec![session("019f", "/fabric", "Rollcall picker")],
            Vec::new(),
        );
        app.input_mode = InputMode::Search;

        for character in "ready".chars() {
            assert_eq!(
                app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE)),
                Action::None
            );
        }

        assert_eq!(app.visible_session_count(), 1);
    }

    #[test]
    fn progressive_refresh_keeps_the_same_session_selected_after_reordering() {
        let mut first = session("019f", "/fabric", "First");
        first.last_interaction_unix_seconds = 200;
        let mut second = session("019e", "/fabric", "Second");
        second.last_interaction_unix_seconds = 100;
        let mut app = app_with_sessions(vec![first, second], Vec::new());
        app.select_next();
        let selected_id = app
            .selected_session()
            .map(|session| session.id.clone())
            .expect("a session should be selected");

        app.active.reverse();
        app.rebuild_rows_selecting(Some(&selected_id));

        assert_eq!(
            app.selected_session().map(|session| session.id.as_str()),
            Some(selected_id.as_str())
        );
    }

    #[test]
    fn live_observations_update_working_and_completed_state() {
        let observed_host = codex::observed_host("local");
        let mut candidate = session("019f", "/fabric", "Live thread");
        candidate.host.clone_from(&observed_host);
        candidate.id = format!("{observed_host}:codex:019f");
        candidate.runtime = CodexRuntime::ExternalFrontend;
        candidate.tmux = None;
        let mut app = app_with_sessions(vec![candidate], Vec::new());

        app.apply_live_observations(
            "local",
            &BTreeMap::from([(
                "019f".to_owned(),
                LiveObservation {
                    activity: CodexActivity::Working,
                    owner: LiveOwner::ExternalFrontend,
                },
            )]),
        )
        .expect("working observation should apply");
        assert_eq!(app.active[0].activity, CodexActivity::Working);

        app.apply_live_observations(
            "local",
            &BTreeMap::from([(
                "019f".to_owned(),
                LiveObservation {
                    activity: CodexActivity::Completed,
                    owner: LiveOwner::ExternalFrontend,
                },
            )]),
        )
        .expect("completion observation should apply");
        assert_eq!(app.active[0].activity, CodexActivity::Completed);
        assert!(app.unread_ids.contains(&app.active[0].id));
    }

    #[test]
    fn working_sessions_sort_ahead_of_newer_completed_sessions() {
        let mut completed = session("019f", "/newer", "Completed");
        completed.last_interaction_unix_seconds = 200;
        let mut working = session("019e", "/older", "Working");
        working.activity = CodexActivity::Working;
        working.last_interaction_unix_seconds = 100;
        let app = app_with_sessions(vec![completed, working], Vec::new());

        assert_eq!(
            app.selected_session().map(|session| session.title.as_str()),
            Some("Working")
        );
    }

    #[test]
    fn unread_completed_sessions_sort_ahead_of_working_sessions() {
        let completed = session("019f", "/completed", "Unread completion");
        let mut working = session("019e", "/working", "Working");
        working.activity = CodexActivity::Working;
        let mut app = app_with_sessions(vec![working, completed], Vec::new());
        app.unread_ids.insert("topo:codex:019f".to_owned());
        app.rebuild_rows_selecting(None);

        assert_eq!(
            app.selected_session().map(|session| session.title.as_str()),
            Some("Unread completion")
        );
    }

    #[test]
    fn equivalent_ssh_aliases_are_probed_once_using_the_shortest_name() {
        let host = |alias: &str, hostname: &str| SshHost {
            alias: alias.to_owned(),
            hostname: hostname.to_owned(),
            user: Some("fin".to_owned()),
            port: 22,
            source: "test".to_owned(),
            identity_files: Vec::new(),
            connectivity: ConnectivityState::Unknown,
            resolution_error: None,
        };

        assert_eq!(
            deduplicate_discovery_targets(vec![
                host("aurkitu.daggertooth-byzantine.ts.net", "100.64.0.1"),
                host("aurkitu", "100.64.0.1"),
                host("coda", "100.64.0.2"),
            ]),
            vec!["local", "aurkitu", "coda"]
        );
    }

    #[test]
    fn watch_restricts_network_work_to_the_selected_host() {
        let targets = vec!["local".to_owned(), "aurkitu".to_owned(), "coda".to_owned()];

        assert_eq!(
            restrict_discovery_targets(targets.clone(), "local"),
            ["local"]
        );
        assert_eq!(
            restrict_discovery_targets(targets.clone(), "coda"),
            ["coda"]
        );
        assert_eq!(
            restrict_discovery_targets(targets, "unlisted"),
            ["unlisted"]
        );
    }

    #[test]
    fn watch_transition_output_supports_text_and_json_lines() {
        let transition = SessionTransition {
            session_id: "topo:codex:019f".to_owned(),
            title: "Needs\tattention".to_owned(),
            host: "topo".to_owned(),
            kind: SessionTransitionKind::Input,
            observed_at_unix_seconds: 100,
        };
        let mut text = Vec::new();
        write_transition(&mut text, &transition, false).expect("text event should render");
        assert_eq!(
            String::from_utf8(text).expect("text should be UTF-8"),
            "100\tinput\ttopo\tNeeds attention\ttopo:codex:019f\n"
        );

        let mut json = Vec::new();
        write_transition(&mut json, &transition, true).expect("JSON event should render");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&json).expect("event should be JSON"),
            serde_json::json!({
                "sessionId": "topo:codex:019f",
                "title": "Needs\tattention",
                "host": "topo",
                "kind": "input",
                "observedAtUnixSeconds": 100,
            })
        );
    }

    #[test]
    fn watch_receives_the_same_transitions_as_the_picker() {
        let (sender, receiver) = mpsc::channel();
        let mut app = app_with_sessions(Vec::new(), Vec::new());
        app.transition_tx = Some(sender);
        let transition = SessionTransition {
            session_id: "topo:codex:019f".to_owned(),
            title: "Ready".to_owned(),
            host: "topo".to_owned(),
            kind: SessionTransitionKind::Completed,
            observed_at_unix_seconds: 100,
        };

        app.handle_transitions(std::slice::from_ref(&transition));

        assert_eq!(
            receiver
                .try_recv()
                .expect("watch should receive transition"),
            transition
        );
    }

    #[test]
    fn one_shot_watch_finishes_after_initial_work_drains() {
        let mut app = app_with_sessions(Vec::new(), Vec::new());
        app.initial_targets_remaining.insert("local".to_owned());
        assert!(!app.initial_refresh_complete());

        app.initial_targets_remaining.clear();
        app.pending_hosts.insert("local".to_owned());
        assert!(!app.initial_refresh_complete());

        app.pending_hosts.clear();
        assert!(app.initial_refresh_complete());
    }

    #[test]
    fn unchanged_sessions_with_messages_skip_expensive_detail_reads() {
        let session = session("019f", "/fabric", "Rollcall");
        let cached = BTreeMap::from([(
            session.native_session_id.clone(),
            (
                session.last_interaction_unix_seconds,
                session.updated_unix_seconds,
                true,
            ),
        )]);

        assert!(!session_needs_detail(&session, &cached));
    }

    #[test]
    fn changed_or_unseen_sessions_request_detail_reads() {
        let session = session("019f", "/fabric", "Rollcall");
        let stale = BTreeMap::from([(
            session.native_session_id.clone(),
            (
                session.last_interaction_unix_seconds.saturating_sub(1),
                session.updated_unix_seconds,
                true,
            ),
        )]);

        assert!(session_needs_detail(&session, &stale));
        assert!(session_needs_detail(&session, &BTreeMap::new()));
    }

    #[test]
    fn dense_rows_truncate_with_an_ellipsis() {
        assert_eq!(truncate_with_ellipsis("abcdef", 4), "abc…");
        assert_eq!(truncate_with_ellipsis("a", 1), "a");
        assert_eq!(truncate_with_ellipsis("ab", 1), "…");
    }

    fn app_with_sessions(active: Vec<CodexSession>, settled: Vec<CodexSession>) -> PickerApp {
        let fresh_ids = active
            .iter()
            .chain(&settled)
            .map(|session| session.id.clone())
            .collect();
        let (refresh_tx, refresh_rx) = mpsc::channel();
        let mut app = PickerApp {
            store: Store::open_memory().expect("memory store should open"),
            active,
            settled,
            rows: Vec::new(),
            query: String::new(),
            selected_row: 0,
            input_mode: InputMode::Browse,
            view: DashboardView::Active,
            show_details: false,
            host_filter: "all".to_owned(),
            host_choices: vec!["all".to_owned(), "topo".to_owned()],
            host_selected: 0,
            discovery_targets: vec!["local".to_owned()],
            pending_hosts: BTreeSet::new(),
            reachable_hosts: BTreeSet::from(["local".to_owned()]),
            enriching_hosts: BTreeSet::new(),
            host_errors: BTreeMap::new(),
            fresh_ids,
            unread_ids: HashSet::new(),
            transition_tx: None,
            initial_targets_remaining: BTreeSet::new(),
            refresh_generation: 0,
            refresh_tx,
            refresh_rx,
            reconnect: BTreeMap::from([(
                "local".to_owned(),
                HostReconnectState::new(Instant::now()),
            )]),
            reconnect_jitter_seed: 0,
            activity_pending: BTreeSet::new(),
            last_activity_poll: BTreeMap::new(),
            limit: 50,
            status: None,
            notice: None,
            preview_request_id: 0,
            preview: None,
        };
        app.rebuild_rows();
        app
    }

    use std::{
        collections::{BTreeMap, BTreeSet, HashSet},
        sync::mpsc::{self, TryRecvError},
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    use crate::reconnect::HostReconnectState;
}
