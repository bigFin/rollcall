use super::{app_with_sessions, session};
use crate::{
    domain::{Activity, RuntimeOwner},
    picker::{Action, DashboardRow, DashboardSection, InputMode},
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[test]
fn foreign_frontend_cannot_be_attached_without_managed_tmux() {
    let mut loaded = session("019f", "/fabric", "Loaded thread");
    loaded.runtime = RuntimeOwner::ExternalFrontend;
    loaded.tmux = None;
    loaded.activity = Activity::Working;
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
fn quick_host_scope_keys_cycle_and_reset_the_filter() {
    let mut app = app_with_sessions(vec![session("019f", "/fabric", "Topo")], Vec::new());
    app.host_choices = vec!["all".to_owned(), "coda".to_owned(), "topo".to_owned()];
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE)),
        Action::SetHostFilter("coda".to_owned())
    );
    app.set_host_filter("coda".to_owned());
    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE)),
        Action::SetHostFilter("all".to_owned())
    );
}

#[test]
fn completed_foreign_frontend_is_also_left_alone() {
    let mut loaded = session("019f", "/fabric", "Loaded thread");
    loaded.runtime = RuntimeOwner::ExternalFrontend;
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
    loaded.runtime = RuntimeOwner::SharedBackend;
    loaded.tmux = None;
    let mut app = app_with_sessions(vec![loaded], Vec::new());

    assert_eq!(
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        Action::Attach("topo:codex:019f".to_owned())
    );
}

#[test]
fn host_groups_can_collapse_without_losing_project_state() {
    let mut app = app_with_sessions(vec![session("019f", "/fabric", "One")], Vec::new());
    let host_row = app
        .rows
        .iter()
        .position(|row| matches!(row, DashboardRow::Host { .. }))
        .expect("host group should be rendered");
    app.selected_row = host_row;

    app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));

    assert!(matches!(
        app.rows.get(host_row),
        Some(DashboardRow::Host {
            expanded: false,
            ..
        })
    ));
    assert_eq!(app.visible_session_count(), 0);
    assert!(app.selected_group_key().is_some());
}

#[test]
fn search_auto_expands_collapsed_archive_and_selects_match() {
    let mut app = app_with_sessions(
        Vec::new(),
        vec![session("019f", "/fabric", "Searchable archive")],
    );
    app.input_mode = InputMode::Search;

    for character in "searchable".chars() {
        app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
    }

    assert_eq!(app.visible_session_count(), 1);
    assert_eq!(
        app.selected_session().map(|session| session.title.as_str()),
        Some("Searchable archive")
    );
    assert!(app.rows.iter().any(|row| matches!(
        row,
        DashboardRow::Section {
            section: DashboardSection::Archive,
            expanded: true,
            count: 1,
        }
    )));
    assert!(
        app.rows
            .iter()
            .any(|row| matches!(row, DashboardRow::Group { expanded: true, .. }))
    );
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
fn preview_archive_action_uses_the_current_projection() {
    let mut candidate = session("019f", "/fabric", "Moved thread");
    candidate.runtime = RuntimeOwner::Resumable;
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
    candidate.runtime = RuntimeOwner::ExternalFrontend;
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
fn tab_toggles_the_selected_project_group() {
    let mut active = session("019f", "/fabric", "Active");
    active.activity = Activity::Working;
    let mut app = app_with_sessions(vec![active], vec![session("019e", "/fabric", "Settled")]);

    assert_eq!(
        app.selected_session().map(|session| session.title.as_str()),
        Some("Active")
    );
    app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));

    assert!(app.rows.iter().any(|row| matches!(
        row,
        DashboardRow::Group {
            cwd,
            expanded: false,
            ..
        } if cwd == "/fabric"
    )));
    assert_eq!(app.visible_session_count(), 0);
    assert!(app.selected_group_key().is_some());
    let group = app
        .selected_group_key()
        .expect("collapsed project group should remain selected");
    app.active.reverse();
    app.rebuild_rows();
    assert!(app.collapsed_groups.contains(&group));
    assert!(app.rows.iter().any(|row| matches!(
        row,
        DashboardRow::Group {
            expanded: false,
            ..
        }
    )));
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
