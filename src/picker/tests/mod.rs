mod dashboard;
mod input;
mod refresh;
mod view;

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    sync::mpsc,
    time::Instant,
};

use crate::{
    domain::{Activity, AgentKind, RuntimeOwner, Session, TmuxBinding},
    picker::{DashboardSection, InputMode, PickerApp, dashboard::unix_now_seconds},
    reconnect::HostReconnectState,
    store::Store,
};

fn session(id: &str, cwd: &str, title: &str) -> Session {
    let now = unix_now_seconds();
    Session {
        id: format!("topo:codex:{id}"),
        host: "topo".to_owned(),
        agent: AgentKind::Codex,
        native_session_id: id.to_owned(),
        title: title.to_owned(),
        cwd: cwd.to_owned(),
        source: "cli".to_owned(),
        activity: Activity::Completed,
        last_message: "The implementation is ready.".to_owned(),
        last_interaction_unix_seconds: now,
        updated_unix_seconds: now,
        runtime: RuntimeOwner::TmuxFrontend,
        tmux: Some(TmuxBinding {
            session: "agents".to_owned(),
            pane: "%3".to_owned(),
        }),
    }
}

fn app_with_sessions(active: Vec<Session>, settled: Vec<Session>) -> PickerApp {
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
        expanded_sections: BTreeSet::from([
            DashboardSection::Current,
            DashboardSection::LastDay,
            DashboardSection::LastWeek,
        ]),
        collapsed_groups: HashSet::new(),
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
        reconnect: BTreeMap::from([("local".to_owned(), HostReconnectState::new(Instant::now()))]),
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
