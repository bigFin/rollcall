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
    assert!(session_matches(&session, "codex"));
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
fn different_activity_and_recency_share_one_host_and_project() {
    let mut working = session("working", "/project", "Working");
    working.activity = Activity::Working;
    let today = session("today", "/project", "Today");
    let mut week = session("week", "/project", "Week");
    week.last_interaction_unix_seconds = unix_now_seconds() - 172_800;
    let app = app_with_sessions(vec![working, today, week], Vec::new());
    assert_eq!(
        app.rows
            .iter()
            .filter(|row| matches!(row, DashboardRow::Host { .. }))
            .count(),
        1
    );
    assert_eq!(
        app.rows
            .iter()
            .filter(|row| matches!(row, DashboardRow::Group { .. }))
            .count(),
        1
    );
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
fn projects_stay_alphabetical_instead_of_jumping_when_activity_changes() {
    let now = unix_now_seconds();
    let mut completed = session("019f", "/newer", "Completed");
    completed.last_interaction_unix_seconds = now;
    let mut working = session("019e", "/older", "Working");
    working.activity = Activity::Working;
    working.last_interaction_unix_seconds = now - 100;
    let app = app_with_sessions(vec![completed, working], Vec::new());

    assert_eq!(
        app.selected_session().map(|session| session.title.as_str()),
        Some("Completed")
    );
}

#[test]
fn unread_markers_do_not_reorder_project_groups() {
    let completed = session("019f", "/zzz-completed", "Unread completion");
    let mut working = session("019e", "/aaa-working", "Working");
    working.activity = Activity::Working;
    let mut app = app_with_sessions(vec![working, completed], Vec::new());
    app.unread_ids.insert("topo:codex:019f".to_owned());
    app.rebuild_rows_selecting(None);

    assert_eq!(
        app.selected_session().map(|session| session.title.as_str()),
        Some("Working")
    );
}

#[test]
fn local_sessions_precede_newer_working_remote_sessions() {
    let local = crate::agents::observed_host("local");
    let mut local_session = session("local", "/project", "Local older session");
    local_session.host = local.clone();
    local_session.last_interaction_unix_seconds = unix_now_seconds() - 172_800;
    let mut remote = session("remote", "/project", "Remote working session");
    remote.host = "aaa-remote".to_owned();
    remote.activity = Activity::Working;
    let mut app = app_with_sessions(vec![remote, local_session], Vec::new());
    assert!(matches!(&app.rows[0], DashboardRow::Host { host, .. } if host == &local));
    assert_eq!(app.selected_session().unwrap().title, "Local older session");
    app.rebuild_host_choices();
    assert_eq!(&app.host_choices[..2], &["all".to_owned(), local]);
}

#[test]
fn recent_counts_include_live_unread_and_archived_sessions_and_respect_search() {
    let mut working = session("working", "/project", "Recent working");
    working.activity = Activity::Working;
    let unread = session("unread", "/project", "Recent unread");
    let archived = session("archived", "/project", "Recent archived");
    let mut week = session("week", "/project", "Earlier this week");
    week.last_interaction_unix_seconds = unix_now_seconds() - 172_800;
    week.runtime = crate::domain::RuntimeOwner::Resumable;
    let mut app = app_with_sessions(vec![working, unread, week], vec![archived]);
    app.unread_ids.insert("topo:codex:unread".to_owned());
    let counts = app.section_counts();
    assert_eq!(counts[&DashboardSection::LastDay], 3);
    assert_eq!(counts[&DashboardSection::LastWeek], 4);
    assert_eq!(counts[&DashboardSection::Current], 3);
    assert_eq!(counts[&DashboardSection::Archive], 1);
    app.query = "recent working".to_owned();
    assert_eq!(app.section_counts()[&DashboardSection::LastDay], 1);
    app.host_filter = "no-such-host".to_owned();
    assert!(app.section_counts().is_empty());
}

#[test]
fn equal_recency_has_a_stable_id_tiebreaker() {
    let first = session("a", "/project", "First");
    let mut second = session("b", "/project", "Second");
    second.last_interaction_unix_seconds = first.last_interaction_unix_seconds;
    let mut app = app_with_sessions(vec![second, first], Vec::new());
    assert_eq!(app.selected_session().unwrap().title, "First");
    app.active.reverse();
    app.rebuild_rows_selecting(None);
    assert_eq!(app.selected_session().unwrap().title, "First");
}
