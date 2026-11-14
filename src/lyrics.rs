//! Optional lyrics from LRCLIB. This module never blocks the audio worker.
use crate::api::TrackInfo;
use anyhow::Result;
use serde::Deserialize;

#[derive(Clone, Debug)]
pub struct LyricLine {
    pub at_ms: u64,
    pub text: String,
}
#[derive(Clone, Debug)]
pub struct Lyrics {
    pub plain: String,
    pub synced: Vec<LyricLine>,
    pub instrumental: bool,
}
impl Lyrics {
    pub fn active_line(&self, position_ms: u64) -> Option<usize> {
        self.synced
            .partition_point(|line| line.at_ms <= position_ms)
            .checked_sub(1)
    }
    pub fn lines(&self) -> Vec<&str> {
        if self.synced.is_empty() {
            self.plain.lines().collect()
        } else {
            self.synced.iter().map(|l| l.text.as_str()).collect()
        }
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    #[serde(default)]
    track_name: String,
    #[serde(default)]
    artist_name: String,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    instrumental: bool,
    plain_lyrics: Option<String>,
    synced_lyrics: Option<String>,
}
impl Record {
    fn into_lyrics(self) -> Option<Lyrics> {
        let synced = self
            .synced_lyrics
            .as_deref()
            .map(parse_lrc)
            .unwrap_or_default();
        let plain = self.plain_lyrics.unwrap_or_default();
        (self.instrumental || !plain.trim().is_empty() || !synced.is_empty()).then_some(Lyrics {
            plain,
            synced,
            instrumental: self.instrumental,
        })
    }
}
fn timestamp(text: &str) -> Option<u64> {
    let (minutes, seconds) = text.split_once(':')?;
    let minutes = minutes.parse::<u64>().ok()?;
    let seconds = seconds.parse::<f64>().ok()?;
    if !seconds.is_finite() || !(0.0..60.0).contains(&seconds) {
        return None;
    }
    minutes
        .checked_mul(60000)?
        .checked_add((seconds * 1000.0).round() as u64)
}
pub fn parse_lrc(text: &str) -> Vec<LyricLine> {
    let mut lines = vec![];
    let mut offset = 0i64;
    for raw in text.lines() {
        let mut rest = raw.trim().trim_start_matches('\u{feff}');
        let mut times = vec![];
        while let Some(after) = rest.strip_prefix('[') {
            let Some((tag, after)) = after.split_once(']') else {
                break;
            };
            if let Some(value) = tag.strip_prefix("offset:") {
                if let Ok(value) = value.trim().parse() {
                    offset = value;
                }
            } else if let Some(time) = timestamp(tag) {
                times.push(time);
            }
            rest = after;
        }
        for at_ms in times {
            lines.push(LyricLine {
                at_ms,
                text: rest.trim().to_owned(),
            });
        }
    }
    for line in &mut lines {
        line.at_ms = line.at_ms.saturating_add_signed(offset);
    }
    lines.sort_by_key(|line| line.at_ms);
    lines
}
pub fn fetch(track: &TrackInfo) -> Result<Option<Lyrics>> {
    fetch_from(track, "https://lrclib.net/api")
}
fn fetch_from(track: &TrackInfo, base: &str) -> Result<Option<Lyrics>> {
    let http = reqwest::blocking::Client::builder()
        .user_agent("Lumitide/1.5.2 (https://github.com/JCooper-Bit/lumitide)")
        .timeout(std::time::Duration::from_secs(10))
        .build()?;
    let mut query = vec![
        ("artist_name", track.artist_name.clone()),
        ("track_name", track.title.clone()),
    ];
    if !track.album_name.is_empty() {
        query.push(("album_name", track.album_name.clone()));
    }
    if track.duration > 0 {
        query.push(("duration", track.duration.to_string()));
    }
    let response = http.get(format!("{base}/get")).query(&query).send()?;
    if response.status().is_success() {
        if let Some(lyrics) = response.json::<Record>()?.into_lyrics() {
            return Ok(Some(lyrics));
        }
    } else if response.status() != reqwest::StatusCode::NOT_FOUND {
        response.error_for_status()?;
    }
    let mut records: Vec<Record> = http
        .get(format!("{base}/search"))
        .query(&[
            ("track_name", &track.title),
            ("artist_name", &track.artist_name),
        ])
        .send()?
        .error_for_status()?
        .json()?;
    records.retain(|r| {
        r.track_name.eq_ignore_ascii_case(&track.title)
            && r.artist_name.eq_ignore_ascii_case(&track.artist_name)
            && (track.duration == 0 || (r.duration - track.duration as f64).abs() <= 5.0)
    });
    records.sort_by(|a, b| {
        (a.duration - track.duration as f64)
            .abs()
            .total_cmp(&(b.duration - track.duration as f64).abs())
    });
    Ok(records.into_iter().find_map(Record::into_lyrics))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        player::test_track,
        test_support::{Reply, Server},
    };
    #[test]
    fn timestamps_offsets_multiple_tags_and_seek() {
        let lines=parse_lrc("[ar:Example]\n[offset:-500]\n[00:01.25][00:03.000]First line\n[00:02.50]Second line\n[bad]Ignored");
        let lyrics = Lyrics {
            plain: String::new(),
            synced: lines,
            instrumental: false,
        };
        assert_eq!(
            lyrics.synced.iter().map(|l| l.at_ms).collect::<Vec<_>>(),
            vec![750, 2000, 2500]
        );
        assert_eq!(lyrics.active_line(0), None);
        assert_eq!(lyrics.active_line(2200), Some(1));
        assert_eq!(lyrics.active_line(800), Some(0));
    }
    #[test]
    fn fetches_synced_and_plain_lyrics_without_tidal_auth() {
        let server = Server::new(vec![Reply::json(
            serde_json::json!({"plainLyrics":"First line\nSecond line","syncedLyrics":"[00:00.00]First line\n[00:02.00]Second line","instrumental":false}),
        )]);
        let lyrics = fetch_from(&test_track(1), &server.url).unwrap().unwrap();
        assert_eq!(lyrics.active_line(2500), Some(1));
        assert!(server.requests.lock().unwrap()[0].contains("duration=180"));
    }
    #[test]
    fn fallback_matches_duration_and_handles_missing_and_instrumental() {
        let server = Server::new(vec![
            Reply {
                status: 404,
                headers: String::new(),
                body: String::new(),
            },
            Reply::json(serde_json::json!([
                {"trackName":"Track 1","artistName":"Artist","duration":20,"plainLyrics":"Wrong version"},
                {"trackName":"Track 1","artistName":"Artist","duration":180,"plainLyrics":"Matching line"}
            ])),
            Reply::json(serde_json::json!({"instrumental":true})),
        ]);
        let lyrics = fetch_from(&test_track(1), &server.url).unwrap().unwrap();
        assert_eq!(lyrics.plain, "Matching line");
        assert!(lyrics.synced.is_empty());
        assert!(
            fetch_from(&test_track(2), &server.url)
                .unwrap()
                .unwrap()
                .instrumental
        );
        let missing = Server::new(vec![
            Reply {
                status: 404,
                headers: String::new(),
                body: String::new(),
            },
            Reply::json(serde_json::json!([])),
        ]);
        assert!(fetch_from(&test_track(3), &missing.url).unwrap().is_none());
    }
}
