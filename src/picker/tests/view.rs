use std::time::{SystemTime, UNIX_EPOCH};

use crate::picker::view::text::{
    format_age, preview_window, truncate_with_ellipsis, wrap_preview_body,
};

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
fn preview_window_starts_at_the_newest_lines_and_scrolls_toward_history() {
    let lines = wrap_preview_body("one\ntwo\nthree\nfour\nfive", 20);

    assert_eq!(preview_window(&lines, 3, 0), "three\nfour\nfive");
    assert_eq!(preview_window(&lines, 3, 1), "two\nthree\nfour");
    assert_eq!(preview_window(&lines, 3, usize::MAX), "one\ntwo\nthree");
    assert_eq!(wrap_preview_body("abcdefgh", 3), vec!["abc", "def", "gh"]);
}

#[test]
fn dense_rows_truncate_with_an_ellipsis() {
    assert_eq!(truncate_with_ellipsis("abcdef", 4), "abc…");
    assert_eq!(truncate_with_ellipsis("a", 1), "a");
    assert_eq!(truncate_with_ellipsis("ab", 1), "…");
}

#[test]
fn terminal_theme_keeps_default_background_across_views_and_sizes() {
    use crate::{
        domain::RuntimeOwner,
        picker::{InputMode, view},
    };
    use ratatui::{
        Terminal,
        backend::TestBackend,
        style::{Color, Modifier},
    };

    let mut candidate = super::session("theme", "/project", "Readable terminal title");
    candidate.runtime = RuntimeOwner::Resumable;
    candidate.tmux = None;
    let mut app = super::app_with_sessions(vec![candidate], Vec::new());
    app.start_preview();
    for (width, height) in [(100, 24), (42, 12), (24, 8)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        for mode in [
            InputMode::Browse,
            InputMode::Hosts,
            InputMode::Help,
            InputMode::Preview,
        ] {
            app.input_mode = mode;
            terminal.draw(|frame| view::draw(frame, &app)).unwrap();
            let cells = &terminal.backend().buffer().content;
            assert!(
                cells.iter().all(|cell| cell.bg == Color::Reset),
                "{mode:?} must not paint a theme background"
            );
            assert!(
                cells
                    .iter()
                    .all(|cell| !matches!(cell.fg, Color::Rgb(..) | Color::Indexed(_))),
                "{mode:?} must use the terminal palette"
            );
            if mode == InputMode::Browse && height == 24 {
                assert!(
                    cells
                        .iter()
                        .any(|cell| cell.modifier.contains(Modifier::REVERSED))
                );
            }
        }
    }
}

#[test]
fn table_columns_align_across_different_titles_and_activities() {
    use crate::{
        domain::{Activity, AgentKind},
        picker::view,
    };
    use ratatui::{Terminal, backend::TestBackend};

    let mut first = super::session("table-a", "/project", "Alpha title");
    first.activity = Activity::Working;
    first.last_message = "First message".to_owned();
    let mut second = super::session("table-b", "/project", "Wide 你好世界 title");
    second.agent = AgentKind::Pi;
    second.activity = Activity::WaitingApproval;
    second.last_message = "Second\nmessage".to_owned();
    let app = super::app_with_sessions(vec![first, second], Vec::new());
    let mut terminal = Terminal::new(TestBackend::new(120, 24)).unwrap();
    terminal.draw(|frame| view::draw(frame, &app)).unwrap();
    let buffer = terminal.backend().buffer();
    let text = |x: u16, y: u16, len: u16| {
        (x..x + len)
            .map(|x| buffer[(x, y)].symbol())
            .collect::<String>()
    };
    let column = |label: &str| {
        (0..120 - label.len() as u16)
            .find(|x| text(*x, 3, label.len() as u16) == label)
            .unwrap()
    };
    let harness = column("HARNESS");
    let activity = column("ACTIVITY");
    let updated = column("UPDATED");
    let message = column("LAST MESSAGE");
    for (title, agent, status, preview) in [
        ("Alpha", "codex", "working", "First message"),
        ("Wide", "pi", "approval", "Second message"),
    ] {
        let y = (4..22).find(|y| text(0, *y, 120).contains(title)).unwrap();
        assert_eq!(text(harness, y, 7).trim(), agent);
        assert_eq!(text(activity, y, 10).trim(), status);
        assert!(!text(updated, y, 7).trim().is_empty());
        assert!(text(message, y, 120 - message).starts_with(preview));
    }
    for (width, height) in [(80, 18), (60, 12), (24, 8), (8, 4)] {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| view::draw(frame, &app)).unwrap();
    }
}

#[test]
fn session_action_feedback_survives_refresh_and_then_expires() {
    use crate::picker::{NOTICE_DURATION, PickerApp, RefreshEvent, view};
    use ratatui::{Terminal, backend::TestBackend};

    let candidate = super::session("feedback", "/project", "Feedback target");
    let mut app = super::app_with_sessions(vec![candidate.clone()], Vec::new());
    app.store
        .record(&app.active)
        .expect("session should be cached");
    let mut terminal = Terminal::new(TestBackend::new(100, 14)).expect("terminal should open");
    let mut footer = |app: &PickerApp| {
        terminal
            .draw(|frame| view::draw(frame, app))
            .expect("picker should render");
        let buffer = terminal.backend().buffer();
        (0..100)
            .map(|x| buffer[(x, 13)].symbol())
            .collect::<String>()
    };

    for archived in [Some(true), Some(false), None] {
        match archived {
            Some(archived) => app
                .set_archived(&candidate.id, archived)
                .expect("archive should change"),
            None => app
                .acknowledge_session(&candidate.id, true)
                .expect("session should be acknowledged"),
        }
        assert!(footer(&app).contains(&candidate.title));
        app.apply_refresh_event(RefreshEvent::Detailed {
            generation: app.refresh_generation,
            target: candidate.host.clone(),
            sessions: vec![candidate.clone()],
        })
        .expect("inventory refresh should finish");
        assert!(
            footer(&app).contains(&candidate.title),
            "refresh must not erase action feedback"
        );

        app.notice.as_mut().expect("confirmation should expire").1 =
            std::time::Instant::now() - NOTICE_DURATION;
        app.expire_notice();
        assert!(
            !footer(&app).contains(&candidate.title),
            "expired feedback must yield to current status"
        );
    }
}
