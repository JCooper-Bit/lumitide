//! One terminal event loop for browsing, editing, and playback controls.
use crate::api::{AlbumInfo, ArtistInfo, MixInfo, PlaylistInfo, TidalClient, TrackInfo};
use crate::player::{Player, Snapshot};
use anyhow::{bail, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame, Terminal,
};
use std::collections::HashSet;
use std::io;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SearchKind {
    Tracks,
    Albums,
    Artists,
}
impl SearchKind {
    fn next(self) -> Self {
        match self {
            Self::Tracks => Self::Albums,
            Self::Albums => Self::Artists,
            Self::Artists => Self::Tracks,
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Tracks => "Tracks",
            Self::Albums => "Albums",
            Self::Artists => "Artists",
        }
    }
}
pub enum Start {
    Home,
    Search {
        query: String,
        kind: SearchKind,
        limit: u32,
    },
    Playlists,
    Library,
    Mixes,
    Album {
        query: Option<String>,
        id: Option<u64>,
        artist: Option<String>,
    },
}
#[derive(Clone)]
enum Route {
    Liked,
    SavedAlbums,
    Followed,
    Playlists,
    Mixes,
    Library,
    Local,
    Config,
    ArtistTracks(u64),
    ArtistAlbums(u64, bool),
}
#[derive(Clone, Copy)]
enum Setting {
    Folder,
    Limit,
    Theme,
    Drops,
    Colors,
    Calm,
    Hints,
}

#[derive(Clone)]
enum Row {
    Setting(String, Setting),
    Track(TrackInfo),
    Album(AlbumInfo),
    Artist(ArtistInfo),
    Playlist(PlaylistInfo),
    Mix(MixInfo),
    Route(String, Route),
}
impl Row {
    fn label(&self) -> String {
        match self {
            Self::Track(t) => format!("{} — {}", t.title, t.artist_name),
            Self::Album(a) => format!("{} — {}", a.title, a.artist_name),
            Self::Artist(a) => a.name.clone(),
            Self::Playlist(p) => p.title.clone(),
            Self::Mix(m) => m.title.clone(),
            Self::Route(label, _) | Self::Setting(label, _) => label.clone(),
        }
    }
    fn favorite(&self) -> Option<(&'static str, u64)> {
        match self {
            Self::Track(t) => Some(("tracks", t.id)),
            Self::Album(a) => Some(("albums", a.id)),
            Self::Artist(a) => Some(("artists", a.id)),
            _ => None,
        }
    }
}
#[derive(Clone)]
enum PageKind {
    Home,
    Browse,
    Search,
    Queue,
    Playing,
    Settings,
    Playlist {
        id: String,
        etag: String,
        indices: Vec<usize>,
    },
    Picker(Vec<TrackInfo>),
}
#[derive(Clone)]
struct Page {
    title: String,
    rows: Vec<Row>,
    cursor: usize,
    filter: String,
    kind: PageKind,
}
impl Page {
    fn new(title: impl Into<String>, rows: Vec<Row>) -> Self {
        Self {
            title: title.into(),
            rows,
            cursor: 0,
            filter: String::new(),
            kind: PageKind::Browse,
        }
    }
    fn visible(&self) -> Vec<usize> {
        use fuzzy_matcher::{skim::SkimMatcherV2, FuzzyMatcher};
        let matcher = SkimMatcherV2::default();
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, r)| {
                self.filter.is_empty() || matcher.fuzzy_match(&r.label(), &self.filter).is_some()
            })
            .map(|(i, _)| i)
            .collect()
    }
    fn selected(&self) -> Option<(usize, Row)> {
        self.visible()
            .get(self.cursor)
            .and_then(|&i| self.rows.get(i).cloned().map(|r| (i, r)))
    }
}
enum Input {
    Search(String),
    Filter(String),
    PlaylistName(String),
    Folder(String),
    Seek(String),
}
enum JobResult {
    Page(Page),
    PlayAlbum(Page, Vec<TrackInfo>),
    Favorites(HashSet<(String, u64)>),
    Favorite(String, u64, bool),
    Toast(String),
    Collection(Vec<TrackInfo>, char),
}
struct Completion {
    navigation: Option<u64>,
    result: Result<JobResult>,
}
struct App {
    client: TidalClient,
    player: Player,
    page: Page,
    history: Vec<Page>,
    input: Option<Input>,
    help: bool,
    search_kind: SearchKind,
    query: String,
    limit: u32,
    favorites: HashSet<(String, u64)>,
    favorites_loaded: bool,
    favorite_pending: HashSet<(String, u64)>,
    tx: Sender<Completion>,
    rx: Receiver<Completion>,
    navigation: u64,
    loading: bool,
    notice: String,
    notice_until: Instant,
    last_save: Instant,
    theme: crate::theme::Theme,
    last_theme: Instant,
    redraw: bool,
    show_hints: bool,
}
fn home() -> Page {
    let mut p = Page::new(
        "L U M I T I D E",
        vec![
            Row::Route("My library".into(), Route::Library),
            Row::Route("My playlists".into(), Route::Playlists),
            Row::Route("My mixes".into(), Route::Mixes),
            Row::Route("Local files".into(), Route::Local),
            Row::Route("Config".into(), Route::Config),
        ],
    );
    p.kind = PageKind::Home;
    p
}
fn library() -> Page {
    Page::new(
        "My library",
        vec![
            Row::Route("Liked tracks".into(), Route::Liked),
            Row::Route("Saved albums".into(), Route::SavedAlbums),
            Row::Route("Followed artists".into(), Route::Followed),
        ],
    )
}
fn settings() -> Page {
    let c = crate::config::load();
    let mut page = Page::new(
        "Settings — Enter changes a value",
        vec![
            Row::Setting(
                format!("Download folder: {}", c.output_dir),
                Setting::Folder,
            ),
            Row::Setting(
                format!("Search limit: {} (Enter +10)", c.search_limit),
                Setting::Limit,
            ),
            Row::Setting(
                format!("Theme: {:?} (Enter cycles)", c.theme),
                Setting::Theme,
            ),
            Row::Setting(
                format!("Drop detection: {}", c.drop_detection),
                Setting::Drops,
            ),
            Row::Setting(
                format!("Always colour: {}", c.always_color),
                Setting::Colors,
            ),
            Row::Setting(format!("Calm mode: {}", c.calm_mode), Setting::Calm),
            Row::Setting(
                format!("Controls hint: {}", c.show_controls_hint),
                Setting::Hints,
            ),
        ],
    );
    page.kind = PageKind::Settings;
    page
}
fn playlist_page(c: &TidalClient, id: &str, title: String) -> Result<Page> {
    let (entries, etag) = c.playlist_snapshot(id)?;
    let indices = entries.iter().map(|e| e.index).collect();
    let mut p = Page::new(
        title,
        entries.into_iter().map(|e| Row::Track(e.track)).collect(),
    );
    p.kind = PageKind::Playlist {
        id: id.into(),
        etag,
        indices,
    };
    Ok(p)
}
fn all_favorites(c: &TidalClient) -> Result<HashSet<(String, u64)>> {
    let mut out = HashSet::new();
    for kind in ["tracks", "albums", "artists"] {
        let mut offset = 0;
        loop {
            let (ids, total) = match kind {
                "tracks" => {
                    let (items, total) = c.liked_tracks_page(offset, 100)?;
                    (items.into_iter().map(|t| t.id).collect::<Vec<_>>(), total)
                }
                "albums" => {
                    let (items, total) = c.favorite_albums_page(offset, 100)?;
                    (items.into_iter().map(|t| t.id).collect(), total)
                }
                _ => {
                    let (items, total) = c.favorite_artists_page(offset, 100)?;
                    (items.into_iter().map(|t| t.id).collect(), total)
                }
            };
            let count = ids.len() as u64;
            out.extend(ids.into_iter().map(|id| (kind.to_owned(), id)));
            offset += count;
            if count == 0 || (total > 0 && offset >= total) || (total == 0 && count < 100) {
                break;
            }
        }
    }
    Ok(out)
}

