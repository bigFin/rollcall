use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::ListItem,
};

use crate::{
    domain::{Activity, Session},
    picker::{DashboardSection, GroupConnectivity},
};

use super::{
    text::{display_directory, format_age, truncate_with_ellipsis},
    theme::{muted_style, palette},
};

pub(super) fn section_item(
    section: DashboardSection,
    count: usize,
    expanded: bool,
) -> ListItem<'static> {
    let marker = if expanded { "▾" } else { "▸" };
    let style = match section {
        DashboardSection::Current => Style::default()
            .fg(palette::GREEN)
            .add_modifier(Modifier::BOLD),
        DashboardSection::LastDay | DashboardSection::LastWeek | DashboardSection::Archive => {
            Style::default().add_modifier(Modifier::BOLD)
        }
    };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{marker} "), style),
        Span::styled(section.label(), style),
        Span::styled(format!(" · {count}"), muted_style()),
    ]))
}

pub(super) fn host_item(
    host: &str,
    count: usize,
    connectivity: GroupConnectivity,
    expanded: bool,
    width: u16,
) -> ListItem<'static> {
    let (connectivity_label, connectivity_style) = connectivity_label(connectivity);
    let suffix = format!(" · {count}{connectivity_label}");
    let host_width = usize::from(width)
        .saturating_sub(4 + suffix.chars().count())
        .max(8);
    let marker = if expanded { "▾" } else { "▸" };
    ListItem::new(Line::from(vec![
        Span::styled(
            format!("  {marker} "),
            connectivity_style.add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate_with_ellipsis(host, host_width),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(suffix, connectivity_style),
    ]))
}

pub(super) fn group_item(
    cwd: &str,
    count: usize,
    connectivity: GroupConnectivity,
    expanded: bool,
    width: u16,
) -> ListItem<'static> {
    let (connectivity_label, connectivity_style) = connectivity_label(connectivity);
    let suffix = format!(" · {count}{connectivity_label}");
    let directory = display_directory(cwd);
    let cwd_width = usize::from(width)
        .saturating_sub(6 + suffix.chars().count())
        .max(8);
    let marker = if expanded { "▾" } else { "▸" };
    ListItem::new(Line::from(vec![
        Span::styled(format!("    {marker} "), connectivity_style),
        Span::styled(
            truncate_with_ellipsis(&directory, cwd_width),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(suffix, connectivity_style),
    ]))
}

fn connectivity_label(connectivity: GroupConnectivity) -> (String, Style) {
    match connectivity {
        GroupConnectivity::Online => (String::new(), Style::default().fg(palette::GREEN)),
        GroupConnectivity::Checking => (
            " · checking".to_owned(),
            Style::default().fg(palette::YELLOW),
        ),
        GroupConnectivity::Offline(retry_in) => (
            retry_in.map_or_else(
                || " · offline".to_owned(),
                |seconds| format!(" · offline · retry {seconds}s"),
            ),
            Style::default().fg(palette::RED),
        ),
        GroupConnectivity::Blocked => (
            " · blocked".to_owned(),
            Style::default()
                .fg(palette::RED)
                .add_modifier(Modifier::BOLD),
        ),
        GroupConnectivity::Cached => (" · cached".to_owned(), muted_style()),
    }
}

pub(super) fn session_item(
    session: &Session,
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
        (true, Activity::WaitingApproval) => "approval ",
        (true, Activity::WaitingInput) => "input ",
        (true, Activity::Failed) => "failed ",
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
                    .fg(palette::YELLOW)
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
        if session.activity == Activity::Working && fresh {
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

pub(super) fn activity_style(activity: Activity) -> (&'static str, Style, &'static str) {
    match activity {
        Activity::Working => {
            let (marker, color) = working_pulse();
            (marker, Style::default().fg(color), "working")
        }
        Activity::WaitingApproval => ("◆", Style::default().fg(palette::YELLOW), "approval"),
        Activity::WaitingInput => ("◆", Style::default().fg(palette::YELLOW), "input"),
        Activity::Completed => ("✓", muted_style(), "completed"),
        Activity::Failed => ("!", Style::default().fg(palette::RED), "failed"),
        Activity::Unknown => ("○", muted_style(), "unknown"),
    }
}

pub(super) fn working_pulse() -> (&'static str, Color) {
    let frame = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_millis() / 250 % 4);
    let frame = usize::try_from(frame).unwrap_or_default();
    (
        ["◐", "◓", "◑", "◒"][frame],
        [palette::GREEN, palette::AQUA, palette::GREEN, palette::BLUE][frame],
    )
}
