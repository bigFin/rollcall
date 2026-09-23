use std::{env, io, sync::OnceLock};

use ratatui::{
    buffer::Buffer,
    style::{Color, Modifier, Style},
};

use super::tmux;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Theme {
    #[default]
    Terminal,
    Everforest,
    Tmux,
}

pub(super) struct Palette {
    pub(super) background: Color,
    pub(super) text: Color,
    muted: Color,
    selection: Color,
    pub(super) green: Color,
    pub(super) aqua: Color,
    pub(super) blue: Color,
    pub(super) yellow: Color,
    pub(super) red: Color,
}

const TERMINAL: Palette = Palette {
    background: Color::Reset,
    text: Color::Reset,
    muted: Color::Reset,
    selection: Color::Reset,
    green: Color::Green,
    aqua: Color::Cyan,
    blue: Color::Blue,
    yellow: Color::Yellow,
    red: Color::Red,
};

// Everforest Dark, medium contrast: https://github.com/sainnhe/everforest
const EVERFOREST: Palette = Palette {
    background: Color::Rgb(0x2d, 0x35, 0x3b),
    text: Color::Rgb(0xd3, 0xc6, 0xaa),
    muted: Color::Rgb(0x9d, 0xa9, 0xa0),
    selection: Color::Rgb(0x42, 0x50, 0x47),
    green: Color::Rgb(0xa7, 0xc0, 0x80),
    aqua: Color::Rgb(0x83, 0xc0, 0x92),
    blue: Color::Rgb(0x7f, 0xbb, 0xb3),
    yellow: Color::Rgb(0xdb, 0xbc, 0x7f),
    red: Color::Rgb(0xe6, 0x7e, 0x80),
};

static THEME: OnceLock<Theme> = OnceLock::new();
static TMUX_THEME: OnceLock<tmux::Theme> = OnceLock::new();

// Resolve once before entering the alternate screen. Return the canonical name
// so popup callers can explicitly forward it through tmux's server environment.
pub(in crate::picker) fn init() -> io::Result<&'static str> {
    let value = match env::var("ROLLCALL_THEME") {
        Ok(value) => Some(value),
        Err(env::VarError::NotPresent) => None,
        Err(error) => return Err(io::Error::new(io::ErrorKind::InvalidInput, error)),
    };
    let theme = Theme::parse(value.as_deref())?;
    Ok(match THEME.get_or_init(|| theme) {
        Theme::Terminal => "terminal",
        Theme::Everforest => "everforest",
        Theme::Tmux => {
            TMUX_THEME.get_or_init(tmux::Theme::detect);
            "tmux"
        }
    })
}

impl Theme {
    fn parse(value: Option<&str>) -> io::Result<Self> {
        match value
            .unwrap_or("terminal")
            .trim()
            .to_ascii_lowercase()
            .as_str()
        {
            "" | "terminal" => Ok(Self::Terminal),
            "everforest" => Ok(Self::Everforest),
            "tmux" => Ok(Self::Tmux),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ROLLCALL_THEME must be terminal, everforest, or tmux",
            )),
        }
    }

    const fn palette(self) -> &'static Palette {
        match self {
            Self::Terminal | Self::Tmux => &TERMINAL,
            Self::Everforest => &EVERFOREST,
        }
    }

    fn base_style(self) -> Style {
        Style::default()
            .fg(self.palette().text)
            .bg(self.palette().background)
    }

    fn selection_style(self) -> Style {
        let style = Style::default()
            .fg(self.palette().text)
            .bg(self.palette().selection)
            .remove_modifier(Modifier::DIM)
            .add_modifier(Modifier::BOLD);
        match self {
            Self::Terminal | Self::Tmux => style.add_modifier(Modifier::REVERSED),
            Self::Everforest => style,
        }
    }

    fn muted_style(self) -> Style {
        let style = Style::default().fg(self.palette().muted);
        match self {
            Self::Terminal | Self::Tmux => style.add_modifier(Modifier::DIM),
            Self::Everforest => style,
        }
    }
}

fn current() -> Theme {
    THEME.get().copied().unwrap_or_default()
}

pub(in crate::picker) fn apply(buffer: &mut Buffer) {
    if let Some(theme) = TMUX_THEME.get() {
        theme.apply(buffer);
    }
}

pub(super) fn palette() -> &'static Palette {
    current().palette()
}

pub(super) fn base_style() -> Style {
    current().base_style()
}

pub(super) fn selection_style() -> Style {
    current().selection_style()
}

pub(super) fn muted_style() -> Style {
    current().muted_style()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_is_default_and_unknown_themes_are_rejected() {
        assert_eq!(Theme::parse(None).unwrap(), Theme::Terminal);
        assert_eq!(Theme::parse(Some("")).unwrap(), Theme::Terminal);
        assert_eq!(
            Theme::parse(Some(" EVERFOREST ")).unwrap(),
            Theme::Everforest
        );
        assert_eq!(Theme::parse(Some("tmux")).unwrap(), Theme::Tmux);
        assert!(Theme::parse(Some("everforrest")).is_err());
    }

    #[test]
    fn terminal_styles_preserve_defaults_and_use_palette_accents() {
        let theme = Theme::Terminal;
        assert_eq!(theme.base_style().fg, Some(Color::Reset));
        assert_eq!(theme.base_style().bg, Some(Color::Reset));
        assert!(theme.muted_style().add_modifier.contains(Modifier::DIM));
        assert!(
            theme
                .selection_style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(theme.selection_style().sub_modifier.contains(Modifier::DIM));
        assert_eq!(theme.palette().green, Color::Green);
        assert_eq!(theme.palette().red, Color::Red);
    }

    #[test]
    fn everforest_keeps_its_explicit_colors_and_selection_background() {
        let theme = Theme::Everforest;
        assert_eq!(theme.base_style().bg, Some(Color::Rgb(0x2d, 0x35, 0x3b)));
        assert_eq!(
            theme.selection_style().bg,
            Some(Color::Rgb(0x42, 0x50, 0x47))
        );
        assert!(
            !theme
                .selection_style()
                .add_modifier
                .contains(Modifier::REVERSED)
        );
        assert!(!theme.muted_style().add_modifier.contains(Modifier::DIM));
    }
}
