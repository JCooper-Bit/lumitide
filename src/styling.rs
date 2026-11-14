//! Fixed deep-red presentation. No desktop palette detection or theme configuration.
use ratatui::style::Color;
pub const BACKGROUND: Color = Color::Rgb(19, 10, 14);
pub const SURFACE: Color = Color::Rgb(30, 14, 21);
pub const FOREGROUND: Color = Color::Rgb(241, 226, 220);
pub const MUTED: Color = Color::Rgb(180, 143, 151);
pub const ACCENT: Color = Color::Rgb(222, 86, 110);
pub const BORDER: Color = Color::Rgb(102, 36, 54);
pub const SELECTED: Color = Color::Rgb(69, 22, 37);
pub const PALETTE: &[(u8, u8, u8)] = &[(222, 86, 110), (184, 44, 72), (240, 134, 147)];
