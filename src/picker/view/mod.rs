mod overlays;
mod rows;
pub(in crate::picker) mod text;
pub(in crate::picker) mod theme;
mod tmux;

use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListState, Paragraph, Wrap},
};

use crate::{
    domain::RuntimeOwner,
    picker::{
        ALL_HOSTS, DashboardRow, DashboardSection, InputMode, PickerApp,
        dashboard::is_navigable_row,
    },
};

use self::{
    overlays::{draw_help, draw_hosts, draw_preview},
    rows::{activity_style, group_item, host_item, section_item, session_item},
    text::{compact_home, format_age, truncate_with_ellipsis},
    theme::{base_style, muted_style, palette, selection_style},
};

pub(super) fn draw(frame: &mut Frame<'_>, app: &PickerApp) {
    frame.render_widget(Block::default().style(base_style()), frame.area());
    if app.show_details && frame.area().height >= 20 {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(4),
                Constraint::Length(2),
            ])
            .split(frame.area());
        draw_header(frame, app, areas[0]);
        draw_sessions(frame, app, areas[1]);
        draw_selected_detail(frame, app, areas[2]);
        draw_footer(frame, app, areas[3]);
    } else {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(2),
            ])
            .split(frame.area());
        draw_header(frame, app, areas[0]);
        draw_sessions(frame, app, areas[1]);
        draw_footer(frame, app, areas[2]);
    }

    match app.input_mode {
        InputMode::Hosts => draw_hosts(frame, app),
        InputMode::Help => draw_help(frame),
        InputMode::Preview => draw_preview(frame, app),
        InputMode::Browse | InputMode::Search => {}
    }
}

