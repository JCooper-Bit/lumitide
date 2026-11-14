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
    #[serde(default)]
    manual: Vec<usize>,
    #[serde(default)]
    pending: Vec<usize>,
    #[serde(default)]
    source_current: Option<usize>,
}
impl Queue {
    pub fn replace(&mut self, tracks: Vec<TrackInfo>, start: usize) {
        self.tracks = tracks;
        self.current = if self.tracks.is_empty() {
            None
        } else {
            Some(start.min(self.tracks.len() - 1))
        };
        self.source_current = self.current;
        self.history.clear();
        self.manual.clear();
        self.pending.clear();
        self.reset_shuffle();
    }
    fn is_manual(&self, index: usize) -> bool {
        self.manual.contains(&index)
    }
    fn reset_shuffle(&mut self) {
        let only_manual = self.manual.len() == self.tracks.len();
        self.remaining = (0..self.tracks.len())
            .filter(|i| {
                if only_manual {
                    Some(*i) != self.current
                } else {
                    Some(*i) != self.source_current && !self.is_manual(*i)
                }
            })
            .collect();
        self.remaining.shuffle(&mut rand::thread_rng());
    }
    pub fn toggle_shuffle(&mut self) {
        self.shuffle = !self.shuffle;
        self.reset_shuffle();
    }
    fn source_next(&self) -> Option<usize> {
        if self.manual.len() == self.tracks.len()
            && self.repeat == Repeat::All
            && !self.tracks.is_empty()
        {
            return self.current.map(|i| (i + 1) % self.tracks.len());
        }
        let anchor = self
            .current
            .filter(|&i| !self.is_manual(i))
            .or(self.source_current);
        let begin = anchor.map_or(0, |i| i + 1);
        (begin..self.tracks.len())
            .find(|&i| !self.is_manual(i))
            .or_else(|| {
                if self.repeat == Repeat::All {
                    (0..self.tracks.len()).find(|&i| !self.is_manual(i))
                } else {
                    None
                }
            })
    }
    fn take_next(&mut self) -> Option<usize> {
        if !self.pending.is_empty() {
            return Some(self.pending.remove(0));
        }
        if self.shuffle {
            if self.remaining.is_empty() && self.repeat == Repeat::All {
                self.reset_shuffle();
            }
            self.remaining.pop().or_else(|| {
                (self.repeat == Repeat::All)
                    .then_some(self.source_current)
                    .flatten()
            })
        } else {
            self.source_next()
        }
    }
    /// Explicitly queued tracks are FIFO and take priority over the source collection.
    pub fn advance(&mut self, natural: bool) {
        let Some(current) = self.current else {
            return;
        };
        if natural && self.repeat == Repeat::One {
            return;
        }
        if !self.is_manual(current) {
            self.source_current = Some(current);
        }
        let next = self.take_next();
        self.history.push(current);
        self.current = next;
        if let Some(next) = next {
            if !self.is_manual(next) {
                self.source_current = Some(next);
            }
        }
    }
    fn upcoming(&self) -> Option<&TrackInfo> {
        let current = self.current?;
        let next = if self.repeat == Repeat::One {
            Some(current)
        } else if let Some(&next) = self.pending.first() {
            Some(next)
        } else if self.shuffle {
            self.remaining.last().copied()
        } else {
            self.source_next()
        };
        next.and_then(|i| self.tracks.get(i))
    }
    pub fn previous(&mut self) {
        if let Some(previous) = self.history.pop() {
            if let Some(current) = self.current {
                if self.shuffle && !self.is_manual(current) {
                    self.remaining.push(current);
                }
            }
            self.remaining.retain(|&i| i != previous);
            self.pending.retain(|&i| i != previous);
            self.current = Some(previous);
            self.source_current = if !self.is_manual(previous) {
                Some(previous)
            } else {
                self.history
                    .iter()
                    .rev()
                    .copied()
                    .find(|&i| !self.is_manual(i))
            };
        } else if let Some(current) = self.current {
            self.jump(current.saturating_sub(1));
        }
    }
    fn jump(&mut self, index: usize) {
        if index >= self.tracks.len() {
            return;
        }
        self.current = Some(index);
        self.pending.retain(|&i| i != index);
        if !self.is_manual(index) {
            self.source_current = Some(index);
        }
        self.history.clear();
        self.reset_shuffle();
    }
    fn insert(&mut self, tracks: Vec<TrackInfo>, next: bool) {
        if tracks.is_empty() {
            return;
        }
        let after_current = self.current.map_or(self.tracks.len(), |i| i + 1);
        let at = if next {
            after_current
        } else {
            self.pending
                .iter()
                .max()
                .map_or(after_current, |i| (i + 1).max(after_current))
        };
        let count = tracks.len();
        self.tracks.splice(at..at, tracks);
        let shift = |i: usize| if i >= at { i + count } else { i };
        self.current = self.current.map(shift);
        self.source_current = self.source_current.map(shift);
        for list in [
            &mut self.history,
            &mut self.remaining,
            &mut self.manual,
            &mut self.pending,
        ] {
            for i in list {
                *i = shift(*i);
            }
        }
        self.manual.extend(at..at + count);
        if next {
            self.pending.splice(0..0, at..at + count);
        } else {
            self.pending.extend(at..at + count);
        }
    }
    pub fn insert_next(&mut self, tracks: Vec<TrackInfo>) {
        self.insert(tracks, true);
    }
    pub fn append(&mut self, tracks: Vec<TrackInfo>) {
        self.insert(tracks, false);
    }
    pub fn remove(&mut self, at: usize) -> bool {
        if at >= self.tracks.len() {
            return false;
        }
        let changed = self.current == Some(at);
        let removed_anchor = self.source_current == Some(at);
        self.tracks.remove(at);
        let remap = |i: usize| if i > at { i - 1 } else { i };
        self.current = self.current.filter(|&i| i != at).map(remap);
        self.source_current = self.source_current.filter(|&i| i != at).map(remap);
        for list in [
            &mut self.history,
            &mut self.remaining,
            &mut self.manual,
            &mut self.pending,
        ] {
            list.retain(|&i| i != at);
            for i in list {
                *i = remap(*i);
            }
        }
        if removed_anchor {
            self.source_current = (0..at).rev().find(|&i| !self.is_manual(i));
        }
        if changed {
            self.current = self.take_next();
            if let Some(current) = self.current {
                if !self.is_manual(current) {
                    self.source_current = Some(current);
                }
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
        self.source_current = self.source_current.map(remap);
        for list in [
            &mut self.history,
            &mut self.remaining,
            &mut self.manual,
            &mut self.pending,
        ] {
            for i in list {
                *i = remap(*i);
            }
        }
        self.pending.sort_unstable();
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
    pub started: AtomicBool,
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
            started: AtomicBool::new(false),
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
    prefetcher: Option<thread::JoinHandle<()>>,
    cache: Arc<crate::buffering::PlaybackCache>,
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
        runner: fn(
            &mut TidalClient,
            &TrackInfo,
            Arc<Transport>,
            Arc<crate::buffering::PlaybackCache>,
        ) -> Result<String>,
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
        saved.queue.manual.retain(|&i| i < saved.queue.tracks.len());
        saved
            .queue
            .pending
            .retain(|&i| i < saved.queue.tracks.len());
        if saved
            .queue
            .source_current
            .is_none_or(|i| i >= saved.queue.tracks.len())
        {
            saved.queue.source_current = saved
                .queue
                .current
                .filter(|&i| !saved.queue.manual.contains(&i));
        }
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
        let cache = Arc::new(crate::buffering::PlaybackCache::default());
        let playback_cache = cache.clone();
        let (prefetch_state, prefetch_transport, prefetch_stop, prefetch_cache, prefetch_client) = (
            state.clone(),
            transport.clone(),
            shutdown.clone(),
            cache.clone(),
            client.clone(),
        );
        let prefetcher = thread::spawn(move || {
            while !prefetch_stop.load(Ordering::Relaxed) {
                let wanted = {
                    let s = prefetch_state.lock().unwrap();
                    let current = if s.active {
                        s.saved
                            .queue
                            .current
                            .and_then(|i| s.saved.queue.tracks.get(i))
                    } else {
                        None
                    };
                    if let Some(current) = current {
                        let next = s.saved.queue.upcoming().cloned();
                        let mut ids = vec![current.id];
                        if let Some(next) = &next {
                            ids.push(next.id);
                        }
                        prefetch_cache.retain(&ids);
                        Some((s.generation, current.id, next))
                    } else {
                        prefetch_cache.cancel_all();
                        None
                    }
                };
                if let Some((generation, current, Some(next))) = wanted {
                    let transport = prefetch_transport.lock().unwrap().clone();
                    if transport.generation == generation
                        && transport.started.load(Ordering::Acquire)
                        && prefetch_cache.contains(current)
                    {
                        let _ = prefetch_cache.preload(&prefetch_client, &next);
                    }
                }
                thread::sleep(Duration::from_millis(50));
            }
            prefetch_cache.cancel_all();
        });
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
                    runner(&mut client, &track, ctl.clone(), playback_cache.clone())
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
            prefetcher: Some(prefetcher),
            cache,
            save_path,
        }
    }
    #[cfg(test)]
    pub fn for_test() -> Self {
        fn fake(
            _: &mut TidalClient,
            _: &TrackInfo,
            ctl: Arc<Transport>,
            _: Arc<crate::buffering::PlaybackCache>,
        ) -> Result<String> {
            ctl.visual.lock().unwrap().cover = vec![Line::raw("cover")];
            ctl.started.store(true, Ordering::Release);
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
            loading: s.active && (!installed || !ctl.started.load(Ordering::Acquire)),
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
                s.saved.queue.jump(at);
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
                let first = s.saved.queue.pending.first().copied().unwrap_or(0);
                s.saved.queue.jump(first);
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
        self.cache.cancel_all();
        if let Some(prefetcher) = self.prefetcher.take() {
            let _ = prefetcher.join();
        }
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
        fn finish(
            _: &mut TidalClient,
            t: &TrackInfo,
            _: Arc<Transport>,
            _: Arc<crate::buffering::PlaybackCache>,
        ) -> Result<String> {
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
    fn next_track_is_prepared_before_current_playback_finishes() {
        use crate::test_support::{manifest, Server};
        let cdn = Server::audio(2);
        let api = Server::new(vec![manifest(&cdn.url), manifest(&cdn.url)]);
        fn hold(
            c: &mut TidalClient,
            track: &TrackInfo,
            ctl: Arc<Transport>,
            cache: Arc<crate::buffering::PlaybackCache>,
        ) -> Result<String> {
            let item = cache.prepare(c, track)?;
            while !item.ready.load(Ordering::Acquire) {
                if ctl.stop.load(Ordering::Relaxed) {
                    return Ok("quit".into());
                }
                thread::sleep(Duration::from_millis(5));
            }
            ctl.started.store(true, Ordering::Release);
            *ctl.visual.lock().unwrap() = Visual {
                cover: vec![Line::raw("cover")],
                ..Visual::default()
            };
            while !ctl.stop.load(Ordering::Relaxed) {
                thread::sleep(Duration::from_millis(5));
            }
            Ok("quit".into())
        }
        let player = Player::with_saved(api.client(), SavedPlayer::default(), None, hold);
        player.play_album(vec![test_track(1), test_track(2)]);
        let start = std::time::Instant::now();
        while cdn.requests.lock().unwrap().len() < 2 && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            cdn.requests.lock().unwrap().len(),
            2,
            "next track was not preloaded"
        );
        assert_eq!(
            player.snapshot().queue.current,
            Some(0),
            "current playback should still be running"
        );
        player.next();
        let start = std::time::Instant::now();
        while player.snapshot().loading && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(player.snapshot().queue.current, Some(1));
        assert!(!player.snapshot().loading);
        assert_eq!(
            api.requests.lock().unwrap().len(),
            2,
            "preloaded stream should be reused"
        );
    }
    #[test]
    fn preload_prediction_matches_playback_for_shuffle_repeat_and_play_next() {
        for repeat in [Repeat::Off, Repeat::All, Repeat::One] {
            let mut q = Queue::default();
            q.replace(tracks(5), 2);
            q.repeat = repeat;
            for shuffle in [false, true] {
                q.shuffle = shuffle;
                let predicted = q.upcoming().map(|t| t.id);
                let mut actual = q.clone();
                actual.advance(true);
                assert_eq!(predicted, actual.current.map(|i| actual.tracks[i].id));
            }
        }
    }
    #[test]
    fn queued_tracks_are_fifo_then_album_resumes_even_after_restart() {
        let mut q = Queue::default();
        q.replace(vec![test_track(1), test_track(2), test_track(3)], 0);
        q.append(vec![test_track(10)]);
        q.append(vec![test_track(11), test_track(12)]);
        q.insert_next(vec![test_track(9)]);
        assert_eq!(
            q.tracks.iter().map(|t| t.id).collect::<Vec<_>>(),
            vec![1, 9, 10, 11, 12, 2, 3]
        );
        let mut played = vec![];
        while let Some(i) = q.current {
            played.push(q.tracks[i].id);
            let expected = q.upcoming().map(|t| t.id);
            q.advance(true);
            assert_eq!(expected, q.current.map(|i| q.tracks[i].id));
            q = serde_json::from_slice(&serde_json::to_vec(&q).unwrap()).unwrap();
        }
        assert_eq!(played, vec![1, 9, 10, 11, 12, 2, 3]);
    }
    #[test]
    fn explicit_queue_has_priority_with_shuffle_and_survives_edits() {
        let mut q = Queue::default();
        q.replace(tracks(4), 0);
        q.toggle_shuffle();
        q.append(vec![test_track(10)]);
        q.append(vec![test_track(11)]);
        q.move_item(1, 2);
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 11);
        q.append(vec![test_track(12)]);
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 10);
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 12);
        let mut source = vec![];
        while q.current.is_some() {
            q.advance(true);
            if let Some(i) = q.current {
                source.push(q.tracks[i].id);
            }
        }
        source.sort_unstable();
        assert_eq!(source, vec![1, 2, 3]);
        q.shuffle = false;
        q.replace(tracks(3), 0);
        q.append(vec![test_track(10), test_track(11)]);
        assert!(!q.remove(1));
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 11);
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 1);
    }
    #[test]
    fn queue_added_after_album_ends_starts_before_restarting_album() {
        let player = Player::for_test();
        player.play_album(vec![test_track(1), test_track(2)]);
        player.next();
        player.next();
        assert!(player.snapshot().queue.current.is_none());
        player.enqueue(vec![test_track(9)], false);
        player.toggle_pause();
        let state = player.snapshot();
        assert_eq!(state.queue.tracks[state.queue.current.unwrap()].id, 9);
    }
    #[test]
    fn adding_during_queued_playback_preserves_order_and_duplicate_tracks() {
        let mut q = Queue::default();
        q.replace(vec![test_track(1), test_track(2)], 0);
        q.append(vec![test_track(2)]);
        q.advance(true);
        q.append(vec![test_track(9)]);
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 9);
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 2);
        q.advance(true);
        assert!(q.current.is_none());
        q = Queue::default();
        q.append(vec![test_track(8), test_track(9)]);
        q.jump(0);
        q.repeat = Repeat::All;
        q.advance(true);
        q.advance(true);
        assert_eq!(q.tracks[q.current.unwrap()].id, 8);
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
