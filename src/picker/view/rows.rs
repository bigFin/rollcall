use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::{
    layout::Constraint,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Cell, Row},
};

use crate::{
    domain::{Activity, Session},
    picker::{DashboardSection, GroupConnectivity},
};

use super::{
    text::{compact_home, format_age},
    theme::{muted_style, palette},
};

/// One layout for the header, groups, and sessions; never size by row content.
pub(super) struct Columns {
    title: usize,
    harness: bool,
    state: bool,
    age: bool,
    message: usize,
}

impl Columns {
    pub(super) fn new(width: u16) -> Self {
        let width = usize::from(width).saturating_sub(2); // selection marker
        let harness = width >= 74;
        let state = width >= 58;
        let age = width >= 22;
        let has_message = width >= 98;
        let fixed = usize::from(harness) * 9 + usize::from(state) * 12 + usize::from(age) * 9;
        let remaining = width.saturating_sub(fixed + usize::from(has_message) * 2);
        let title = if has_message {
            (remaining / 2).min(48)
        } else {
            remaining
        };
        Self {
            title,
            harness,
            state,
            age,
            message: if has_message { remaining - title } else { 0 },
        }
    }

    pub(super) fn widths(&self) -> Vec<Constraint> {
        let mut widths = vec![Constraint::Length(self.title as u16)];
        if self.harness {
            widths.push(Constraint::Length(7));
        }
        if self.state {
            widths.push(Constraint::Length(10));
        }
        if self.age {
            widths.push(Constraint::Length(7));
        }
        if self.message > 0 {
            widths.push(Constraint::Length(self.message as u16));
        }
        widths
    }

    pub(super) fn header(&self) -> Row<'static> {
        self.row(
            Line::raw("SESSION / PROJECT"),
            "HARNESS",
            Line::raw("ACTIVITY"),
            "UPDATED",
            "LAST MESSAGE",
        )
        .style(muted_style().add_modifier(Modifier::BOLD))
    }

    fn row(
        &self,
        title: Line<'static>,
        harness: &str,
        state: Line<'static>,
        age: &str,
        message: &str,
    ) -> Row<'static> {
        let mut cells = vec![Cell::from(title)];
        if self.harness {
            cells.push(Cell::from(harness.to_owned()).style(muted_style()));
        }
        if self.state {
            cells.push(Cell::from(state));
        }
        if self.age {
            cells.push(Cell::from(age.to_owned()).style(muted_style()));
        }
        if self.message > 0 {
            cells.push(Cell::from(fit(message, self.message)).style(muted_style()));
        }
        Row::new(cells)
    }

    fn title(&self, prefix: &str, text: &str, style: Style) -> Line<'static> {
        let prefix_width = Line::raw(prefix).width();
        Line::from(vec![
            Span::styled(prefix.to_owned(), style),
            Span::styled(fit(text, self.title.saturating_sub(prefix_width)), style),
        ])
    }
}

// A table cell must stay on one line and be bounded in terminal cells, not bytes
// or Unicode scalar count (wide CJK characters otherwise overflow columns).
fn fit(text: &str, width: usize) -> String {
    let clean = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if Line::raw(clean.as_str()).width() <= width {
        return clean;
    }
    if width == 0 {
        return String::new();
    }
    let mut end = 0;
    for (offset, character) in clean.char_indices() {
        let next = offset + character.len_utf8();
        if Line::raw(&clean[..next]).width() >= width {
            break;
        }
        end = next;
    }
    format!("{}…", &clean[..end])
}

pub(super) fn section_item(
    section: DashboardSection,
    count: usize,
    expanded: bool,
    columns: &Columns,
) -> Row<'static> {
    let marker = if expanded { "▾ " } else { "▸ " };
    columns.row(
        columns.title(
            marker,
            &format!("{} ({count})", section.label()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        "",
        Line::default(),
        "",
        "",
    )
}

pub(super) fn host_item(
    host: &str,
    local: bool,
    count: usize,
    connectivity: GroupConnectivity,
    expanded: bool,
    columns: &Columns,
) -> Row<'static> {
    let (label, style) = connectivity_label(connectivity);
    let marker = if expanded { "▾ " } else { "▸ " };
    let name = format!("{host}{} ({count})", if local { " (local)" } else { "" });
    columns.row(
        columns.title(marker, &name, Style::default().add_modifier(Modifier::BOLD)),
        "",
        Line::styled(label, style),
        "",
        "",
    )
}

pub(super) fn group_item(
    cwd: &str,
    count: usize,
    expanded: bool,
    columns: &Columns,
) -> Row<'static> {
    let marker = if expanded { "  ▾ " } else { "  ▸ " };
    columns.row(
        columns.title(
            marker,
            &format!("{} ({count})", compact_home(cwd)),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        "",
        Line::default(),
        "",
        "",
    )
}

fn connectivity_label(connectivity: GroupConnectivity) -> (&'static str, Style) {
    match connectivity {
        GroupConnectivity::Online => ("online", Style::default().fg(palette().green)),
        GroupConnectivity::Checking => ("checking", Style::default().fg(palette().yellow)),
        GroupConnectivity::Offline(_) => ("offline", Style::default().fg(palette().red)),
        GroupConnectivity::Blocked => ("blocked", Style::default().fg(palette().red)),
        GroupConnectivity::Cached => ("cached", muted_style()),
    }
}

pub(super) fn session_item(
    session: &Session,
    fresh: bool,
    unread: bool,
    columns: &Columns,
) -> Row<'static> {
    let (marker, style, label) = if fresh {
        activity_style(session.activity)
    } else {
        ("◌", muted_style(), "cached")
    };
    let marker = if unread { "•" } else { marker };
    let style = if unread {
        Style::default().fg(palette().yellow)
    } else {
        style
    };
    let prefix = format!("    {marker} ");
    let mut title = columns.title(&prefix, &session.title, Style::default());
    title.spans[0].style = style;
    columns.row(
        title,
        session.agent.as_str(),
        Line::styled(label, style),
        &format_age(session.last_interaction_unix_seconds),
        &session.last_message,
    )
}

pub(super) fn activity_style(activity: Activity) -> (&'static str, Style, &'static str) {
    match activity {
        Activity::Working => {
            let (marker, color) = working_pulse();
            (marker, Style::default().fg(color), "working")
        }
        Activity::WaitingApproval => ("◆", Style::default().fg(palette().yellow), "approval"),
        Activity::WaitingInput => ("◆", Style::default().fg(palette().yellow), "input"),
        Activity::Completed => ("✓", muted_style(), "completed"),
        Activity::Failed => ("!", Style::default().fg(palette().red), "failed"),
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
        [
            palette().green,
            palette().aqua,
            palette().green,
            palette().blue,
        ][frame],
    )
}

#[cfg(test)]
mod tests {
    use super::fit;
    #[test]
    fn table_text_is_single_line_and_fits_wide_characters() {
        assert_eq!(fit("hello\nworld", 20), "hello world");
        assert_eq!(fit("你好世界", 5), "你好…");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("hello", 0), "");
    }
}