fn parse_timestamp(text: &str) -> Result<u64> {
    let parts = text.trim().split(':').collect::<Vec<_>>();
    let seconds = match parts.as_slice() {
        [seconds] => seconds.parse::<u64>().ok(),
        [minutes, seconds] => minutes
            .parse::<u64>()
            .ok()
            .zip(seconds.parse::<u64>().ok())
            .filter(|(_, s)| *s < 60)
            .and_then(|(m, s)| m.checked_mul(60)?.checked_add(s)),
        _ => None,
    }
    .and_then(|s| s.checked_mul(1000));
    seconds.ok_or_else(|| anyhow::anyhow!("Use seconds or mm:ss, for example 90 or 1:30."))
}
fn playing_page() -> Page {
    let mut p = Page::new("Now playing", vec![]);
    p.kind = PageKind::Playing;
    p
}
fn album_start(
    c: &TidalClient,
    query: Option<String>,
    id: Option<u64>,
    artist: Option<String>,
) -> Result<JobResult> {
    let album_id = if let Some(id) = id {
        id
    } else {
        let query = query.unwrap_or_default();
        let albums = c.search_albums(&query, 100)?;
        let matches: Vec<_> = albums
            .into_iter()
            .filter(|a| {
                a.title.eq_ignore_ascii_case(query.trim())
                    && artist
                        .as_ref()
                        .is_none_or(|name| a.artist_name.eq_ignore_ascii_case(name.trim()))
            })
            .collect();
        if matches.len() != 1 {
            bail!("Expected one exact album match; found {}. Use --artist or --id to identify the album.",matches.len());
        }
        matches[0].id
    };
    let tracks = c.album_tracks(album_id)?;
    if tracks.is_empty() {
        bail!("This album contains no playable tracks.");
    }
    Ok(JobResult::PlayAlbum(
        Page::new(
            tracks[0].album_name.clone(),
            tracks.iter().cloned().map(Row::Track).collect(),
        ),
        tracks,
    ))
}

