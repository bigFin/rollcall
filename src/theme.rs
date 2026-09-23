use std::{env, process::Command};

use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier},
};

/// Terminal colors remain the fallback; tmux supplies only explicitly set colors.
#[derive(Debug, Default)]
pub(crate) struct Theme {
    foreground: Option<Color>,
    background: Option<Color>,
    accent: Option<Color>,
    attention: Option<Color>,
    error: Option<Color>,
    selection: Option<(Color, Color)>,
}

impl Theme {
    pub(crate) fn detect() -> Self {
        if env::var_os("TMUX").is_none() {
            return Self::default();
        }
        let output = Command::new("tmux")
            .args(["display-message", "-p", "#{popup-style}\n#{status-style}\n#{window-status-current-style}\n#{window-status-activity-style}\n#{window-status-bell-style}\n#{popup-border-style}"])
            .output();
        match output {
            Ok(output) if output.status.success() => {
                Self::from_styles(&String::from_utf8_lossy(&output.stdout))
            }
            _ => Self::default(),
        }
    }

    fn from_styles(styles: &str) -> Self {
        let mut lines = styles.lines();
        let popup = lines.next().unwrap_or_default();
        let status = lines.next().unwrap_or_default();
        let selected = lines.next().unwrap_or_default();
        let activity = lines.next().unwrap_or_default();
        let bell = lines.next().unwrap_or_default();
        let border = lines.next().unwrap_or_default();
        let selection = color(selected, "fg").zip(color(selected, "bg"));
        Self {
            foreground: color(popup, "fg").or_else(|| color(status, "fg")),
            background: color(popup, "bg").or_else(|| color(status, "bg")),
            accent: color(border, "fg").or_else(|| color(selected, "bg")),
            attention: color(activity, "fg"),
            error: color(bell, "fg"),
            selection,
        }
    }

    pub(crate) fn apply(&self, buffer: &mut Buffer) {
        for cell in &mut buffer.content {
            if cell.modifier.contains(Modifier::REVERSED)
                && let Some((foreground, background)) = self.selection
            {
                cell.modifier.remove(Modifier::REVERSED);
                cell.fg = foreground;
                cell.bg = background;
                continue;
            }
            cell.fg = match cell.fg {
                Color::Reset | Color::Gray => self.foreground.unwrap_or(cell.fg),
                Color::Cyan => self.accent.unwrap_or(cell.fg),
                Color::Yellow => self.attention.unwrap_or(cell.fg),
                Color::Red => self.error.unwrap_or(cell.fg),
                other => other,
            };
            if cell.bg == Color::Reset {
                cell.bg = self.background.unwrap_or(Color::Reset);
            }
        }
    }
}

fn color(style: &str, component: &str) -> Option<Color> {
    let value = style.split(',').rev().find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == component).then_some(value)
    })?;
    if value == "default" {
        return None;
    }
    if let Some(index) = value
        .strip_prefix("colour")
        .or_else(|| value.strip_prefix("color"))
    {
        return index.parse().ok().map(Color::Indexed);
    }
    value.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Rect;

    #[test]
    fn accepts_rgb_named_and_indexed_tmux_colors() {
        assert_eq!(
            color("fg=#c4a7e7,bold", "fg"),
            Some(Color::Rgb(196, 167, 231))
        );
        assert_eq!(color("bg=colour234", "bg"), Some(Color::Indexed(234)));
        assert_eq!(color("fg=red", "fg"), Some(Color::Red));
        assert_eq!(color("fg=default", "fg"), None);
        assert_eq!(color("fg=nonsense", "fg"), None);
    }

    #[test]
    fn no_tmux_style_keeps_terminal_palette_and_reverse_selection() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].fg = Color::Cyan;
        buffer[(1, 0)].modifier = Modifier::REVERSED;
        let before = buffer.clone();
        Theme::default().apply(&mut buffer);
        assert_eq!(buffer, before);
    }

    #[test]
    fn applies_tmux_background_accent_and_readable_selection() {
        let theme = Theme::from_styles(
            "fg=#d3c6aa,bg=#232a2e\nbg=green\nfg=#232a2e,bg=#c4a7e7,bold\nfg=yellow\nfg=red\nfg=#c4a7e7",
        );
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].fg = Color::Cyan;
        buffer[(1, 0)].modifier = Modifier::REVERSED | Modifier::BOLD;
        theme.apply(&mut buffer);
        assert_eq!(buffer[(0, 0)].fg, Color::Rgb(196, 167, 231));
        assert_eq!(buffer[(0, 0)].bg, Color::Rgb(35, 42, 46));
        assert_eq!(buffer[(1, 0)].fg, Color::Rgb(35, 42, 46));
        assert_eq!(buffer[(1, 0)].bg, Color::Rgb(196, 167, 231));
        assert_eq!(buffer[(1, 0)].modifier, Modifier::BOLD);
    }
}
