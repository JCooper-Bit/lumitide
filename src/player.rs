//! Playback lives on one worker; the TUI never owns the audio device.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use rand::seq::SliceRandom;
use ratatui::text::Line;
use serde::{Deserialize, Serialize};

use crate::api::{TidalClient, TrackInfo};
use crate::auth::Session;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}
impl Repeat {
    pub fn next(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    pub tracks: Vec<TrackInfo>,
    pub current: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
    #[serde(default)]
    history: Vec<usize>,
    #[serde(default)]
    remaining: Vec<usize>,
}
impl Queue {
    pub fn replace(&mut self, tracks: Vec<TrackInfo>, start: usize) {
        self.tracks = tracks;
        self.current = if self.tracks.is_empty() {
            None
        } else {
            Some(start.min(self.tracks.len() - 1))
        };
        self.history.clear();
        self.reset_shuffle();
    }
    fn reset_shuffle(&mut self) {
        self.remaining = (0..self.tracks.len())
            .filter(|i| Some(*i) != self.current)
            .collect();
        self.remaining.shuffle(&mut rand::thread_rng());
    }
    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.reset_shuffle();
    }
    /// Manual next bypasses repeat-one. Natural completion honours it.
    pub fn advance(&mut self, natural: bool) {
        let Some(i) = self.current else {
            return;
        };
        if natural && self.repeat == Repeat::One {
            return;
        }
        let next = if self.shuffle {
            if self.remaining.is_empty() && self.repeat == Repeat::All {
                self.reset_shuffle();
            }
            self.remaining
                .pop()
                .or_else(|| (self.repeat == Repeat::All).then_some(i))
        } else if i + 1 < self.tracks.len() {
            Some(i + 1)
        } else if self.repeat == Repeat::All {
            Some(0)
        } else {
            None
        };
        self.history.push(i);
        self.current = next;
    }
    pub fn previous(&mut self) {
        if let Some(i) = self.history.pop() {
            if let Some(current) = self.current {
                if self.shuffle {
                    self.remaining.push(current);
                }
            }
            self.remaining.retain(|&n| n != i);
            self.current = Some(i);
        } else if let Some(i) = self.current {
            self.current = Some(i.saturating_sub(1));
        }
    }
    pub fn insert_next(&mut self, tracks: Vec<TrackInfo>) {
        let at = self.current.map_or(self.tracks.len(), |i| i + 1);
        let count = tracks.len();
        self.tracks.splice(at..at, tracks);
        for i in &mut self.history {
            if *i >= at {
                *i += count;
            }
        }
        for i in &mut self.remaining {
            if *i >= at {
                *i += count;
            }
        }
        // The next inserted item must actually play next, including in shuffle mode.
        self.remaining.extend((at..at + count).rev());
    }
    pub fn append(&mut self, tracks: Vec<TrackInfo>) {
        let start = self.tracks.len();
        self.tracks.extend(tracks);
        let mut new: Vec<_> = (start..self.tracks.len()).collect();
        new.shuffle(&mut rand::thread_rng());
        self.remaining.splice(0..0, new);
    }
    pub fn remove(&mut self, at: usize) -> bool {
        if at >= self.tracks.len() {
            return false;
        }
        self.tracks.remove(at);
        let changed = self.current == Some(at);
        self.current = self.current.and_then(|i| {
            if i == at {
                (at < self.tracks.len()).then_some(at)
            } else {
                Some(if i > at { i - 1 } else { i })
            }
        });
        self.history.retain(|&i| i != at);
        for i in &mut self.history {
            if *i > at {
                *i -= 1;
            }
        }
        self.remaining.retain(|&i| i != at);
        for i in &mut self.remaining {
            if *i > at {
                *i -= 1;
            }
        }
        self.remaining.retain(|&i| Some(i) != self.current);
        changed
    }
    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.tracks.len() || to >= self.tracks.len() {
            return;
        }
        self.tracks.swap(from, to);
        let remap = |i: usize| {
            if i == from {
                to
            } else if i == to {
                from
            } else {
                i
            }
        };
        self.current = self.current.map(remap);
        for i in &mut self.history {
            *i = remap(*i);
        }
        for i in &mut self.remaining {
            *i = remap(*i);
        }
    }
}

