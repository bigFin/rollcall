use std::{
    collections::BTreeMap,
    sync::mpsc::{self, TryRecvError},
};

use super::{app_with_sessions, session};
use crate::{
    agents,
    domain::{Activity, LiveObservation, RuntimeOwner},
    hosts::{ConnectivityState, SshHost},
    picker::{
        InputMode, PreviewContent, RefreshEvent,
        refresh::{
            deduplicate_discovery_targets, normalize_host_filter, restrict_discovery_targets,
            session_needs_detail,
        },
        write_transition,
    },
    store::{SessionTransition, SessionTransitionKind},
};

#[test]
fn archive_roundtrip_preserves_the_selected_session_when_a_projection_empties() {
    let candidate = session("019f", "/fabric", "Archive roundtrip");
    let id = candidate.id.clone();
    let mut app = app_with_sessions(vec![candidate], Vec::new());
    app.store
        .record(&app.active)
        .expect("session should be cached");
    app.query = "Archive roundtrip".to_owned();
    app.rebuild_rows();

    for archived in [true, false] {
        app.set_archived(&id, archived)
            .expect("archive state should change");
        assert_eq!(app.selected_session().map(|session| &session.id), Some(&id));
        assert_eq!(app.visible_session_count(), 1);
        assert_eq!(
            app.store.load(archived).expect("projection should load")[0].id,
            id
        );
        assert!(
            app.store
                .load(!archived)
                .expect("other projection should load")
                .is_empty()
        );
    }
}

#[test]
fn local_host_filter_uses_the_observed_hostname() {
    assert_eq!(
        normalize_host_filter("local"),
        agents::observed_host("local")
    );
    assert_eq!(normalize_host_filter("all"), "all");
    assert_eq!(normalize_host_filter("coda"), "coda");
}

#[test]
fn resumable_preview_uses_cached_response_without_starting_a_capture() {
    let mut candidate = session("019f", "/fabric", "Cached thread");
    candidate.runtime = RuntimeOwner::Resumable;
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
fn preview_refresh_ignores_stale_results_and_accepts_the_matching_capture() {
    let mut candidate = session("019f", "/fabric", "Preview refresh");
    candidate.runtime = RuntimeOwner::Resumable;
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
fn live_observations_update_working_and_completed_state() {
    let observed_host = agents::observed_host("local");
    let mut candidate = session("019f", "/fabric", "Live thread");
    candidate.host.clone_from(&observed_host);
    candidate.id = format!("{observed_host}:codex:019f");
    candidate.runtime = RuntimeOwner::ExternalFrontend;
    candidate.tmux = None;
    let session_id = candidate.id.clone();
    let mut app = app_with_sessions(vec![candidate], Vec::new());

    app.apply_live_observations(
        "local",
        &BTreeMap::from([(
            session_id.clone(),
            LiveObservation {
                activity: Activity::Working,
                runtime: RuntimeOwner::ExternalFrontend,
                tmux: None,
            },
        )]),
    )
    .expect("working observation should apply");
    assert_eq!(app.active[0].activity, Activity::Working);

    app.apply_live_observations(
        "local",
        &BTreeMap::from([(
            session_id,
            LiveObservation {
                activity: Activity::Completed,
                runtime: RuntimeOwner::ExternalFrontend,
                tmux: None,
            },
        )]),
    )
    .expect("completion observation should apply");
    assert_eq!(app.active[0].activity, Activity::Completed);
    assert!(app.unread_ids.contains(&app.active[0].id));
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
