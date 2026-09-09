use ratatui::style::{Modifier, Style};

// Everforest Dark, medium contrast: https://github.com/sainnhe/everforest
pub(super) mod palette {
    use ratatui::style::Color;

    pub(in crate::picker::view) const BACKGROUND: Color = Color::Rgb(0x2d, 0x35, 0x3b);
    pub(in crate::picker::view) const TEXT: Color = Color::Rgb(0xd3, 0xc6, 0xaa);
    pub(in crate::picker::view) const MUTED: Color = Color::Rgb(0x9d, 0xa9, 0xa0);
    pub(in crate::picker::view) const SELECTION: Color = Color::Rgb(0x42, 0x50, 0x47);
    pub(in crate::picker::view) const GREEN: Color = Color::Rgb(0xa7, 0xc0, 0x80);
    pub(in crate::picker::view) const AQUA: Color = Color::Rgb(0x83, 0xc0, 0x92);
    pub(in crate::picker::view) const BLUE: Color = Color::Rgb(0x7f, 0xbb, 0xb3);
    pub(in crate::picker::view) const YELLOW: Color = Color::Rgb(0xdb, 0xbc, 0x7f);
    pub(in crate::picker::view) const RED: Color = Color::Rgb(0xe6, 0x7e, 0x80);
}

pub(super) fn base_style() -> Style {
    Style::default().fg(palette::TEXT).bg(palette::BACKGROUND)
}

pub(super) fn selection_style() -> Style {
    Style::default()
        .fg(palette::TEXT)
        .bg(palette::SELECTION)
        .add_modifier(Modifier::BOLD)
}

pub(super) fn muted_style() -> Style {
    Style::default().fg(palette::MUTED)
}