#[derive(Clone, Default)]
pub struct Visual {
    pub cover: Vec<Line<'static>>,
    pub spectrum: Vec<Line<'static>>,
    pub color: Option<(u8, u8, u8)>,
}

pub struct Transport {
    pub generation: u64,
    pub stop: Arc<AtomicBool>,
    pub paused: AtomicBool,
    pub seek_ms: AtomicU64,
    pub position_ms: AtomicU64,
    pub volume: Arc<Mutex<f32>>,
    pub visual: Mutex<Visual>,
    pub error: Mutex<Option<String>>,
}
impl Transport {
    pub(crate) fn new(
        volume: Arc<Mutex<f32>>,
        position: u64,
        paused: bool,
        generation: u64,
    ) -> Self {
        Self {
            generation,
            stop: Arc::new(AtomicBool::new(false)),
            paused: AtomicBool::new(paused),
            seek_ms: AtomicU64::new(if position > 0 { position } else { u64::MAX }),
            position_ms: AtomicU64::new(position),
            volume,
            visual: Mutex::new(Visual::default()),
            error: Mutex::new(None),
        }
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SavedPlayer {
    pub queue: Queue,
    pub position_ms: u64,
}
struct State {
    saved: SavedPlayer,
    generation: u64,
    active: bool,
    paused: bool,
    loading: bool,
    error: Option<String>,
}
pub struct Snapshot {
    pub queue: Queue,
    pub position_ms: u64,
    pub paused: bool,
    pub loading: bool,
    pub error: Option<String>,
    pub visual: Visual,
    pub volume: f32,
}
pub struct Player {
    state: Arc<Mutex<State>>,
    transport: Arc<Mutex<Arc<Transport>>>,
    shutdown: Arc<AtomicBool>,
    volume: Arc<Mutex<f32>>,
    worker: Option<thread::JoinHandle<()>>,
    save_path: Option<std::path::PathBuf>,
}
fn state_path() -> std::path::PathBuf {
    crate::config::directory().join("playback.json")
}
impl Player {
    pub fn new(session: Session) -> Self {
        let saved: SavedPlayer = std::fs::read(state_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        Self::with_saved(
            TidalClient::new(session),
            saved,
            Some(state_path()),
            crate::preview::run_background,
        )
    }
    fn with_saved(
        mut client: TidalClient,
        mut saved: SavedPlayer,
        save_path: Option<std::path::PathBuf>,
        runner: fn(&mut TidalClient, &TrackInfo, Arc<Transport>) -> Result<String>,
    ) -> Self {
        if saved
            .queue
            .current
            .is_some_and(|i| i >= saved.queue.tracks.len())
        {
            saved.queue.current = None;
        }
        saved
            .queue
            .history
            .retain(|&i| i < saved.queue.tracks.len());
        saved
            .queue
            .remaining
            .retain(|&i| i < saved.queue.tracks.len());
        let volume = Arc::new(Mutex::new(crate::config::load().volume));
        // Restored queues start paused, so opening the app never unexpectedly makes sound.
        let state = Arc::new(Mutex::new(State {
            saved,
            generation: 0,
            active: false,
            paused: true,
            loading: false,
            error: None,
        }));
        let transport = Arc::new(Mutex::new(Arc::new(Transport::new(
            volume.clone(),
            0,
            true,
            0,
        ))));
        let shutdown = Arc::new(AtomicBool::new(false));
        let (s, t, stop, vol) = (
            state.clone(),
            transport.clone(),
            shutdown.clone(),
            volume.clone(),
        );
        let worker = thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let pending = {
                    let mut s = s.lock().unwrap();
                    if s.active {
                        s.loading = true;
                        s.saved
                            .queue
                            .current
                            .and_then(|i| s.saved.queue.tracks.get(i).cloned())
                            .map(|track| (s.generation, track, s.saved.position_ms, s.paused))
                    } else {
                        None
                    }
                };
                let Some((generation, track, position, paused)) = pending else {
                    thread::sleep(Duration::from_millis(30));
                    continue;
                };
                let ctl = Arc::new(Transport::new(vol.clone(), position, paused, generation));
                *t.lock().unwrap() = ctl.clone();
                // A command can arrive between the snapshot and transport installation.
                if s.lock().unwrap().generation != generation {
                    continue;
                }
                let result = (|| {
                    if client.session.is_expired() {
                        let session = crate::auth::refreshed_session(&client.session)?;
                        client = TidalClient::new(session);
                    }
                    runner(&mut client, &track, ctl.clone())
                })();
                let mut s = s.lock().unwrap();
                if s.generation != generation {
                    continue;
                }
                s.loading = false;
                s.saved.position_ms = 0;
                match result {
                    Ok(reason) if reason == "end" || reason == "next" => {
                        s.saved.queue.advance(reason == "end")
                    }
                    Ok(reason) if reason == "prev" => s.saved.queue.previous(),
                    Ok(reason) if reason == "quit" => {
                        s.active = false;
                        s.paused = true;
                    }
                    Ok(_) => {
                        s.error = Some(
                            "Audio decoding failed. Press n to skip or Enter to retry.".into(),
                        );
                        s.active = false;
                    }
                    Err(e) => {
                        s.error = Some(e.to_string());
                        s.active = false;
                    }
                }
                if s.saved.queue.current.is_none() {
                    s.active = false;
                }
                s.generation += 1;
            }
        });
        Self {
            state,
            transport,
            shutdown,
            volume,
            worker: Some(worker),
            save_path,
        }
    }
    #[cfg(test)]
    pub fn for_test() -> Self {
        fn fake(_: &mut TidalClient, _: &TrackInfo, ctl: Arc<Transport>) -> Result<String> {
            ctl.visual.lock().unwrap().cover = vec![Line::raw("cover")];
            while !ctl.stop.load(Ordering::Relaxed) {
                let seek = ctl.seek_ms.swap(u64::MAX, Ordering::Relaxed);
                if seek != u64::MAX {
                    ctl.position_ms.store(seek, Ordering::Relaxed);
                }
                thread::sleep(Duration::from_millis(5));
            }
            Ok("quit".into())
        }
        Self::with_saved(
            TidalClient::new(crate::test_support::session()),
            SavedPlayer::default(),
            None,
            fake,
        )
    }
    pub fn snapshot(&self) -> Snapshot {
        let s = self.state.lock().unwrap();
        let ctl = self.transport.lock().unwrap().clone();
        let installed = ctl.generation == s.generation;
        let visual = if installed {
            ctl.visual.lock().unwrap().clone()
        } else {
            Visual::default()
        };
        let error = s.error.clone().or_else(|| {
            if installed {
                ctl.error.lock().unwrap().clone()
            } else {
                None
            }
        });
        let volume = *self.volume.lock().unwrap();
        Snapshot {
            queue: s.saved.queue.clone(),
            position_ms: if s.active && installed {
                ctl.position_ms.load(Ordering::Relaxed)
            } else {
                s.saved.position_ms
            },
            paused: if s.active && installed {
                ctl.paused.load(Ordering::Relaxed)
            } else {
                s.paused || !s.active
            },
            loading: s.active && visual.cover.is_empty(),
            error,
            visual,
            volume,
        }
    }

