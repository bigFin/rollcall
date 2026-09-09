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
