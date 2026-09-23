use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
};

use crate::{
    picker::{ALL_HOSTS, PickerApp, PreviewContent},
    reconnect::{HostPhase, HostReconnectState},
};

use super::{
    rows::working_pulse,
    text::{preview_window, truncate_with_ellipsis, wrap_preview_body},
    theme::{base_style, muted_style, palette, selection_style},
};

pub(super) fn draw_help(frame: &mut Frame<'_>) {
    let area = centered_rect(72, 84, frame.area());
    frame.render_widget(Clear, area);
    let lines = vec![
        Line::from("  j/k or arrows   move"),
        Line::from("  g/G             first / last"),
        Line::from("  enter           attach / resume; toggle selected group/section"),
        Line::from("  space/tab       expand / collapse selected group/section"),
        Line::from("  p               preview pane / response"),
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
                .fg(palette().text)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from("  ◌ cached / checking / offline"),
    ];
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .style(base_style())
                    .title(" Rollcall help ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(palette().blue)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

pub(super) fn draw_preview(frame: &mut Frame<'_>, app: &PickerApp) {
    let Some(preview) = app.preview.as_ref() else {
        return;
    };
    let area = centered_rect(90, 88, frame.area());
    frame.render_widget(Clear, area);
    let block = Block::default()
        .style(base_style())
        .title(" Session preview ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(palette().blue));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let (source, body, style) = match &preview.content {
        PreviewContent::Loading => (
            "capturing live tmux pane",
            format!("{} Loading preview…", working_pulse().0),
            Style::default().fg(palette().blue),
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
                    .fg(palette().yellow)
                    .add_modifier(Modifier::BOLD),
            )))
            .block(Block::default().borders(Borders::TOP)),
            sections[2],
        );
    }
}

pub(super) fn draw_hosts(frame: &mut Frame<'_>, app: &PickerApp) {
    let area = centered_rect(62, 70, frame.area());
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
            let (active, settled) = if host == ALL_HOSTS {
                (app.active.len(), app.settled.len())
            } else {
                (
                    app.active
                        .iter()
                        .filter(|session| session.host == *host)
                        .count(),
                    app.settled
                        .iter()
                        .filter(|session| session.host == *host)
                        .count(),
                )
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
            ListItem::new(format!(
                "{host:<24} {active} active · {settled} settled{current}{connectivity}"
            ))
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(
            Block::default()
                .style(base_style())
                .title(" Host filter ")
                .borders(Borders::ALL)
                .border_style(Style::default().fg(palette().blue)),
        )
        .highlight_symbol("› ")
        .highlight_style(selection_style());
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