    fn change(&self, edit: impl FnOnce(&mut State)) {
        let mut s = self.state.lock().unwrap();
        edit(&mut s);
        s.generation += 1;
        s.loading = s.active;
        s.error = None;
        self.transport
            .lock()
            .unwrap()
            .stop
            .store(true, Ordering::Relaxed);
    }
    pub fn play(&self, tracks: Vec<TrackInfo>, start: usize) {
        self.change(|s| {
            s.saved.queue.replace(tracks, start);
            s.saved.position_ms = 0;
            s.active = s.saved.queue.current.is_some();
            s.paused = false;
        });
    }
    pub fn play_album(&self, tracks: Vec<TrackInfo>) {
        self.change(|s| {
            s.saved.queue.shuffle = false;
            s.saved.queue.repeat = Repeat::Off;
            s.saved.queue.replace(tracks, 0);
            s.saved.position_ms = 0;
            s.active = s.saved.queue.current.is_some();
            s.paused = false;
        });
    }
    pub fn select(&self, at: usize) {
        self.change(|s| {
            if at < s.saved.queue.tracks.len() {
                s.saved.queue.current = Some(at);
                s.saved.queue.history.clear();
                s.saved.queue.reset_shuffle();
                s.saved.position_ms = 0;
                s.active = true;
                s.paused = false;
            }
        });
    }
    pub fn next(&self) {
        self.change(|s| {
            s.saved.queue.advance(false);
            s.saved.position_ms = 0;
            s.active = s.saved.queue.current.is_some();
            s.paused = false;
        });
    }
    pub fn previous(&self) {
        self.change(|s| {
            s.saved.queue.previous();
            s.saved.position_ms = 0;
            s.active = s.saved.queue.current.is_some();
            s.paused = false;
        });
    }
    pub fn toggle_pause(&self) {
        let mut s = self.state.lock().unwrap();
        if !s.active {
            if s.saved.queue.current.is_none() && !s.saved.queue.tracks.is_empty() {
                s.saved.queue.current = Some(0);
                s.saved.queue.history.clear();
                s.saved.queue.reset_shuffle();
            }
            if s.saved.queue.current.is_some() {
                s.active = true;
                s.paused = false;
                s.generation += 1;
                s.error = None;
            }
        } else {
            let ctl = self.transport.lock().unwrap();
            s.paused = if ctl.generation == s.generation {
                !ctl.paused.load(Ordering::Relaxed)
            } else {
                !s.paused
            };
        }
        self.transport
            .lock()
            .unwrap()
            .paused
            .store(s.paused, Ordering::Relaxed);
    }
    pub fn enqueue(&self, tracks: Vec<TrackInfo>, next: bool) {
        let mut s = self.state.lock().unwrap();
        if next {
            s.saved.queue.insert_next(tracks);
        } else {
            s.saved.queue.append(tracks);
        }
    }
    pub fn remove(&self, at: usize) {
        let mut s = self.state.lock().unwrap();
        if s.saved.queue.remove(at) {
            s.saved.position_ms = 0;
            s.generation += 1;
            s.active = s.saved.queue.current.is_some();
            self.transport
                .lock()
                .unwrap()
                .stop
                .store(true, Ordering::Relaxed);
        }
    }
    pub fn reorder(&self, from: usize, to: usize) {
        self.state.lock().unwrap().saved.queue.move_item(from, to);
    }
    pub fn shuffle(&self) {
        self.state.lock().unwrap().saved.queue.toggle_shuffle();
    }
    pub fn repeat(&self) {
        let mut s = self.state.lock().unwrap();
        s.saved.queue.repeat = s.saved.queue.repeat.next();
    }
    pub fn seek(&self, delta: i64) {
        let ctl = self.transport.lock().unwrap();
        let pending = ctl.seek_ms.load(Ordering::Relaxed);
        let pos = if pending != u64::MAX {
            pending
        } else {
            ctl.position_ms.load(Ordering::Relaxed)
        }
        .saturating_add_signed(delta);
        ctl.seek_ms.store(pos, Ordering::Relaxed);
    }
    pub fn seek_to(&self, milliseconds: u64) {
        self.transport
            .lock()
            .unwrap()
            .seek_ms
            .store(milliseconds, Ordering::Relaxed);
    }
    pub fn volume(&self, delta: f32) {
        let mut v = self.volume.lock().unwrap();
        *v = (*v + delta).clamp(0.0, 1.0);
        let _ = crate::config::save_volume(*v);
    }
    pub fn save(&self) -> Result<()> {
        let snapshot = self.snapshot();
        let saved = SavedPlayer {
            queue: snapshot.queue,
            position_ms: snapshot.position_ms,
        };
        let Some(path) = self.save_path.clone() else {
            return Ok(());
        };
        std::fs::create_dir_all(path.parent().unwrap())?;
        let mut tmp = tempfile::NamedTempFile::new_in(path.parent().unwrap())?;
        use std::io::Write;
        tmp.write_all(&serde_json::to_vec(&saved)?)?;
        tmp.persist(path)?;
        Ok(())
    }
}
impl Drop for Player {
    fn drop(&mut self) {
        let _ = self.save();
        self.shutdown.store(true, Ordering::Relaxed);
        self.transport
            .lock()
            .unwrap()
            .stop
            .store(true, Ordering::Relaxed);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tracks(n: usize) -> Vec<TrackInfo> {
        (0..n)
            .map(|i| {
                serde_json::from_value(serde_json::json!({
        "id":i,"title":format!("Track {i}"),"artist_name":"Artist","album_name":"Album",
        "album_cover":null,"album_artist":"Artist","album_copyright":null,"album_release_year":null,
        "artists":[],"track_num":1,"volume_num":1,"isrc":null,"audio_quality":"HIGH","duration":180
    })).unwrap()
            })
            .collect()
    }
    #[test]
    fn repeat_and_stop_behaviour() {
        let mut q = Queue::default();
        q.replace(tracks(2), 0);
        q.repeat = Repeat::One;
        q.advance(true);
        assert_eq!(q.current, Some(0));
        q.advance(false);
        assert_eq!(q.current, Some(1));
        q.repeat = Repeat::Off;
        q.advance(true);
        assert_eq!(q.current, None);
        q.previous();
        assert_eq!(q.current, Some(1));
        q.repeat = Repeat::All;
        q.advance(true);
        assert_eq!(q.current, Some(0));
    }
    #[test]
    fn shuffle_visits_each_track_once_and_previous_retraces() {
        let mut q = Queue::default();
        q.replace(tracks(8), 0);
        q.toggle_shuffle();
        let mut visited = vec![];
        while let Some(i) = q.current {
            assert!(!visited.contains(&i));
            visited.push(i);
            q.advance(true);
        }
        assert_eq!(visited.len(), 8);
        q.previous();
        assert_eq!(q.current, visited.last().copied());
    }
    #[test]
    fn worker_plays_full_album_in_order_and_restart_restores_paused() {
        static VISITED: Mutex<Vec<u64>> = Mutex::new(Vec::new());
        fn finish(_: &mut TidalClient, t: &TrackInfo, _: Arc<Transport>) -> Result<String> {
            VISITED.lock().unwrap().push(t.id);
            Ok("end".into())
        }
        let mut player = Player::with_saved(
            TidalClient::new(crate::test_support::session()),
            SavedPlayer::default(),
            None,
            finish,
        );
        player.shuffle();
        player.repeat();
        player.repeat();
        player.play_album(vec![test_track(1), test_track(2), test_track(3)]);
        let start = std::time::Instant::now();
        while player.snapshot().queue.current.is_some() && start.elapsed() < Duration::from_secs(2)
        {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(*VISITED.lock().unwrap(), vec![1, 2, 3]);
        assert!(!player.snapshot().queue.shuffle);
        assert_eq!(player.snapshot().queue.repeat, Repeat::Off);
        let path = tempfile::tempdir().unwrap();
        player.save_path = Some(path.path().join("playback.json"));
        {
            let mut s = player.state.lock().unwrap();
            s.saved.queue.replace(vec![test_track(8), test_track(9)], 1);
            s.saved.position_ms = 43000;
        }
        player.save().unwrap();
        let saved: SavedPlayer =
            serde_json::from_slice(&std::fs::read(player.save_path.as_ref().unwrap()).unwrap())
                .unwrap();
        let restored = Player::with_saved(
            TidalClient::new(crate::test_support::session()),
            saved,
            None,
            finish,
        );
        let state = restored.snapshot();
        assert_eq!(state.queue.current, Some(1));
        assert_eq!(state.position_ms, 43000);
        assert!(state.paused);
    }

    #[test]
    fn edits_preserve_current_track_and_insert_next() {
        let mut q = Queue::default();
        q.replace(tracks(4), 2);
        q.move_item(2, 0);
        assert_eq!(q.tracks[q.current.unwrap()].id, 2);
        assert!(!q.remove(1));
        assert_eq!(q.tracks[q.current.unwrap()].id, 2);
        q.toggle_shuffle();
        q.insert_next(tracks(2));
        q.advance(false);
        assert_eq!(q.tracks[q.current.unwrap()].id, 0);
        let saved = serde_json::to_vec(&SavedPlayer {
            queue: q,
            position_ms: 43000,
        })
        .unwrap();
        let restored: SavedPlayer = serde_json::from_slice(&saved).unwrap();
        assert_eq!(restored.position_ms, 43000);
        assert_eq!(restored.queue.tracks[restored.queue.current.unwrap()].id, 0);
    }
}

#[cfg(test)]
pub fn test_track(id: u64) -> TrackInfo {
    serde_json::from_value(serde_json::json!({"id":id,"title":format!("Track {id}"),"artist_name":"Artist","album_name":"Album",
        "album_cover":null,"album_artist":"Artist","album_copyright":null,"album_release_year":null,"artists":[],"track_num":1,"volume_num":1,"isrc":null,"audio_quality":"HIGH","duration":180})).unwrap()
}
