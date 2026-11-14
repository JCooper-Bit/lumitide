use std::collections::VecDeque;
use std::io::{self, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fs, thread};

use anyhow::Result;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{
        disable_raw_mode, enable_raw_mode, Clear, ClearType, EnterAlternateScreen,
        LeaveAlternateScreen,
    },
};
use ratatui::{backend::CrosstermBackend, Terminal};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::DecoderOptions;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

use crate::api::{TidalClient, TrackInfo};
use crate::color_state::ColorState;
use crate::config;
use crate::cover::{render_cover, render_placeholder, ART_CHARS};
use crate::panel::{self, PanelState};
use crate::spectrum::{self, FFT_SIZE, NUM_BARS};

const DROP_DURATION_SECS: f64 = 10.0;
const TARGET_SR: u32 = 22_050;

// Discrete volume levels (as linear multipliers, 1% – 100%)
const VOL_PRESETS: &[f32] = &[
    0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08, 0.09, 0.10, 0.20, 0.30, 0.40, 0.50, 0.60, 0.70,
    0.80, 0.90, 1.00,
];

fn vol_up(v: f32) -> f32 {
    VOL_PRESETS
        .iter()
        .copied()
        .find(|&p| p > v + 1e-4)
        .unwrap_or(1.00)
}
fn vol_down(v: f32) -> f32 {
    VOL_PRESETS
        .iter()
        .copied()
        .rev()
        .find(|&p| p < v - 1e-4)
        .unwrap_or(0.0)
}

type AppTerminal = Terminal<CrosstermBackend<io::Stdout>>;

fn setup_terminal() -> Result<AppTerminal> {
    crossterm::style::force_color_output(true);
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, Clear(ClearType::All), EnterAlternateScreen)?;
    Ok(Terminal::new(CrosstermBackend::new(stdout))?)
}

fn teardown_terminal(terminal: &mut AppTerminal) {
    let _ = disable_raw_mode();
    let _ = execute!(terminal.backend_mut(), LeaveAlternateScreen);
}

/// Register with the OS media controls (SMTC / MPRIS / MPRemoteCommandCenter).
/// Returns None silently if the platform doesn't support it or initialisation fails.
fn build_media_controls(
    title: &str,
    artist: &str,
    album: &str,
    duration: u64,
    cover_url: Option<&str>,
    tx: std::sync::mpsc::Sender<souvlaki::MediaControlEvent>,
) -> Option<souvlaki::MediaControls> {
    use souvlaki::{MediaControls, MediaMetadata, MediaPlayback, PlatformConfig};
    #[cfg(target_os = "linux")]
    if std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_none()
        && !std::env::var_os("XDG_RUNTIME_DIR")
            .map(std::path::PathBuf::from)
            .is_some_and(|p| p.join("bus").exists())
    {
        return None;
    }

    #[cfg(target_os = "windows")]
    let config = {
        // Windows Terminal has no Win32 HWND (virtual console), so we create a
        // hidden 1×1 window purely to give SMTC a valid owner handle.
        extern "system" {
            fn CreateWindowExW(
                ex_style: u32,
                class_name: *const u16,
                window_name: *const u16,
                style: u32,
                x: i32,
                y: i32,
                w: i32,
                h: i32,
                parent: *mut std::ffi::c_void,
                menu: *mut std::ffi::c_void,
                instance: *mut std::ffi::c_void,
                param: *mut std::ffi::c_void,
            ) -> *mut std::ffi::c_void;
        }
        let class: Vec<u16> = "Static\0".encode_utf16().collect();
        let name: Vec<u16> = "lumitide\0".encode_utf16().collect();
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class.as_ptr(),
                name.as_ptr(),
                0,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return None;
        }
        PlatformConfig {
            dbus_name: "",
            display_name: "Lumitide",
            hwnd: Some(hwnd),
        }
    };

    #[cfg(target_os = "macos")]
    let config = PlatformConfig {
        dbus_name: "",
        display_name: "Lumitide",
        hwnd: None,
    };

    #[cfg(target_os = "linux")]
    let config = PlatformConfig {
        dbus_name: "org.mpris.MediaPlayer2.lumitide",
        display_name: "Lumitide",
        hwnd: None,
    };

    let mut controls = MediaControls::new(config).ok()?;
    // attach must come first — it triggers SMTC session initialisation on Windows.
    // set_metadata called before attach returns 0x80070032 (media type not initialized).
    let _ = controls.attach(move |event| {
        let _ = tx.send(event);
    });
    let _ = controls.set_playback(MediaPlayback::Playing { progress: None });
    let _ = controls.set_metadata(MediaMetadata {
        title: Some(title),
        artist: Some(artist),
        album: Some(album),
        cover_url,
        duration: Some(Duration::from_secs(duration)),
    });
    Some(controls)
}

// ─── Streaming file wrapper ───────────────────────────────────────────────────

struct StreamingFile {
    file: fs::File,
    download_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl io::Read for StreamingFile {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Ok(0);
            }
            match self.file.read(buf) {
                Ok(0) => {
                    if self.stop.load(Ordering::Relaxed) {
                        return Ok(0);
                    }
                    if self.download_done.load(Ordering::Relaxed) {
                        return Ok(0); // true EOF
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                other => return other,
            }
        }
    }
}

impl Seek for StreamingFile {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl MediaSource for StreamingFile {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        // Symphonia's MP4/M4A demuxer requires a known byte length to seek within
        // the container. Return the current file size — correct for local files
        // (always complete) and reflects downloaded bytes for streaming files.
        self.file.metadata().ok().map(|m| m.len())
    }
}

// ─── Public entry points ──────────────────────────────────────────────────────

