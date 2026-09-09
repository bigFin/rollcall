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
