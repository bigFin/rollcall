use super::{app_with_sessions, session};
use crate::{
    domain::Activity,
    picker::{
        DashboardRow, DashboardSection, DashboardSelection, WEEK_SECONDS,
        dashboard::{dashboard_section, session_matches, unix_now_seconds},
    },
};

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
fn sessions_are_nested_beneath_directory_and_host_groups() {
    let mut other_host = session("019d", "/tmp/project", "Three");
    other_host.host = "coda".to_owned();
    other_host.id = "coda:codex:019d".to_owned();
    let app = app_with_sessions(
        vec![
            session("019f", "/fabric", "One"),
            session("019e", "/fabric", "Two"),
            other_host,
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
    assert_eq!(
        app.rows
            .iter()
            .filter(|row| matches!(row, DashboardRow::Host { .. }))
            .count(),
        2
    );
    let host_row = app
        .rows
        .iter()
        .position(|row| matches!(row, DashboardRow::Host { .. }))
        .expect("host group should be rendered");
    let project_row = app
        .rows
        .iter()
        .position(|row| matches!(row, DashboardRow::Group { .. }))
        .expect("project group should be rendered");
    assert!(host_row < project_row);
    assert!(matches!(
        app.rows.get(project_row + 1),
        Some(DashboardRow::Session(_))
    ));
    assert_eq!(app.visible_session_count(), 3);
}

#[test]
fn dashboard_sections_prioritize_live_activity_then_recency() {
    let now = 2_000_000;
    let mut current = session("current", "/fabric", "Current");
    current.activity = Activity::Working;
    let mut today = session("today", "/fabric", "Today");
    today.last_interaction_unix_seconds = now - 60;
    let mut week = session("week", "/fabric", "Week");
    week.last_interaction_unix_seconds = now - 86_401;
    let archived = session("archived", "/fabric", "Archive");

    assert_eq!(
        dashboard_section(&current, false, false, now),
        DashboardSection::Current
    );
    assert_eq!(
        dashboard_section(&today, false, false, now),
        DashboardSection::LastDay
    );
    assert_eq!(
        dashboard_section(&week, false, false, now),
        DashboardSection::LastWeek
    );
    assert_eq!(
        dashboard_section(&archived, true, false, now),
        DashboardSection::Archive
    );

    let mut old = session("old", "/fabric", "Old");
    old.last_interaction_unix_seconds = now - WEEK_SECONDS - 1;
    assert_eq!(
        dashboard_section(&old, false, false, now),
        DashboardSection::Archive
    );
}

#[test]
fn archive_is_present_but_collapsed_by_default() {
    let app = app_with_sessions(Vec::new(), vec![session("019f", "/fabric", "Archive")]);

    assert_eq!(app.visible_session_count(), 0);
    assert!(app.rows.iter().any(|row| matches!(
        row,
        DashboardRow::Section {
            section: DashboardSection::Archive,
            expanded: false,
            count: 1,
        }
    )));
}

#[test]
fn archive_section_can_be_expanded_without_changing_other_buckets() {
    let mut app = app_with_sessions(
        vec![session("019e", "/fabric", "Recent")],
        vec![session("019f", "/fabric", "Archive")],
    );
    app.expanded_sections.insert(DashboardSection::Archive);
    app.rebuild_rows();

    assert_eq!(app.visible_session_count(), 2);
    assert_eq!(
        app.selected_session().map(|session| session.title.as_str()),
        Some("Recent")
    );
}

#[test]
fn progressive_refresh_keeps_the_same_session_selected_after_reordering() {
    let now = unix_now_seconds();
    let mut first = session("019f", "/fabric", "First");
    first.last_interaction_unix_seconds = now;
    let mut second = session("019e", "/fabric", "Second");
    second.last_interaction_unix_seconds = now.saturating_sub(1);
    let mut app = app_with_sessions(vec![first, second], Vec::new());
    app.select_next();
    let selected_id = app
        .selected_session()
        .map(|session| session.id.clone())
        .expect("a session should be selected");

    app.active.reverse();
    app.rebuild_rows_selecting(Some(DashboardSelection::Session(selected_id.clone())));

    assert_eq!(
        app.selected_session().map(|session| session.id.as_str()),
        Some(selected_id.as_str())
    );
}

#[test]
fn working_sessions_sort_ahead_of_newer_completed_sessions() {
    let mut completed = session("019f", "/newer", "Completed");
    completed.last_interaction_unix_seconds = 200;
    let mut working = session("019e", "/older", "Working");
    working.activity = Activity::Working;
    working.last_interaction_unix_seconds = 100;
    let app = app_with_sessions(vec![completed, working], Vec::new());

    assert_eq!(
        app.selected_session().map(|session| session.title.as_str()),
        Some("Working")
    );
}

#[test]
fn unread_attention_stays_ahead_of_other_current_sessions() {
    let completed = session("019f", "/completed", "Unread completion");
    let mut working = session("019e", "/working", "Working");
    working.activity = Activity::Working;
    let mut app = app_with_sessions(vec![working, completed], Vec::new());
    app.unread_ids.insert("topo:codex:019f".to_owned());
    app.rebuild_rows_selecting(None);

    assert_eq!(
        app.selected_session().map(|session| session.title.as_str()),
        Some("Unread completion")
    );
}