fn draw_header(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    let counts = app.section_counts();
    let count = |section| counts.get(&section).copied().unwrap_or_default();
    let title = Line::from(vec![
        Span::styled(" Rollcall ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(
            format!(
                " {} {} ",
                DashboardSection::Current.label(),
                count(DashboardSection::Current)
            ),
            Style::default()
                .fg(palette().green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " {} {} ",
                DashboardSection::LastDay.label(),
                count(DashboardSection::LastDay)
            ),
            muted_style(),
        ),
        Span::styled(
            format!(
                " {} {} ",
                DashboardSection::LastWeek.label(),
                count(DashboardSection::LastWeek)
            ),
            muted_style(),
        ),
        Span::styled(
            format!(
                " {} {} ",
                DashboardSection::Archive.label(),
                count(DashboardSection::Archive)
            ),
            muted_style(),
        ),
        Span::styled(
            format!(
                "  {}",
                if app.host_filter == ALL_HOSTS {
                    "all hosts"
                } else {
                    &app.host_filter
                }
            ),
            muted_style(),
        ),
    ]);
    let mut subtitle = vec![Span::styled(
        format!("{} up", app.reachable_hosts.len()),
        Style::default().fg(palette().green),
    )];
    if !app.pending_hosts.is_empty() {
        subtitle.extend([
            Span::raw(" · "),
            Span::styled(
                format!("{} checking", app.pending_hosts.len()),
                Style::default().fg(palette().yellow),
            ),
        ]);
    }
    subtitle.extend([
        Span::raw(" · "),
        Span::styled(
            format!("{} down", app.host_errors.len()),
            if app.host_errors.is_empty() {
                muted_style()
            } else {
                Style::default().fg(palette().red)
            },
        ),
    ]);
    if !app.unread_ids.is_empty() {
        subtitle.extend([
            Span::raw(" · "),
            Span::styled(
                format!("{} unread", app.unread_ids.len()),
                Style::default()
                    .fg(palette().yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]);
    }
    if app.input_mode == InputMode::Search || !app.query.is_empty() {
        subtitle.extend([
            Span::raw("    "),
            Span::styled(
                format!("/{}", app.query),
                Style::default()
                    .fg(palette().blue)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(" · {} shown", app.visible_session_count()),
                muted_style(),
            ),
        ]);
    }
    frame.render_widget(
        Paragraph::new(vec![title, Line::from(subtitle)])
            .block(Block::default().borders(Borders::BOTTOM)),
        area,
    );
}

fn draw_sessions(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    let items = app
        .rows
        .iter()
        .map(|row| match row {
            DashboardRow::Section {
                section,
                count,
                expanded,
            } => section_item(*section, *count, *expanded),
            DashboardRow::Host {
                host,
                count,
                fresh,
                expanded,
                ..
            } => host_item(
                host,
                *count,
                app.group_connectivity(host, *fresh),
                *expanded,
                area.width,
            ),
            DashboardRow::Group {
                cwd,
                host,
                count,
                fresh,
                expanded,
                ..
            } => group_item(
                cwd,
                *count,
                app.group_connectivity(host, *fresh),
                *expanded,
                area.width,
            ),
            DashboardRow::Session(session_ref) => {
                let session = app.session_for_ref(*session_ref);
                session_item(
                    session,
                    app.fresh_ids.contains(&session.id),
                    app.unread_ids.contains(&session.id),
                    area.width,
                )
            }
        })
        .collect::<Vec<_>>();
    let list = List::new(items)
        .highlight_style(selection_style())
        .highlight_symbol("› ");
    let mut state = ListState::default();
    if app.rows.get(app.selected_row).is_some_and(is_navigable_row) {
        state.select(Some(app.selected_row));
    }
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_selected_detail(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    let Some(session) = app.selected_session() else {
        frame.render_widget(
            Paragraph::new("").block(Block::default().title(" Selected ").borders(Borders::TOP)),
            area,
        );
        return;
    };

    let (_, status_style, status) = if app.fresh_ids.contains(&session.id) {
        activity_style(session.activity)
    } else {
        ("◌", muted_style(), "cached")
    };
    let runtime = match session.runtime {
        RuntimeOwner::TmuxFrontend => session.tmux.as_ref().map_or_else(
            || "tmux".to_owned(),
            |binding| format!("tmux {}", binding.session),
        ),
        RuntimeOwner::SharedBackend => "Rollcall backend".to_owned(),
        RuntimeOwner::ExternalFrontend => "external frontend".to_owned(),
        RuntimeOwner::Resumable => "resumable".to_owned(),
    };
    let width = usize::from(area.width).saturating_sub(2);
    let title_width = width.saturating_sub(status.len() + 3);
    let message = if session.last_message.is_empty() {
        "No cached agent response yet."
    } else {
        &session.last_message
    };
    let metadata = format!(
        "{} / {} · {} · {} · {}{}",
        compact_home(&session.cwd),
        session.host,
        format_age(session.last_interaction_unix_seconds),
        runtime,
        session.native_session_id,
        if app.unread_ids.contains(&session.id) {
            " · unread"
        } else {
            ""
        }
    );
    let lines = vec![
        Line::from(vec![
            Span::styled(
                format!("{status:<9}"),
                status_style.add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate_with_ellipsis(&session.title, title_width),
                Style::default().add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            truncate_with_ellipsis(message, width),
            Style::default().fg(palette().text),
        )),
        Line::from(Span::styled(
            truncate_with_ellipsis(&metadata, width),
            muted_style(),
        )),
    ];
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .title(" Selected ")
                .borders(Borders::TOP)
                .border_style(muted_style()),
        ),
        area,
    );
}

fn draw_footer(frame: &mut Frame<'_>, app: &PickerApp, area: Rect) {
    let content = match app.input_mode {
        InputMode::Browse => {
            if let Some((notice, _)) = &app.notice {
                Line::from(Span::styled(
                    notice.clone(),
                    Style::default()
                        .fg(palette().green)
                        .add_modifier(Modifier::BOLD),
                ))
            } else if let Some(status) = &app.status {
                Line::from(Span::styled(
                    status.clone(),
                    Style::default().fg(palette().yellow),
                ))
            } else if let Some(archived) = app
                .selected_session()
                .and_then(|session| app.archived_state_for_session(&session.id))
            {
                Line::from(vec![
                    Span::styled("↵", Style::default().fg(palette().blue)),
                    Span::raw(if archived {
                        " restore+open  "
                    } else {
                        " open  "
                    }),
                    Span::styled("a", Style::default().fg(palette().blue)),
                    Span::raw(if archived { " restore  " } else { " settle  " }),
                    Span::styled("x", Style::default().fg(palette().blue)),
                    Span::raw(" read  "),
                    Span::styled("tab", Style::default().fg(palette().blue)),
                    Span::raw(" collapse group  "),
                    Span::styled("/", Style::default().fg(palette().blue)),
                    Span::raw(" search  "),
                    Span::styled("?", Style::default().fg(palette().blue)),
                    Span::raw(" keys"),
                ])
            } else {
                Line::from(vec![
                    Span::styled("↵/space/tab", Style::default().fg(palette().blue)),
                    Span::raw(" toggle row  "),
                    Span::styled("/", Style::default().fg(palette().blue)),
                    Span::raw(" search  "),
                    Span::styled("?", Style::default().fg(palette().blue)),
                    Span::raw(" keys"),
                ])
            }
        }
        InputMode::Search => Line::from(vec![
            Span::raw("type to filter  "),
            Span::styled("ctrl-u", Style::default().fg(palette().blue)),
            Span::raw(" clear  "),
            Span::styled("enter/esc", Style::default().fg(palette().blue)),
            Span::raw(" done"),
        ]),
        InputMode::Hosts => Line::from(vec![
            Span::styled("j/k", Style::default().fg(palette().blue)),
            Span::raw(" move  "),
            Span::styled("enter", Style::default().fg(palette().blue)),
            Span::raw(" filter  "),
            Span::styled("esc", Style::default().fg(palette().blue)),
            Span::raw(" close"),
        ]),
        InputMode::Help => Line::from(vec![
            Span::styled("?/enter/esc", Style::default().fg(palette().blue)),
            Span::raw(" close help"),
        ]),
        InputMode::Preview => Line::from(vec![
            Span::styled("enter", Style::default().fg(palette().blue)),
            Span::raw(" open  "),
            Span::styled("a", Style::default().fg(palette().blue)),
            Span::raw(
                app.preview
                    .as_ref()
                    .and_then(|preview| app.archived_state_for_session(&preview.session_id))
                    .map_or(" settle/restore  ", |archived| {
                        if archived { " restore  " } else { " settle  " }
                    }),
            ),
            Span::styled("x", Style::default().fg(palette().blue)),
            Span::raw(" read  "),
            Span::styled("j/k", Style::default().fg(palette().blue)),
            Span::raw(" scroll  "),
            Span::styled("esc", Style::default().fg(palette().blue)),
            Span::raw(" close"),
        ]),
    };
    frame.render_widget(
        Paragraph::new(content)
            .block(Block::default().borders(Borders::TOP))
            .wrap(Wrap { trim: true }),
        area,
    );
}
