use crossterm::event::{KeyCode, KeyEvent};
/// Normalize commands only; query, path and playlist-name text keeps its case.
pub fn command(mut key: KeyEvent) -> KeyEvent {
    if let KeyCode::Char(c) = key.code {
        key.code = KeyCode::Char(c.to_ascii_lowercase());
    }
    key
}