pub fn run(client: TidalClient, start: Start) -> Result<()> {
    let player = Player::new(client.session.clone());
    let (tx, rx) = mpsc::channel();
    let mut app = App {
        client,
        player,
        page: home(),
        history: vec![],
        input: None,
        help: false,
        search_kind: SearchKind::Tracks,
        query: String::new(),
        limit: crate::config::load().search_limit.clamp(1, 100),
        favorites: HashSet::new(),
        favorites_loaded: false,
        favorite_pending: HashSet::new(),
        tx,
        rx,
        navigation: 0,
        loading: false,
        notice: String::new(),
        notice_until: Instant::now(),
        last_save: Instant::now(),
        theme: crate::theme::load(&crate::config::load()),
        last_theme: Instant::now(),
        redraw: false,
        show_hints: true,
    };
    app.spawn(None, |c| Ok(JobResult::Favorites(all_favorites(&c)?)));
    match start {
        Start::Home => {}
        Start::Library => app.navigate(library()),
        Start::Playlists => app.route(Route::Playlists)?,
        Start::Mixes => app.route(Route::Mixes)?,
        Start::Search { query, kind, limit } => {
            app.query = query;
            app.search_kind = kind;
            app.limit = limit;
            app.search();
        }
        Start::Album { query, id, artist } => {
            app.load("Album", move |c| album_start(&c, query, id, artist))
        }
    }
    let mut terminal = Screen::new()?;
    app.event_loop(&mut terminal.terminal)
}
struct Screen {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}
impl Screen {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let result = (|| {
            let mut stdout = io::stdout();
            execute!(stdout, EnterAlternateScreen)?;
            Terminal::new(CrosstermBackend::new(stdout))
        })();
        match result {
            Ok(terminal) => Ok(Self { terminal }),
            Err(e) => {
                let _ = disable_raw_mode();
                let _ = execute!(io::stdout(), LeaveAlternateScreen);
                Err(e.into())
            }
        }
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
    }
}
impl App {
    fn toast(&mut self, message: impl Into<String>) {
        self.notice = message.into();
        self.notice_until = Instant::now() + Duration::from_secs(7);
    }
    fn spawn(
        &self,
        navigation: Option<u64>,
        job: impl FnOnce(TidalClient) -> Result<JobResult> + Send + 'static,
    ) {
        let tx = self.tx.clone();
        let mut session = self.client.session.clone();
        std::thread::spawn(move || {
            let result = (|| {
                if session.is_expired() {
                    session = crate::auth::refreshed_session(&session)?;
                }
                job(TidalClient::new(session))
            })();
            let _ = tx.send(Completion { navigation, result });
        });
    }
    fn navigate(&mut self, page: Page) {
        self.navigation += 1;
        if !self.loading {
            self.history.push(self.page.clone());
        }
        self.loading = false;
        self.page = page;
    }
    fn load(
        &mut self,
        title: impl Into<String>,
        job: impl FnOnce(TidalClient) -> Result<JobResult> + Send + 'static,
    ) {
        self.navigate(Page::new(title, vec![]));
        self.loading = true;
        self.spawn(Some(self.navigation), job);
    }
    fn search(&mut self) {
        let (q, kind, limit) = (self.query.trim().to_owned(), self.search_kind, self.limit);
        if q.is_empty() {
            return;
        }
        self.load(format!("{}: {q}", kind.label()), move |c| {
            let rows = match kind {
                SearchKind::Tracks => c
                    .search_tracks(&q, limit)?
                    .into_iter()
                    .map(Row::Track)
                    .collect(),
                SearchKind::Albums => c
                    .search_albums(&q, limit)?
                    .into_iter()
                    .map(Row::Album)
                    .collect(),
                SearchKind::Artists => c
                    .search_artists(&q, limit)?
                    .into_iter()
                    .map(Row::Artist)
                    .collect(),
            };
            let mut p = Page::new(format!("{}: {q}", kind.label()), rows);
            p.kind = PageKind::Search;
            Ok(JobResult::Page(p))
        });
    }
    fn route(&mut self, route: Route) -> Result<()> {
        match route {
            Route::Library => self.navigate(library()),
            Route::Playlists => self.load("My playlists", |c| {
                Ok(JobResult::Page(Page::new(
                    "My playlists",
                    c.playlists()?.into_iter().map(Row::Playlist).collect(),
                )))
            }),
            Route::Mixes => self.load("My mixes", |c| {
                Ok(JobResult::Page(Page::new(
                    "My mixes",
                    c.mixes()?.into_iter().map(Row::Mix).collect(),
                )))
            }),
            Route::Liked | Route::SavedAlbums | Route::Followed => {
                self.load("My library", move |c| {
                    let mut rows = vec![];
                    let mut offset = 0;
                    loop {
                        let (batch, total): (Vec<Row>, u64) = match route {
                            Route::Liked => {
                                let (v, t) = c.liked_tracks_page(offset, 100)?;
                                (v.into_iter().map(Row::Track).collect(), t)
                            }
                            Route::SavedAlbums => {
                                let (v, t) = c.favorite_albums_page(offset, 100)?;
                                (v.into_iter().map(Row::Album).collect(), t)
                            }
                            _ => {
                                let (v, t) = c.favorite_artists_page(offset, 100)?;
                                (v.into_iter().map(Row::Artist).collect(), t)
                            }
                        };
                        let count = batch.len() as u64;
                        rows.extend(batch);
                        offset += count;
                        if count == 0
                            || (total > 0 && offset >= total)
                            || (total == 0 && count < 100)
                        {
                            break;
                        }
                    }
                    Ok(JobResult::Page(Page::new(
                        match route {
                            Route::Liked => "Liked tracks",
                            Route::SavedAlbums => "Saved albums",
                            _ => "Followed artists",
                        },
                        rows,
                    )))
                })
            }
            Route::ArtistTracks(id) => self.load("Top tracks", move |c| {
                Ok(JobResult::Page(Page::new(
                    "Top tracks",
                    c.artist_top_tracks(id, 100)?
                        .into_iter()
                        .map(Row::Track)
                        .collect(),
                )))
            }),
            Route::ArtistAlbums(id, singles) => {
                self.load(if singles { "Singles & EPs" } else { "Albums" }, move |c| {
                    Ok(JobResult::Page(Page::new(
                        if singles { "Singles & EPs" } else { "Albums" },
                        c.artist_albums(id, singles)?
                            .into_iter()
                            .map(Row::Album)
                            .collect(),
                    )))
                })
            }
            Route::Config => self.navigate(settings()),
            Route::Local => {
                let was_playing = !self.player.snapshot().paused;
                if was_playing {
                    self.player.toggle_pause();
                }
                disable_raw_mode()?;
                execute!(io::stdout(), LeaveAlternateScreen)?;
                let result = crate::local::run(false);
                enable_raw_mode()?;
                execute!(io::stdout(), EnterAlternateScreen)?;
                if was_playing {
                    self.player.toggle_pause();
                }
                self.redraw = true;
                match result {
                    Ok(route) if route == "mixes" => self.route(Route::Mixes)?,
                    Ok(route) if route == "search" => {
                        self.input = Some(Input::Search(String::new()))
                    }
                    Err(e) => self.toast(e.to_string()),
                    _ => {}
                }
            }
        }
        Ok(())
    }
    fn enter(&mut self) -> Result<()> {
        let Some((index, row)) = self.page.selected() else {
            return Ok(());
        };
        if let PageKind::Picker(tracks) = &self.page.kind {
            if let Row::Playlist(p) = row {
                let tracks = tracks.clone();
                self.spawn(None, move |c| {
                    c.add_to_playlist(&p.id, &tracks)?;
                    Ok(JobResult::Toast(format!(
                        "Updated {} (existing tracks skipped)",
                        p.title
                    )))
                });
                self.back();
                self.toast("Adding to playlist…");
            }
            return Ok(());
        }
        match row {
            Row::Setting(_, setting) => {
                let mut cfg = crate::config::load();
                match setting {
                    Setting::Folder => {
                        self.input = Some(Input::Folder(cfg.output_dir));
                        return Ok(());
                    }
                    Setting::Limit => {
                        cfg.search_limit = if cfg.search_limit >= 100 {
                            10
                        } else {
                            (cfg.search_limit + 10).min(100)
                        }
                    }
                    Setting::Theme => {
                        cfg.theme = cfg.theme.next();
                        cfg.pywal = false;
                    }
                    Setting::Drops => cfg.drop_detection = !cfg.drop_detection,
                    Setting::Colors => cfg.always_color = !cfg.always_color,
                    Setting::Calm => cfg.calm_mode = !cfg.calm_mode,
                    Setting::Hints => cfg.show_controls_hint = !cfg.show_controls_hint,
                }
                crate::config::save(&cfg)?;
                let cursor = self.page.cursor;
                self.page = settings();
                self.page.cursor = cursor;
                self.theme = crate::theme::load(&cfg);
                self.limit = cfg.search_limit.clamp(1, 100);
            }
            Row::Track(_) => {
                if matches!(self.page.kind, PageKind::Queue) {
                    self.player.select(index);
                } else {
                    let tracks: Vec<_> = self
                        .page
                        .visible()
                        .into_iter()
                        .filter_map(|i| match &self.page.rows[i] {
                            Row::Track(t) => Some(t.clone()),
                            _ => None,
                        })
                        .collect();
                    self.player.play(tracks, self.page.cursor);
                    self.navigate(playing_page());
                }
                self.toast("Playing — N now playing · Space pause · Q queue");
            }
            Row::Album(a) => self.load(a.title.clone(), move |c| {
                Ok(JobResult::Page(Page::new(
                    a.title,
                    c.album_tracks(a.id)?.into_iter().map(Row::Track).collect(),
                )))
            }),
            Row::Artist(a) => self.navigate(Page::new(
                a.name,
                vec![
                    Row::Route("Top tracks".into(), Route::ArtistTracks(a.id)),
                    Row::Route("Albums".into(), Route::ArtistAlbums(a.id, false)),
                    Row::Route("Singles & EPs".into(), Route::ArtistAlbums(a.id, true)),
                ],
            )),
            Row::Playlist(p) => self.load(p.title.clone(), move |c| {
                Ok(JobResult::Page(playlist_page(&c, &p.id, p.title)?))
            }),
            Row::Mix(m) => self.load(m.title.clone(), move |c| {
                Ok(JobResult::Page(Page::new(
                    m.title,
                    c.mix_tracks(&m.id)?.into_iter().map(Row::Track).collect(),
                )))
            }),
            Row::Route(_, r) => self.route(r)?,
        }
        Ok(())
    }
    fn back(&mut self) -> bool {
        self.navigation += 1;
        self.loading = false;
        if let Some(page) = self.history.pop() {
            self.page = page;
            false
        } else {
            true
        }
    }
    fn selected_row(&self) -> Option<Row> {
        if matches!(self.page.kind, PageKind::Playing) {
            let s = self.player.snapshot();
            s.queue
                .current
                .and_then(|i| s.queue.tracks.get(i).cloned())
                .map(Row::Track)
        } else {
            self.page.selected().map(|(_, r)| r)
        }
    }
    fn collection(&self, row: Row, action: char) {
        self.spawn(None, move |c| {
            let tracks = match row {
                Row::Track(t) => vec![t],
                Row::Album(a) => c.album_tracks(a.id)?,
                Row::Playlist(p) => c.playlist_tracks(&p.id)?,
                Row::Mix(m) => c.mix_tracks(&m.id)?,
                _ => bail!("Select a track, album, mix, or playlist."),
            };
            Ok(JobResult::Collection(tracks, action))
        });
    }
    fn favorite(&mut self) {
        let Some((kind, id)) = self.selected_row().and_then(|r| r.favorite()) else {
            return;
        };
        if !self.favorites_loaded {
            self.toast("Favourites are still loading. Try f again shortly.");
            return;
        }
        let key = (kind.to_owned(), id);
        if !self.favorite_pending.insert(key.clone()) {
            return;
        }
        let enabled = !self.favorites.contains(&key);
        self.spawn(None, move |c| {
            c.set_favorite(kind, id, enabled)?;
            Ok(JobResult::Favorite(kind.into(), id, enabled))
        });
        self.toast("Updating favourites…");
    }
    fn edit_playlist(&mut self, delta: Option<isize>) {
        let PageKind::Playlist { id, etag, indices } = &self.page.kind else {
            return;
        };
        let Some((at, _)) = self.page.selected() else {
            return;
        };
        let (id, etag, indices, title) = (
            id.clone(),
            etag.clone(),
            indices.clone(),
            self.page.title.clone(),
        );
        let destination = delta.map(|d| at.saturating_add_signed(d));
        if destination.is_some_and(|to| to >= indices.len() || to == at) {
            return;
        }
        self.loading = true;
        let nav = self.navigation;
        self.spawn(Some(nav), move |c| {
            if let Some(to) = destination {
                c.move_playlist_item(&id, indices[at], indices[to], &etag)?;
            } else {
                c.remove_playlist_item(&id, indices[at], &etag)?;
            }
            let mut p = playlist_page(&c, &id, title)?;
            p.cursor = destination
                .unwrap_or(at)
                .min(p.rows.len().saturating_sub(1));
            Ok(JobResult::Page(p))
        });
    }
    fn key(&mut self, key: KeyEvent) -> Result<bool> {
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
        {
            return Ok(true);
        }
        if self.help {
            self.help = false;
            return Ok(false);
        }
        if let Some(input) = self.input.as_mut() {
            match key.code {
                KeyCode::Esc => {
                    self.input = None;
                }
                KeyCode::Tab if matches!(input, Input::Search(_)) => {
                    self.search_kind = self.search_kind.next()
                }
                KeyCode::Backspace => match input {
                    Input::Search(s)
                    | Input::Filter(s)
                    | Input::PlaylistName(s)
                    | Input::Folder(s)
                    | Input::Seek(s) => {
                        s.pop();
                    }
                },
                KeyCode::Char(c) => match input {
                    Input::Search(s)
                    | Input::Filter(s)
                    | Input::PlaylistName(s)
                    | Input::Folder(s)
                    | Input::Seek(s) => s.push(c),
                },
                KeyCode::Enter => match self.input.take().unwrap() {
                    Input::Search(q) => {
                        self.query = q;
                        self.search();
                    }
                    Input::Filter(q) => {
                        self.page.filter = q;
                        self.page.cursor = 0;
                    }
                    Input::Seek(timestamp) => {
                        self.player.seek_to(parse_timestamp(&timestamp)?);
                    }
                    Input::Folder(folder) => {
                        if !folder.trim().is_empty() {
                            let mut cfg = crate::config::load();
                            cfg.output_dir = folder.trim().to_owned();
                            crate::config::save(&cfg)?;
                            let cursor = self.page.cursor;
                            self.page = settings();
                            self.page.cursor = cursor;
                        }
                    }
                    Input::PlaylistName(name) => {
                        if !name.trim().is_empty() {
                            let picker = match &self.page.kind {
                                PageKind::Picker(t) => Some(t.clone()),
                                _ => None,
                            };
                            self.loading = true;
                            let nav = self.navigation;
                            self.spawn(Some(nav), move |c| {
                                c.create_playlist(name.trim())?;
                                let mut p = Page::new(
                                    "My playlists",
                                    c.playlists()?.into_iter().map(Row::Playlist).collect(),
                                );
                                if let Some(t) = picker {
                                    p.kind = PageKind::Picker(t);
                                }
                                Ok(JobResult::Page(p))
                            });
                        }
                    }
                },
                _ => {}
            }
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('/') => self.input = Some(Input::Search(self.query.clone())),
            KeyCode::Char('F') => self.input = Some(Input::Filter(self.page.filter.clone())),
            KeyCode::Tab if matches!(self.page.kind, PageKind::Search) => {
                self.search_kind = self.search_kind.next();
                self.search();
            }
            KeyCode::Esc | KeyCode::Char('q') => return Ok(self.back()),
            KeyCode::Up | KeyCode::Char('k') => {
                self.page.cursor = self.page.cursor.saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.page.cursor =
                    (self.page.cursor + 1).min(self.page.visible().len().saturating_sub(1))
            }
            KeyCode::Home => self.page.cursor = 0,
            KeyCode::End => self.page.cursor = self.page.visible().len().saturating_sub(1),
            KeyCode::Enter => {
                if !self.loading {
                    self.enter()?;
                }
            }
            KeyCode::Char(' ') => self.player.toggle_pause(),
            KeyCode::Right | KeyCode::Char('n') => self.player.next(),
            KeyCode::Left | KeyCode::Char('p') => self.player.previous(),
            KeyCode::Char('+') | KeyCode::Char('=') => self.player.volume(0.05),
            KeyCode::Char('-') => self.player.volume(-0.05),
            KeyCode::Char('g') => self.input = Some(Input::Seek(String::new())),
            KeyCode::Char('[') => self.player.seek(-10000),
            KeyCode::Char(']') => self.player.seek(10000),
            KeyCode::Char('{') => self.player.seek(-60000),
            KeyCode::Char('}') => self.player.seek(60000),
            KeyCode::Char('s') => self.player.shuffle(),
            KeyCode::Char('R') => self.player.repeat(),
            KeyCode::Char('N') => {
                let mut p = Page::new("Now playing", vec![]);
                p.kind = PageKind::Playing;
                self.navigate(p);
            }
            KeyCode::Char('Q') => {
                let mut p = Page::new("Playback queue", vec![]);
                p.kind = PageKind::Queue;
                self.navigate(p);
            }
            KeyCode::Char('b') => self.navigate(home()),
            KeyCode::Char('l') => self.navigate(library()),
            KeyCode::Char('t') => self.route(Route::Playlists)?,
            KeyCode::Char('m') => self.route(Route::Mixes)?,
            KeyCode::Char('f') => self.favorite(),
            KeyCode::Char(action @ ('a' | 'P' | 'd' | 'w')) => {
                if let Some(row) = self.selected_row() {
                    self.collection(row, action);
                    self.toast("Loading selection…");
                }
            }
            KeyCode::Char('r') => {
                if let Some(Row::Track(t)) = self.selected_row() {
                    self.load("Track radio", move |c| {
                        Ok(JobResult::Page(Page::new(
                            "Track radio",
                            c.track_radio(t.id)?.into_iter().map(Row::Track).collect(),
                        )))
                    });
                }
            }
            KeyCode::Delete => {
                if matches!(self.page.kind, PageKind::Queue) {
                    if let Some((i, _)) = self.page.selected() {
                        self.player.remove(i);
                    }
                } else if !self.loading {
                    self.edit_playlist(None);
                }
            }
            KeyCode::Char(key @ ('J' | 'K')) => {
                if matches!(self.page.kind, PageKind::Queue) {
                    if let Some((i, _)) = self.page.selected() {
                        let to = if key == 'J' {
                            i + 1
                        } else {
                            i.saturating_sub(1)
                        };
                        self.player.reorder(i, to);
                        self.page.cursor = to.min(self.page.rows.len().saturating_sub(1));
                    }
                } else if !self.loading {
                    self.edit_playlist(Some(if key == 'J' { 1 } else { -1 }));
                }
            }
            KeyCode::Char('u') => {
                if let PageKind::Playlist { id, .. } = &self.page.kind {
                    let id = id.clone();
                    let title = self.page.title.clone();
                    self.load(title.clone(), move |c| {
                        Ok(JobResult::Page(playlist_page(&c, &id, title)?))
                    });
                } else if matches!(self.page.kind, PageKind::Search) {
                    self.search();
                } else if self.page.title == "My playlists" {
                    self.route(Route::Playlists)?;
                } else {
                    self.spawn(None, |c| Ok(JobResult::Favorites(all_favorites(&c)?)));
                    self.toast("Refreshing favourites…");
                }
            }
            KeyCode::Char('c') => {
                if self.page.title == "My playlists"
                    || matches!(self.page.kind, PageKind::Picker(_))
                {
                    self.input = Some(Input::PlaylistName(String::new()));
                }
            }
            _ => {}
        }
        Ok(false)
    }
    fn completions(&mut self) {
        while let Ok(c) = self.rx.try_recv() {
            if c.navigation.is_some_and(|n| n != self.navigation) {
                continue;
            }
            if c.navigation == Some(self.navigation) {
                self.loading = false;
            }
            match c.result {
                Err(e) => {
                    self.favorite_pending.clear();
                    self.toast(e.to_string());
                }
                Ok(JobResult::Favorites(f)) => {
                    self.favorites = f;
                    self.favorites_loaded = true;
                }
                Ok(JobResult::Favorite(kind, id, on)) => {
                    let key = (kind, id);
                    self.favorite_pending.remove(&key);
                    if on {
                        self.favorites.insert(key);
                    } else {
                        self.favorites.remove(&key);
                    }
                    self.toast(if on {
                        "Saved to favourites"
                    } else {
                        "Removed from favourites"
                    });
                }
                Ok(JobResult::Toast(t)) => self.toast(t),
                Ok(JobResult::PlayAlbum(p, tracks)) => {
                    self.page = p;
                    self.player.play_album(tracks);
                    self.navigate(playing_page());
                }
                Ok(JobResult::Collection(tracks, action)) => match action {
                    'd' => {
                        if let Some(q) = crate::DOWNLOAD_QUEUE.get() {
                            q.push_tracks(
                                tracks,
                                self.client.session.clone(),
                                crate::config::load().output_path(),
                            );
                        }
                        self.toast("Queued for download");
                    }
                    'w' => self.load(
                        "Choose playlist — Enter adds selection · c creates",
                        move |c| {
                            let mut p = Page::new(
                                "My playlists",
                                c.playlists()?.into_iter().map(Row::Playlist).collect(),
                            );
                            p.kind = PageKind::Picker(tracks);
                            Ok(JobResult::Page(p))
                        },
                    ),
                    _ => {
                        let n = tracks.len();
                        self.player.enqueue(tracks, action == 'P');
                        self.toast(format!(
                            "Added {n} tracks {}",
                            if action == 'P' {
                                "to play next"
                            } else {
                                "to the queue"
                            }
                        ));
                    }
                },
                Ok(JobResult::Page(p)) => self.page = p,
            }
        }
    }
    fn event_loop(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
        loop {
            self.completions();
            if self.last_theme.elapsed() > Duration::from_secs(1) {
                let cfg = crate::config::load();
                self.show_hints = cfg.show_controls_hint;
                self.theme = crate::theme::load(&cfg);
                self.last_theme = Instant::now();
            }
            let snapshot = self.player.snapshot();
            if matches!(self.page.kind, PageKind::Queue) {
                self.page.rows = snapshot
                    .queue
                    .tracks
                    .iter()
                    .cloned()
                    .map(Row::Track)
                    .collect();
                self.page.cursor = self
                    .page
                    .cursor
                    .min(self.page.visible().len().saturating_sub(1));
            }
            if self.redraw {
                terminal.clear()?;
                self.redraw = false;
            }
            terminal.draw(|f| self.draw(f, &snapshot))?;
            if self.last_save.elapsed() > Duration::from_secs(5) {
                if let Err(e) = self.player.save() {
                    self.toast(format!("Could not save playback: {e}"));
                }
                self.last_save = Instant::now();
            }
            if event::poll(Duration::from_millis(90))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match self.key(key) {
                            Ok(true) => break,
                            Err(e) => self.toast(e.to_string()),
                            _ => {}
                        }
                    }
                }
            }
        }
        self.player.save()?;
        Ok(())
    }
    fn draw(&self, f: &mut Frame, s: &Snapshot) {
        let area = f.area();
        let accent = if self.theme.source == "album" {
            s.visual
                .color
                .map(|(r, g, b)| ratatui::style::Color::Rgb(r, g, b))
                .unwrap_or(self.theme.accent)
        } else {
            self.theme.accent
        };
        let dim = Style::default().fg(self.theme.muted);
        f.render_widget(
            Block::default().style(
                Style::default()
                    .fg(self.theme.foreground)
                    .bg(self.theme.background),
            ),
            area,
        );
        let chunks = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(0),
            Constraint::Length(2),
            Constraint::Length(1),
            Constraint::Length(2),
        ])
        .split(area);
        let header = format!(
            " {}{}  {} · theme {}",
            self.page.title,
            if self.loading { " · loading…" } else { "" },
            if self.page.filter.is_empty() {
                String::new()
            } else {
                format!("[filter: {}]", self.page.filter)
            },
            self.theme.source
        );
        if matches!(self.page.kind, PageKind::Playing) {
            if let Some(t) = s.queue.current.and_then(|i| s.queue.tracks.get(i)) {
                crate::panel::render(
                    f,
                    &crate::panel::PanelState {
                        cover_lines: &s.visual.cover,
                        track_name: &t.title,
                        artist_name: &t.artist_name,
                        album_name: &t.album_name,
                        track_label: None,
                        elapsed: s.position_ms as f64 / 1000.0,
                        total: t.duration as f64,
                        volume: s.volume,
                        paused: s.paused,
                        dl_status: if s.loading { "Buffering…" } else { "" },
                        bar_color: s.visual.color,
                        vis_lines: &s.visual.spectrum,
                        is_local: false,
                        show_controls: false,
                        show_controls_hint: false,
                        queue_status: None,
                    },
                );
            } else {
                f.render_widget(
                    Paragraph::new("Nothing playing. / search · Space resume saved queue"),
                    chunks[1],
                );
            }
        } else {
            let visible = self.page.visible();
            let height = chunks[1].height as usize;
            let start = self.page.cursor.saturating_sub(height.saturating_sub(1));
            let rows: Vec<_> = visible
                .iter()
                .enumerate()
                .skip(start)
                .take(height)
                .map(|(pos, &idx)| {
                    let row = &self.page.rows[idx];
                    let favorite = row
                        .favorite()
                        .is_some_and(|(k, id)| self.favorites.contains(&(k.into(), id)));
                    let playing =
                        matches!(self.page.kind, PageKind::Queue) && s.queue.current == Some(idx);
                    let label = format!(
                        "{}{}{} {}",
                        if pos == self.page.cursor { ">" } else { " " },
                        if playing { "▶" } else { " " },
                        if favorite { "♥" } else { " " },
                        row.label()
                    );
                    ListItem::new(label).style(if pos == self.page.cursor {
                        Style::default().fg(accent).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default()
                    })
                })
                .collect();
            if rows.is_empty() {
                f.render_widget(
                    Paragraph::new(if self.loading {
                        "Loading…"
                    } else {
                        "No items. / search · c create playlist · Esc back"
                    })
                    .style(dim),
                    chunks[1],
                );
            } else {
                f.render_widget(List::new(rows), chunks[1]);
            }
        }
        f.render_widget(
            Paragraph::new(header).style(Style::default().fg(accent).add_modifier(Modifier::BOLD)),
            chunks[0],
        );
        let status = if let Some(t) = s.queue.current.and_then(|i| s.queue.tracks.get(i)) {
            format!(
                " {} {} — {}  {}/{}  vol {}%  shuffle {}  repeat {:?}",
                if s.loading {
                    "Buffering"
                } else if s.paused {
                    "Paused"
                } else {
                    "▶"
                },
                t.title,
                t.artist_name,
                crate::utils::fmt_time(s.position_ms as f64 / 1000.0),
                crate::utils::fmt_time(t.duration as f64),
                (s.volume * 100.0) as u32,
                if s.queue.shuffle { "on" } else { "off" },
                s.queue.repeat
            )
        } else {
            format!(
                " Nothing playing · Space restores saved queue · shuffle {} · repeat {:?}",
                s.queue.shuffle, s.queue.repeat
            )
        };
        f.render_widget(
            Paragraph::new(status).style(Style::default().fg(accent)),
            chunks[2],
        );
        if self.show_hints {
            f.render_widget(Paragraph::new(" / search · Enter play/open · a queue · P play next · N playing · Q queue · ? keys · Ctrl-q quit").style(dim),chunks[3]);
        }
        let message = if self.notice_until > Instant::now() {
            self.notice.clone()
        } else {
            s.error
                .clone()
                .or_else(|| crate::DOWNLOAD_QUEUE.get().and_then(|q| q.status()))
                .unwrap_or_default()
        };
        let input = match &self.input {
            Some(Input::Search(q)) => format!(
                " Search {} [Tab changes type]: {q}▏",
                self.search_kind.label()
            ),
            Some(Input::Filter(q)) => format!(" Filter: {q}▏"),
            Some(Input::PlaylistName(q)) => format!(" Playlist name: {q}▏"),
            Some(Input::Seek(q)) => format!(" Seek to mm:ss (or seconds): {q}▏"),
            Some(Input::Folder(q)) => format!(" Download folder: {q}▏"),
            None => message,
        };
        f.render_widget(Paragraph::new(input), chunks[4]);
        if self.help {
            let rect = Rect::new(
                area.x + area.width / 20,
                area.y + area.height / 20,
                area.width * 9 / 10,
                area.height * 9 / 10,
            );
            f.render_widget(Clear, rect);
            f.render_widget(Paragraph::new("Navigate: ↑↓ / j k · Enter play/open · Esc/q back · Ctrl-q quit\n/ search · Tab switch tracks/albums/artists · F filter current list\nb home · l library · t playlists · m mixes · N now playing\n\nSpace pause/resume · n/→ next · p/← previous · +/- volume\n[ / ] seek ±10s · { / } seek ±60s · g seek to timestamp (waits for buffering)\ns shuffle · R cycle repeat off/all/one\nQ playback queue · a append selection · P play next\nQueue: Delete remove · J/K move down/up · Enter jump to track\n\nf like/unlike track, save/unsave album, follow/unfollow artist\nw add selection to playlist · c create playlist in playlist browser\nPlaylist tracks: Delete remove · J/K reorder · u reload\nd download selection · r open track radio\n\nPlayback continues while browsing. Saved queues restore paused.\nPress any key to close.")
                .block(Block::default().borders(Borders::ALL).title("Key bindings").border_style(Style::default().fg(accent))),rect);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::test_track;
    fn app() -> App {
        let (tx, rx) = mpsc::channel();
        App {
            client: TidalClient::new(crate::test_support::session()),
            player: Player::for_test(),
            page: home(),
            history: vec![],
            input: None,
            help: false,
            search_kind: SearchKind::Tracks,
            query: String::new(),
            limit: 10,
            favorites: HashSet::new(),
            favorites_loaded: true,
            favorite_pending: HashSet::new(),
            tx,
            rx,
            navigation: 0,
            loading: false,
            notice: String::new(),
            notice_until: Instant::now(),
            last_save: Instant::now(),
            theme: crate::theme::Theme::default(),
            last_theme: Instant::now(),
            redraw: false,
            show_hints: true,
        }
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[test]
    fn browsing_and_queue_keys_do_not_stop_audio() {
        let mut app = app();
        app.page = Page::new(
            "Album",
            vec![
                Row::Track(test_track(1)),
                Row::Track(test_track(2)),
                Row::Track(test_track(3)),
            ],
        );
        app.key(key(KeyCode::Enter)).unwrap();
        assert_eq!(app.player.snapshot().queue.current, Some(0));
        app.key(key(KeyCode::Char('Q'))).unwrap();
        app.page.rows = app
            .player
            .snapshot()
            .queue
            .tracks
            .into_iter()
            .map(Row::Track)
            .collect();
        app.key(key(KeyCode::Down)).unwrap();
        app.key(key(KeyCode::Char('J'))).unwrap();
        assert_eq!(app.player.snapshot().queue.tracks[2].id, 2);
        app.key(key(KeyCode::Delete)).unwrap();
        assert_eq!(app.player.snapshot().queue.tracks.len(), 2);
        app.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(app.page.title, "Now playing");
        app.key(key(KeyCode::Esc)).unwrap();
        assert_eq!(app.page.title, "Album");
        assert!(!app.player.snapshot().paused);
        app.key(key(KeyCode::Char('/'))).unwrap();
        app.key(key(KeyCode::Tab)).unwrap();
        assert_eq!(app.search_kind, SearchKind::Albums);
        app.key(key(KeyCode::Esc)).unwrap();
        assert!(!app.player.snapshot().paused);
        app.key(key(KeyCode::Char('s'))).unwrap();
        assert!(app.player.snapshot().queue.shuffle);
        app.key(key(KeyCode::Char('R'))).unwrap();
        assert_eq!(
            app.player.snapshot().queue.repeat,
            crate::player::Repeat::All
        );
        app.key(key(KeyCode::Char(' '))).unwrap();
        assert!(app.player.snapshot().paused);
    }
    #[test]
    fn renders_empty_lists_help_and_now_playing_at_small_sizes() {
        let mut app = app();
        app.player.play(vec![test_track(1)], 0);
        for (width, height) in [(1, 1), (20, 6), (80, 24), (120, 45)] {
            let backend = ratatui::backend::TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            for kind in [PageKind::Browse, PageKind::Playing, PageKind::Queue] {
                app.page.kind = kind;
                for help in [false, true] {
                    app.help = help;
                    terminal
                        .draw(|f| app.draw(f, &app.player.snapshot()))
                        .unwrap();
                }
            }
        }
    }
    #[test]
    fn filtering_preserves_original_playlist_and_queue_indices() {
        let mut p = Page::new(
            "Test",
            vec![
                Row::Track(test_track(5)),
                Row::Track(test_track(6)),
                Row::Track(test_track(7)),
            ],
        );
        p.filter = "Track 6".into();
        assert_eq!(p.visible(), vec![1]);
        assert_eq!(p.selected().unwrap().0, 1);
    }
}

#[cfg(test)]
mod startup_tests {
    use super::*;
    use crate::test_support::{Reply, Server};
    #[test]
    fn startup_album_resolves_artist_and_fetches_full_album_without_prompt() {
        let server = Server::new(vec![
            Reply::json(
                serde_json::json!({"albums":{"items":[{"id":1,"title":"Album","artist":{"id":9,"name":"Wrong"}},{"id":2,"title":"Album","artist":{"id":8,"name":"Right"}}]}}),
            ),
            Reply::json(
                serde_json::json!({"items":[{"id":10,"title":"One","album":{"id":2,"title":"Album"}}],"totalNumberOfItems":2}),
            ),
            Reply::json(
                serde_json::json!({"items":[{"id":11,"title":"Two","album":{"id":2,"title":"Album"}}],"totalNumberOfItems":2}),
            ),
        ]);
        let JobResult::PlayAlbum(page, tracks) = album_start(
            &server.client(),
            Some("album".into()),
            None,
            Some("right".into()),
        )
        .unwrap() else {
            panic!("expected album")
        };
        assert_eq!(page.rows.len(), 2);
        assert_eq!(
            tracks.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![10, 11]
        );
        assert!(server.requests.lock().unwrap()[1].starts_with("GET /albums/2/tracks?"));
    }
    #[test]
    fn ambiguous_album_returns_actionable_error() {
        let server = Server::new(vec![Reply::json(
            serde_json::json!({"albums":{"items":[{"id":1,"title":"Album"},{"id":2,"title":"Album"}]}}),
        )]);
        let result = album_start(&server.client(), Some("Album".into()), None, None);
        assert!(result.err().unwrap().to_string().contains("--id"));
    }
}

#[cfg(test)]
mod seek_tests {
    use super::*;
    #[test]
    fn timestamp_validation() {
        assert_eq!(parse_timestamp("1:30").unwrap(), 90000);
        assert_eq!(parse_timestamp("90").unwrap(), 90000);
        for s in ["-5", "1:70", "1:2:3", "", "18446744073709551615"] {
            assert!(parse_timestamp(s).is_err());
        }
    }
}
