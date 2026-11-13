# Verification

`cargo test --all-targets` runs queue/shuffle/repeat/history tests, mock HTTP tests for pagination and playlist/favourite edits, startup album resolution, CLI validation, and TUI navigation/rendering at several terminal sizes. These tests do not need a Tidal account or audio output.

The opt-in audio smoke test uses the checked-in 12-second silent FLAC fixture. On Linux, a null ALSA output lets it exercise the real decoder/output/seek/pause/stop path without physical speakers:

```sh
cat > /tmp/lumitide-alsa-null.conf <<'ALSA'
pcm.!default {
    type null
    hint { show on description "Silent test output" }
}
ALSA
ALSA_CONFIG_PATH=/tmp/lumitide-alsa-null.conf \
  cargo test audio_engine_pauses_seeks_and_stops -- --ignored --nocapture
```

The fixture was generated with:

```sh
ffmpeg -f lavfi -i anullsrc=r=44100:cl=stereo -t 12 -c:a flac -map_metadata -1 silence.flac
```

Live Tidal account tests remain opt-in. Mock requests verify paths, bodies, pagination, mixed video/track indices, and conditional edit headers; they cannot verify account permissions, current service availability, or individual album availability. Desktop palette parsers and live reload can be tested with generated JSON files; an actual Noctalia desktop is not required.
