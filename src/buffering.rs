//! At most the current track and its likely successor are retained on disk.
//! Audio preparation and cover fetching run independently of the audio device.
use crate::api::{EncryptionInfo, TidalClient, TrackInfo};
use anyhow::Result;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::thread;

pub struct PreparedTrack {
    pub file: tempfile::NamedTempFile,
    pub done: Arc<AtomicBool>,
    pub ready: Arc<AtomicBool>,
    pub bytes: Arc<AtomicU64>,
    pub total: Arc<AtomicU64>,
    pub cover: Mutex<Option<Vec<u8>>>,
    pub error: Mutex<Option<String>>,
    cancelled: AtomicBool,
}
impl PreparedTrack {
    fn start(mut client: TidalClient, track: TrackInfo) -> Result<Arc<Self>> {
        let item = Arc::new(Self {
            file: tempfile::Builder::new().suffix(".audio").tempfile()?,
            done: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(false)),
            bytes: Arc::new(AtomicU64::new(0)),
            total: Arc::new(AtomicU64::new(0)),
            cover: Mutex::new(None),
            error: Mutex::new(None),
            cancelled: AtomicBool::new(false),
        });
        let audio = item.clone();
        thread::spawn(move || {
            let result = (|| {
                if audio.cancelled.load(Ordering::Relaxed) {
                    return Ok(());
                }
                client.session = crate::auth::refreshed_session(&client.session)?;
                let stream = client.stream_url(track.id)?;
                // Cover network latency must never hold up audio buffering.
                if let Some(id) = track.album_cover {
                    let cover_client = client.clone();
                    let cover = audio.clone();
                    thread::spawn(move || {
                        if !cover.cancelled.load(Ordering::Relaxed) {
                            if let Ok(bytes) = cover_client.fetch_cover(&id, 320) {
                                *cover.cover.lock().unwrap() = Some(bytes);
                            }
                        }
                    });
                }
                audio.download(&stream.url, stream.encryption)
            })();
            if let Err(error) = result {
                *audio.error.lock().unwrap() = Some(error.to_string());
            }
            audio.done.store(true, Ordering::Release);
            audio.ready.store(true, Ordering::Release);
        });
        Ok(item)
    }
    fn download(&self, url: &str, encryption: Option<EncryptionInfo>) -> Result<()> {
        use ctr::cipher::{KeyIvInit, StreamCipher};
        type Cipher = ctr::Ctr64BE<aes::Aes128>;
        let mut cipher = encryption.map(|enc| {
            let mut iv = [0u8; 16];
            iv[..8].copy_from_slice(&enc.nonce);
            Cipher::new_from_slices(&enc.key, &iv).expect("valid key")
        });
        if self.cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(120))
            .build()?;
        let mut response = http.get(url).send()?.error_for_status()?;
        if let Some(size) = response.content_length() {
            self.total.store(size, Ordering::Relaxed);
        }
        if self.cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut file = std::fs::File::create(self.file.path())?;
        let mut buffer = vec![0u8; 65536];
        loop {
            if self.cancelled.load(Ordering::Relaxed) {
                break;
            }
            let n = response.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            if let Some(cipher) = &mut cipher {
                cipher.apply_keystream(&mut buffer[..n]);
            }
            file.write_all(&buffer[..n])?;
            let count = self.bytes.fetch_add(n as u64, Ordering::Release) + n as u64;
            if count >= 65536 {
                self.ready.store(true, Ordering::Release);
            }
        }
        Ok(())
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        // Unlink promptly even if a cancelled network request is still winding down.
        let _ = std::fs::remove_file(self.file.path());
    }
}
#[derive(Default)]
pub struct PlaybackCache {
    entries: Mutex<HashMap<u64, Arc<PreparedTrack>>>,
}
impl PlaybackCache {
    fn request(
        &self,
        client: &TidalClient,
        track: &TrackInfo,
        retry_failed: bool,
    ) -> Result<Arc<PreparedTrack>> {
        let mut entries = self.entries.lock().unwrap();
        if let Some(item) = entries.get(&track.id) {
            if !item.cancelled.load(Ordering::Relaxed)
                && (!retry_failed || item.error.lock().unwrap().is_none())
            {
                return Ok(item.clone());
            }
        }
        let item = PreparedTrack::start(client.clone(), track.clone())?;
        entries.insert(track.id, item.clone());
        Ok(item)
    }
    pub fn prepare(&self, client: &TidalClient, track: &TrackInfo) -> Result<Arc<PreparedTrack>> {
        self.request(client, track, true)
    }
    pub fn preload(&self, client: &TidalClient, track: &TrackInfo) -> Result<Arc<PreparedTrack>> {
        self.request(client, track, false)
    }
    pub fn contains(&self, id: u64) -> bool {
        self.entries.lock().unwrap().contains_key(&id)
    }
    pub fn retain(&self, ids: &[u64]) {
        self.entries.lock().unwrap().retain(|id, item| {
            if ids.contains(id) {
                true
            } else {
                item.cancel();
                false
            }
        });
    }
    pub fn cancel_all(&self) {
        self.retain(&[]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        player::test_track,
        test_support::{manifest, Reply, Server},
    };
    use std::time::{Duration, Instant};
    fn wait(item: &PreparedTrack) {
        let start = Instant::now();
        while !item.done.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(item.done.load(Ordering::Acquire));
        assert!(item.error.lock().unwrap().is_none());
    }
    #[test]
    fn prepared_audio_is_reused_and_evicted() {
        let cdn = Server::audio(2);
        let api = Server::new(vec![manifest(&cdn.url), manifest(&cdn.url)]);
        let cache = PlaybackCache::default();
        let client = api.client();
        let track = test_track(1);
        let first = cache.prepare(&client, &track).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &cache.prepare(&client, &track).unwrap()
        ));
        wait(&first);
        assert_eq!(
            std::fs::read(first.file.path()).unwrap(),
            include_bytes!("../tests/fixtures/silence.flac")
        );
        let next = cache.prepare(&client, &test_track(2)).unwrap();
        wait(&next);
        assert!(Arc::ptr_eq(
            &next,
            &cache.prepare(&client, &test_track(2)).unwrap()
        ));
        assert_eq!(api.requests.lock().unwrap().len(), 2);
        assert_eq!(cdn.requests.lock().unwrap().len(), 2);
        cache.retain(&[2]);
        assert!(!cache.contains(1));
        assert!(cache.contains(2));
        cache.cancel_all();
        assert!(!cache.contains(2));
    }
    #[test]
    fn failed_preload_is_retryable() {
        let cdn = Server::audio(1);
        let api = Server::new(vec![
            Reply {
                status: 503,
                headers: String::new(),
                body: "offline".into(),
            },
            manifest(&cdn.url),
        ]);
        let cache = PlaybackCache::default();
        let client = api.client();
        let track = test_track(1);
        let failed = cache.prepare(&client, &track).unwrap();
        let start = Instant::now();
        while !failed.done.load(Ordering::Acquire) && start.elapsed() < Duration::from_secs(3) {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(failed.error.lock().unwrap().is_some());
        assert!(Arc::ptr_eq(
            &failed,
            &cache.preload(&client, &track).unwrap()
        ));
        assert_eq!(api.requests.lock().unwrap().len(), 1);
        let retried = cache.prepare(&client, &track).unwrap();
        assert!(!Arc::ptr_eq(&failed, &retried));
        wait(&retried);
    }
}