/// Run the original audio/visualisation engine without reading terminal events.
pub fn run_background(
    client: &mut TidalClient,
    track: &TrackInfo,
    ctl: Arc<crate::player::Transport>,
    cache: Arc<crate::buffering::PlaybackCache>,
) -> Result<String> {
    let prepared = cache.prepare(client, track)?;
    while !prepared.ready.load(Ordering::Acquire) && !prepared.done.load(Ordering::Acquire) {
        if ctl.stop.load(Ordering::Relaxed) {
            return Ok("quit".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    if ctl.stop.load(Ordering::Relaxed) {
        return Ok("quit".into());
    }
    if let Some(error) = prepared.error.lock().unwrap().clone() {
        anyhow::bail!(error);
    }
    if prepared.bytes.load(Ordering::Acquire) == 0 {
        anyhow::bail!("No audio was received from Tidal.");
    }
    while ctl.seek_ms.load(Ordering::Relaxed) != u64::MAX && !prepared.done.load(Ordering::Acquire)
    {
        if ctl.stop.load(Ordering::Relaxed) {
            return Ok("quit".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let cover = prepared.cover.lock().unwrap().clone();
    play(
        None,
        prepared.file.path(),
        prepared.done.clone(),
        prepared.bytes.clone(),
        prepared.total.clone(),
        track,
        cover.as_deref(),
        false,
        None,
        ctl.volume.clone(),
        false,
        false,
        Some(ctl),
        Some(prepared.clone()),
    )
}

/// Play a local audio file using the same UI.
pub fn run_local(
    path: &str,
    track: &TrackInfo,
    cover_bytes: Option<&[u8]>,
    debug: bool,
    track_label: Option<String>,
    shared_volume: Option<Arc<Mutex<f32>>>,
) -> Result<String> {
    struct Cleanup;
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen);
        }
    }
    let _cleanup = Cleanup;
    let terminal = setup_terminal()?;
    let download_done = Arc::new(AtomicBool::new(true)); // already on disk
    let dl_bytes = Arc::new(AtomicU64::new(0));
    let dl_total = Arc::new(AtomicU64::new(0));
    let volume = shared_volume.unwrap_or_else(|| Arc::new(Mutex::new(config::load().volume)));
    play(
        Some(terminal),
        std::path::Path::new(path),
        download_done,
        dl_bytes,
        dl_total,
        track,
        cover_bytes,
        debug,
        track_label.as_deref(),
        volume,
        true,
        false,
        None,
        None,
    )
}

// ─── Core playback + UI loop ──────────────────────────────────────────────────

fn play(
    mut terminal: Option<AppTerminal>,
    path: &std::path::Path,
    download_done: Arc<AtomicBool>,
    dl_bytes: Arc<AtomicU64>,
    dl_total: Arc<AtomicU64>,
    track: &TrackInfo,
    cover_bytes: Option<&[u8]>,
    _debug: bool,
    track_label: Option<&str>,
    volume: Arc<Mutex<f32>>,
    is_local: bool,
    already_saved: bool,
    remote: Option<Arc<crate::player::Transport>>,
    buffered: Option<Arc<crate::buffering::PreparedTrack>>,
) -> Result<String> {
    let cfg = config::load();

    // ── Probe for sample rate / channels ─────────────────────────────────────
    let stop_all = remote
        .as_ref()
        .map(|r| r.stop.clone())
        .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
    let probe_result = probe_audio(path, download_done.clone(), stop_all.clone());
    if probe_result.is_err() {
        // Must teardown before propagating — play() owns terminal by value
        if let Some(t) = terminal.as_mut() {
            teardown_terminal(t);
        }
    }
    let (sample_rate, channels) = probe_result?;

    // ── Render cover art (once) ───────────────────────────────────────────────
    let art_chars = if track_label.is_some() { 22 } else { ART_CHARS };
    let mut cover_art = cover_bytes
        .map(|b| render_cover(b, art_chars))
        .unwrap_or_else(|| render_placeholder(art_chars));
    let palette = crate::styling::PALETTE.to_vec();

    // ── Media controls (SMTC / MPRIS / MPRemoteCommandCenter) ────────────────
    // Write cover bytes to a temp file and pass a file:// URL.
    // HTTPS URLs via CreateFromUri require a message pump for async download;
    // GetFileFromPathAsync blocks synchronously and works without one.
    // Keep _cover_tmp alive for the track duration so the file isn't deleted.
    let mut _cover_tmp: Option<tempfile::NamedTempFile> = cover_bytes.and_then(|bytes| {
        use std::io::Write;
        let mut tmp = tempfile::Builder::new().suffix(".jpg").tempfile().ok()?;
        tmp.write_all(bytes).ok()?;
        Some(tmp)
    });
    let cover_url_str: Option<String> = _cover_tmp.as_ref().map(|tmp| {
        // souvlaki strips "file://" then passes the remainder to GetFileFromPathAsync,
        // which requires native Windows backslash paths — do NOT convert to forward slashes.
        format!("file://{}", tmp.path().to_string_lossy())
    });
    let (media_tx, media_rx) = std::sync::mpsc::channel::<souvlaki::MediaControlEvent>();
    let mut media_controls = build_media_controls(
        &track.title,
        &track.artist_name,
        &track.album_name,
        track.duration,
        cover_url_str.as_deref(),
        media_tx,
    );

    // ── Shared state ──────────────────────────────────────────────────────────
    let spec_buf: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(vec![0.0f32; FFT_SIZE]));
    let audio_buf: Arc<Mutex<VecDeque<f32>>> = Arc::new(Mutex::new(VecDeque::with_capacity(
        sample_rate as usize * 4,
    )));
    let current_sample = Arc::new(AtomicU64::new(0));
    let playback_done = Arc::new(AtomicBool::new(false));
    let paused = Arc::new(AtomicBool::new(
        remote
            .as_ref()
            .is_some_and(|r| r.paused.load(Ordering::Relaxed)),
    ));
    struct StopOnDrop(Arc<AtomicBool>);
    impl Drop for StopOnDrop {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let _stop_guard = StopOnDrop(stop_all.clone());
    let seek_request =
        Arc::new(AtomicU64::new(remote.as_ref().map_or(u64::MAX, |r| {
            r.seek_ms.swap(u64::MAX, Ordering::Relaxed)
        })));
    let seek_error = Arc::new(Mutex::new(None::<String>));

    let dl_status: Arc<Mutex<String>> = Arc::new(Mutex::new(if already_saved {
        "✓ Saved".to_string()
    } else {
        String::new()
    }));
    let dl_flash_until: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

    let drop_times: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let beat_times: Arc<Mutex<Vec<f64>>> = Arc::new(Mutex::new(Vec::new()));
    let analysis_done = Arc::new(AtomicBool::new(false));

    let total_frames = probe_total_frames(path, download_done.clone(), stop_all.clone())
        .unwrap_or(track.duration * sample_rate as u64);

    // Query device config early so decode thread can resample to device rate
    let (device_sr, device_ch) = {
        let host = cpal::default_host();
        host.default_output_device()
            .and_then(|d| d.default_output_config().ok())
            .map(|c| (c.sample_rate().0, c.channels() as usize))
            .unwrap_or((sample_rate, channels))
    };

    // ── Decode thread ─────────────────────────────────────────────────────────
    let decode_thread = {
        let spec_buf = spec_buf.clone();
        let audio_buf = audio_buf.clone();
        let current_sample = current_sample.clone();
        let playback_done = playback_done.clone();
        let stop_all = stop_all.clone();
        let download_done = download_done.clone();
        let path = path.to_path_buf();
        let ch = channels;
        let file_sr = sample_rate;
        let seek_request = seek_request.clone();
        let seek_error = seek_error.clone();

        thread::spawn(move || {
            let file = match fs::File::open(&path) {
                Ok(f) => f,
                Err(_) => {
                    playback_done.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let streaming = StreamingFile {
                file,
                download_done: download_done.clone(),
                stop: stop_all.clone(),
            };
            let mss = MediaSourceStream::new(Box::new(streaming), Default::default());
            let probed = match symphonia::default::get_probe().format(
                &Hint::new(),
                mss,
                &FormatOptions::default(),
                &MetadataOptions::default(),
            ) {
                Ok(p) => p,
                Err(_) => {
                    playback_done.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let mut format = probed.format;
            let track_info = match format.default_track() {
                Some(t) => t.clone(),
                None => {
                    playback_done.store(true, Ordering::Relaxed);
                    return;
                }
            };
            let mut decoder = match symphonia::default::get_codecs()
                .make(&track_info.codec_params, &DecoderOptions::default())
            {
                Ok(d) => d,
                Err(_) => {
                    playback_done.store(true, Ordering::Relaxed);
                    return;
                }
            };

            let mut discard_until: Option<f64> = None;
            let time_base = track_info.codec_params.time_base;
            loop {
                if stop_all.load(Ordering::Relaxed) {
                    break;
                }
                let requested = seek_request.swap(u64::MAX, Ordering::Relaxed);
                if requested != u64::MAX {
                    use symphonia::core::formats::{SeekMode, SeekTo};
                    use symphonia::core::units::Time;
                    let seconds = requested as f64 / 1000.0;
                    match format.seek(
                        SeekMode::Accurate,
                        SeekTo::Time {
                            time: Time::from(seconds),
                            track_id: Some(track_info.id),
                        },
                    ) {
                        Ok(_) => {
                            decoder.reset();
                            playback_done.store(false, Ordering::Relaxed);
                            audio_buf.lock().unwrap_or_else(|e| e.into_inner()).clear();
                            current_sample
                                .store((seconds * device_sr as f64) as u64, Ordering::Relaxed);
                            discard_until = Some(seconds);
                        }
                        Err(e) => *seek_error.lock().unwrap() = Some(format!("Cannot seek: {e}")),
                    }
                }

                // Backpressure: don't decode too far ahead
                loop {
                    if stop_all.load(Ordering::Relaxed) {
                        return;
                    }
                    let buf_len = audio_buf.lock().unwrap_or_else(|e| e.into_inner()).len();
                    if buf_len < device_sr as usize * device_ch * 2
                        || seek_request.load(Ordering::Relaxed) != u64::MAX
                    {
                        break;
                    }
                    thread::sleep(Duration::from_millis(5));
                }

                match format.next_packet() {
                    Ok(packet) => {
                        if packet.track_id() != track_info.id {
                            continue;
                        }
                        let packet_time = time_base
                            .map(|tb| tb.calc_time(packet.ts()))
                            .map(|t| t.seconds as f64 + t.frac)
                            .unwrap_or(0.0);
                        match decoder.decode(&packet) {
                            Ok(decoded) => {
                                let spec = *decoded.spec();
                                let mut sbuf =
                                    SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                                sbuf.copy_interleaved_ref(decoded);
                                let all_samples = sbuf.samples();
                                let skip = discard_until
                                    .map(|target| {
                                        ((target - packet_time).max(0.0) * file_sr as f64) as usize
                                            * ch
                                    })
                                    .unwrap_or(0)
                                    .min(all_samples.len());
                                if skip == all_samples.len() {
                                    continue;
                                }
                                discard_until = None;
                                let samples = &all_samples[skip..];

                                // Update spectrum ring buffer (mono mix)
                                {
                                    let mono: Vec<f32> = if ch == 2 {
                                        samples.chunks(2).map(|c| (c[0] + c[1]) * 0.5).collect()
                                    } else {
                                        samples.to_vec()
                                    };
                                    let mut sb = spec_buf.lock().unwrap_or_else(|e| e.into_inner());
                                    let n = mono.len().min(sb.len());
                                    let len = sb.len();
                                    sb.rotate_left(n);
                                    sb[len - n..].copy_from_slice(&mono[..n]);
                                }

                                // Convert channels then resample for the device
                                let device_samples =
                                    convert_audio(samples, ch, device_ch, file_sr, device_sr);
                                let mut buf = audio_buf.lock().unwrap_or_else(|e| e.into_inner());
                                buf.extend(device_samples.iter().copied());
                            }
                            Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                            Err(_) => break,
                        }
                    }
                    Err(symphonia::core::errors::Error::IoError(e))
                        if e.kind() == io::ErrorKind::UnexpectedEof =>
                    {
                        playback_done.store(true, Ordering::Relaxed);
                        while !stop_all.load(Ordering::Relaxed)
                            && seek_request.load(Ordering::Relaxed) == u64::MAX
                        {
                            thread::sleep(Duration::from_millis(10));
                        }
                    }
                    Err(_) => break,
                }
            }
            playback_done.store(true, Ordering::Relaxed);
        })
    };
    struct DecoderGuard(Option<thread::JoinHandle<()>>, Arc<AtomicBool>);
    impl Drop for DecoderGuard {
        fn drop(&mut self) {
            self.1.store(true, Ordering::Relaxed);
            if let Some(handle) = self.0.take() {
                let _ = handle.join();
            }
        }
    }
    let _decoder_guard = DecoderGuard(Some(decode_thread), stop_all.clone());

    // ── cpal audio stream ─────────────────────────────────────────────────────
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("No audio output device"))?;

    let cpal_config = cpal::StreamConfig {
        channels: device_ch as cpal::ChannelCount,
        sample_rate: cpal::SampleRate(device_sr),
        buffer_size: cpal::BufferSize::Default,
    };

    let audio_buf_cpal = audio_buf.clone();
    let paused_cpal = paused.clone();
    let volume_cpal = volume.clone();
    let playback_done_cpal = playback_done.clone();
    let current_sample_cpal = current_sample.clone();
    let output_error = Arc::new(Mutex::new(None::<String>));
    let output_error_cpal = output_error.clone();

    macro_rules! build_stream {
        ($sample:ty) => {
            device.build_output_stream::<$sample, _, _>(
                &cpal_config,
                move |data: &mut [$sample], _| {
                    if paused_cpal.load(Ordering::Relaxed) {
                        data.fill(<$sample as cpal::Sample>::from_sample(0.0f32));
                        return;
                    }
                    let vol = *volume_cpal.lock().unwrap_or_else(|e| e.into_inner());
                    let mut buf = audio_buf_cpal.lock().unwrap_or_else(|e| e.into_inner());
                    let mut underrun = false;
                    let mut consumed = 0u64;
                    for sample in data.iter_mut() {
                        if let Some(s) = buf.pop_front() {
                            *sample = <$sample as cpal::Sample>::from_sample(s * vol);
                            consumed += 1;
                        } else {
                            *sample = <$sample as cpal::Sample>::from_sample(0.0f32);
                            underrun = true;
                        }
                    }
                    current_sample_cpal.fetch_add(consumed / device_ch as u64, Ordering::Relaxed);
                    if underrun && buf.is_empty() && playback_done_cpal.load(Ordering::Relaxed) {
                        // All decoded audio consumed
                    }
                },
                move |err| {
                    *output_error_cpal.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(err.to_string());
                },
                None,
            )
        };
    }
    let stream = match device.default_output_config()?.sample_format() {
        cpal::SampleFormat::F32 => build_stream!(f32),
        cpal::SampleFormat::F64 => build_stream!(f64),
        cpal::SampleFormat::I16 => build_stream!(i16),
        cpal::SampleFormat::U16 => build_stream!(u16),
        cpal::SampleFormat::I32 => build_stream!(i32),
        cpal::SampleFormat::U32 => build_stream!(u32),
        other => anyhow::bail!("Unsupported output sample format: {other:?}"),
    }?;
    stream.play()?;
    if let Some(remote) = &remote {
        remote.started.store(true, Ordering::Release);
    }

    // ── Analysis thread (drop + beat detection) ───────────────────────────────
    if !cfg.calm_mode {
        let path = path.to_path_buf();
        let drop_times = drop_times.clone();
        let beat_times = beat_times.clone();
        let analysis_done = analysis_done.clone();
        let stop_all = stop_all.clone();
        let download_done = download_done.clone();
        let drop_det = cfg.drop_detection;

        thread::spawn(move || {
            // Wait for full download before analysis
            while !download_done.load(Ordering::Relaxed) {
                if stop_all.load(Ordering::Relaxed) {
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
            if stop_all.load(Ordering::Relaxed) {
                return;
            }

            if let Ok(audio) = load_mono_f32(&path, TARGET_SR, &stop_all) {
                // Beat detection
                let beats = detect_beats(&audio, TARGET_SR);
                *beat_times.lock().unwrap_or_else(|e| e.into_inner()) = beats;

                if drop_det {
                    let drops = detect_drops(&audio, TARGET_SR);
                    *drop_times.lock().unwrap_or_else(|e| e.into_inner()) = drops;
                }
            }
            analysis_done.store(true, Ordering::Relaxed);
        });
    }

    // terminal already set up by caller

    let mut bar_peaks = vec![0.0f32; NUM_BARS];
    let mut bar_peak_hold = vec![0u32; NUM_BARS];
    let mut cs = ColorState::new(palette.clone());
    let mut drop_active_until: Option<Instant> = None;
    let mut show_controls = false;
    let mut fired_drops: std::collections::HashSet<u64> = Default::default();
    let mut beat_snap: Vec<f64> = Vec::new();
    let mut beat_idx: usize = 0;
    let band_edges = spectrum::compute_band_edges(sample_rate);

    let mut media_tick = Instant::now();
    let mut cover_loaded = cover_bytes.is_some();
    let result_str = loop {
        if let Some(error) = output_error.lock().unwrap().take() {
            anyhow::bail!("Audio output failed: {error}");
        }
        if let Some(buffered) = &buffered {
            if let Some(error) = buffered.error.lock().unwrap().clone() {
                anyhow::bail!(error);
            }
            if !cover_loaded {
                if let Some(bytes) = buffered.cover.lock().unwrap().as_ref() {
                    cover_art = render_cover(bytes, art_chars);
                    cover_loaded = true;
                    use std::io::Write;
                    if let Ok(mut file) = tempfile::Builder::new().suffix(".jpg").tempfile() {
                        if file.write_all(bytes).is_ok() {
                            let url = format!("file://{}", file.path().to_string_lossy());
                            if let Some(mc) = media_controls.as_mut() {
                                let _ = mc.set_metadata(souvlaki::MediaMetadata {
                                    title: Some(&track.title),
                                    artist: Some(&track.artist_name),
                                    album: Some(&track.album_name),
                                    duration: Some(Duration::from_secs(track.duration)),
                                    cover_url: Some(&url),
                                });
                            }
                            _cover_tmp = Some(file);
                        }
                    }
                }
            }
        }
        if let Some(ctl) = &remote {
            if ctl.stop.load(Ordering::Relaxed) {
                break "quit".to_string();
            }
            paused.store(ctl.paused.load(Ordering::Relaxed), Ordering::Relaxed);
            ctl.position_ms.store(
                current_sample.load(Ordering::Relaxed) * 1000 / device_sr as u64,
                Ordering::Relaxed,
            );
            // Seeking waits for a complete file, avoiding incorrect MP4/FLAC offsets during buffering.
            if download_done.load(Ordering::Relaxed) {
                let request = ctl.seek_ms.swap(u64::MAX, Ordering::Relaxed);
                if request != u64::MAX {
                    let max_ms = track.duration.saturating_mul(1000).saturating_sub(250);
                    seek_request.store(
                        if max_ms > 0 {
                            request.min(max_ms)
                        } else {
                            request
                        },
                        Ordering::Relaxed,
                    );
                }
            }
            if let Some(error) = seek_error.lock().unwrap().take() {
                *ctl.error.lock().unwrap() = Some(error);
            }
        }
        let elapsed_secs = current_sample.load(Ordering::Relaxed) as f64 / device_sr as f64;
        let total_secs = if total_frames > 0 {
            total_frames as f64 / sample_rate as f64
        } else {
            0.0
        };

        if media_tick.elapsed() > Duration::from_millis(500) {
            if let Some(mc) = media_controls.as_mut() {
                let progress = Some(souvlaki::MediaPosition(Duration::from_secs_f64(
                    elapsed_secs,
                )));
                let state = if paused.load(Ordering::Relaxed) {
                    souvlaki::MediaPlayback::Paused { progress }
                } else {
                    souvlaki::MediaPlayback::Playing { progress }
                };
                let _ = mc.set_playback(state);
            }
            media_tick = Instant::now();
        }
        // Check for drops
        if !cfg.calm_mode {
            let dt_list = drop_times.lock().unwrap_or_else(|e| e.into_inner()).clone();
            for &dt in &dt_list {
                let key = (dt * 1000.0) as u64;
                if !fired_drops.contains(&key) && elapsed_secs >= dt {
                    fired_drops.insert(key);
                    drop_active_until =
                        Some(Instant::now() + Duration::from_secs_f64(DROP_DURATION_SECS));
                }
            }
        }
        let drop_active = drop_active_until.map_or(false, |t| Instant::now() < t) && !cfg.calm_mode;
        cs.update(drop_active);

        // Beat advance
        if drop_active {
            if beat_snap.is_empty() {
                let bt = beat_times.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if !bt.is_empty() {
                    beat_snap = bt;
                }
            }
            while beat_idx < beat_snap.len() && elapsed_secs >= beat_snap[beat_idx] {
                cs.advance();
                beat_idx += 1;
            }
        }

        let bar_color = cs.current_color();
        let cover_lines = if cs.colors_active() {
            &cover_art.color
        } else {
            &cover_art.mono
        };

        let dl_stat = dl_status.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let dl_b = dl_bytes.load(Ordering::Relaxed);
        let dl_t = dl_total.load(Ordering::Relaxed);

        // Build visualisation lines
        let spec = spec_buf.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let vis = panel::build_vis_lines(
            &spec,
            &band_edges,
            &mut bar_peaks,
            &mut bar_peak_hold,
            bar_color,
            &dl_stat,
            dl_b,
            dl_t,
            cfg.calm_mode,
        );

        let vol = *volume.lock().unwrap_or_else(|e| e.into_inner());
        let queue_status = crate::DOWNLOAD_QUEUE.get().and_then(|q| q.status());
        let panel_state = PanelState {
            cover_lines,
            track_name: &track.title,
            artist_name: &track.artist_name,
            album_name: &track.album_name,
            track_label,
            elapsed: elapsed_secs,
            total: total_secs,
            volume: vol,
            paused: paused.load(Ordering::Relaxed),
            dl_status: &dl_stat,
            bar_color,
            vis_lines: &vis,
            is_local,
            show_controls,
            show_controls_hint: cfg.show_controls_hint,
            queue_status,
        };

        if let Some(t) = terminal.as_mut() {
            t.draw(|f| panel::render(f, &panel_state))?;
        }
        if let Some(ctl) = &remote {
            *ctl.visual.lock().unwrap() = crate::player::Visual {
                cover: cover_lines.to_vec(),
                spectrum: vis.clone(),
                color: bar_color,
            };
        }

        // ── Keyboard ─────────────────────────────────────────────────────────
        if terminal.is_none() {
            thread::sleep(Duration::from_millis(25));
        }
        if terminal.is_some() && event::poll(Duration::from_millis(90))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => {
                            stop_all.store(true, Ordering::Relaxed);
                            break "quit".to_string();
                        }
                        KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Right => {
                            stop_all.store(true, Ordering::Relaxed);
                            break "next".to_string();
                        }
                        KeyCode::Char('p') | KeyCode::Left => {
                            stop_all.store(true, Ordering::Relaxed);
                            break "prev".to_string();
                        }
                        KeyCode::Char('r') | KeyCode::Char('R') => {
                            stop_all.store(true, Ordering::Relaxed);
                            break format!("radio:{}", track.id);
                        }
                        KeyCode::Char(' ') => {
                            let p = paused.load(Ordering::Relaxed);
                            paused.store(!p, Ordering::Relaxed);
                            if let Some(ref mut mc) = media_controls {
                                let _ = mc.set_playback(if p {
                                    souvlaki::MediaPlayback::Playing { progress: None }
                                } else {
                                    souvlaki::MediaPlayback::Paused { progress: None }
                                });
                            }
                        }
                        KeyCode::Up | KeyCode::Char('+') | KeyCode::Char('=') => {
                            {
                                let mut v = volume.lock().unwrap_or_else(|e| e.into_inner());
                                *v = vol_up(*v);
                            }
                            let _ = config::save_volume(
                                *volume.lock().unwrap_or_else(|e| e.into_inner()),
                            );
                        }
                        KeyCode::Down | KeyCode::Char('-') => {
                            {
                                let mut v = volume.lock().unwrap_or_else(|e| e.into_inner());
                                *v = vol_down(*v);
                            }
                            let _ = config::save_volume(
                                *volume.lock().unwrap_or_else(|e| e.into_inner()),
                            );
                        }
                        KeyCode::Char('?') => {
                            show_controls = !show_controls;
                        }
                        KeyCode::Char('d') | KeyCode::Char('D') => {
                            if already_saved || is_local {
                                *dl_flash_until.lock().unwrap_or_else(|e| e.into_inner()) =
                                    Some(Instant::now() + Duration::from_secs(2));
                            } else {
                                let mut stat = dl_status.lock().unwrap_or_else(|e| e.into_inner());
                                if stat.is_empty() {
                                    let already_buffered = download_done.load(Ordering::Relaxed);
                                    if already_buffered {
                                        // Streaming done — copy is instant, no bar needed.
                                        // Show status in the info line only.
                                        *stat = "✓ Saving...".to_string();
                                        drop(stat);
                                        let temp_path = path.to_path_buf();
                                        let track_clone = track.clone();
                                        let cover_owned: Option<Vec<u8>> =
                                            cover_bytes.map(|b| b.to_vec());
                                        let out_dir = cfg.output_path();
                                        let dl_status2 = dl_status.clone();
                                        let dummy = Arc::new(AtomicU64::new(0));
                                        thread::spawn(move || {
                                            match save_streamed_track(
                                                &temp_path,
                                                &track_clone,
                                                cover_owned.as_deref(),
                                                &out_dir,
                                                &dummy,
                                            ) {
                                                Ok(_) => {
                                                    *dl_status2
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner()) =
                                                        "✓ Saved".to_string()
                                                }
                                                Err(e) => {
                                                    *dl_status2
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner()) =
                                                        format!("✗ {e}")
                                                }
                                            }
                                        });
                                    } else {
                                        // Still buffering — show the real streaming
                                        // progress bar (dl_bytes/dl_total already animating).
                                        *stat = "⬇ Downloading...".to_string();
                                        drop(stat);
                                        let temp_path = path.to_path_buf();
                                        let track_clone = track.clone();
                                        let cover_owned: Option<Vec<u8>> =
                                            cover_bytes.map(|b| b.to_vec());
                                        let dl_done = download_done.clone();
                                        let out_dir = cfg.output_path();
                                        let dl_status2 = dl_status.clone();
                                        let dummy = Arc::new(AtomicU64::new(0));
                                        thread::spawn(move || {
                                            while !dl_done.load(Ordering::Relaxed) {
                                                thread::sleep(Duration::from_millis(200));
                                            }
                                            match save_streamed_track(
                                                &temp_path,
                                                &track_clone,
                                                cover_owned.as_deref(),
                                                &out_dir,
                                                &dummy,
                                            ) {
                                                Ok(_) => {
                                                    *dl_status2
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner()) =
                                                        "✓ Saved".to_string()
                                                }
                                                Err(e) => {
                                                    *dl_status2
                                                        .lock()
                                                        .unwrap_or_else(|e| e.into_inner()) =
                                                        format!("✗ {e}")
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // ── Media control events (OS media keys / Bluetooth) ─────────────────
        let mut media_nav: Option<String> = None;
        let mut media_changed_pause = false;
        while let Ok(event) = media_rx.try_recv() {
            media_changed_pause |= matches!(
                event,
                souvlaki::MediaControlEvent::Play
                    | souvlaki::MediaControlEvent::Pause
                    | souvlaki::MediaControlEvent::Toggle
            );
            use souvlaki::MediaControlEvent::*;
            match event {
                Play => {
                    paused.store(false, Ordering::Relaxed);
                    if let Some(ref mut mc) = media_controls {
                        let _ =
                            mc.set_playback(souvlaki::MediaPlayback::Playing { progress: None });
                    }
                }
                Pause => {
                    paused.store(true, Ordering::Relaxed);
                    if let Some(ref mut mc) = media_controls {
                        let _ = mc.set_playback(souvlaki::MediaPlayback::Paused { progress: None });
                    }
                }
                Toggle => {
                    let p = paused.load(Ordering::Relaxed);
                    paused.store(!p, Ordering::Relaxed);
                    if let Some(ref mut mc) = media_controls {
                        let _ = mc.set_playback(if p {
                            souvlaki::MediaPlayback::Playing { progress: None }
                        } else {
                            souvlaki::MediaPlayback::Paused { progress: None }
                        });
                    }
                }
                SetPosition(position) => {
                    if let Some(ctl) = &remote {
                        ctl.seek_ms
                            .store(position.0.as_millis() as u64, Ordering::Relaxed);
                    } else {
                        seek_request.store(position.0.as_millis() as u64, Ordering::Relaxed);
                    }
                }
                Seek(direction) | SeekBy(direction, _) => {
                    let milliseconds = if let SeekBy(_, duration) = event {
                        duration.as_millis() as u64
                    } else {
                        10000
                    };
                    let current = current_sample.load(Ordering::Relaxed) * 1000 / device_sr as u64;
                    let target = match direction {
                        souvlaki::SeekDirection::Forward => current.saturating_add(milliseconds),
                        souvlaki::SeekDirection::Backward => current.saturating_sub(milliseconds),
                    };
                    if let Some(ctl) = &remote {
                        ctl.seek_ms.store(target, Ordering::Relaxed);
                    } else {
                        seek_request.store(target, Ordering::Relaxed);
                    }
                }
                SetVolume(value) => {
                    let value = (value as f32).clamp(0.0, 1.0);
                    *volume.lock().unwrap_or_else(|e| e.into_inner()) = value;
                    let _ = config::save_volume(value);
                    #[cfg(target_os = "linux")]
                    if let Some(mc) = media_controls.as_mut() {
                        let _ = mc.set_volume(value as f64);
                    }
                }
                Next => {
                    stop_all.store(true, Ordering::Relaxed);
                    media_nav = Some("next".into());
                    break;
                }
                Previous => {
                    stop_all.store(true, Ordering::Relaxed);
                    media_nav = Some("prev".into());
                    break;
                }
                Stop => {
                    stop_all.store(true, Ordering::Relaxed);
                    media_nav = Some("quit".into());
                    break;
                }
                _ => {}
            }
        }
        if media_changed_pause {
            if let Some(ctl) = &remote {
                ctl.paused
                    .store(paused.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }
        if let Some(nav) = media_nav {
            break nav;
        }

        // Check if playback finished
        if playback_done.load(Ordering::Relaxed)
            && audio_buf
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_empty()
        {
            // If song ended in < 0.5s it's a decode failure, not a real ending
            let played = current_sample.load(Ordering::Relaxed) as f64 / device_sr as f64;
            if played < 0.5 {
                break "fail".to_string();
            }
            break "end".to_string();
        }
    };

    // ── TUI teardown ──────────────────────────────────────────────────────────
    // Persist first palette color for the next song's transition arrow

    // Use teardown_terminal (ignores errors) so a teardown failure never loses result_str
    if let Some(t) = terminal.as_mut() {
        teardown_terminal(t);
    }
    stop_all.store(true, Ordering::Relaxed);

    Ok(result_str)
}

// ─── In-preview save ─────────────────────────────────────────────────────────

/// Copy the fully-streamed temp file to the output dir with a proper name,
/// then embed track metadata and cover art. Updates `progress` as bytes are written.
fn save_streamed_track(
    temp_path: &std::path::Path,
    track: &TrackInfo,
    cover_bytes: Option<&[u8]>,
    output_dir: &str,
    progress: &AtomicU64,
) -> Result<()> {
    use crate::metadata;
    use crate::utils::safe_filename;
    use std::io::{Read, Write};

    let out_dir = std::path::Path::new(output_dir);
    std::fs::create_dir_all(out_dir)?;

    // Detect actual format from the temp file's magic bytes
    let ext = {
        let mut magic = [0u8; 12];
        let n = fs::File::open(temp_path)
            .and_then(|mut fh| {
                use std::io::Read;
                fh.read(&mut magic)
            })
            .unwrap_or(0);
        crate::utils::audio_extension(&magic[..n])
    };
    let filename = safe_filename(&format!("{} - {}.{}", track.artist_name, track.title, ext));
    let dest = out_dir.join(&filename);

    // Copy in chunks so the progress bar actually moves
    let mut src = fs::File::open(temp_path)?;
    let mut dst = fs::File::create(&dest)?;
    let mut buf = vec![0u8; 256 * 1024]; // 256 KB chunks
    let mut written: u64 = 0;
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n])?;
        written += n as u64;
        progress.store(written, Ordering::Relaxed);
    }

    metadata::embed(&dest, track, cover_bytes)?;
    Ok(())
}

// ─── Audio utilities ──────────────────────────────────────────────────────────

fn probe_audio(
    path: &std::path::Path,
    download_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Result<(u32, usize)> {
    let file = fs::File::open(path)?;
    // Use StreamingFile so symphonia's internal seeks wait for data rather than
    // hitting EOF on a partially-downloaded file (required for MP4/isomp4 demuxer).
    let streaming = StreamingFile {
        file,
        download_done,
        stop,
    };
    let mss = MediaSourceStream::new(Box::new(streaming), Default::default());
    let probed = symphonia::default::get_probe().format(
        &Hint::new(),
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let track = probed
        .format
        .default_track()
        .ok_or_else(|| anyhow::anyhow!("No default track"))?;
    let sr = track.codec_params.sample_rate.unwrap_or(44100);
    let ch = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
    Ok((sr, ch))
}

fn probe_total_frames(
    path: &std::path::Path,
    download_done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
) -> Option<u64> {
    let file = fs::File::open(path).ok()?;
    let streaming = StreamingFile {
        file,
        download_done,
        stop,
    };
    let mss = MediaSourceStream::new(Box::new(streaming), Default::default());
    let probed = symphonia::default::get_probe()
        .format(
            &Hint::new(),
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )
        .ok()?;
    probed.format.default_track()?.codec_params.n_frames
}

/// Read whole FLAC, mix to mono, optionally downsample to `target_sr`.
fn load_mono_f32(path: &std::path::Path, target_sr: u32, stop: &AtomicBool) -> Result<Vec<f32>> {
    let file = fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let probed = symphonia::default::get_probe().format(
        &Hint::new(),
        mss,
        &FormatOptions::default(),
        &MetadataOptions::default(),
    )?;
    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| anyhow::anyhow!("No track"))?
        .clone();
    let file_sr = track.codec_params.sample_rate.unwrap_or(44100);
    let ch = track.codec_params.channels.map(|c| c.count()).unwrap_or(2);
    let mut decoder =
        symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut samples: Vec<f32> = Vec::with_capacity(file_sr as usize * 300);
    loop {
        // Bail out early if the song was skipped — frees memory immediately
        if stop.load(Ordering::Relaxed) {
            return Err(anyhow::anyhow!("stopped"));
        }
        match format.next_packet() {
            Ok(packet) => match decoder.decode(&packet) {
                Ok(decoded) => {
                    let mut sbuf =
                        SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
                    sbuf.copy_interleaved_ref(decoded);
                    let s = sbuf.samples();
                    if ch == 2 {
                        samples.extend(s.chunks(2).map(|c| (c[0] + c[1]) * 0.5));
                    } else {
                        samples.extend_from_slice(s);
                    }
                }
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(_) => break,
            },
            Err(_) => break,
        }
    }

    // Nearest-neighbour downsample if needed
    if file_sr != target_sr {
        let ratio = file_sr as f64 / target_sr as f64;
        let new_len = (samples.len() as f64 / ratio) as usize;
        samples = (0..new_len)
            .map(|i| {
                let idx = ((i as f64 * ratio) as usize).min(samples.len() - 1);
                samples[idx]
            })
            .collect();
    }
    Ok(samples)
}

// ─── Drop + beat detection ────────────────────────────────────────────────────

fn detect_beats(audio: &[f32], sr: u32) -> Vec<f64> {
    let hop = (0.05 * sr as f64) as usize;
    if hop == 0 || audio.len() < hop {
        return Vec::new();
    }
    let n = audio.len() / hop;
    let energy: Vec<f32> = (0..n)
        .map(|i| {
            let seg = &audio[i * hop..(i + 1) * hop];
            seg.iter().map(|&s| s * s).sum::<f32>() / hop as f32
        })
        .collect();
    let onset: Vec<f32> = energy.windows(2).map(|w| (w[1] - w[0]).max(0.0)).collect();
    let height = percentile(&onset, 65.0);
    let min_dist = (0.25 / 0.05) as usize;
    find_peaks(&onset, Some(height), Some(min_dist))
        .into_iter()
        .map(|i| i as f64 * 0.05)
        .collect()
}

fn detect_drops(audio: &[f32], sr: u32) -> Vec<f64> {
    const FRAME_SEC: f64 = 0.1;
    const SUPPRESS_SEC: f64 = 30.0;
    const MIN_DROP_SEC: f64 = 25.0;
    const MAX_DROPS: usize = 3;
    const CTX_BACK_SEC: f64 = 20.0;
    const CTX_FRONT_SEC: f64 = 3.0;
    const SUSTAIN_SEC: f64 = 10.0;
    const DELTA_DB: f64 = 2.0;

    let frame_step = (FRAME_SEC * sr as f64) as usize;
    if frame_step == 0 {
        return Vec::new();
    }
    let n = audio.len() / frame_step;
    let lookback = (2.0 * sr as f64) as usize;
    let ctx_back = (CTX_BACK_SEC / FRAME_SEC) as usize;
    let ctx_front = (CTX_FRONT_SEC / FRAME_SEC) as usize;
    let sustain = (SUSTAIN_SEC / FRAME_SEC) as usize;

    let lb: Vec<f64> = (0..n)
        .map(|fi| {
            let pos = fi * frame_step;
            let seg = &audio[pos.saturating_sub(lookback)..pos];
            if seg.is_empty() {
                return -90.0;
            }
            let rms = (seg.iter().map(|&s| (s * s) as f64).sum::<f64>() / seg.len() as f64).sqrt();
            20.0 * (rms + 1e-9).log10()
        })
        .collect();

    let long_rms_db = {
        let rms = (audio.iter().map(|&s| (s * s) as f64).sum::<f64>() / audio.len() as f64).sqrt();
        20.0 * (rms + 1e-9).log10()
    };

    let quiet_thresh = long_rms_db - 1.5;
    let loud_thresh = long_rms_db;
    let sustain_thresh = long_rms_db;

    let mut drops = Vec::new();
    let mut last_drop = -SUPPRESS_SEC;

    for fi in 0..n {
        if drops.len() >= MAX_DROPS {
            break;
        }
        let ft = fi as f64 * FRAME_SEC;
        if ft < MIN_DROP_SEC {
            continue;
        }

        let cs = fi.saturating_sub(ctx_back);
        let ce = fi.saturating_sub(ctx_front);
        if ce <= cs {
            continue;
        }
        let lb_ctx = lb[cs..ce].iter().cloned().fold(f64::INFINITY, f64::min);
        let lb_now = lb[fi];
        let lb_delta = lb_now - lb_ctx;
        let lb_fut = lb[fi..n.min(fi + sustain)].iter().sum::<f64>()
            / (n.min(fi + sustain) - fi).max(1) as f64;

        if lb_ctx < quiet_thresh
            && lb_now > loud_thresh
            && lb_delta > DELTA_DB
            && lb_fut > sustain_thresh
            && ft - last_drop > SUPPRESS_SEC
        {
            drops.push(ft);
            last_drop = ft;
        }
    }
    drops
}

fn find_peaks(x: &[f32], height: Option<f32>, distance: Option<usize>) -> Vec<usize> {
    let mut peaks = Vec::new();
    for i in 1..x.len().saturating_sub(1) {
        if x[i] > x[i - 1] && x[i] > x[i + 1] {
            if height.map_or(true, |h| x[i] >= h) {
                peaks.push(i);
            }
        }
    }
    if let Some(dist) = distance {
        let mut filtered = Vec::new();
        for p in peaks {
            if filtered.last().map_or(true, |&last| p - last >= dist) {
                filtered.push(p);
            }
        }
        filtered
    } else {
        peaks
    }
}

fn percentile(data: &[f32], p: f64) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let mut sorted = data.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = (p / 100.0 * (sorted.len() - 1) as f64) as usize;
    sorted[idx]
}

/// Convert interleaved audio from (file_ch, file_sr) to (device_ch, device_sr).
fn convert_audio(
    input: &[f32],
    file_ch: usize,
    device_ch: usize,
    file_sr: u32,
    device_sr: u32,
) -> Vec<f32> {
    let file_ch = file_ch.max(1);
    let device_ch = device_ch.max(1);
    let frames_in = input.len() / file_ch;

    // Step 1: channel conversion (produce mono-ish or stereo from file channels)
    let mono_or_converted: Vec<f32> = if file_ch == device_ch {
        input.to_vec()
    } else if device_ch == 1 {
        // Mix all file channels down to mono
        (0..frames_in)
            .map(|f| {
                let sum: f32 = (0..file_ch).map(|c| input[f * file_ch + c]).sum();
                sum / file_ch as f32
            })
            .collect()
    } else if file_ch == 1 {
        // Duplicate mono to all device channels
        let mut out = Vec::with_capacity(frames_in * device_ch);
        for f in 0..frames_in {
            let s = input[f];
            for _ in 0..device_ch {
                out.push(s);
            }
        }
        out
    } else {
        // Generic: mix file channels to stereo (left = ch0, right = ch1, rest ignored)
        let mut out = Vec::with_capacity(frames_in * device_ch);
        for f in 0..frames_in {
            for dc in 0..device_ch {
                let fc = dc.min(file_ch - 1);
                out.push(input[f * file_ch + fc]);
            }
        }
        out
    };

    // Step 2: sample rate conversion via linear interpolation
    if file_sr == device_sr {
        return mono_or_converted;
    }
    let ratio = file_sr as f64 / device_sr as f64;
    let frames_out = (frames_in as f64 / ratio).ceil() as usize;
    let mut out = Vec::with_capacity(frames_out * device_ch);
    for i in 0..frames_out {
        let pos = i as f64 * ratio;
        let idx0 = pos as usize;
        let idx1 = (idx0 + 1).min(frames_in.saturating_sub(1));
        let frac = (pos - idx0 as f64) as f32;
        for c in 0..device_ch {
            let s0 = mono_or_converted[idx0 * device_ch + c];
            let s1 = mono_or_converted[idx1 * device_ch + c];
            out.push(s0 + (s1 - s0) * frac);
        }
    }
    out
}

#[cfg(test)]
mod engine_tests {
    use super::*;
    /// ALSA null output makes this deterministic on machines without speakers.
    #[test]
    #[ignore = "requires audio output; see docs/testing.md for the ALSA null device"]
    fn audio_engine_pauses_seeks_and_stops() {
        let ctl = Arc::new(crate::player::Transport::new(
            Arc::new(Mutex::new(0.1)),
            0,
            true,
            1,
        ));
        let remote = ctl.clone();
        let worker = thread::spawn(move || {
            let mut track = crate::player::test_track(1);
            track.duration = 12;
            play(
                None,
                std::path::Path::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/fixtures/silence.flac"
                )),
                Arc::new(AtomicBool::new(true)),
                Arc::new(AtomicU64::new(0)),
                Arc::new(AtomicU64::new(0)),
                &track,
                None,
                false,
                None,
                remote.volume.clone(),
                true,
                false,
                Some(remote),
                None,
            )
        });
        let start = Instant::now();
        while ctl.visual.lock().unwrap().cover.is_empty()
            && start.elapsed() < Duration::from_secs(5)
        {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            !ctl.visual.lock().unwrap().cover.is_empty(),
            "audio engine did not initialise"
        );
        assert_eq!(ctl.position_ms.load(Ordering::Relaxed), 0);
        ctl.seek_ms.store(5000, Ordering::Relaxed);
        let start = Instant::now();
        while ctl.position_ms.load(Ordering::Relaxed) < 4900
            && start.elapsed() < Duration::from_secs(3)
        {
            thread::sleep(Duration::from_millis(20));
        }
        let position = ctl.position_ms.load(Ordering::Relaxed);
        ctl.stop.store(true, Ordering::Relaxed);
        assert_eq!(worker.join().unwrap().unwrap(), "quit");
        assert!(
            (4900..=5100).contains(&position),
            "seek position was {position}"
        );
    }
}
