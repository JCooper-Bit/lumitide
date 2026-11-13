//! Read desktop-generated palettes; never run shell commands or modify desktop configuration.
use ratatui::style::Color;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Auto,
    Noctalia,
    Pywal,
    Terminal,
    Album,
}
impl Mode {
    pub fn next(self) -> Self {
        match self {
            Self::Auto => Self::Noctalia,
            Self::Noctalia => Self::Pywal,
            Self::Pywal => Self::Terminal,
            Self::Terminal => Self::Album,
            Self::Album => Self::Auto,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Theme {
    pub foreground: Color,
    pub background: Color,
    pub accent: Color,
    pub muted: Color,
    pub palette: Vec<(u8, u8, u8)>,
    pub source: &'static str,
}
impl Default for Theme {
    fn default() -> Self {
        Self {
            foreground: Color::Reset,
            background: Color::Reset,
            accent: Color::Cyan,
            muted: Color::Gray,
            palette: vec![],
            source: "terminal",
        }
    }
}
pub fn rgb(hex: &str) -> Option<(u8, u8, u8)> {
    let s = hex.trim().strip_prefix('#').unwrap_or(hex.trim());
    // QML may use #AARRGGBB; ignore the alpha channel.
    let s = if s.len() == 8 { s.get(2..)? } else { s };
    if s.len() != 6 || !s.is_ascii() {
        return None;
    }
    Some((
        u8::from_str_radix(&s[0..2], 16).ok()?,
        u8::from_str_radix(&s[2..4], 16).ok()?,
        u8::from_str_radix(&s[4..6], 16).ok()?,
    ))
}
fn color(value: &Value) -> Option<(u8, u8, u8)> {
    if let Some(s) = value.as_str() {
        return rgb(s);
    }
    value
        .get("default")
        .or_else(|| value.get("color"))
        .and_then(color)
}
fn from_json(root: &Value, source: &'static str) -> Option<Theme> {
    let obj = root.get("colors").unwrap_or(root);
    let obj = obj.get("dark").unwrap_or(obj);
    let find = |keys: &[&str]| keys.iter().find_map(|key| obj.get(*key).and_then(color));
    let primary = find(&["mPrimary", "primary", "color4", "color1"])?;
    let secondary = find(&["mSecondary", "secondary", "color5"]).unwrap_or(primary);
    let tertiary = find(&["mTertiary", "tertiary", "color6"]).unwrap_or(secondary);
    let special = root.get("special");
    let background = find(&["mSurface", "surface", "background"])
        .or_else(|| special?.get("background").and_then(color));
    let foreground = find(&["mOnSurface", "on_surface", "onSurface", "foreground"])
        .or_else(|| special?.get("foreground").and_then(color));
    let muted = find(&[
        "mOnSurfaceVariant",
        "on_surface_variant",
        "onSurfaceVariant",
        "color8",
    ])
    .or(foreground);
    let c = |(r, g, b)| Color::Rgb(r, g, b);
    Some(Theme {
        foreground: foreground.map(c).unwrap_or(Color::Reset),
        background: background.map(c).unwrap_or(Color::Reset),
        accent: c(primary),
        muted: muted.map(c).unwrap_or(Color::Gray),
        palette: vec![primary, secondary, tertiary],
        source,
    })
}
fn read(path: PathBuf, source: &'static str) -> Option<Theme> {
    from_json(
        &serde_json::from_slice::<Value>(&std::fs::read(path).ok()?).ok()?,
        source,
    )
}
pub fn load(cfg: &crate::config::Config) -> Theme {
    let mode = if cfg.pywal { Mode::Pywal } else { cfg.theme };
    if matches!(mode, Mode::Auto | Mode::Noctalia) {
        if let Some(path) = &cfg.theme_file {
            if let Some(theme) = read(
                PathBuf::from(crate::config::expand_theme_path(path)),
                "custom",
            ) {
                return theme;
            }
        }
        let mut paths = vec![];
        if let Some(dir) = std::env::var_os("NOCTALIA_CONFIG_DIR") {
            paths.push(PathBuf::from(dir).join("colors.json"));
        }
        if let Some(dir) = dirs::config_dir() {
            paths.push(dir.join("noctalia/colors.json"));
        }
        if let Some(dir) = dirs::cache_dir() {
            paths.push(dir.join("noctalia/colors.json"));
        }
        for path in paths {
            if let Some(theme) = read(path, "noctalia") {
                return theme;
            }
        }
    }
    if matches!(mode, Mode::Auto | Mode::Pywal) {
        let mut paths = vec![];
        if let Some(dir) = dirs::cache_dir() {
            paths.push(dir.join("wal/colors.json"));
        }
        if let Some(home) = dirs::home_dir() {
            paths.push(home.join(".cache/wal/colors.json"));
        }
        for path in paths {
            if let Some(theme) = read(path, "pywal") {
                return theme;
            }
        }
    }
    // ANSI colours inherit the terminal palette, including Noctalia/Matugen-generated terminal themes.
    let mut fallback = Theme::default();
    if mode == Mode::Album {
        fallback.source = "album";
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn noctalia_and_matugen_and_pywal() {
        let n=from_json(&serde_json::json!({"mPrimary":"#112233","mSurface":"#010203","mOnSurface":"#fefdfc","mOnSurfaceVariant":"#aabbcc"}),"noctalia").unwrap();
        assert_eq!(n.accent, Color::Rgb(17, 34, 51));
        assert_eq!(n.background, Color::Rgb(1, 2, 3));
        let m = from_json(
            &serde_json::json!({"colors":{"dark":{"primary":{"default":{"color":"#abcdef"}}}}}),
            "custom",
        )
        .unwrap();
        assert_eq!(m.palette[0], (171, 205, 239));
        let w=from_json(&serde_json::json!({"special":{"background":"#000000","foreground":"#eeeeee"},"colors":{"color4":"#123456"}}),"pywal").unwrap();
        assert_eq!(w.background, Color::Rgb(0, 0, 0));
    }
    #[test]
    fn generated_palette_changes_are_seen_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("colors.json");
        let mut cfg = crate::config::Config::default();
        cfg.theme_file = Some(path.to_string_lossy().into_owned());
        std::fs::write(&path, r##"{"mPrimary":"#123456"}"##).unwrap();
        assert_eq!(load(&cfg).accent, Color::Rgb(18, 52, 86));
        std::fs::write(&path, r##"{"mPrimary":"#abcdef"}"##).unwrap();
        assert_eq!(load(&cfg).accent, Color::Rgb(171, 205, 239));
        std::fs::write(&path, "incomplete").unwrap();
        assert!(read(path, "custom").is_none());
    }
    #[test]
    fn invalid_hex_cannot_panic() {
        for s in ["#ééé", "bad", "#12345g", "#💙ab"] {
            assert!(rgb(s).is_none());
        }
        assert_eq!(rgb("#ff112233"), Some((17, 34, 51)));
    }
}
